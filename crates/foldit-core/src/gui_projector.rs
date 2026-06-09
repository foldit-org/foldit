//! GUI projection state for the third `SessionUpdate` consumer.
//!
//! `GuiProjector` is the state half of the GUI consumer: a single
//! history-version debounce cursor. Its `consume` method - the projection
//! that mirrors `Session` / `VisoEngine` / `RunnerClient` state into
//! `FrontendState` - lives here, alongside the projection
//! helpers it calls ([`Session::display_score`](crate::session::Session::display_score),
//! `project_history`, `bubble_to_payload`).
//! The scoring-mode display policy, tutorial-bubble flow, and puzzle
//! objective live on [`crate::session::Session`] and reach the consumer
//! through their own `SessionUpdate` variants.
//!
//! Unlike [`crate::render_projector::RenderProjector`] and the plugin
//! broadcaster, the GUI consumer also reads the History cursor below: the
//! history channel picks up score-driven `live_version` bumps through the
//! cursor's debounce rather than reprojecting the whole panel each tick.

use web_time::{Instant, UNIX_EPOCH};

use foldit_gui::{
    CheckpointInfo, CheckpointKindTag, DirtyFlags, FilterStatus, FrontendState, HistoryLiveUpdate,
    HistorySection, TextBubbleButton, TextBubblePayload, WireId,
};
use viso::{Focus, VisoEngine};

use crate::history::{CheckpointKind, FilterStatus as HistoryFilterStatus, History};
use crate::runner_client::RunnerClient;
use crate::session::{Puzzle, Session, SessionUpdate};

/// State for the GUI consumer (see `GuiProjector::consume` below): the
/// history-version debounce cursor.
pub struct GuiProjector {
    /// Debounce cursor for the history channel (topology + live).
    pub(crate) history_sync: HistorySyncCursor,
}

impl GuiProjector {
    pub(crate) const fn new() -> Self {
        Self {
            history_sync: HistorySyncCursor {
                topology: None,
                live: None,
                live_push_at: None,
            },
        }
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

/// Convert a parsed [`crate::puzzle::Bubble`] into the GUI-bound IPC
/// twin. Tier-1 conversion: text/color/image pass through; buttons are
/// built from `bubble.button` (defaulting to `"Next"`) plus an optional
/// `alt_button`, with `goto` left `None` since clicks close locally.
fn bubble_to_payload(b: &crate::puzzle::Bubble) -> TextBubblePayload {
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
        CheckpointKind::RemoveEntity { .. } => CheckpointKindTag::RemoveEntity,
        CheckpointKind::LaneUndo { .. } => CheckpointKindTag::LaneUndo,
        CheckpointKind::PluginOp { .. } => CheckpointKindTag::PluginOp,
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
/// the `HistoryPanel`. Also called at-site from `App::run_history_command`
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
}

impl GuiProjector {
    /// Project the live `Session` / `VisoEngine` / `RunnerClient` state into
    /// `frontend` - the third consumer of the `SessionUpdate` batch,
    /// alongside the render and plugin projectors.
    ///
    /// Unlike those two it reads several subsystems (the GUI mirrors score,
    /// selection, scene, history, puzzle, bubble, focus, view, loading), so
    /// it does not implement the two-input `SessionUpdateConsumer<Sink>`
    /// trait: that signature can express only one read input (`session`).
    /// Naming the extra inputs here - the `GuiSources` borrows - is what
    /// keeps this honest and out of the `&App` fake-abstraction trap.
    ///
    /// Per-section dirtiness is derived entirely from the drained `updates`
    /// batch - each `SessionUpdate` variant maps to the GUI sections it
    /// invalidates - plus a one-shot `full_populate` flag the tick raises on
    /// session birth (the Loading → `InSession` flip and every reload) to push
    /// every section once. There is no longer an App-side dirty residue: the
    /// mutations that used to raise flags at their App sites now produce the
    /// covering `SessionUpdate` variants, and those variants are mapped here.
    pub(crate) fn consume(
        &mut self,
        updates: &[SessionUpdate],
        full_populate: bool,
        src: &GuiSources<'_>,
        frontend: &mut FrontendState,
    ) {
        // FPS and selected count change every frame - always push them.
        frontend.set_fps(src.engine.fps());
        frontend.ui.selected_count = src.session.selection_total_count();

        let dirty = compute_dirty(updates, full_populate);

        if dirty.is_empty() {
            return;
        }

        // PUZZLE before SCORE: a fresh `set_puzzle_*` resets `complete=false`,
        // and then the score check below can latch victory in the same frame
        // without being overwritten.
        if dirty.contains(DirtyFlags::PUZZLE) {
            project_puzzle(src.session, frontend);
        }
        if dirty.contains(DirtyFlags::TEXT_BUBBLE) {
            frontend.set_text_bubble(current_bubble_payload(src.session.puzzle()));
        }
        if dirty.contains(DirtyFlags::SCORE) {
            project_score(src.session, frontend);
        }
        if dirty.contains(DirtyFlags::ACTIONS) {
            project_actions(src.session, src.driver, frontend);
        }
        if dirty.contains(DirtyFlags::VIEW) {
            project_view(src.session, src.host, frontend);
        }
        if dirty.contains(DirtyFlags::SELECTION) {
            project_selection(src.session, frontend);
        }
        if dirty.contains(DirtyFlags::SCENE) {
            project_scene(src.session, frontend);
        }

        sync_history(&mut self.history_sync, src.session, frontend);
    }
}

/// Derive the dirty section set for this batch: the one-shot `full_populate`
/// seed plus the per-variant fold mapping each `SessionUpdate` to the GUI
/// sections it invalidates.
fn compute_dirty(updates: &[SessionUpdate], full_populate: bool) -> DirtyFlags {
    let mut dirty = if full_populate {
        DirtyFlags::all()
    } else {
        DirtyFlags::empty()
    };
    for update in updates {
        dirty |= match update {
            SessionUpdate::ScoresChanged => DirtyFlags::SCORE,
            SessionUpdate::Edit { tentative: true }
            | SessionUpdate::PreviewUpdated => DirtyFlags::SCENE,
            SessionUpdate::Edit { tentative: false }
            | SessionUpdate::PreviewAdded
            | SessionUpdate::PreviewDiscarded
            | SessionUpdate::FocusChanged => DirtyFlags::SCENE | DirtyFlags::ACTIONS,
            SessionUpdate::HeadMoved => DirtyFlags::SCENE | DirtyFlags::SCORE | DirtyFlags::ACTIONS,
            SessionUpdate::ViewOptionsChanged => DirtyFlags::VIEW,
            SessionUpdate::SelectionChanged => DirtyFlags::SELECTION | DirtyFlags::ACTIONS,
            SessionUpdate::BubbleChanged => DirtyFlags::TEXT_BUBBLE,
            SessionUpdate::PuzzleChanged => DirtyFlags::PUZZLE,
        };
    }
    dirty
}

/// Project the `PUZZLE` section: the puzzle-panel title/objective plus the
/// puzzle-swap bubble push.
fn project_puzzle(session: &Session, frontend: &mut FrontendState) {
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
fn project_score(session: &Session, frontend: &mut FrontendState) {
    if let Some(score) = session.display_score() {
        frontend.set_score(score, false);
        // Victory check: with a puzzle loaded, latch it complete the
        // first time the score crosses the toml completion energy.
        // Higher game score = better fold (game-score formula
        // negates), so the comparison is `>=`.
        if let Some(p) = session.puzzle() {
            if p.completion_energy > 0.0 && score >= p.completion_energy {
                frontend.mark_puzzle_complete();
            }
        }
    }
}

/// Project the `ACTIONS` section: the focus- + selection-aware op catalog.
fn project_actions(session: &Session, driver: &RunnerClient, frontend: &mut FrontendState) {
    // Availability depends on focus + selection + lock state.
    // Source focus from the authoritative session (same as the
    // SCENE arm below), then hand the driver the selection + an
    // entity-type closure.
    let focus = match session.focus() {
        Focus::Entity(eid) => Some(eid),
        Focus::All => None,
    };
    let actions =
        driver.actions_catalog(focus, session.selection(), |id| session.entity_type(id));
    frontend.set_actions(actions);
}

/// Project the `VIEW` section: view options, the static schema, and the
/// host-sourced preset list.
fn project_view(session: &Session, host: &dyn crate::HostResources, frontend: &mut FrontendState) {
    // Source of truth is the session, not the engine: the engine is
    // a follower that the tick re-applies on `ViewOptionsChanged`.
    frontend.view.options = serde_json::to_value(session.view_options()).unwrap_or_default();

    // Schema is static - only set once
    if frontend.view.options_schema.is_null() {
        frontend.view.options_schema =
            serde_json::to_value(viso::options::VisoOptions::json_schema()).unwrap_or_default();
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
    frontend.view.active_preset = session.active_preset().map(String::from);
}

/// Project the `SELECTION` section: the per-entity residue selection.
fn project_selection(session: &Session, frontend: &mut FrontendState) {
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
fn project_scene(session: &Session, frontend: &mut FrontendState) {
    use molex::MoleculeType;
    let mut scene_entities = Vec::new();
    for (eid, _meta) in session.iter() {
        let Some(entity) = session.entity(eid) else {
            continue;
        };
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
fn sync_history(cursor: &mut HistorySyncCursor, session: &Session, frontend: &mut FrontendState) {
    let topology = session.history().topology_version();
    let live = session.history().live_version();
    let topology_changed = cursor.topology != Some(topology);
    let live_changed = cursor.live != Some(live);

    if topology_changed {
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
