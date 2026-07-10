//! GUI projection state for the third `SessionUpdate` consumer.
//!
//! `GuiProjector` is the state half of the GUI consumer: a single
//! history-version debounce cursor. Its `consume` method projects
//! `Session` / `VisoEngine` / `RunnerClient` state into `GuiState`
//! and additionally reads the History cursor.

use web_time::{Instant, UNIX_EPOCH};

use foldit_gui::{
    CheckpointInfo, CheckpointKindTag, DirtyFlags, FilterStatus, GuiState, HistoryLiveUpdate,
    HistorySection, SegmentInfo, SegmentTarget, TextBubbleButton, TextBubblePayload, WireId,
};
use viso::{Focus, VisoEngine};

use crate::app::score_coordinator::ScoreCoordinator;
use crate::history::{CheckpointKind, FilterStatus as HistoryFilterStatus, History};
use crate::runner_client::RunnerClient;
use crate::session::{Puzzle, Session, SessionUpdate, SessionUpdateConsumer};

/// State for the GUI consumer.
pub struct GuiProjector {
    /// Debounce cursor for the history channel (topology + live).
    pub(crate) history_sync: HistorySyncCursor,
    /// Accumulator of App-owned GUI dirty bits the `SessionUpdate` batch cannot
    /// express (segment), plus the full-populate seed
    /// (`DirtyFlags::all()`) raised on session birth. Drained at `consume`.
    pending: DirtyFlags,
}

impl GuiProjector {
    pub(crate) const fn new() -> Self {
        Self {
            history_sync: HistorySyncCursor {
                topology: None,
                live: None,
                live_push_at: None,
            },
            pending: DirtyFlags::empty(),
        }
    }

    /// OR-accumulate App-owned dirty bits to drain on the next `consume`.
    pub(crate) fn mark_dirty(&mut self, flags: DirtyFlags) {
        self.pending |= flags;
    }
}

/// Tracks the last history versions pushed to the frontend so the GUI
/// consumer can debounce/skip redundant reprojections.
pub struct HistorySyncCursor {
    /// Last `History::topology_version()` pushed. `None` forces an
    /// initial push (no `u64::MAX` sentinel).
    pub(crate) topology: Option<u64>,
    /// Last `History::live_version()` pushed; mid-action score updates only.
    pub(crate) live: Option<u64>,
    /// Wall-clock of the last live push. Gates the 50ms (20Hz) debounce.
    pub(crate) live_push_at: Option<Instant>,
}

// `f64` is the wire type (JS reads it as a `number`). Epoch-millis stays
// far below f64's 2^53 exact-integer ceiling, so no precision is lost.
#[allow(clippy::cast_precision_loss)]
fn timestamp_ms(t: web_time::SystemTime) -> f64 {
    t.duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_millis() as f64)
}

/// Convert a parsed [`crate::puzzle_toml::Bubble`] into the GUI-bound IPC
/// twin. Tier-1 conversion: text/color/image pass through; buttons are
/// built from `bubble.button` (defaulting to `"Next"`) plus an optional
/// `alt_button`, with `goto` left `None` since clicks close locally.
fn bubble_to_payload(b: &crate::puzzle_toml::Bubble) -> TextBubblePayload {
    let mut buttons = vec![TextBubbleButton {
        text: b.button.clone().unwrap_or_else(|| "Next".to_owned()),
        goto: None,
    }];
    if let Some(alt) = b.alt_button.as_ref() {
        buttons.push(TextBubbleButton {
            text: alt.clone(),
            goto: None,
        });
    }
    TextBubblePayload {
        text: b.text.clone(),
        color: b.color.clone(),
        image: b.image.clone(),
        buttons,
    }
}

const fn checkpoint_kind_tag(k: &CheckpointKind) -> CheckpointKindTag {
    match k {
        CheckpointKind::Loaded { .. } => CheckpointKindTag::Load,
        CheckpointKind::PromotedPreview { .. } => CheckpointKindTag::PromotedPreview,
        CheckpointKind::AddEntity { .. } => CheckpointKindTag::AddEntity,
        CheckpointKind::LaneUndo { .. } => CheckpointKindTag::LaneUndo,
        CheckpointKind::PluginOp { .. } => CheckpointKindTag::PluginOp,
        CheckpointKind::NativeEdit { .. } => CheckpointKindTag::NativeEdit,
    }
}

const fn filter_status_wire(s: &HistoryFilterStatus) -> FilterStatus {
    match s {
        HistoryFilterStatus::Pass => FilterStatus::Pass,
        HistoryFilterStatus::Fail(_) => FilterStatus::Fail,
        HistoryFilterStatus::NotEvaluated => FilterStatus::NotEvaluated,
    }
}

/// Project the backend `History` into the wire payload consumed by
/// the `HistoryPanel`. Also called at-site from `Session::apply_history_command`
/// for curation changes that don't bump `topology_version`.
// `topology_version` is `f64` on the wire (JS `number`); the counter
// increments per topology change and stays far below f64's 2^53 ceiling.
#[allow(clippy::cast_precision_loss)]
pub fn project_history(store: &Session) -> HistorySection {
    let history = store.history();
    let cps = history.checkpoints();
    let head_id = cps.head();
    let root_id = cps.root();

    let checkpoints: Vec<CheckpointInfo> = cps
        .iter()
        .map(|(id, ckpt)| {
            let entity_heads = ckpt
                .entity_heads
                .iter()
                .map(|(eid, snap)| (*eid, WireId::new(*snap)))
                .collect();
            CheckpointInfo {
                id: WireId::new(id),
                parent: ckpt.parent.map(WireId::new),
                children: ckpt.children.iter().copied().map(WireId::new).collect(),
                entity_heads,
                entity: ckpt.kind.entity(),
                kind: checkpoint_kind_tag(&ckpt.kind),
                label: ckpt.label.to_string(),
                timestamp_ms: timestamp_ms(ckpt.timestamp),
                raw_score: ckpt.raw_score,
                game_score: ckpt.game_score,
                filter_status: filter_status_wire(&ckpt.filter_status),
                // No committed checkpoint is ever tentative.
                tentative: false,
                pinned: cps.is_pinned(id),
                exclude_from_best: ckpt.exclude_from_best,
            }
        })
        .collect();

    HistorySection {
        checkpoints,
        checkpoint_head: Some(WireId::new(head_id)),
        checkpoint_root: Some(WireId::new(root_id)),
        best: cps.best().map(WireId::new),
        best_that_counts: cps.best_that_counts().map(WireId::new),
        topology_version: history.topology_version() as f64,
    }
}

/// Build the small `HistoryLiveUpdate` payload for the current head
/// (always the tentative when `ongoing == Active`; when Idle, the head
/// is the recently-stamped checkpoint).
fn project_history_live(history: &History) -> Option<HistoryLiveUpdate> {
    let head_id = history.checkpoints().head();
    let ckpt = history.checkpoint(head_id)?;
    Some(HistoryLiveUpdate {
        checkpoint_id: WireId::new(head_id),
        raw_score: ckpt.raw_score,
        game_score: ckpt.game_score,
        label: ckpt.label.to_string(),
        filter_status: filter_status_wire(&ckpt.filter_status),
    })
}

/// The `TextBubblePayload` for the active puzzle's current bubble, or
/// `None` when no puzzle is loaded, the puzzle has no tutorial sequence, or
/// the cursor has walked past the last bubble.
fn current_bubble_payload(puzzle: Option<&Puzzle>) -> Option<TextBubblePayload> {
    let puzzle = puzzle?;
    let cursor = puzzle.current_bubble?;
    puzzle.bubbles.as_ref()?.get(cursor).map(bubble_to_payload)
}

/// The disjoint borrows the GUI projection reads. Named explicitly (not
/// `&App`) so the projection's real dependencies are visible at the call
/// site rather than hidden behind a god-object borrow.
pub struct GuiSources<'a> {
    pub session: &'a Session,
    pub engine: &'a VisoEngine,
    pub driver: &'a RunnerClient,
    /// Host resource access - the view-preset directory listing for the
    /// `VIEW` section. Read only on `not(wasm)`.
    pub host: &'a dyn crate::HostResources,
    pub scores: &'a ScoreCoordinator,
}

/// Project the live `Session` / `VisoEngine` / `RunnerClient` state into
/// `GuiState`. Per-section dirtiness is derived from the drained `updates`
/// batch OR'd with the owned `pending` accumulator (drained here).
///
/// Returns `true` when the segment arm auto-closed (the cached target no
/// longer resolves) so the App clears its copy of the open target.
impl SessionUpdateConsumer for GuiProjector {
    type Sources<'a> = GuiSources<'a>;
    type Sink = GuiState;
    type Out = bool;
    fn consume(
        &mut self,
        updates: &[SessionUpdate],
        sources: GuiSources<'_>,
        frontend: &mut GuiState,
    ) -> bool {
        // FPS and selected count change every frame - always push them.
        frontend.set_fps(sources.engine.fps());
        frontend.ui.selected_count = sources.session.selection_total_count();

        let dirty = compute_dirty(updates) | std::mem::take(&mut self.pending);
        let force_history = updates
            .iter()
            .any(|u| matches!(u, SessionUpdate::CurationChanged));

        if dirty.is_empty() && !force_history {
            return false;
        }

        // PUZZLE before SCORE: a fresh `set_puzzle_*` resets `complete=false`,
        // and then the score check below can latch victory in the same frame
        // without being overwritten.
        if dirty.contains(DirtyFlags::PUZZLE) {
            project_puzzle(sources.session, frontend);
        }
        if dirty.contains(DirtyFlags::TEXT_BUBBLE) {
            frontend.set_text_bubble(current_bubble_payload(sources.session.puzzle()));
        }
        if dirty.contains(DirtyFlags::SCORE) {
            project_score(sources.session, frontend);
        }
        if dirty.contains(DirtyFlags::ACTIONS) {
            project_actions(sources.session, sources.driver, frontend);
        }
        if dirty.contains(DirtyFlags::VIEW) {
            project_view(sources.host, frontend);
        }
        if dirty.contains(DirtyFlags::SELECTION) {
            project_selection(sources.session, frontend);
        }
        if dirty.contains(DirtyFlags::SCENE) {
            project_scene(sources.session, sources.engine, frontend);
        }
        let auto_closed = if dirty.contains(DirtyFlags::SEGMENT) {
            let (info, closed) = project_segment(
                frontend.segment_target(),
                sources.session,
                sources.scores,
                sources.engine,
            );
            frontend.set_segment_info(info);
            closed
        } else {
            false
        };

        sync_history(
            &mut self.history_sync,
            sources.session,
            frontend,
            force_history,
        );
        auto_closed
    }
}

/// Derive the dirty section set for this batch: the per-variant fold mapping
/// each `SessionUpdate` to the GUI sections it invalidates.
fn compute_dirty(updates: &[SessionUpdate]) -> DirtyFlags {
    let mut dirty = DirtyFlags::empty();
    for update in updates {
        dirty |= match update {
            // SEGMENT (not SELECTION): a score tick refreshes the open
            // segment's energies. Identity + SS are cached on the target
            // and never recomputed here, so no DSSP runs per tick.
            SessionUpdate::ScoresChanged => DirtyFlags::SCORE | DirtyFlags::SEGMENT,
            SessionUpdate::Edit { tentative: true }
            | SessionUpdate::PreviewUpdated
            | SessionUpdate::EntityAppearanceChanged => DirtyFlags::SCENE,
            SessionUpdate::Edit { tentative: false }
            | SessionUpdate::PreviewAdded
            | SessionUpdate::PreviewDiscarded
            | SessionUpdate::FocusChanged => DirtyFlags::SCENE | DirtyFlags::ACTIONS,
            SessionUpdate::HeadMoved { .. } => {
                DirtyFlags::SCENE | DirtyFlags::SCORE | DirtyFlags::ACTIONS
            }
            SessionUpdate::ViewOptionsChanged => DirtyFlags::VIEW,
            SessionUpdate::SelectionChanged => DirtyFlags::SELECTION | DirtyFlags::ACTIONS,
            SessionUpdate::BubbleChanged => DirtyFlags::TEXT_BUBBLE,
            SessionUpdate::PuzzleChanged => DirtyFlags::PUZZLE,
            // Drives the full history push via `force_history`, not a section.
            SessionUpdate::CurationChanged => DirtyFlags::empty(),
        };
    }
    dirty
}

/// Project the `PUZZLE` section: the puzzle-panel title/objective plus the
/// puzzle-swap bubble push.
fn project_puzzle(session: &Session, frontend: &mut GuiState) {
    // The puzzle panel's title is the standalone session title,
    // which on a puzzle load equals the puzzle name.
    match session.puzzle() {
        Some(p) => frontend.set_puzzle_game(
            p.id,
            session.title().to_owned(),
            p.start_energy,
            p.completion_energy,
        ),
        // The free-form session has no objective; the title is the
        // file-derived structure name.
        None => frontend.set_puzzle_scientist(session.title().to_owned()),
    }
    // Bubble push on puzzle swap: render the cursor's current
    // bubble (always index 0 right after a puzzle load, since the
    // cursor starts there). Subsequent AdvanceBubble actions
    // re-push via the DirtyFlags::TEXT_BUBBLE arm below.
    frontend.set_text_bubble(current_bubble_payload(session.puzzle()));
}

/// Project the `SCORE` section: the display score plus the puzzle victory
/// latch.
fn project_score(session: &Session, frontend: &mut GuiState) {
    if let Some(score) = session.display_score() {
        frontend.set_score(score, false);
        if let Some(p) = session.puzzle() {
            // Record the display score into high-score progress.
            // `record_progress` is monotonic (writes only on positive +
            // beats-best), so it is the sole gate the menu's unlock math
            // reads; a puzzle counts complete once its best is positive.
            frontend.record_progress(p.id, score);
            // Victory check: with a puzzle loaded, latch it complete the
            // first time the score crosses the toml completion energy.
            // Higher game score = better fold (game-score formula
            // negates), so the comparison is `>=`.
            if p.completion_energy > 0.0 && score >= p.completion_energy {
                frontend.mark_puzzle_complete();
            }
        }
    }
}

/// Project the `SEGMENT` section: the per-residue info panel.
///
/// Identity and SS come from the cached `target`; only the energies and
/// the screen anchor are rebuilt here, so a streaming score never re-runs
/// DSSP. Returns the fresh `segment_info` payload (`None` clears the panel)
/// and `true` when the cached target no longer resolves (entity or residue
/// gone) so the App drops its copy. The caller applies the payload, keeping
/// the target read (a borrow of `GuiState`) disjoint from the write.
fn project_segment(
    target: Option<&SegmentTarget>,
    session: &Session,
    scores: &ScoreCoordinator,
    engine: &VisoEngine,
) -> (Option<SegmentInfo>, bool) {
    let Some(target) = target else {
        return (None, false);
    };

    // Auto-close when the target stops resolving (topology swap, entity or
    // residue removed). The cached identity is stale at that point.
    let Some(entity) = session.entity(target.entity) else {
        return (None, true);
    };
    let still_present = entity.residues().is_some_and(|r| target.residue < r.len());
    if !still_present {
        return (None, true);
    }

    // Fresh energies: the raw per-term row for this residue zipped against
    // the session term names, plus the weighted scalar. Empty / zero when
    // no breakdown is stamped yet (right after load, or on wasm).
    let term_names = scores.term_names().to_vec();
    let (term_values, weighted) = session.current_composition_breakdown().map_or_else(
        || (Vec::new(), 0.0_f32),
        |breakdown| {
            let term_values = breakdown
                .per_residue_terms
                .iter()
                .find(|rts| {
                    rts.entity_id == target.entity && rts.residue_index as usize == target.residue
                })
                .map(|row| row.terms.clone())
                .unwrap_or_default();
            #[allow(
                clippy::cast_possible_truncation,
                reason = "the panel's weighted scalar is a display value; f32 precision suffices"
            )]
            let weighted = breakdown
                .weighted_per_residue(scores.term_names(), scores.term_weights())
                .into_iter()
                .find(|(eid, res, _)| *eid == target.entity && *res as usize == target.residue)
                .map_or(0.0, |(_, _, score)| score as f32);
            (term_values, weighted)
        },
    );

    // Fresh anchor: the residue's CA atom projected to the screen. `None`
    // when off-screen / behind the camera (the panel hides its tail).
    let anchor = ca_world_position(entity, target.residue)
        .and_then(|world| engine.world_to_screen(world))
        .map(|v| (v.x, v.y));

    let info = SegmentInfo {
        residue_number: target.residue_number,
        chain: target.chain.clone(),
        aa_three: target.aa_three.clone(),
        aa_one: target.aa_one.clone(),
        ss_label: target.ss_label.clone(),
        term_names,
        term_values,
        weighted,
        anchor,
    };
    (Some(info), false)
}

/// World position of a residue's CA atom, or `None` for a non-protein
/// entity or a residue with no CA in its atom range.
pub fn ca_world_position(entity: &molex::MoleculeEntity, residue: usize) -> Option<glam::Vec3> {
    let protein = entity.as_protein()?;
    let range = protein.residues.get(residue)?.atom_range.clone();
    let names = protein.columns.name.get(range.clone())?;
    let positions = protein.columns.position.get(range)?;
    names
        .iter()
        .zip(positions)
        .find(|(name, _)| *name == b"CA  ")
        .map(|(_, pos)| *pos)
}

/// Resolve `(eid, residue)` into a segment target, computing the residue
/// identity (number, chain, amino acid) and its secondary structure once
/// via a single `recompute_ss()` over the head assembly and caching them
/// on the target. `None` when the entity or residue does not resolve.
pub fn resolve_segment_target(
    session: &Session,
    eid: molex::EntityId,
    residue: usize,
) -> Option<SegmentTarget> {
    let entity = session.entity(eid)?;
    let res = entity.residues()?.get(residue)?;
    let residue_number = res.seq_id();
    let chain = entity
        .pdb_chain_id()
        .map_or_else(String::new, str::to_owned);
    let aa = molex::chemistry::AminoAcid::from_code(res.name);
    let aa_three = String::from_utf8_lossy(&res.name).trim().to_owned();
    let aa_one = aa.map_or_else(String::new, |a| (a.one_letter() as char).to_string());

    let mut assembly = session.head_assembly();
    assembly.recompute_ss();
    let ss_label = foldit_gui::ss_label(assembly.ss_types(eid).get(residue).copied());

    Some(SegmentTarget {
        entity: eid,
        residue,
        residue_number,
        chain,
        aa_three,
        aa_one,
        ss_label,
    })
}

/// Project the `ACTIONS` section: the focus- + selection-aware op catalog.
fn project_actions(session: &Session, driver: &RunnerClient, frontend: &mut GuiState) {
    // Availability depends on focus + selection + lock state.
    // Source focus from the authoritative session (same as the
    // SCENE arm below), then hand the driver the selection + an
    // entity-type closure.
    let focus = match session.focus() {
        Focus::Entity(eid) => Some(eid),
        Focus::All => None,
    };
    // Design gate: a property of the current focus-scoped selection, so it
    // is computed once and folded into each design-gated op's `enabled`
    // inside the driver. The design mask is host-owned and never reaches
    // the orchestrator.
    let selection_designable = session.selection_is_designable();
    let actions = driver.actions_catalog(focus, session.selection(), selection_designable, |id| {
        session.entity_type(id)
    });
    let groups = driver.plugin_groups();
    let running = driver.running_actions();
    frontend.set_actions(actions, groups, running, focus.map(|e| e.raw()));
    frontend.set_op_progress(driver.op_progress());
}

/// Project the `VIEW` section: view options, the static schema, and the
/// host-sourced preset list.
fn project_view(host: &dyn crate::HostResources, frontend: &mut GuiState) {
    // Source of truth is the frontend's faithful (sparse) view options, not
    // the engine: the engine is a follower that the tick re-applies on
    // `ViewOptionsChanged`. Deserialize back into viso's options, then
    // densify the `display` group — the only sparse one (its
    // `DisplayOverrides` drop `None` fields on serialization) — so the
    // settings panel reads each control's effective value instead of falling
    // back to a control minimum. The other groups are already dense.
    let mut display_dense: viso::options::VisoOptions =
        serde_json::from_value(frontend.view_options_raw().clone()).unwrap_or_default();
    display_dense.display = display_dense.display.with_resolved_overrides();
    let dense = serde_json::to_value(&display_dense).unwrap_or_default();
    frontend.set_view_options_dense(dense);

    if frontend.view.options_schema.is_null() {
        frontend.view.options_schema =
            serde_json::to_value(viso::options::VisoOptions::json_schema()).unwrap_or_default();
    }

    if frontend.view.appearance_schema.is_null() {
        frontend.view.appearance_schema =
            serde_json::to_value(viso::DisplayOverrides::json_schema()).unwrap_or_default();
    }

    // The presets *list* is a disk/library read (App/host), not
    // session state, so it stays here.
    #[cfg(not(target_arch = "wasm32"))]
    {
        frontend.view.available_presets = host
            .view_presets_dir()
            .map(viso::options::VisoOptions::list_presets)
            .unwrap_or_default();
    }
    // `frontend.view.active_preset` is already authoritative on the frontend
    // (set by the inbound manual / preset edit), so this arm leaves it as-is.

    // This arm writes `frontend.view.*` by direct field assignment, so it
    // must raise the VIEW bit itself: the transmit step only emits the view
    // section when the bit is set, and the options/schema written above are
    // otherwise populated but never sent.
    frontend.mark_dirty(DirtyFlags::VIEW);
}

/// Project the `SELECTION` section: the per-entity residue selection.
fn project_selection(session: &Session, frontend: &mut GuiState) {
    let entries: Vec<foldit_gui::EntitySelection> = session
        .selection()
        .iter()
        .map(|(eid, residues)| foldit_gui::EntitySelection {
            entity_id: eid.raw(),
            residues: residues.iter().copied().collect(),
        })
        .collect();
    frontend.set_selection(entries);
}

/// Project the `SCENE` section: the per-entity scene listing plus the
/// focused-entity highlight.
fn project_scene(session: &Session, engine: &VisoEngine, frontend: &mut GuiState) {
    use molex::MoleculeType;
    let mut scene_entities = Vec::new();
    for (eid, _name) in session.iter() {
        let Some(entity) = session.entity(eid) else {
            continue;
        };
        let has_overrides = engine
            .entity_appearance(entity.id())
            .is_some_and(|o| !o.is_empty());
        // The resolved display values: the global display options with this
        // entity's overrides overlaid (or the bare global when it has none),
        // then densified so every field carries its effective value.
        // Serialized flat by field name so a values-bound panel reads each
        // control's current setting directly, with no field falling back to
        // a control minimum because its override slot was `None`.
        let resolved = engine
            .entity_appearance(entity.id())
            .map_or_else(
                || engine.options().display.clone(),
                |o| o.to_display_options(&engine.options().display),
            )
            .with_resolved_overrides();
        let appearance_values = serde_json::to_value(&resolved).unwrap_or_default();
        let mol_str = match entity.molecule_type() {
            MoleculeType::Protein => "protein",
            MoleculeType::DNA => "dna",
            MoleculeType::RNA => "rna",
            MoleculeType::Ligand => "ligand",
            MoleculeType::Ion => "ion",
            MoleculeType::Water => "water",
            MoleculeType::Lipid => "lipid",
            MoleculeType::Cofactor => "cofactor",
            MoleculeType::Solvent => "solvent",
        };
        scene_entities.push(foldit_gui::SceneEntityInfo {
            entity_id: entity.id().raw(),
            label: entity.label(),
            molecule_type: mol_str.to_owned(),
            atom_count: entity.atom_count(),
            residue_count: entity.residue_count(),
            has_overrides,
            appearance_values,
        });
    }
    frontend.set_scene_entities(scene_entities);
    let focused = match session.focus() {
        Focus::Entity(eid) => Some(eid.raw()),
        Focus::All => None,
    };
    frontend.set_focused_entity(focused);
}

/// Push the two-channel history update through the debounce cursor.
///
///   - topology bump → full `HistorySection`
///   - live bump only → small `HistoryLiveUpdate` patch, with a
///     50ms (20Hz) debounce so per-cycle Rosetta scores don't
///     saturate the IPC. The final cycle on commit always lands
///     because committing also bumps `topology_version`.
fn sync_history(
    cursor: &mut HistorySyncCursor,
    session: &Session,
    frontend: &mut GuiState,
    force_history: bool,
) {
    let topology = session.history().topology_version();
    let live = session.history().live_version();
    let topology_changed = cursor.topology != Some(topology);
    let live_changed = cursor.live != Some(live);

    if topology_changed || force_history {
        frontend.set_history(project_history(session));
        cursor.topology = Some(topology);
        cursor.live = Some(live);
        cursor.live_push_at = Some(Instant::now());
    } else if live_changed {
        let now = Instant::now();
        let debounced = cursor
            .live_push_at
            .is_some_and(|t| now.duration_since(t).as_millis() < 50);
        if !debounced {
            if let Some(update) = project_history_live(session.history()) {
                frontend.set_history_live(update);
                cursor.live = Some(live);
                cursor.live_push_at = Some(now);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::path::Path;

    /// Minimal [`crate::HostResources`] stub. `project_view`'s preset read
    /// resolves to `None`, so the test never touches the filesystem.
    struct TestHost;

    impl crate::HostResources for TestHost {
        fn read_file(&self, _path: &str) -> io::Result<Vec<u8>> {
            Err(io::Error::new(io::ErrorKind::NotFound, "test stub"))
        }
        fn view_presets_dir(&self) -> Option<&Path> {
            None
        }
        fn initial_structure_path(&self) -> Option<String> {
            None
        }
    }

    /// `project_view` populates `frontend.view` by direct field write, so it
    /// must also raise the VIEW dirty bit. The transmit step only emits the
    /// view section when that bit is set; without the raise the populated
    /// options/schema are written but never sent, and the JS store keeps a
    /// null schema. Drive the arm directly and assert the bit lands.
    #[test]
    fn project_view_raises_view_dirty_bit() {
        let host = TestHost;
        let mut frontend = GuiState::new();
        // Clear any construction-time dirt so the assertion is about
        // `project_view`'s own raise.
        let _ = frontend.take_dirty();

        project_view(&host, &mut frontend);

        assert!(
            frontend.take_dirty().contains(DirtyFlags::VIEW),
            "project_view must raise VIEW so the populated view section transmits"
        );
    }
}
