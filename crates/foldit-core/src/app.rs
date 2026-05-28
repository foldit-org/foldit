//! Foldit application state — host-agnostic.
//!
//! `App` owns the `Session`, `PluginDriver` (which carries the
//! orchestrator + scene-broadcaster), the two projectors
//! (`RenderProjector`, `GuiProjector`), and the cross-cutting
//! bookkeeping (puzzle metadata, viso engine handle, dirty-flags,
//! history-version trackers). Both the desktop (`foldit-desktop`) and
//! web (`foldit-web`) builds wrap this in their host-specific lifecycle:
//!
//! - desktop: `window::AppRunner` holds the wry webview + winit window
//!   alongside `App`; winit events are converted to host-agnostic
//!   types before being forwarded to `App`'s methods.
//! - web: `foldit_web::FolditApp` holds `App` plus the canvas and JS
//!   callbacks; DOM events are forwarded as `ViewportInput` JSON.

use std::collections::{BTreeMap, BTreeSet};

use web_time::{Instant, UNIX_EPOCH};

use foldit_gui::{
    AppState, CheckpointInfo, CheckpointKindTag, DirtyFlags, FilterStatus, FrontendState,
    HistoryCommand, HistoryLiveUpdate, HistorySection, ScoringMode, TextBubbleButton,
    TextBubblePayload, WireId,
};
use foldit_runner::Orchestrator;
use molex::entity::molecule::id::EntityId;
use viso::{
    classify_click_for_selection, ClickEvent, ClickSelectionAction, Focus,
    KeyBindings, VisoEngine,
};

use crate::session::{Session, SessionError, EntityOrigin};
use crate::gui_projector::GuiProjector;
use crate::history::{CheckpointKind, FilterStatus as HistoryFilterStatus, History};
use crate::plugin_driver::PluginDriver;
#[cfg(not(target_arch = "wasm32"))]
use crate::plugin_driver::{ActiveStreamEntry, OpOutcome};
use crate::render_projector::{self, RenderProjector};
use crate::wire_params;

fn score_for_mode(
    raw: Option<f64>,
    game: Option<f64>,
    mode: ScoringMode,
) -> Option<f64> {
    match mode {
        ScoringMode::Game => game,
        ScoringMode::Scientist => raw,
    }
}

fn timestamp_ms(t: web_time::SystemTime) -> f64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0)
}

/// Convert a parsed [`crate::puzzle::Bubble`] into the GUI-bound IPC
/// twin. Tier-1 conversion: text/color/image pass through; buttons are
/// built from `bubble.button` (defaulting to `"Next"`) plus an optional
/// `alt_button`, with `goto` left `None` since clicks close locally.
fn bubble_to_payload(b: &crate::puzzle::Bubble) -> TextBubblePayload {
    let mut buttons = vec![TextBubbleButton {
        text: b.button.clone().unwrap_or_else(|| "Next".to_string()),
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

fn checkpoint_kind_tag(k: &CheckpointKind) -> CheckpointKindTag {
    match k {
        CheckpointKind::Loaded { .. } => CheckpointKindTag::Load,
        CheckpointKind::Wiggle { .. } => CheckpointKindTag::Wiggle,
        CheckpointKind::Shake { .. } => CheckpointKindTag::Shake,
        CheckpointKind::Minimize { .. } => CheckpointKindTag::Minimize,
        CheckpointKind::ManualMove { .. } => CheckpointKindTag::ManualMove,
        CheckpointKind::Mutate { .. } => CheckpointKindTag::Mutate,
        CheckpointKind::Rfd3 { .. } => CheckpointKindTag::Rfd3,
        CheckpointKind::Mpnn { .. } => CheckpointKindTag::Mpnn,
        CheckpointKind::PromotedPreview { .. } => CheckpointKindTag::PromotedPreview,
        CheckpointKind::AddEntity { .. } => CheckpointKindTag::AddEntity,
        CheckpointKind::RemoveEntity { .. } => CheckpointKindTag::RemoveEntity,
        CheckpointKind::LaneUndo { .. } => CheckpointKindTag::LaneUndo,
        CheckpointKind::PluginOp { .. } => CheckpointKindTag::PluginOp,
    }
}

fn filter_status_wire(s: &HistoryFilterStatus) -> FilterStatus {
    match s {
        HistoryFilterStatus::Pass => FilterStatus::Pass,
        HistoryFilterStatus::Fail(_) => FilterStatus::Fail,
        HistoryFilterStatus::NotEvaluated => FilterStatus::NotEvaluated,
    }
}

/// Project the backend `History` into the wire payload consumed by
/// the `HistoryPanel`.
fn project_history(store: &Session) -> HistorySection {
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
                tentative: ckpt.tentative,
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

/// Outcome of a [`HistoryCommand`] dispatch — drives the per-frame
/// follow-up the dispatcher must run (republish to viso, mark dirty,
/// or nothing at all).
enum HistoryOutcome {
    /// Checkpoint head moved; rerun [`App::after_head_move`].
    HeadMoved,
    /// Curation flag changed (pin / unpin / exclude from best). No
    /// head move; just mark `ACTIONS` dirty so the GUI reflects it.
    Curated,
    /// The command was a no-op (e.g., undo at root). No follow-up.
    Noop,
}

/// Read the score for the *current head checkpoint*, projected through
/// the active scoring mode. Replaces the old `App::latest_score` field
/// (G1: derive, don't store).
fn head_score(store: &Session, mode: ScoringMode) -> Option<f64> {
    let head_id = store.history().checkpoints().head();
    let ckpt = store.history().checkpoint(head_id)?;
    score_for_mode(ckpt.raw_score, ckpt.game_score, mode)
}

/// Move one freshly-loaded entity through the preview→promote pipeline
/// so it lands in history with an `AddEntity` checkpoint. Returns the
/// committed [`EntityId`].
///
/// Ambient (water / ion / solvent) and zero-residue entities — the
/// het-residue stubs the parser emits for cofactors / waters in
/// structure files — are kept as previews (transient) so viso still
/// renders them, but they DO NOT push a history checkpoint. They aren't
/// undoable from the user's perspective; pushing one `AddEntity` per
/// stub clutters the history (`1bfe` produced 3 root-level dots: one
/// `Loaded` + two `AddEntity` for chain A and a water).
fn load_entity_into_history(
    store: &mut Session,
    entity: molex::MoleculeEntity,
    name: String,
) -> Option<EntityId> {
    use molex::MoleculeType;
    let mol_type = entity.molecule_type();
    let is_ambient = matches!(
        mol_type,
        MoleculeType::Water | MoleculeType::Ion | MoleculeType::Solvent
    );
    let zero_residue = entity.residue_count() == 0;
    let id = store.insert_preview(entity, name.clone(), EntityOrigin::Loaded);
    if is_ambient || zero_residue {
        // Leave it transient: visible in viso, absent from history.
        return Some(id);
    }
    match store.promote_preview(
        id,
        CheckpointKind::AddEntity {
            entity: id,
            kind: mol_type,
        },
        None,
        None,
        std::borrow::Cow::Owned(format!("Loaded {name}")),
    ) {
        Ok(_) => Some(id),
        Err(e) => {
            log::error!("Failed to promote loaded entity '{name}': {e}");
            None
        }
    }
}

/// First protein entity in the head checkpoint, used as a fallback
/// focus when an op-dispatch carries no `focused_entity_id`. Wave-1
/// puzzles ship 1-chain proteins; a missing focus is the rule, not
/// the exception. Returns `None` when no protein entity exists.
fn first_protein_entity(store: &Session) -> Option<EntityId> {
    store.proteins().next().map(|(eid, _, _)| eid)
}

/// Bridge a runner [`EntityId`](foldit_runner::orchestrator::EntityId)
/// to the orchestrator's [`EntityType`](foldit_runner::orchestrator::EntityType)
/// by looking the entity up in the [`Session`] and mapping the
/// `MoleculeEntity` variant 1:1. Used by `dispatch_op` callers so the
/// orchestrator can pick per-entity locks instead of falling back to
/// `LockTargets::SessionWide`. Returns `None` when the runner id has no
/// matching molex id in the current head (no committed lane or preview).
#[cfg(not(target_arch = "wasm32"))]
fn entity_type_of(
    store: &Session,
    id: foldit_runner::orchestrator::EntityId,
) -> Option<foldit_runner::orchestrator::EntityType> {
    use foldit_runner::orchestrator::EntityType;
    use molex::MoleculeEntity;
    let molex_id = store.ids().find(|m| u64::from(m.raw()) == id.0)?;
    store.entity(molex_id).map(|me| match me {
        MoleculeEntity::Protein(_) => EntityType::Protein,
        MoleculeEntity::NucleicAcid(_) => EntityType::NucleicAcid,
        MoleculeEntity::SmallMolecule(_) => EntityType::SmallMolecule,
        MoleculeEntity::Bulk(_) => EntityType::Bulk,
    })
}

/// Translate the runner-side manifest enum to a concrete viso
/// [`Transition`]. The variant names are kept in 1:1 sync — this is a
/// mechanical mapping, not a semantic rename.
#[cfg(not(target_arch = "wasm32"))]
fn resolve_transition(
    kind: foldit_runner::orchestrator::TransitionKind,
) -> viso::Transition {
    use foldit_runner::orchestrator::TransitionKind;
    match kind {
        TransitionKind::Snap => viso::Transition::snap(),
        TransitionKind::Smooth => viso::Transition::smooth(),
        TransitionKind::CollapseExpand => viso::Transition::collapse_expand(
            std::time::Duration::from_millis(150),
            std::time::Duration::from_millis(150),
        ),
        TransitionKind::BackboneThenExpand => viso::Transition::backbone_then_expand(
            std::time::Duration::from_millis(200),
            std::time::Duration::from_millis(100),
        ),
    }
}

/// Overwrite the ongoing action's tentative payload from a streaming
/// assembly. Only the entity locked by `begin_action` is rewritten;
/// peer entities in the same incoming Assembly are ignored (whole-pose
/// streams under the single-entity action model). Score fields are
/// propagated when the plugin embedded a total; per-residue / game
/// scoring stay on their own refresh path.
///
/// Returns `true` if a payload swap actually fired.
fn apply_streaming_assembly(
    store: &mut Session,
    incoming: &molex::Assembly,
    raw_score: Option<f64>,
) -> bool {
    let mut applied = false;
    let res = store.action_update(raw_score, raw_score, None, |entity_mut| {
        if let Some(src) = incoming.entity(entity_mut.id()) {
            *entity_mut = src.clone();
            applied = true;
        }
    });
    if let Err(e) = res {
        log::trace!("action_update skipped: {e}");
        return false;
    }
    applied
}

/// Main application state — thin glue connecting the render engine,
/// plugin driver, document, and the two projectors. `App` also owns the
/// host-bound [`FrontendState`] mirror (so the load state-machine and
/// the GUI projection both live on the same side of the host seam) and
/// the [`AppState`] machine that gates the Loading → InPuzzle transition.
pub struct App {
    engine: Option<VisoEngine>,
    keybindings: KeyBindings,
    store: Session,
    ui_dirty: DirtyFlags,
    plugin_driver: PluginDriver,
    render_projector: RenderProjector,
    gui_projector: GuiProjector,
    /// Host-provided filesystem / resource access. The only path through
    /// which foldit-core touches the filesystem outside puzzle loading.
    host: Box<dyn crate::HostResources>,
    /// Display name for the currently loaded structure (file stem on
    /// free-form loads, puzzle name on `LoadPuzzle`). `None` before any
    /// load; `structure_title()` falls back to `"Unknown"` in that case.
    structure_title: Option<String>,
    /// Objective metadata for the currently loaded puzzle. `None` in
    /// Scientist mode (free-form structure load) and at startup before
    /// any load; `Some` only after `LoadPuzzle` populates it from the
    /// puzzle TOML.
    loaded_puzzle: Option<LoadedPuzzle>,
    /// Frontend mirror — written by [`Self::populate_frontend`] each
    /// tick and drained by the host via [`Self::serialize_frontend_dirty`].
    frontend: FrontendState,
    /// Top-level GUI state-machine value. `App` owns the Loading →
    /// InPuzzle transition gated on `awaiting_initial_score` +
    /// `has_initial_score()`.
    app_state: AppState,
    /// Set after `load_initial_structure` returns; cleared in `tick`
    /// once the first plugin score lands. Mirrors the desktop runner's
    /// old field.
    awaiting_initial_score: bool,
    /// Empty inner sets are never stored: removing the last residue
    /// on an entity removes the entity entry, so iterating
    /// `selected_entities` yields only entities that currently have
    /// at least one selected residue.
    selection: BTreeMap<EntityId, BTreeSet<u32>>,
}

/// Objective metadata for an active puzzle. Populated from the puzzle
/// TOML on a `LoadPuzzle` event. Free-form structure loads (Scientist
/// mode) have no objective and so leave [`App::loaded_puzzle`] as `None`.
#[derive(Debug, Clone)]
struct LoadedPuzzle {
    id: u32,
    title: String,
    starting_score: f64,
    target_score: f64,
}

impl App {
    /// Display title for the currently loaded structure, or `"Unknown"`
    /// before any load. Refreshed at every load site (`LoadStructure`,
    /// `LoadPuzzle`, `load_initial_structure`).
    pub fn structure_title(&self) -> String {
        self.structure_title
            .clone()
            .unwrap_or_else(|| "Unknown".to_string())
    }

    pub fn new(host: Box<dyn crate::HostResources>) -> Self {
        Self {
            engine: None,
            keybindings: KeyBindings::default(),
            store: Session::new(),
            ui_dirty: DirtyFlags::empty(),
            plugin_driver: PluginDriver::new(),
            render_projector: RenderProjector::new(),
            gui_projector: GuiProjector::new(),
            host,
            structure_title: None,
            // No puzzle objective until `LoadPuzzle` fires; free-form
            // `LoadStructure` keeps this as `None` too.
            loaded_puzzle: None,
            frontend: FrontendState::new(),
            app_state: AppState::Loading,
            awaiting_initial_score: false,
            selection: BTreeMap::new(),
        }
    }

    /// True once the Rosetta backend has delivered its first score
    /// update for the current session. Read by [`Self::tick`] to gate
    /// the Loading → InPuzzle transition.
    fn has_initial_score(&self) -> bool {
        head_score(&self.store, self.gui_projector.scoring_mode).is_some()
    }

    // ── Engine-only delegation ──

    pub fn resize(&mut self, width: u32, height: u32) {
        if let Some(engine) = &mut self.engine {
            engine.resize(width, height);
        }
    }

    pub fn set_surface_scale(&mut self, scale_factor: f64) {
        if let Some(ref mut engine) = self.engine {
            engine.set_render_scale(if scale_factor < 2.0 { 2 } else { 1 });
        }
    }

    pub fn update_engine(&mut self, dt: f32) {
        if let Some(engine) = &mut self.engine {
            engine.update(dt);
        }
    }

    pub fn render(&mut self) {
        if let Some(engine) = &mut self.engine {
            if let Err(e) = engine.render() {
                log::error!("Render error: {:?}", e);
            }
        }
    }

    // ── Backend update processing ──

    pub fn apply_backend_updates(&mut self) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            use foldit_runner::orchestrator::{
                EntityId as RunnerEntityId, PluginUpdate,
            };

            let updates = self.plugin_driver.drain_updates();
            if updates.is_empty() {
                return;
            }

            let mut visual_dirty = false;
            let mut had_terminal = false;
            // Resolved animation preset for whichever stream produced
            // the latest visual update in this drain, queued on the
            // engine so the next tick's render-projector publish
            // animates in instead of snapping. Single-action invariant
            // means at most one stream is mutating at a time, so
            // last-write-wins is correct. The entity is captured
            // alongside the transition (not re-resolved via
            // `ongoing().locked_entity()` after the loop) because the
            // `Final` arm commits the action inline and moves
            // OngoingState to Idle, after which locked_entity is
            // None — the Invoke path's apply_invoke_result captures
            // the same way for the same reason.
            let mut pending_transition: Option<(
                molex::entity::molecule::id::EntityId,
                foldit_runner::orchestrator::TransitionKind,
            )> = None;
            for update in updates {
                match update {
                    PluginUpdate::Pending {
                        request_id,
                        latest_assembly,
                        progress,
                        stage,
                    } => {
                        let Some(assembly) = latest_assembly else {
                            log::trace!(
                                "plugin update Pending rid={request_id} \
                                 progress={progress:?} stage={stage:?} \
                                 entities=0 (skipped: no assembly)"
                            );
                            continue;
                        };
                        if apply_streaming_assembly(
                            &mut self.store,
                            &assembly,
                            None,
                        ) {
                            visual_dirty = true;
                            if let (Some(entry), Some(entity)) = (
                                self.plugin_driver.stream_entry(request_id),
                                self.store
                                    .history()
                                    .ongoing()
                                    .locked_entity(),
                            ) {
                                pending_transition =
                                    Some((entity, entry.transition));
                            }
                            // Per-frame score refresh while a stream
                            // runs. Pendings don't trigger the
                            // broadcast-driven refresh path (only
                            // committed mutations do), so without this
                            // the score widget + per-residue color
                            // overlay sit stale until Final/Cancel
                            // commits. The query is synchronous and
                            // cheap (~ms for rosetta); Pending cadence
                            // is gated by `POLL_INTERVAL` (50ms), so
                            // worst case is ~20 score queries/sec.
                            self.refresh_scores();
                            self.ui_dirty |= DirtyFlags::SCORE;
                        }
                    }
                    PluginUpdate::Cancelled {
                        request_id,
                        assembly,
                    } => {
                        // Cancel-as-commit: same shape as Final below.
                        had_terminal = true;
                        let stream_transition = self
                            .plugin_driver
                            .stream_entry(request_id)
                            .map(|e| e.transition);
                        if apply_streaming_assembly(
                            &mut self.store,
                            &assembly,
                            None,
                        ) {
                            let locked_for_transition = self
                                .store
                                .history()
                                .ongoing()
                                .locked_entity();
                            if let Err(e) = self.store.commit_action() {
                                log::warn!(
                                    "plugin update Cancelled rid={request_id} \
                                     commit_action failed: {e}"
                                );
                            }
                            visual_dirty = true;
                            if let (Some(t), Some(entity)) =
                                (stream_transition, locked_for_transition)
                            {
                                pending_transition = Some((entity, t));
                            }
                        }
                        let _ = self
                            .plugin_driver
                            .release_terminal_stream(request_id);
                        log::info!(
                            "plugin update Cancelled rid={request_id} \
                             entities={}",
                            assembly.entities().len()
                        );
                    }
                    PluginUpdate::Final {
                        request_id,
                        assembly,
                        ..
                    } => {
                        had_terminal = true;
                        let stream_transition = self
                            .plugin_driver
                            .stream_entry(request_id)
                            .map(|e| e.transition);
                        if apply_streaming_assembly(
                            &mut self.store,
                            &assembly,
                            None,
                        ) {
                            // Capture the locked entity *before*
                            // commit_action moves OngoingState to Idle:
                            // queue_entity_transition runs after this
                            // loop and would otherwise have nothing to
                            // key against.
                            let locked_for_transition = self
                                .store
                                .history()
                                .ongoing()
                                .locked_entity();
                            // Stream completed cleanly: commit the
                            // tentative so the partial result becomes
                            // a permanent undo entry.
                            if let Err(e) = self.store.commit_action() {
                                log::warn!(
                                    "plugin update Final rid={request_id} \
                                     commit_action failed: {e}"
                                );
                            }
                            visual_dirty = true;
                            if let (Some(t), Some(entity)) =
                                (stream_transition, locked_for_transition)
                            {
                                pending_transition = Some((entity, t));
                            }
                        }
                        let _ = self
                            .plugin_driver
                            .release_terminal_stream(request_id);
                        log::info!(
                            "plugin update Final rid={request_id} \
                             entities={}",
                            assembly.entities().len()
                        );
                    }
                    PluginUpdate::Error {
                        request_id,
                        message,
                    } => {
                        // Spontaneous failure only (timeout, exception,
                        // transport, STALE_GEN). Never commits; abort
                        // is gated to this stream's entity so a stale
                        // Error can't drop another op's tentative. Peek
                        // at the entry through `stream_entry` to read
                        // `handle.entities` before
                        // `release_terminal_stream` consumes the handle.
                        had_terminal = true;
                        let owns_tentative = self
                            .plugin_driver
                            .stream_entry(request_id)
                            .and_then(|entry| {
                                self.store
                                    .history()
                                    .ongoing()
                                    .locked_entity()
                                    .map(|locked| {
                                        entry.handle.entities.iter().any(
                                            |runner_eid: &RunnerEntityId| {
                                                runner_eid.0
                                                    == u64::from(locked.raw())
                                            },
                                        )
                                    })
                            })
                            .unwrap_or(false);
                        if owns_tentative {
                            if let Err(e) = self.store.abort_action() {
                                log::warn!(
                                    "plugin update Error rid={request_id} \
                                     abort_action failed: {e}"
                                );
                            } else {
                                visual_dirty = true;
                            }
                        }
                        let _ = self
                            .plugin_driver
                            .release_terminal_stream(request_id);
                        log::warn!(
                            "plugin update Error rid={request_id} \
                             message={message}"
                        );
                    }
                }
            }

            if visual_dirty {
                if let Some(engine) = self.engine.as_mut() {
                    // Queue the animation preset for the streaming
                    // entity *before* set_assembly fires on the next
                    // tick — the engine picks both up together and
                    // animates from current GPU positions to the new
                    // pose; without the transition, sync_scene_to_renderers
                    // snaps. The entity was captured at update time
                    // (single-action invariant — at most one in flight
                    // at a time). The publish itself is spine-driven
                    // by `tick` via the Edit/HeadMoved emitted by
                    // `action_update` / `commit_action`.
                    if let Some((entity, kind)) = pending_transition {
                        engine.queue_entity_transition(
                            entity.raw(),
                            resolve_transition(kind),
                        );
                    }
                }
            }
            if had_terminal {
                self.ui_dirty |= DirtyFlags::SCORE
                    | DirtyFlags::ACTIONS
                    | DirtyFlags::SCENE
                    | DirtyFlags::HISTORY;
            } else if visual_dirty {
                // Mid-stream visual updates without a terminal event:
                // the scene needs a re-publish but not a full UI sync.
                self.ui_dirty |= DirtyFlags::SCENE;
            }
        }
    }

    /// Query every plugin's `score` op, merge totals into the head
    /// checkpoint (bumping `live_version` for the GuiProjector to pick
    /// up), and push per-residue scores directly to viso for
    /// color-by-score display modes. Off the `SessionUpdate` spine
    /// entirely: scores have two consumers (the GuiProjector via
    /// `HistorySyncCursor` and viso via a direct overlay push) and
    /// neither needs to ride the spine (RX10 decision B).
    ///
    /// Called once per [`Self::tick`] (cadence unweld from
    /// per-broadcast — RX13).
    fn poll_plugin_scores(&mut self) {
        if self.plugin_driver.orchestrator.is_none() {
            return;
        }
        self.refresh_scores();
        self.ui_dirty |= DirtyFlags::SCORE | DirtyFlags::HISTORY;
    }

    /// Fan out the well-known `score` query across every plugin that
    /// registered it, merge totals into the head checkpoint, and push
    /// per-residue scores to the render engine for color-by-score modes.
    ///
    /// Called once at bootstrap (flips `has_initial_score()`, opening the
    /// loading gate) and again after every host-originated broadcast (so
    /// post-edit rescores update both the score widget and the residue
    /// colors).
    ///
    /// Today only Rosetta returns a non-trivial report. When more scorers
    /// come online the merge becomes app-wide -- the host stays generic
    /// either way.
    #[cfg(not(target_arch = "wasm32"))]
    fn refresh_scores(&mut self) {
        use foldit_runner::orchestrator::DispatchContext;
        use std::collections::HashMap;

        let Some(orch) = self.plugin_driver.orchestrator.as_mut() else {
            return;
        };
        let reports = orch.collect_scores(&DispatchContext::default());
        if reports.is_empty() {
            return;
        }

        let mut total: Option<f64> = None;
        // entity_id -> Vec<(residue_index, score)>; merged across all
        // reporting plugins (later writers stack on top of earlier ones
        // for now -- when multiple plugins score per-residue we'll need a
        // merge strategy choice).
        let mut per_entity: HashMap<u32, Vec<(u32, f64)>> = HashMap::new();
        for (plugin_id, report) in &reports {
            if total.is_none() {
                total = Some(f64::from(report.total));
            }
            log::info!(
                "[App] score from {plugin_id}: total={} terms={} per_residue={}",
                report.total,
                report.terms.len(),
                report.per_residue.len()
            );
            for rs in &report.per_residue {
                let Some(rref) = rs.residue.as_ref() else { continue };
                #[allow(clippy::cast_possible_truncation)]
                let entity_id = rref.entity_id as u32;
                per_entity.entry(entity_id).or_default().push((
                    rref.residue_index,
                    f64::from(rs.score),
                ));
            }
        }
        if let Some(t) = total {
            // Convert the rosetta raw score (REU) to foldit's game-mode
            // display. Verbatim port of the
            // `rosetta_score_to_game_score_either(use_minimum=true,
            // internal=false)` branch at
            // `plugins/rosetta/deps/rosetta-interactive/source/src/interactive/
            // rosetta_util/rosetta_util.cc:2702`, using the constants
            // declared on lines 2662-2663 + 2664 of the same file.
            // Bridge-side the converter slot
            // (`rosetta_score_to_game_score`) is null because nothing in
            // the new plugin-host architecture calls
            // `register_score_functions`; doing the linear map here is
            // the right home anyway -- the formula is universal foldit
            // policy, not rosetta-specific, and lives next to the
            // `ScoringMode` selector that decides which view reaches the
            // GUI.
            const SCORE_OFFSET: f64 = 800.0;
            const SCORE_SCALE: f64 = 10.0;
            const SCORE_MINIMUM: f64 = 0.0;
            let raw = t;
            let game = ((-raw + SCORE_OFFSET) * SCORE_SCALE).max(SCORE_MINIMUM);
            self.store.set_head_scores(Some(raw), Some(game));
        }

        // Push per-residue scores into the engine so Score / ScoreRelative
        // color schemes have data. Each entity's score Vec is sized to
        // its full residue count; missing residues default to 0.0 (the
        // mid-palette stop in absolute mode, the lower quantile in
        // relative mode -- close enough for a first-pass render).
        let Some(engine) = self.engine.as_mut() else { return };
        // Build (raw_entity_id -> residue_count) once via head_assembly so
        // we don't need a mut borrow on store to mint molex EntityIds.
        let head = self.store.head_assembly();
        let residue_counts: HashMap<u32, usize> = head
            .entities()
            .iter()
            .map(|e| (e.id().raw(), e.residue_count()))
            .collect();
        for (entity_id, mut entries) in per_entity {
            let Some(&residue_count) = residue_counts.get(&entity_id) else {
                log::warn!(
                    "[App] per-residue scores arrived for unknown entity \
                     {entity_id} (host has entities {:?})",
                    residue_counts.keys().collect::<Vec<_>>()
                );
                continue;
            };
            let mut scores = vec![0.0_f64; residue_count];
            entries.sort_unstable_by_key(|(idx, _)| *idx);
            let entry_count = entries.len();
            for (idx, val) in entries {
                let i = idx as usize;
                if i < scores.len() {
                    scores[i] = val;
                }
            }
            log::info!(
                "[App] applied {entry_count} per-residue scores to viso entity \
                 {entity_id} (residue_count={residue_count})"
            );
            engine.set_per_residue_scores(entity_id, Some(scores));
        }
    }

    // ── Keybinding dispatch ──

    /// Catalog hotkey fallback. Runs only after a viso built-in
    /// `handle_key_press` *miss*, so built-ins always win. On a match
    /// against a plugin manifest `[[buttons]]` hotkey, dispatch the op
    /// through the same `handle_dispatch_op` sink a button click uses
    /// (focus/selection not captured here yet — same as a click with
    /// no GUI selection). Returns true if an op was dispatched.
    #[cfg(not(target_arch = "wasm32"))]
    fn try_hotkey_dispatch(&mut self, key_str: &str) -> bool {
        let op_id = self.plugin_driver.orchestrator.as_ref().and_then(|orch| {
            orch.ops_catalog()
                .into_iter()
                .find(|e| e.hotkey.as_deref() == Some(key_str))
                .map(|e| e.op_id)
        });
        let Some(op_id) = op_id else { return false };
        log::info!("hotkey {key_str:?} -> dispatch plugin op {op_id:?}");
        self.handle_dispatch_op(foldit_gui::OpDispatch {
            op_id,
            focused_entity_id: None,
            params: std::collections::HashMap::new(),
        });
        true
    }

    #[cfg(target_arch = "wasm32")]
    fn try_hotkey_dispatch(&mut self, _key_str: &str) -> bool {
        false
    }

    /// Dispatch a keybinding by physical-key string ("KeyR", "KeyT",
    /// "Tab", ...). Hosts convert their native keycode to this string
    /// before calling (winit: `format!("{key:?}")`; web: DOM `code`).
    /// On a viso built-in miss, falls through to the plugin hotkey
    /// catalog (built-ins win by being checked first).
    pub fn handle_keybinding(&mut self, key_str: &str) -> bool {
        // foldit-specific overrides: trajectory load-on-demand, ESC =
        // cancel-in-flight-op, and the dropped auto-rotate binding.
        // These short-circuit the generic viso keybinding dispatch.
        match key_str {
            "KeyT" => {
                let Some(engine) = &mut self.engine else { return false };
                if engine.has_trajectory() {
                    engine.toggle_trajectory();
                } else if let Some(path) = trajectory_path_from_args() {
                    engine.load_trajectory(std::path::Path::new(&path));
                } else {
                    log::info!("No trajectory loaded. Pass --trajectory <path.dcd> to load one.");
                }
                return true;
            }
            "Escape" => {
                // First ESC clears any active selection; only a second
                // ESC (selection already empty) cancels in-flight
                // streams + previews. Keeps a selection-clear gesture
                // from accidentally dropping work-in-flight.
                if !self.selection_is_empty() {
                    self.clear_selection();
                } else {
                    #[cfg(not(target_arch = "wasm32"))]
                    self.plugin_driver.cancel_all_active_streams();
                    self.cancel_operations();
                }
                return true;
            }
            // Auto-rotate keybinding is intentionally dropped in foldit.
            "KeyR" => return true,
            _ => {}
        }

        let Some(engine) = &mut self.engine else { return false };

        if !self.keybindings.dispatch(key_str, engine) {
            return self.try_hotkey_dispatch(key_str);
        }

        // Focus-changing keys need a GUI dirty flush; viso's built-in
        // ran above but doesn't know about foldit's projector cadence.
        if matches!(key_str, "Tab" | "Backquote") {
            log::info!(
                "Focus: {}",
                render_projector::focus_description(&self.store, &engine.focus())
            );
            self.ui_dirty |= DirtyFlags::SCENE | DirtyFlags::UI;
        }
        true
    }

    /// Cancel the in-flight operation: drop any in-progress preview
    /// entities, republish, and flag the GUI dirty. Selection is a
    /// separate concept (see `clear_selection`); cancelling an operation
    /// does not touch it. Stream lock release + commit live in
    /// `apply_backend_updates`' terminal arms; doing them here races a
    /// follow-up dispatch that's quick enough to slip in before the
    /// terminal drains. Lives on `App` so the `RenderProjector` stays a
    /// field touched only inside App methods (the coordination
    /// boundary), never threaded as a parameter.
    fn cancel_operations(&mut self) {
        if self.engine.is_none() {
            return;
        }
        log::info!("Cancelling current operation");
        let preview_ids: Vec<EntityId> = self.store.preview_ids().collect();
        if !preview_ids.is_empty() {
            for id in &preview_ids {
                self.store.remove_preview(*id);
            }
            // PreviewDiscarded rides the spine — the next tick's render
            // projector republishes.
            log::info!(
                "Removed {} in-progress preview entities",
                preview_ids.len()
            );
        }
        self.ui_dirty |= DirtyFlags::ACTIONS | DirtyFlags::LOADING;
    }

    // ── Viewport input (from webview) ──

    pub fn handle_viewport_input(&mut self, input: foldit_gui::ViewportInput) {
        use foldit_gui::ViewportInput;

        // Pull-drag interception runs ahead of viso's regular input
        // routing so an active drag suppresses camera rotation/pan.
        // A pull opens on *drag*, not press: a left-press on an atom
        // falls through to viso (which records the down-target, so a
        // press+release with no move resolves to a residue selection);
        // the first pointer-move with the button still held that
        // resolves to a valid pull route opens the drag instead.
        // `mouse_pressed()` is viso's own press bit, set by the
        // preceding PointerDown's normal-path handling below.
        #[cfg(not(target_arch = "wasm32"))]
        {
            match &input {
                ViewportInput::PointerMove { x, y, .. }
                    if self
                        .engine
                        .as_ref()
                        .is_some_and(viso::VisoEngine::mouse_pressed)
                        && self.plugin_driver.stream_host.pull_drag.is_none() =>
                {
                    if self.try_begin_pull_drag(*x, *y) {
                        // viso recorded the press; drop its mouse
                        // state so the now-suppressed pointer-up
                        // can't fire a stray click → selection.
                        if let Some(engine) = self.engine.as_mut() {
                            engine.release_mouse_state();
                        }
                        self.update_pull_drag(*x, *y);
                        self.finalize_viewport_input();
                        return;
                    }
                }
                ViewportInput::PointerMove { x, y, .. }
                    if self.plugin_driver.stream_host.pull_drag.is_some() =>
                {
                    self.update_pull_drag(*x, *y);
                    self.finalize_viewport_input();
                    return;
                }
                ViewportInput::PointerUp { .. }
                    if self.plugin_driver.stream_host.pull_drag.is_some() =>
                {
                    self.end_pull_drag();
                    self.finalize_viewport_input();
                    return;
                }
                _ => {}
            }
        }

        // Hotkey resolved in the `Key` arm below via a disjoint field
        // borrow (`self.plugin_driver`, not `self.engine`); the actual
        // dispatch is deferred to after the match so the `engine`
        // borrow is released before `handle_dispatch_op` takes
        // `&mut self`.
        #[cfg(not(target_arch = "wasm32"))]
        let mut pending_hotkey_op: Option<String> = None;
        // ESC routing needs `&mut self`, but `engine` is borrowed for
        // the rest of the match and used again by
        // `update_all_visualizations` after it. Defer past that last
        // engine use, mirroring the `pending_hotkey_op` deferral. The
        // deferred block reads `selection_is_empty()` live (matching
        // `handle_keybinding`) so any selection mutation that happens
        // earlier in this call (e.g. a deferred `pending_click`) can't
        // strand the ESC arm against a stale snapshot.
        let mut pending_escape = false;

        let Some(engine) = &mut self.engine else { return };

        // `Some` only if a left-button release classified as a click;
        // deferred so the selection mutations below run after the
        // `engine` borrow ends.
        let mut pending_click: Option<ClickEvent> = None;

        match input {
            ViewportInput::PointerDown {
                x, y, button, shift, ..
            } => {
                let viso_button = match button {
                    0 => viso::MouseButton::Left,
                    2 => viso::MouseButton::Right,
                    1 => viso::MouseButton::Middle,
                    _ => return,
                };
                engine.feed_modifiers(shift);
                engine.set_cursor_pos(x, y);
                engine.feed_pointer_motion(x, y);
                let _ = engine.feed_pointer_button(viso_button, true);
            }
            ViewportInput::PointerUp {
                x, y, button, shift, ..
            } => {
                let viso_button = match button {
                    0 => viso::MouseButton::Left,
                    2 => viso::MouseButton::Right,
                    1 => viso::MouseButton::Middle,
                    _ => return,
                };
                engine.feed_modifiers(shift);
                engine.set_cursor_pos(x, y);
                engine.feed_pointer_motion(x, y);
                pending_click = engine.feed_pointer_button(viso_button, false);
            }
            ViewportInput::PointerMove { x, y, shift, .. } => {
                engine.feed_modifiers(shift);
                engine.set_cursor_pos(x, y);
                engine.feed_pointer_motion(x, y);
            }
            ViewportInput::Scroll { delta } => {
                engine.feed_scroll(delta);
            }
            ViewportInput::Key { code, pressed } => {
                if pressed {
                    // foldit-specific overrides land first; viso's
                    // generic table picks up the rest.
                    match code.as_str() {
                        // Drop viso's R-binding for turntable auto-rotate;
                        // we don't expose a rotate keybinding in foldit.
                        "KeyR" => {}
                        "KeyT" => {
                            if engine.has_trajectory() {
                                engine.toggle_trajectory();
                            } else if let Some(path) = trajectory_path_from_args() {
                                engine.load_trajectory(std::path::Path::new(&path));
                            }
                        }
                        "Escape" => {
                            // Two-stage clear/cancel resolved in the
                            // deferred block below against a live
                            // `selection_is_empty()` read, so a
                            // pending_click applied in the same call
                            // window can't desync the branch choice.
                            // Mirrors the `handle_keybinding` ESC arm.
                            pending_escape = true;
                        }
                        other => {
                            if self.keybindings.dispatch(other, engine) {
                                if matches!(other, "Tab" | "Backquote") {
                                    // (lock update deferred — see comment above)
                                    self.ui_dirty |= DirtyFlags::SCENE | DirtyFlags::UI;
                                }
                            } else {
                                // No viso built-in claims this key — resolve it
                                // against the plugin hotkey catalog. Disjoint
                                // field borrow (`self.plugin_driver`) so it
                                // coexists with the live `engine` borrow;
                                // dispatch is deferred to after the match.
                                #[cfg(not(target_arch = "wasm32"))]
                                {
                                    pending_hotkey_op = self
                                        .plugin_driver
                                        .orchestrator
                                        .as_ref()
                                        .and_then(|orch| {
                                            orch.ops_catalog()
                                                .into_iter()
                                                .find(|e| {
                                                    e.hotkey.as_deref()
                                                        == Some(other)
                                                })
                                                .map(|e| e.op_id)
                                        });
                                    if pending_hotkey_op.is_none() {
                                        log::debug!(
                                            "Unhandled key code from frontend: {other}"
                                        );
                                    }
                                }
                                #[cfg(target_arch = "wasm32")]
                                log::debug!("Unhandled key code from frontend: {other}");
                            }
                        }
                    }
                }
            }
            ViewportInput::Resize { .. } => {
                // Ignored: JS sends CSS pixels (logical) which are wrong on HiDPI.
            }
        }

        self.ui_dirty |= DirtyFlags::UI;

        // Update drag/pull/band visualizations after input
        #[cfg(not(target_arch = "wasm32"))]
        let pull = self.plugin_driver.stream_host.pull_drag.as_ref().map(|d| d.pull_info.clone());
        #[cfg(target_arch = "wasm32")]
        let pull: Option<viso::PullInfo> = None;
        update_all_visualizations(engine, pull);

        // `engine`'s last use was above — `&mut self` is free again, so
        // the deferred actions below can run. Apply the pending click
        // before the ESC branch so the latter's `selection_is_empty()`
        // read sees the post-click state (any single call only carries
        // one of the two today, but the live-read keeps the two-stage
        // ESC gesture correct regardless of producer order).
        if let Some(click) = pending_click {
            self.apply_click_to_selection(&click);
        }

        if pending_escape {
            if !self.selection_is_empty() {
                self.clear_selection();
            } else {
                #[cfg(not(target_arch = "wasm32"))]
                self.plugin_driver.cancel_all_active_streams();
                self.cancel_operations();
            }
        }

        // A hotkey resolved in the `Key` arm dispatches through the same
        // sink a button click uses (item 78); built-ins already won by
        // `handle_key_press` being checked first.
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(op_id) = pending_hotkey_op {
            log::info!("hotkey -> dispatch plugin op {op_id:?}");
            self.handle_dispatch_op(foldit_gui::OpDispatch {
                op_id,
                focused_entity_id: None,
                params: std::collections::HashMap::new(),
            });
        }
    }

    /// Called after the pull-drag interception path. Mirrors the
    /// trailing visualization update the regular `handle_viewport_input`
    /// flow does (the spine drain itself is tick-driven now).
    /// Pre-snapshots the pull info so the engine borrow doesn't overlap
    /// with `&self.plugin_driver.stream_host.pull_drag`.
    #[cfg(not(target_arch = "wasm32"))]
    fn finalize_viewport_input(&mut self) {
        self.ui_dirty |= DirtyFlags::UI;
        let pull = self.plugin_driver.stream_host.pull_drag.as_ref().map(|d| d.pull_info.clone());
        if let Some(engine) = self.engine.as_mut() {
            update_all_visualizations(engine, pull);
        }
    }

    /// Pointer-down on an atom: classify the pick, dispatch the matching
    /// pull op-id, install drag state, and feed viso the initial
    /// PullInfo. Returns true if a drag was initiated (so the caller
    /// suppresses the regular viso input flow), false otherwise.
    ///
    /// Rejected picks (non-protein entity, hydrogen atom, no atom under
    /// the cursor, dispatch failure) leave `self.plugin_driver.stream_host.pull_drag = None`
    /// and return false, letting the click fall through to camera /
    /// selection handling.
    #[cfg(not(target_arch = "wasm32"))]
    fn try_begin_pull_drag(&mut self, x: f32, y: f32) -> bool {
        use foldit_runner::orchestrator::{
            EntityId as RunnerEntityId, ResidueRef,
            DispatchContext as RunnerDispatchContext, TransitionKind,
        };

        let Some(engine) = self.engine.as_ref() else { return false };
        let target = engine.hovered_target();
        let store = &self.store;
        let route = match target {
            viso::PickTarget::Atom { entity_id, atom_idx } => {
                crate::pull_drag::route_atom_pick(store, entity_id, atom_idx)
            }
            viso::PickTarget::Residue(flat) => engine
                .picked_residue_atom(flat, (x, y))
                .and_then(|picked| {
                    let molex_id = store
                        .ids()
                        .find(|id| id.raw() == picked.entity_id)?;
                    crate::pull_drag::route_residue_pick(
                        store,
                        flat,
                        &picked.atom_name,
                        molex_id,
                        picked.local_residue,
                    )
                }),
            viso::PickTarget::None => None,
        };
        let Some(route) = route else { return false };

        let params = crate::pull_drag::build_start_params(&route);
        let pull_info = crate::pull_drag::build_pull_info(&route, (x, y));

        let ctx = RunnerDispatchContext {
            focused_entity_id: Some(RunnerEntityId(u64::from(route.entity_id.raw()))),
            selection: vec![ResidueRef {
                entity_id: RunnerEntityId(u64::from(route.entity_id.raw())),
                residue_index: route.residue_in_entity,
            }],
        };

        let Some(orch) = self.plugin_driver.orchestrator.as_mut() else { return false };
        let Some(cached) = orch.plugin_registry().get_op(route.op_id).cloned() else {
            log::warn!(
                "try_begin_pull_drag: op id {:?} missing from registry",
                route.op_id,
            );
            return false;
        };
        let plugin_id = cached.plugin_id.clone();

        let (rid, handle) = match orch.dispatch_start_stream(
            route.op_id,
            ctx,
            params,
            |id| entity_type_of(store, id),
        ) {
            Ok(r) => r,
            Err(e) => {
                log::warn!(
                    "try_begin_pull_drag: dispatch_start_stream {:?} failed: {e}",
                    route.op_id,
                );
                return false;
            }
        };

        // History side-effect — same shape as button-driven dispatch
        // so the drag's eventual commit_action lands as a regular
        // PluginOp entry. Failure is non-fatal (commit_action becomes
        // a no-op on an idle store).
        let action_entity = self
            .store
            .ids()
            .find(|id| id.raw() == route.entity_id.raw());
        if let Some(entity) = action_entity {
            let kind = CheckpointKind::PluginOp {
                entity,
                plugin_id: plugin_id.clone(),
                op_id: String::from(route.op_id),
                display: String::from("Pull"),
            };
            if let Err(e) =
                self.store.begin_action(kind, String::from("Pull"))
            {
                log::trace!("try_begin_pull_drag: begin_action skipped: {e}");
            }
        }

        let _ = self.plugin_driver.stream_host.active_streams.insert(
            rid,
            ActiveStreamEntry {
                handle,
                plugin_id: plugin_id.clone(),
                transition: TransitionKind::default(),
            },
        );
        self.plugin_driver.stream_host.pull_drag = Some(crate::pull_drag::PullDrag {
            request_id: rid,
            plugin_id,
            pull_info,
        });
        true
    }

    /// Pointer-move during an active drag: re-resolve the world-space
    /// drag target through the camera, and push a single-key
    /// `endpoint` Vec3 update to the running stream. Also refreshes
    /// `pull_info.screen_target` so the next visualization pass moves
    /// the cone tip with the cursor.
    #[cfg(not(target_arch = "wasm32"))]
    fn update_pull_drag(&mut self, x: f32, y: f32) {
        use foldit_runner::orchestrator::ParamValue;

        let Some(drag) = self.plugin_driver.stream_host.pull_drag.as_mut() else { return };
        drag.pull_info.screen_target = (x, y);

        let (residue, atom_name, plugin_id, request_id) = (
            drag.pull_info.atom.residue,
            drag.pull_info.atom.atom_name.clone(),
            drag.plugin_id.clone(),
            drag.request_id,
        );

        let Some(engine) = self.engine.as_ref() else { return };
        let Some(atom_pos) = engine.resolve_atom_position(residue, &atom_name)
        else {
            return;
        };
        let target =
            engine.screen_to_world_at_depth(glam::Vec2::new(x, y), atom_pos);

        let Some(orch) = self.plugin_driver.orchestrator.as_ref() else { return };
        let mut params = std::collections::HashMap::new();
        let _ = params.insert(
            String::from("endpoint"),
            ParamValue::Vec3([target.x, target.y, target.z]),
        );
        if let Err(e) = orch.dispatch_update_stream(&plugin_id, request_id, params)
        {
            log::trace!(
                "update_pull_drag: dispatch_update_stream rid={request_id} failed: {e}"
            );
        }
    }

    /// Pointer-up (or any cancel signal): tear down the drag state
    /// and ask the orchestrator to cancel the stream. The stream's
    /// terminal `PluginUpdate::Cancelled` flows through
    /// `apply_backend_updates` → `commit_action`, so the partial pull
    /// becomes a permanent undo entry.
    #[cfg(not(target_arch = "wasm32"))]
    fn end_pull_drag(&mut self) {
        let Some(drag) = self.plugin_driver.stream_host.pull_drag.take() else { return };
        let Some(orch) = self.plugin_driver.orchestrator.as_ref() else { return };
        if let Err(e) =
            orch.dispatch_cancel_stream(&drag.plugin_id, drag.request_id)
        {
            log::trace!(
                "end_pull_drag: dispatch_cancel_stream rid={} failed: {e}",
                drag.request_id,
            );
        }
    }

    pub fn handle_trigger_action(&mut self, action: foldit_gui::ActionId) {
        // Undo / Redo are not plugin ops -- they operate on the
        // Session history directly, with `&mut self` to reach
        // both store + engine.
        match action {
            foldit_gui::ActionId::Undo => self.handle_undo(),
            foldit_gui::ActionId::Redo => self.handle_redo(),
        }
    }

    /// Dispatch a plugin op by op-id. Resolves the op against the
    /// orchestrator's `PluginRegistry` to pick Invoke vs Start_stream;
    /// builds a `DispatchContext` from the GUI-provided focus and the
    /// authoritative in-core `App.selection`. Op-ids unknown to the
    /// registry are logged and dropped (the catalog couldn't have
    /// surfaced them, so this is either a stale GUI cache or a
    /// misrouted message).
    pub fn handle_dispatch_op(&mut self, op: foldit_gui::OpDispatch) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            // Drain pending terminals so rapid follow-up dispatches
            // see released locks.
            self.apply_backend_updates();

            use foldit_runner::orchestrator::{
                EntityId as RunnerEntityId, ResidueRef,
                DispatchContext as RunnerDispatchContext,
            };

            // Snapshot the authoritative selection store before the
            // upcoming `&mut self.plugin_driver` borrow. Flatten
            // BTreeMap<EntityId, BTreeSet<u32>> into the wire-shape
            // ResidueRef list the orchestrator's DispatchContext
            // expects.
            let selection: Vec<ResidueRef> = self
                .selection
                .iter()
                .flat_map(|(entity, residues)| {
                    let runner_id = RunnerEntityId(u64::from(entity.raw()));
                    residues.iter().map(move |&residue_index| ResidueRef {
                        entity_id: runner_id,
                        residue_index,
                    })
                })
                .collect();

            let Some(orch) = self.plugin_driver.orchestrator.as_mut() else {
                log::warn!(
                    "handle_dispatch_op({:?}): orchestrator not initialized",
                    op.op_id
                );
                return;
            };

            let Some(cached) =
                orch.plugin_registry().get_op(&op.op_id).cloned()
            else {
                log::warn!(
                    "handle_dispatch_op: op-id {:?} not in registry",
                    op.op_id
                );
                return;
            };

            // Resolve display label + animation preset from the
            // manifest catalog. Falls back to the op id + Smooth when
            // the op isn't surfaced as a button (the dispatcher still
            // routes; the history entry just shows the op id, and
            // animation gets the host default).
            let (display, transition) = orch
                .ops_catalog()
                .into_iter()
                .find(|e| e.plugin_id == cached.plugin_id && e.op_id == op.op_id)
                .map_or_else(
                    || {
                        (
                            op.op_id.clone(),
                            foldit_runner::orchestrator::TransitionKind::default(),
                        )
                    },
                    |e| (e.display, e.transition),
                );
            let plugin_id = cached.plugin_id.clone();

            let ctx = RunnerDispatchContext {
                focused_entity_id: op.focused_entity_id.map(RunnerEntityId),
                selection,
            };
            let params: std::collections::HashMap<
                String,
                foldit_runner::orchestrator::ParamValue,
            > = op
                .params
                .into_iter()
                .map(|(k, v)| (k, wire_params::param_value_from_wire(v)))
                .collect();

            // Drop the orchestrator borrow before reaching back into
            // `self.plugin_driver` for the consolidated dispatch.
            let _ = orch;
            // Hoist a shared borrow of the store so the lookup closure
            // can capture it alongside the upcoming `&mut self.plugin_driver`
            // call (disjoint field paths).
            let store = &self.store;
            let dispatch_outcome = self.plugin_driver.dispatch_op(
                &op.op_id,
                cached.kind,
                ctx,
                params,
                plugin_id.clone(),
                transition,
                |id| entity_type_of(store, id),
            );

            // Resolve which entity this op targets. The focused entity
            // wins; otherwise pick the lone protein in the head
            // checkpoint. For multi-entity sessions with no focus we
            // skip the history side-effect entirely (the action still
            // runs plugin-side, the viewport just won't reflect the
            // tentative until a per-op multi-entity action kind
            // exists). `EntityId` is opaque — look the raw u32 up
            // against the store's existing ids instead of minting a
            // new one (which would advance the allocator).
            let action_entity = op
                .focused_entity_id
                .and_then(|raw| {
                    self.store
                        .ids()
                        .find(|id| u64::from(id.raw()) == raw)
                })
                .or_else(|| first_protein_entity(&self.store));

            // Skip on dispatch failure: any open tentative belongs to
            // a prior op, not this one.
            if dispatch_outcome.is_ok() {
                if let Some(entity) = action_entity {
                    let kind = CheckpointKind::PluginOp {
                        entity,
                        plugin_id: plugin_id.clone(),
                        op_id: op.op_id.clone(),
                        display: display.clone(),
                    };
                    if let Err(e) = self.store.begin_action(kind, display) {
                        log::trace!(
                            "handle_dispatch_op({:?}): begin_action skipped: {e}",
                            op.op_id
                        );
                    }
                }
            }

            match dispatch_outcome {
                Ok(OpOutcome::Stream) => {
                    // Stream dispatch: `PluginDriver::dispatch_op`
                    // already inserted the `ActiveStreamEntry`. The
                    // matching terminal arm in `apply_backend_updates`
                    // does the cleanup; nothing else to do here.
                }
                Ok(OpOutcome::Invoke(bytes)) => {
                    self.apply_invoke_result(&bytes, transition);
                }
                Err(e) => {
                    log::error!(
                        "handle_dispatch_op({:?}): dispatch failed: {e}",
                        op.op_id
                    );
                }
            }
            self.ui_dirty |=
                DirtyFlags::ACTIONS | DirtyFlags::SCORE | DirtyFlags::UI;
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = op;
        }
    }

    /// Apply the assembly bytes returned by a one-shot `dispatch_invoke`
    /// to the ongoing tentative and commit it. Mirrors the Stream-side
    /// `Final` path; called from `handle_dispatch_op` for `OpKind::Invoke`.
    /// `transition` is the manifest-declared animation preset, queued
    /// on the locked entity so the next tick's render-projector
    /// publish eases the result in rather than snapping.
    #[cfg(not(target_arch = "wasm32"))]
    fn apply_invoke_result(
        &mut self,
        bytes: &[u8],
        transition: foldit_runner::orchestrator::TransitionKind,
    ) {
        let assembly = match molex::ops::wire::deserialize_assembly(bytes) {
            Ok(a) => a,
            Err(e) => {
                log::warn!("dispatch_invoke: decode failed: {e:?}");
                if self.store.has_ongoing_action() {
                    let _ = self.store.commit_action();
                }
                return;
            }
        };
        let applied =
            apply_streaming_assembly(&mut self.store, &assembly, None);
        if applied {
            let entity = self.store.history().ongoing().locked_entity();
            if let Err(e) = self.store.commit_action() {
                log::warn!("dispatch_invoke: commit_action failed: {e}");
            }
            if let Some(engine) = self.engine.as_mut() {
                if let Some(eid) = entity {
                    // Queue the manifest-declared animation for the
                    // upcoming render projector publish (tick will
                    // call set_assembly which consumes the queued
                    // transition; HeadMoved from commit_action is the
                    // spine signal that triggers the publish).
                    engine.queue_entity_transition(
                        eid.raw(),
                        resolve_transition(transition),
                    );
                }
            }
            self.ui_dirty |=
                DirtyFlags::SCENE | DirtyFlags::HISTORY;
        } else if self.store.has_ongoing_action() {
            // Nothing matched (e.g. plugin returned an empty / unrelated
            // assembly): drop the tentative.
            let _ = self.store.commit_action();
        }
    }

    pub fn handle_parameterized_action(
        &mut self,
        action: foldit_gui::ParameterizedAction,
    ) {
        self.handle_parameterized_action_inner(action);
    }

    fn handle_parameterized_action_inner(
        &mut self,
        action: foldit_gui::ParameterizedAction,
    ) {
        use foldit_gui::ParameterizedAction;

        // History-side commands take &mut self (no engine borrow held).
        if let ParameterizedAction::History { cmd } = action {
            self.run_history_command(cmd);
            return;
        }

        // Bubble cursor advance is engine-independent.
        if let ParameterizedAction::AdvanceBubble { back } = action {
            self.advance_bubble(back);
            return;
        }

        if self.engine.is_none() {
            return;
        }

        // Engine borrow is taken per-arm now (LoadStructure / LoadPuzzle
        // need to release the borrow before `self.tick(0.0)`, which is
        // how the render projector republishes after a load).
        match action {
            ParameterizedAction::LoadStructure { path } => self.handle_load_structure(path),
            ParameterizedAction::LoadPuzzle { puzzle_id } => self.handle_load_puzzle(puzzle_id),
            ParameterizedAction::CreateBand { .. } => {
                log::info!("CreateBand via IPC not yet wired");
            }
            ParameterizedAction::RemoveBand { .. } => {
                log::info!("RemoveBand via IPC not yet wired");
            }
            ParameterizedAction::SetViewOptions { options } => {
                if let Some(engine) = self.engine.as_mut() {
                    match serde_json::from_value::<viso::options::VisoOptions>(options) {
                        Ok(opts) => {
                            engine.set_options(opts);
                            self.ui_dirty |= DirtyFlags::VIEW;
                        }
                        Err(e) => log::error!("Failed to deserialize view options: {}", e),
                    }
                }
            }
            ParameterizedAction::LoadViewPreset { name } => {
                #[cfg(not(target_arch = "wasm32"))]
                if let Some(dir) = self.host.view_presets_dir() {
                    if let Some(engine) = self.engine.as_mut() {
                        engine.load_preset(&name, dir);
                        self.ui_dirty |= DirtyFlags::VIEW;
                    }
                }
                #[cfg(target_arch = "wasm32")]
                let _ = name;
            }
            ParameterizedAction::SaveViewPreset { name } => {
                #[cfg(not(target_arch = "wasm32"))]
                if let Some(dir) = self.host.view_presets_dir() {
                    if let Some(engine) = self.engine.as_mut() {
                        engine.save_preset(&name, dir);
                        self.ui_dirty |= DirtyFlags::VIEW;
                    }
                }
                #[cfg(target_arch = "wasm32")]
                let _ = name;
            }
            ParameterizedAction::History { .. }
            | ParameterizedAction::AdvanceBubble { .. } => {
                // Handled in the early-return block above. The match is
                // exhaustive over `ParameterizedAction` (G10): a new
                // variant without a handler is a compile error.
            }
        }
    }

    /// Free-form file load (Scientist mode). Ingest entities, set
    /// metadata, then tick + fit the camera (tick is how the render
    /// projector republishes — the spine carries `PreviewAdded`s and
    /// `HeadMoved`s from `load_entity_into_history`).
    fn handle_load_structure(&mut self, path: String) {
        match crate::puzzle::load_file_as_entities(&path) {
            Ok((entities, name)) => {
                log::info!("Loaded structure via IPC: {}", name);
                for entity in entities {
                    let _ = load_entity_into_history(&mut self.store, entity, name.clone());
                }
                self.gui_projector.scoring_mode = ScoringMode::Scientist;
                self.structure_title = Some(name.clone());
                self.loaded_puzzle = None;
                self.gui_projector.bubbles.clear();
                self.gui_projector.current_bubble = 0;
                self.ui_dirty |= DirtyFlags::LOADING
                    | DirtyFlags::ACTIONS
                    | DirtyFlags::SCORE
                    | DirtyFlags::PUZZLE;

                // Publish + fit. tick(0.0) drains the spine, publishes
                // via the render projector, and runs engine.update(0.0)
                // so fit_camera_to_focus has bounding-radius to read.
                self.tick(0.0);
                if let Some(engine) = self.engine.as_mut() {
                    engine.fit_camera_to_focus();
                }
            }
            Err(e) => {
                log::error!("Failed to load structure '{}': {}", path, e);
            }
        }
        self.ui_dirty |= DirtyFlags::LOADING | DirtyFlags::SCORE;
    }

    /// Tutorial / campaign puzzle load (Game mode). Ingest entities and
    /// metadata, then tick + snap + apply the puzzle's saved pose.
    fn handle_load_puzzle(&mut self, puzzle_id: u32) {
        let title = self.structure_title();
        self.store.reset();
        self.plugin_driver.reset_for_new_structure();
        // Topology swap: selection keys are entity ids from the
        // outgoing assembly; the new puzzle's entity ids may collide
        // numerically without referring to the same entities, so clear.
        // Going through the mutator self-sets SELECTION dirty.
        self.clear_selection();

        match crate::puzzle::load_puzzle_structure(puzzle_id) {
            Ok(puzzle_data) => {
                self.gui_projector.scoring_mode = ScoringMode::Game;
                self.loaded_puzzle = Some(LoadedPuzzle {
                    id: puzzle_id,
                    title: puzzle_data.name.clone(),
                    starting_score: puzzle_data.start_energy,
                    target_score: puzzle_data.completion_score,
                });
                self.structure_title = Some(puzzle_data.name.clone());
                self.gui_projector.bubbles = puzzle_data.bubbles;
                self.gui_projector.current_bubble = 0;

                #[cfg(not(target_arch = "wasm32"))]
                if let Some(preset_name) = &puzzle_data.view_preset {
                    if let Some(dir) = self.host.view_presets_dir() {
                        if let Some(engine) = self.engine.as_mut() {
                            engine.load_preset(preset_name, dir);
                        }
                    }
                }

                let ss_override = puzzle_data.ss_override;
                let cam = &puzzle_data.camera;
                let cam_eye = glam::Vec3::new(cam.eye[0] as f32, cam.eye[1] as f32, cam.eye[2] as f32);
                let cam_up = glam::Vec3::new(cam.up[0] as f32, cam.up[1] as f32, cam.up[2] as f32);

                let mut ids: Vec<EntityId> = Vec::new();
                for entity in puzzle_data.entities {
                    if let Some(id) = load_entity_into_history(&mut self.store, entity, title.clone()) {
                        ids.push(id);
                    }
                }

                // Topology swap rides the spine — tick's render
                // projector picks `replace_assembly` because the id set
                // differs from the last publish (post-reset = empty).
                self.tick(0.0);

                if let Some(engine) = self.engine.as_mut() {
                    // Snap so bounding_radius reflects molecule extent
                    // (fog driver), then override the pose with the
                    // puzzle's saved eye/up but anchor the orbit
                    // center on the protein centroid.
                    engine.snap_camera_to_focus();
                    if let Some(centroid) = engine.focus_centroid() {
                        engine.set_camera_pose(centroid, cam_eye, cam_up);
                    }

                    if let Some(ss) = ss_override {
                        if let Some(&first_id) = ids.first() {
                            engine.set_ss_override(first_id.raw(), ss);
                        }
                    }
                }

                // Rosetta session init via bridge plugin's `init` +
                // auto-`update_assembly` fan-out lands when the
                // orchestrator's ensure_plugin_registered path is
                // invoked for "rosetta" with the new assembly.
                let _ = puzzle_id;
            }
            Err(e) => log::error!("Failed to load puzzle {}: {}", puzzle_id, e),
        }
        self.ui_dirty |= DirtyFlags::LOADING
            | DirtyFlags::SCORE
            | DirtyFlags::ACTIONS
            | DirtyFlags::PUZZLE;
    }

    // ── Tutorial-bubble cursor ──

    /// Step the tutorial-bubble cursor and mark the bubble dirty so
    /// `populate_frontend` re-pushes the new head. Forward saturates at
    /// `bubbles.len()` (one past the end; the GUI sees `None`
    /// and clears); back saturates at 0.
    fn advance_bubble(&mut self, back: bool) {
        if back {
            self.gui_projector.current_bubble =
                self.gui_projector.current_bubble.saturating_sub(1);
        } else if self.gui_projector.current_bubble < self.gui_projector.bubbles.len() {
            self.gui_projector.current_bubble += 1;
        }
        self.ui_dirty |= DirtyFlags::TEXT_BUBBLE;
    }

    // ── History navigation (Undo / Redo / Jump / Pin) ──

    pub fn handle_undo(&mut self) {
        self.run_history_command(HistoryCommand::Undo);
    }

    pub fn handle_redo(&mut self) {
        self.run_history_command(HistoryCommand::Redo { branch: None });
    }

    /// Common tail for undo / redo / jump_checkpoint: clear cached
    /// per-residue scores (the values were computed against the
    /// *previous* head and become meaningless on a head move; v1 just
    /// blanks them so the structure renders neutral instead of "gray",
    /// v2 will async-reeval), and mark UI dirty. Score is no longer
    /// cached in `App`; the GUI projection reads it off the new head
    /// checkpoint on the next `populate_frontend` (G1). The render
    /// projector republishes via the spine — `HeadMoved` emitted by
    /// undo/redo/jump triggers the next tick's render projector to
    /// pick `replace_assembly` (when entity-id set changed) or
    /// `set_assembly` (when it didn't).
    fn after_head_move(&mut self) {
        if let Some(engine) = self.engine.as_mut() {
            let ids: Vec<EntityId> = self.store.ids().collect();
            for eid in ids {
                engine.set_per_residue_scores(eid.raw(), None);
            }
        }

        self.ui_dirty |= DirtyFlags::SCORE | DirtyFlags::ACTIONS | DirtyFlags::SCENE;
    }

    /// Dispatch a [`HistoryCommand`] from the GUI to the matching
    /// `Session` method. Refusals are logged; the GUI surface
    /// shows the result by virtue of the head not moving (no separate
    /// toast / error channel — `HistoryError::EntityLocked` only
    /// fires while the user's own action is still running, where the
    /// running indicator is the natural feedback). The match is
    /// exhaustive (G10): adding a variant without a handler is a
    /// compile error.
    fn run_history_command(&mut self, cmd: HistoryCommand) {
        if self.engine.is_none() {
            return;
        }
        let result: Result<HistoryOutcome, SessionError> = match cmd {
            HistoryCommand::JumpCheckpoint { id } => self
                .store
                .jump_checkpoint(id.into_inner())
                .map(|_| HistoryOutcome::HeadMoved),
            HistoryCommand::Undo => self.store.undo().map(|opt| match opt {
                Some(_) => HistoryOutcome::HeadMoved,
                None => {
                    log::info!("Undo: already at root");
                    HistoryOutcome::Noop
                }
            }),
            HistoryCommand::Redo { branch } => self
                .store
                .redo(branch.map(|w| w.into_inner()))
                .map(|opt| match opt {
                    Some(_) => HistoryOutcome::HeadMoved,
                    None => {
                        log::info!("Redo: nowhere forward to go");
                        HistoryOutcome::Noop
                    }
                }),
            HistoryCommand::LaneUndo { entity, target } => self
                .store
                .lane_undo(entity, target.into_inner())
                .map(|_| HistoryOutcome::HeadMoved),
            HistoryCommand::LaneRedo { entity, branch } => self
                .store
                .lane_redo(entity, branch.map(|w| w.into_inner()))
                .map(|_| HistoryOutcome::HeadMoved),
            HistoryCommand::PinCheckpoint { id } => self
                .store
                .pin_checkpoint(id.into_inner())
                .map(|_| HistoryOutcome::Curated),
            HistoryCommand::UnpinCheckpoint { id } => self
                .store
                .unpin_checkpoint(id.into_inner())
                .map(|_| HistoryOutcome::Curated),
            HistoryCommand::SetExcludeFromBest { id, exclude } => self
                .store
                .set_exclude_from_best(id.into_inner(), exclude)
                .map(|_| HistoryOutcome::Curated),
            HistoryCommand::AbortAction => self
                .store
                .abort_action()
                .map(|_| HistoryOutcome::HeadMoved),
        };

        match result {
            Ok(HistoryOutcome::HeadMoved) => self.after_head_move(),
            Ok(HistoryOutcome::Curated) => {
                self.ui_dirty |= DirtyFlags::ACTIONS;
            }
            Ok(HistoryOutcome::Noop) => {}
            Err(e) => log::warn!("history command refused: {e}"),
        }
    }

    // ── Native input (when webview is not ready) ──

    pub fn handle_native_mouse_input(
        &mut self,
        button: viso::MouseButton,
        pressed: bool,
    ) {
        let pending_click = if let Some(engine) = &mut self.engine {
            let click = engine.feed_pointer_button(button, pressed);
            update_all_visualizations(engine, None);
            click
        } else {
            None
        };
        if let Some(click) = pending_click {
            self.apply_click_to_selection(&click);
        }
    }

    pub fn handle_native_cursor_moved(&mut self, x: f32, y: f32) {
        if let Some(engine) = &mut self.engine {
            engine.set_cursor_pos(x, y);
            update_all_visualizations(engine, None);
        }
    }

    /// Forward a scroll delta in viso "logical scroll units" (winit
    /// `LineDelta(_, y)` passes `y` directly; `PixelDelta(_, y)` should
    /// pass `y * 0.01`). Conversion lives in the host.
    pub fn handle_native_mouse_wheel(&mut self, scroll_delta: f32) {
        if let Some(engine) = &mut self.engine {
            engine.feed_scroll(scroll_delta);
        }
    }

    pub fn handle_native_modifiers(&mut self, shift: bool) {
        if let Some(engine) = &mut self.engine {
            engine.feed_modifiers(shift);
        }
    }

    // ── Per-frame visual updates ──

    pub fn update_frame_visuals(&mut self) {
        // Pre-snapshot pull info under an immutable borrow so the
        // subsequent `&mut engine` doesn't conflict.
        #[cfg(not(target_arch = "wasm32"))]
        let pull = self.plugin_driver.stream_host.pull_drag.as_ref().map(|d| d.pull_info.clone());
        #[cfg(target_arch = "wasm32")]
        let pull: Option<viso::PullInfo> = None;
        let Some(engine) = &mut self.engine else { return };
        update_all_visualizations(engine, pull);
    }

    // ── Frontend state sync ──

    /// Set the host log mirror on the owned frontend. Hosts call this
    /// to ship the latest log buffer (drained from their own tee).
    pub fn set_frontend_log(&mut self, log: String) {
        self.frontend.set_log(log);
    }

    /// Serialize whatever sections of the owned [`FrontendState`] are
    /// currently dirty into a JSON byte string suitable for an IPC
    /// push, and clear the dirty bits. Returns `None` when nothing
    /// changed since the last drain. The host pipes the bytes straight
    /// into its webview / `wasm-bindgen` callback.
    pub fn serialize_frontend_dirty(&mut self) -> Option<Vec<u8>> {
        foldit_gui::bridge::push::serialize_dirty(&mut self.frontend)
            .map(|v| v.to_string().into_bytes())
    }

    fn populate_frontend(&mut self) {
        let selected_count = self.selection_total_count();
        let engine = match &self.engine {
            Some(e) => e,
            None => return,
        };

        // FPS and selected count change every frame — always push them
        self.frontend.set_fps(engine.fps());
        self.frontend.ui.selected_count = selected_count;

        let app_dirty = self.ui_dirty;
        self.ui_dirty = DirtyFlags::empty();
        if app_dirty.is_empty() {
            return;
        }

        // PUZZLE before SCORE: a fresh `set_puzzle_*` resets `complete=false`,
        // and then the score check below can latch victory in the same frame
        // without being overwritten.
        if app_dirty.contains(DirtyFlags::PUZZLE) {
            match self.gui_projector.scoring_mode {
                ScoringMode::Game => {
                    // Game mode implies LoadPuzzle ran, which populates
                    // `loaded_puzzle`. The two are set together at every
                    // mode-changing site, so a Game-mode tick with no
                    // `loaded_puzzle` is a programming error, not a
                    // user-visible state.
                    if let Some(p) = &self.loaded_puzzle {
                        self.frontend.set_puzzle_game(
                            p.id,
                            p.title.clone(),
                            p.starting_score,
                            p.target_score,
                        );
                    } else {
                        log::warn!(
                            "populate_frontend: Game mode with no loaded_puzzle; skipping set_puzzle_game",
                        );
                    }
                }
                // Scientist mode has no puzzle objective by construction
                // (LoadStructure clears `loaded_puzzle`), so the title
                // is the file-derived structure name.
                ScoringMode::Scientist => {
                    self.frontend.set_puzzle_scientist(self.structure_title())
                }
            }
            // Bubble push on puzzle swap: render the cursor's current
            // bubble (always index 0 right after LoadPuzzle, since the
            // cursor is reset there). Subsequent AdvanceBubble actions
            // re-push via the DirtyFlags::TEXT_BUBBLE arm below.
            self.frontend.set_text_bubble(
                self.gui_projector
                    .bubbles
                    .get(self.gui_projector.current_bubble)
                    .map(bubble_to_payload),
            );
        }
        if app_dirty.contains(DirtyFlags::TEXT_BUBBLE) {
            self.frontend.set_text_bubble(
                self.gui_projector
                    .bubbles
                    .get(self.gui_projector.current_bubble)
                    .map(bubble_to_payload),
            );
        }
        if app_dirty.contains(DirtyFlags::SCORE) {
            if let Some(score) = head_score(&self.store, self.gui_projector.scoring_mode) {
                self.frontend.set_score(score, false);
                // Victory check: in Game mode, latch puzzle as complete the
                // first time current_score crosses the toml target. Higher
                // game score = better fold (game-score formula negates),
                // so the comparison is `>=`.
                if self.gui_projector.scoring_mode == ScoringMode::Game {
                    if let Some(p) = &self.loaded_puzzle {
                        if p.target_score > 0.0 && score >= p.target_score {
                            self.frontend.mark_puzzle_complete();
                        }
                    }
                }
            }
        }
        if app_dirty.contains(DirtyFlags::ACTIONS) {
            self.frontend.set_actions(wire_params::build_actions_list(&self.plugin_driver.orchestrator));
        }
        if app_dirty.contains(DirtyFlags::LOADING) {
            self.frontend.set_loading_progress(None);
        }
        if app_dirty.contains(DirtyFlags::VIEW) {
            self.frontend.view.options = serde_json::to_value(engine.options()).unwrap_or_default();

            // Schema is static — only set once
            if self.frontend.view.options_schema.is_null() {
                self.frontend.view.options_schema =
                    serde_json::to_value(viso::options::VisoOptions::json_schema())
                        .unwrap_or_default();
            }

            #[cfg(not(target_arch = "wasm32"))]
            {
                self.frontend.view.available_presets = self
                    .host
                    .view_presets_dir()
                    .map(viso::options::VisoOptions::list_presets)
                    .unwrap_or_default();
            }
            self.frontend.view.active_preset = engine.active_preset().map(String::from);
        }
        if app_dirty.contains(DirtyFlags::SELECTION) {
            let entries: Vec<foldit_gui::EntitySelection> = self
                .selection
                .iter()
                .map(|(eid, residues)| foldit_gui::EntitySelection {
                    entity_id: eid.raw(),
                    residues: residues.iter().copied().collect(),
                })
                .collect();
            self.frontend.set_selection(entries);
        }
        if app_dirty.contains(DirtyFlags::UI) {
            self.frontend.mark_dirty(DirtyFlags::UI);
        }
        if app_dirty.contains(DirtyFlags::LOADING) || app_dirty.contains(DirtyFlags::SCENE) {
            use molex::MoleculeType;
            let mut scene_entities = Vec::new();
            for (eid, _meta) in self.store.iter() {
                let Some(entity) = self.store.entity(eid) else {
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
                    molecule_type: mol_str.to_string(),
                    atom_count: entity.atom_count(),
                    residue_count: entity.residue_count(),
                });
            }
            self.frontend.set_scene_entities(scene_entities);
            let focused = match engine.focus() {
                Focus::Entity(eid) => Some(eid.raw()),
                Focus::All => None,
            };
            self.frontend.set_focused_entity(focused);
        }

        // History push (two-channel):
        //   - topology bump → full `HistorySection`
        //   - live bump only → small `HistoryLiveUpdate` patch, with a
        //     50ms (20Hz) debounce so per-cycle Rosetta scores don't
        //     saturate the IPC. The final cycle on commit always lands
        //     because committing also bumps `topology_version`.
        let topology = self.store.history().topology_version();
        let live = self.store.history().live_version();
        let cursor = &mut self.gui_projector.history_sync;
        let topology_changed = cursor.last_topology != Some(topology);
        let live_changed = cursor.last_live != Some(live);

        if topology_changed {
            self.frontend.set_history(project_history(&self.store));
            cursor.last_topology = Some(topology);
            cursor.last_live = Some(live);
            cursor.last_live_push_at = Some(Instant::now());
        } else if live_changed {
            let now = Instant::now();
            let debounced = cursor
                .last_live_push_at
                .map_or(false, |t| now.duration_since(t).as_millis() < 50);
            if !debounced {
                if let Some(update) = project_history_live(self.store.history()) {
                    self.frontend.set_history_live(update);
                    cursor.last_live = Some(live);
                    cursor.last_live_push_at = Some(now);
                }
            }
        }

    }

    // ── The per-frame drive loop ──

    /// Drive one frame.
    ///
    /// Order:
    /// 1. drain pending plugin updates (apply to `Session`; emits
    ///    `SessionUpdate`s through the funnel).
    /// 2. drain the `SessionUpdate` spine in one go.
    /// 3. route the batch: plugin broadcaster fan-out and render
    ///    projector publish (both no-op on empty batches).
    /// 4. poll plugin scores (refresh head scores, ui_dirty SCORE/HISTORY).
    /// 5. engine update (camera animation, mesh upload, etc.).
    /// 6. visualization overlay (bands / pull).
    /// 7. Loading → InPuzzle gate (one-shot, on first score).
    /// 8. populate frontend so the next `serialize_frontend_dirty`
    ///    carries the latest snapshot.
    pub fn tick(&mut self, dt: f32) {
        // 1. Plugin updates.
        self.apply_backend_updates();

        // 2-3. Drain the spine once and route to both projectors. The
        //      tick is the sole drain — handlers used to call
        //      `pump_scene_changes` per-event, but that race-condition'd
        //      against the render projector reading the same spine, so
        //      the per-handler pumps were removed in RX13.
        let changes = self.store.take_updates();
        if !changes.is_empty() {
            if let Some(orch) = self.plugin_driver.orchestrator.as_mut() {
                self.plugin_driver
                    .broadcaster
                    .broadcast(&changes, &self.store, orch);
            }
            if let Some(engine) = self.engine.as_mut() {
                self.render_projector
                    .project(&changes, &self.store, engine);
            }
        }

        // 4. Plugin score poll. Tick-driven cadence (RX13).
        self.poll_plugin_scores();

        // 5. Engine update + 6. visualization overlay.
        #[cfg(not(target_arch = "wasm32"))]
        let pull = self.plugin_driver.stream_host.pull_drag.as_ref().map(|d| d.pull_info.clone());
        #[cfg(target_arch = "wasm32")]
        let pull: Option<viso::PullInfo> = None;
        if let Some(engine) = self.engine.as_mut() {
            engine.update(dt);
            update_all_visualizations(engine, pull);
        }

        // 7. State-machine: flip Loading → InPuzzle the first time the
        //    plugin score lands for the just-loaded session.
        if self.awaiting_initial_score && self.has_initial_score() {
            self.app_state = AppState::InPuzzle;
            self.awaiting_initial_score = false;
            self.frontend.set_app_state(AppState::InPuzzle);
            self.frontend.set_puzzle_loaded(true);
            self.frontend.set_score_title(self.structure_title());
            self.frontend.set_puzzle_scientist(self.structure_title());
            self.frontend.mark_all_dirty();
            log::info!("Initial plugin score received — app_state=InPuzzle");
        }

        // 8. Frontend population.
        self.populate_frontend();
    }

    // ── Complex lifecycle (engine attach + initial load) ──

    /// Attach a host-built `VisoEngine` to this App. Hosts are
    /// responsible for constructing the wgpu `RenderContext` against
    /// their own surface (winit window on desktop, `<canvas>` on web)
    /// and applying any preset / render-scale tweaks they want before
    /// handing it over.
    pub fn attach_engine(&mut self, engine: VisoEngine) {
        self.engine = Some(engine);
    }

    /// Load the initial structure, register entities, and create the
    /// initial Rosetta session. Runs AFTER the webview's loading screen
    /// is visible so the user has feedback during the (potentially
    /// slow) load. Requires `create_render_context` to have run first.
    ///
    /// Bootstrap path comes from the host (`HostResources::initial_structure_path`);
    /// `None` is a no-op (e.g. the web shell loads structures via a
    /// separate flow rather than a startup path).
    pub fn load_initial_structure(&mut self) {
        if self.engine.is_none() {
            log::error!("load_initial_structure called before create_render_context");
            return;
        }

        let Some(path) = self.host.initial_structure_path() else {
            return;
        };

        // Parse entities from file
        match crate::puzzle::load_file_as_entities(&path) {
            Ok((entities, name)) => {
                for entity in entities {
                    let _ = load_entity_into_history(&mut self.store, entity, name.clone());
                }
                self.structure_title = Some(name.clone());

                // Publish + fit. tick(0.0) drains the spine, hands the
                // assembly to the render projector, and runs
                // engine.update(0.0) so the pending Assembly is drained
                // before fit_camera reads bounding-radius.
                self.tick(0.0);
                if let Some(engine) = self.engine.as_mut() {
                    engine.fit_camera_to_focus();
                }

                log::info!("Loaded structure: {}", name);

                // Construct + assign the orchestrator BEFORE
                // `bootstrap_plugins` so the method can reach it
                // through `self.plugin_driver.orchestrator.as_mut()`
                // instead of threading a locally-owned `&mut Orchestrator`.
                self.plugin_driver.orchestrator = Some(Orchestrator::new());
                self.bootstrap_plugins();

                // Republish: bootstrap may have committed rosetta's
                // post-Init normalized assembly (full-atom pose) into
                // the store. The HeadMoved emitted by commit_action
                // rides the spine; tick(0.0) flushes it and polls
                // scores, so has_initial_score() flips synchronously.
                self.tick(0.0);
            }
            Err(e) => {
                log::error!("Failed to load structure '{}': {}", path, e);
                self.plugin_driver.orchestrator = Some(Orchestrator::new());
            }
        }

        // Push the now-populated state to the GUI on the next frame:
        // VIEW for the engine options, ACTIONS so the catalog (wiggle
        // etc.) renders, SCORE so the initial number from
        // refresh_scores reaches the score widget, SCENE for the
        // entity list, LOADING to flip out of the loading screen.
        self.ui_dirty |= DirtyFlags::VIEW
            | DirtyFlags::ACTIONS
            | DirtyFlags::SCORE
            | DirtyFlags::SCENE
            | DirtyFlags::LOADING;

        // Arm the Loading → InPuzzle gate. `tick` flips `app_state` the
        // first frame `has_initial_score()` returns true (plugins may
        // not have replied yet by the time we return here).
        self.awaiting_initial_score = true;
    }

    /// Discover plugins under the runtime plugin root and bring up the
    /// always-on Rosetta session with the just-loaded structure as the
    /// initial assembly. Errors are logged and dropped: a missing plugin
    /// dir / dylib should degrade the app to viewer-only, not crash the
    /// load.
    ///
    /// Caller must have assigned `self.plugin_driver.orchestrator` to
    /// `Some(Orchestrator::new())` before calling — this method reaches
    /// the orchestrator through that field rather than taking one as a
    /// parameter.
    ///
    /// If Rosetta's Init returns a non-empty normalized assembly (full-atom
    /// pose with hydrogens / terminal O / etc. added), it is committed as
    /// a follow-up `PluginOp` checkpoint and republished so that
    /// `scene.positions` is seeded at the normalized atom count before any
    /// user action runs. Without this, the first user op would cross an
    /// atom-set boundary mid-action and snap.
    #[cfg(not(target_arch = "wasm32"))]
    fn bootstrap_plugins(&mut self) {
        let Some(plugins_root) = locate_plugins_root() else {
            log::warn!(
                "[App] no plugins root found (set FOLDIT_PLUGINS_ROOT or run \
                 from a workspace checkout); plugins disabled"
            );
            return;
        };
        log::info!(
            "[App] discovering plugins under {}",
            plugins_root.display()
        );

        // Snapshot the initial assembly under an immutable store borrow
        // so we can hand it to `ensure_plugin_registered` for each plugin
        // without re-borrowing across iterations.
        let initial_assembly = {
            let head_before = self.store.head_assembly();
            match molex::ops::wire::serialize_assembly(&head_before) {
                Ok(b) => b,
                Err(e) => {
                    log::warn!(
                        "[App] failed to serialize initial assembly for plugin \
                         registration: {e:?}; plugins disabled"
                    );
                    return;
                }
            }
        };

        // Discover under a mut orch borrow scoped to this block so we
        // can later interleave orch and store calls.
        let discovered = {
            let Some(orch) = self.plugin_driver.orchestrator.as_mut() else {
                return;
            };
            match orch.discover_plugins(&plugins_root) {
                Ok(ids) => ids,
                Err(e) => {
                    log::warn!(
                        "[App] discover_plugins({}) failed: {e}; plugins disabled",
                        plugins_root.display()
                    );
                    return;
                }
            }
        };
        log::info!("[App] discovered plugins: {discovered:?}");

        for plugin_id in &discovered {
            let post_init_bytes = {
                let Some(orch) = self.plugin_driver.orchestrator.as_mut() else {
                    return;
                };
                match orch
                    .ensure_plugin_registered(plugin_id, initial_assembly.clone())
                {
                    Ok(bytes) => bytes,
                    Err(e) => {
                        log::warn!(
                            "[App] ensure_plugin_registered('{plugin_id}') failed: \
                             {e}; {plugin_id} plugin disabled"
                        );
                        continue;
                    }
                }
            };
            log::info!("[App] {plugin_id} plugin ready");

            if plugin_id == "rosetta" {
                self.apply_rosetta_post_init(&post_init_bytes);
            }
        }
    }

    /// Apply rosetta's post-Init normalized assembly (full-atom pose) so
    /// the host's canonical assembly matches the plugin's internal pose
    /// before any user action runs.
    #[cfg(not(target_arch = "wasm32"))]
    fn apply_rosetta_post_init(&mut self, post_init_bytes: &[u8]) {
        if post_init_bytes.is_empty() {
            log::warn!(
                "[App] rosetta post-Init returned no normalized assembly; \
                 first user action will likely snap because scene.positions \
                 stays at the pre-Init atom count."
            );
            return;
        }
        let normalized = match molex::ops::wire::deserialize_assembly(
            post_init_bytes,
        ) {
            Ok(a) => a,
            Err(e) => {
                log::warn!(
                    "[App] rosetta post-Init assembly decode failed: {e:?}; \
                     skipping normalization apply"
                );
                return;
            }
        };
        let Some(target_entity) = first_protein_entity(&self.store) else {
            log::warn!(
                "[App] rosetta post-Init: no protein entity in store; \
                 skipping normalization apply"
            );
            return;
        };
        let kind = CheckpointKind::PluginOp {
            entity: target_entity,
            plugin_id: String::from("rosetta"),
            op_id: String::from("_init_normalize"),
            display: String::from("Init"),
        };
        if let Err(e) = self.store.begin_action(kind, String::from("Init")) {
            log::warn!(
                "[App] rosetta post-Init begin_action failed: {e}; \
                 skipping normalization apply"
            );
            return;
        }
        let applied =
            apply_streaming_assembly(&mut self.store, &normalized, None);
        if !applied {
            log::warn!(
                "[App] rosetta post-Init apply_streaming_assembly did not \
                 update any entity; rolling back tentative. This usually means \
                 the rosetta-returned entity ID does not match any store \
                 entity ID."
            );
            let _ = self.store.commit_action();
            return;
        }
        if let Err(e) = self.store.commit_action() {
            log::warn!(
                "[App] rosetta post-Init commit_action failed: {e}"
            );
            return;
        }
        log::info!(
            "[App] rosetta post-Init assembly applied ({} bytes)",
            post_init_bytes.len()
        );
        // Republish is spine-driven: the HeadMoved from commit_action
        // rides through the next tick's render projector.
    }

    /// Shut down backends and scene processor.
    pub fn shutdown(&mut self) {
        self.plugin_driver.shutdown();
        if let Some(engine) = &mut self.engine {
            engine.shutdown();
        }
    }

    // ── Selection authority ──
    //
    // Apply / clear / toggle / query helpers over [`App::selection`].
    // Invariant maintained across every mutator: per-entity sets are
    // never left empty in the outer map. Removing the last residue on
    // an entity removes the entity entry.

    /// Flatten [`App::selection`] through the engine's per-entity
    /// residue offsets and overwrite the GPU residue selection. No-op
    /// before an engine is attached or before the first full rebuild
    /// has produced an offsets map. Entities absent from the offsets
    /// map (e.g. not yet meshed) contribute nothing.
    fn flush_selection_to_viso(&mut self) {
        let Some(engine) = self.engine.as_mut() else {
            return;
        };
        let mut flat: Vec<i32> = Vec::new();
        {
            let offsets = engine.entity_residue_offsets();
            for (eid, residues) in &self.selection {
                let Some(&base) = offsets.get(eid) else {
                    continue;
                };
                for r in residues {
                    flat.push((base + *r) as i32);
                }
            }
        }
        engine.set_selection(flat);
    }

    /// Apply a viso click-event to the selection store. Empty-area
    /// clicks clear the selection; non-empty expansions either replace
    /// (no modifier) or toggle (shift held) on a per-residue basis.
    /// Targets with an empty expansion (atom picks, non-protein hits)
    /// are no-ops on shift-held click and a clear on plain click; we
    /// follow the same "replace selection with the click's expansion"
    /// rule, which collapses to "clear" when the expansion is empty.
    fn apply_click_to_selection(&mut self, click: &ClickEvent) {
        match classify_click_for_selection(click) {
            ClickSelectionAction::Clear => {
                self.clear_selection();
            }
            ClickSelectionAction::Replace(residues) => {
                self.clear_selection();
                for (entity, residue) in residues {
                    self.select_residue(entity, residue);
                }
            }
            ClickSelectionAction::Toggle(residues) => {
                for (entity, residue) in residues {
                    let _ = self.toggle_residue(entity, residue);
                }
            }
        }
    }

    /// Mark a single residue on `entity` as selected. Idempotent:
    /// re-selecting an already-selected residue is a no-op.
    pub(crate) fn select_residue(&mut self, entity: EntityId, residue_index: u32) {
        self.selection
            .entry(entity)
            .or_default()
            .insert(residue_index);
        self.ui_dirty |= DirtyFlags::SELECTION;
        self.flush_selection_to_viso();
    }

    /// Mark a single residue on `entity` as deselected. Idempotent on
    /// already-empty state. If this empties the per-entity set, the
    /// entity entry is removed from the outer map (sets are never
    /// left empty).
    pub(crate) fn deselect_residue(&mut self, entity: EntityId, residue_index: u32) {
        if let Some(set) = self.selection.get_mut(&entity) {
            set.remove(&residue_index);
            if set.is_empty() {
                self.selection.remove(&entity);
            }
        }
        self.ui_dirty |= DirtyFlags::SELECTION;
        self.flush_selection_to_viso();
    }

    /// Bulk-replace the selection on a single entity. The provided
    /// residues become the entity's full set (not merged into the
    /// existing one). An empty input removes the entity entry.
    pub(crate) fn set_residues_on(
        &mut self,
        entity: EntityId,
        residues: impl IntoIterator<Item = u32>,
    ) {
        let set: BTreeSet<u32> = residues.into_iter().collect();
        if set.is_empty() {
            self.selection.remove(&entity);
        } else {
            self.selection.insert(entity, set);
        }
        self.ui_dirty |= DirtyFlags::SELECTION;
        self.flush_selection_to_viso();
    }

    /// Drop the entire selection across all entities.
    pub(crate) fn clear_selection(&mut self) {
        self.selection.clear();
        self.ui_dirty |= DirtyFlags::SELECTION;
        self.flush_selection_to_viso();
    }

    /// Flip the selected state of `(entity, residue_index)` and return
    /// the new state (`true` if now selected, `false` if now
    /// deselected). Maintains the empty-set-removal invariant.
    pub(crate) fn toggle_residue(&mut self, entity: EntityId, residue_index: u32) -> bool {
        let set = self.selection.entry(entity).or_default();
        let now_selected = set.insert(residue_index);
        if !now_selected {
            set.remove(&residue_index);
            if set.is_empty() {
                self.selection.remove(&entity);
            }
        }
        self.ui_dirty |= DirtyFlags::SELECTION;
        self.flush_selection_to_viso();
        now_selected
    }

    /// Selected residues on a specific entity, or `None` if the entity
    /// has no selection. Sets are never empty by invariant, so
    /// `Some(_)` always carries at least one residue.
    pub(crate) fn selected_residues_on(&self, entity: EntityId) -> Option<&BTreeSet<u32>> {
        self.selection.get(&entity)
    }

    /// Point-query: is `(entity, residue_index)` selected?
    pub(crate) fn is_residue_selected(&self, entity: EntityId, residue_index: u32) -> bool {
        self.selection
            .get(&entity)
            .is_some_and(|set| set.contains(&residue_index))
    }

    /// Iterator over the entities that currently have at least one
    /// residue selected. Order is `BTreeMap`'s natural key order.
    pub(crate) fn selected_entities(&self) -> impl Iterator<Item = EntityId> + '_ {
        self.selection.keys().copied()
    }

    /// True when no residue is selected on any entity.
    pub(crate) fn selection_is_empty(&self) -> bool {
        self.selection.is_empty()
    }

    /// Total number of selected residues across all entities (sum of
    /// per-entity set sizes).
    pub(crate) fn selection_total_count(&self) -> usize {
        self.selection.values().map(|set| set.len()).sum()
    }

    /// Apply a panel-originated selection mutation: wholesale replace
    /// the current selection with `entries`. The wire-side `entity_id`
    /// is a raw `u32`; look it up against the store's existing ids
    /// instead of minting a new one through the allocator (which would
    /// silently advance and break the next genuine allocation).
    /// Entries that don't match any live entity are dropped — panels
    /// can race a structure swap, and a stale id should clear silently
    /// rather than fail loudly. An empty `entries` list clears the
    /// selection entirely.
    ///
    /// Both `clear_selection` and `set_residues_on` self-set
    /// [`foldit_gui::DirtyFlags::SELECTION`] and call
    /// `flush_selection_to_viso`, so the GPU residue selection and the
    /// frontend mirror stay in lockstep without an extra dirty-flag
    /// flush here. Per-entity residue lists are collected into
    /// `BTreeSet`, so duplicate or out-of-order indices in the wire
    /// payload are silently normalized.
    pub fn handle_set_selection(&mut self, entries: Vec<foldit_gui::EntitySelection>) {
        self.clear_selection();
        for entry in entries {
            let Some(entity) = self
                .store
                .ids()
                .find(|id| id.raw() == entry.entity_id)
            else {
                log::trace!(
                    "handle_set_selection: unknown entity_id {} (dropping)",
                    entry.entity_id
                );
                continue;
            };
            self.set_residues_on(entity, entry.residues);
        }
    }
}

// ---------------------------------------------------------------------------
// Bridge: Dispatcher trait impl
// ---------------------------------------------------------------------------

impl foldit_gui::Dispatcher for App {
    /// Webview signaled it's ready — mark every section of the owned
    /// `FrontendState` dirty so the next `serialize_frontend_dirty`
    /// emits a full snapshot. App owns the frontend mirror (RX13), so
    /// this lives here rather than on the host.
    fn on_ready(&mut self) {
        self.frontend.mark_all_dirty();
    }

    fn on_viewport_input(&mut self, input: foldit_gui::ViewportInput) {
        self.handle_viewport_input(input);
    }

    fn on_trigger_action(&mut self, action: foldit_gui::ActionId) {
        self.handle_trigger_action(action);
    }

    fn on_dispatch_op(&mut self, op: foldit_gui::OpDispatch) {
        self.handle_dispatch_op(op);
    }

    fn on_parameterized_action(&mut self, action: foldit_gui::ParameterizedAction) {
        self.handle_parameterized_action(action);
    }

    fn on_set_selection(&mut self, entries: Vec<foldit_gui::EntitySelection>) {
        self.handle_set_selection(entries);
    }

    fn handle_request(
        &mut self,
        kind: foldit_gui::RequestKind,
        payload: serde_json::Value,
    ) -> foldit_gui::RequestResult {
        use foldit_gui::RequestKind;
        match kind {
            RequestKind::ReadResourceFile => {
                let filepath = payload
                    .get("filepath")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "missing 'filepath'".to_string())?;
                let bytes = self.host.read_file(filepath)
                    .map_err(|e| format!("read {}: {}", filepath, e))?;
                use base64::Engine;
                let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                Ok(serde_json::json!({ "encoding": "base64", "content": b64 }))
            }
            RequestKind::GetHotkeyText => {
                // Stub: real implementation would look up display strings
                // for hotkey ids. Until that surface lands, return empty so
                // HelpMenuPanel rejects gracefully instead of timing out.
                let hotkey = payload
                    .get("hotkey")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                Err(format!("hotkey lookup not implemented (hotkey={})", hotkey))
            }
            RequestKind::ServerRequest => {
                // Stub: server requests (news, etc.) require an HTTP client
                // bound here. Defer until a dedicated request handler exists.
                let endpoint = payload
                    .get("endpoint")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                Err(format!("server request not implemented (endpoint={})", endpoint))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Visualization helpers (free functions for split-borrow friendliness)
// ---------------------------------------------------------------------------

/// Update drag/pull/band visualizations. Bands are still inert (the
/// band state machine is the next item to come back online). The pull
/// capsule + cone arrow renders whenever the caller hands a
/// `Some(PullInfo)` from a live drag; clears otherwise so a finished
/// or cancelled drag leaves no overlay.
fn update_all_visualizations(
    engine: &mut VisoEngine,
    pull: Option<viso::PullInfo>,
) {
    engine.update_bands(vec![]);
    engine.update_pull(pull);
}

/// Get the trajectory path from command-line arguments. CLI/host
/// utility — read once on a hotkey + reused by `LoadTrajectory`.
fn trajectory_path_from_args() -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.windows(2).find_map(|w| {
        if w[0] == "--trajectory" {
            Some(w[1].clone())
        } else {
            None
        }
    })
}

/// Locate the runtime plugins directory.
///
/// Resolution order:
///   1. `FOLDIT_PLUGINS_ROOT` environment override (production /
///      bundled deployments point this at the bundle's plugins dir).
///   2. `<exe_dir>/plugins/` if it exists (bundle layout).
///   3. Walk up from `current_exe()` looking for
///      `crates/foldit-runner/plugins/` (dev workflow under cargo).
///
/// Returns `None` if none of these resolve. The caller logs and skips
/// plugin discovery in that case -- the desktop app degrades to viewer-
/// only mode rather than failing the load.
#[cfg(not(target_arch = "wasm32"))]
pub fn locate_plugins_root() -> Option<std::path::PathBuf> {
    if let Some(env) = std::env::var_os("FOLDIT_PLUGINS_ROOT") {
        let p = std::path::PathBuf::from(env);
        if p.is_dir() {
            return Some(p);
        }
    }
    let exe = std::env::current_exe().ok()?;
    if let Some(dir) = exe.parent() {
        let bundle = dir.join("plugins");
        if bundle.is_dir() {
            return Some(bundle);
        }
    }
    let mut cursor = exe.parent()?.to_path_buf();
    loop {
        let candidate =
            cursor.join("crates/foldit-runner/plugins");
        if candidate.is_dir() {
            return Some(candidate);
        }
        if !cursor.pop() {
            break;
        }
    }
    None
}


// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[cfg(test)]
mod selection_tests {
    use super::*;
    use molex::entity::molecule::id::EntityIdAllocator;
    use std::io;
    use std::path::Path;

    /// Minimal [`HostResources`] stub. `App` construction needs one;
    /// these tests never touch the filesystem.
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

    fn fresh_app() -> App {
        App::new(Box::new(TestHost))
    }

    /// Mint a sequence of distinct entity ids in a test-local order.
    /// `EntityId` is opaque, so we allocate via `EntityIdAllocator` and
    /// hand back the n-th id from a freshly-seeded allocator. The map
    /// keys we care about are just "different ids on the same App",
    /// not specific raw values.
    fn mint_ids(n: usize) -> Vec<EntityId> {
        let mut alloc = EntityIdAllocator::new();
        (0..n).map(|_| alloc.allocate()).collect()
    }

    #[test]
    fn new_app_has_empty_selection() {
        let app = fresh_app();
        let ids = mint_ids(1);
        assert!(app.selection_is_empty());
        assert_eq!(app.selection_total_count(), 0);
        assert!(app.selected_residues_on(ids[0]).is_none());
    }

    #[test]
    fn select_residue_is_idempotent() {
        let mut app = fresh_app();
        let ids = mint_ids(1);
        let e = ids[0];
        app.select_residue(e, 7);
        app.select_residue(e, 7);
        app.select_residue(e, 7);
        assert_eq!(app.selection_total_count(), 1);
        assert!(app.is_residue_selected(e, 7));
        let set = app.selected_residues_on(e).expect("present");
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn clear_selection_empties_the_map() {
        let mut app = fresh_app();
        let ids = mint_ids(2);
        app.select_residue(ids[0], 0);
        app.select_residue(ids[1], 5);
        assert_eq!(app.selection_total_count(), 2);
        app.clear_selection();
        assert!(app.selection_is_empty());
        assert!(app.selected_residues_on(ids[0]).is_none());
        assert!(app.selected_residues_on(ids[1]).is_none());
        assert!(!app.is_residue_selected(ids[0], 0));
    }

    #[test]
    fn set_residues_on_replaces_not_merges() {
        let mut app = fresh_app();
        let ids = mint_ids(1);
        let e = ids[0];
        app.select_residue(e, 1);
        app.select_residue(e, 2);
        app.select_residue(e, 3);
        app.set_residues_on(e, [10, 11]);
        let set = app.selected_residues_on(e).expect("present");
        assert_eq!(set.len(), 2);
        assert!(set.contains(&10));
        assert!(set.contains(&11));
        assert!(!set.contains(&1));
        assert!(!set.contains(&2));
        assert!(!set.contains(&3));
    }

    #[test]
    fn set_residues_on_empty_removes_entity_entry() {
        let mut app = fresh_app();
        let ids = mint_ids(1);
        let e = ids[0];
        app.select_residue(e, 9);
        app.set_residues_on(e, std::iter::empty());
        assert!(app.selected_residues_on(e).is_none());
        assert!(app.selection_is_empty());
    }

    #[test]
    fn multi_entity_isolation() {
        let mut app = fresh_app();
        let ids = mint_ids(2);
        let a = ids[0];
        let b = ids[1];
        app.select_residue(a, 1);
        app.select_residue(a, 2);
        app.select_residue(b, 100);

        assert!(app.is_residue_selected(a, 1));
        assert!(app.is_residue_selected(a, 2));
        assert!(!app.is_residue_selected(a, 100));
        assert!(app.is_residue_selected(b, 100));
        assert!(!app.is_residue_selected(b, 1));

        app.clear_selection();
        app.select_residue(a, 1);
        app.set_residues_on(b, [42, 43]);
        // Mutating B must not have touched A.
        assert_eq!(
            app.selected_residues_on(a).expect("present").len(),
            1
        );
    }

    #[test]
    fn deselect_last_residue_removes_entity_entry() {
        let mut app = fresh_app();
        let ids = mint_ids(1);
        let e = ids[0];
        app.select_residue(e, 0);
        app.select_residue(e, 1);
        app.deselect_residue(e, 0);
        // Set is still non-empty: entry must remain.
        assert!(app.selected_residues_on(e).is_some());
        app.deselect_residue(e, 1);
        // Last residue gone: entity entry must be removed.
        assert!(app.selected_residues_on(e).is_none());
        assert!(app.selection_is_empty());
    }

    #[test]
    fn deselect_idempotent_on_missing() {
        let mut app = fresh_app();
        let ids = mint_ids(1);
        let e = ids[0];
        // Deselect a residue that was never selected: no panic, no
        // phantom entity entry left behind.
        app.deselect_residue(e, 99);
        assert!(app.selection_is_empty());
        app.select_residue(e, 1);
        app.deselect_residue(e, 99);
        assert!(app.is_residue_selected(e, 1));
        assert_eq!(app.selection_total_count(), 1);
    }

    #[test]
    fn toggle_residue_round_trips() {
        let mut app = fresh_app();
        let ids = mint_ids(1);
        let e = ids[0];
        // First toggle selects.
        assert!(app.toggle_residue(e, 3));
        assert!(app.is_residue_selected(e, 3));
        // Second toggle deselects and removes the empty entity entry.
        assert!(!app.toggle_residue(e, 3));
        assert!(!app.is_residue_selected(e, 3));
        assert!(app.selected_residues_on(e).is_none());
        // Toggle on a sibling residue while none are selected: same
        // entity, but the entry was removed in step 2, so this is a
        // fresh insert.
        assert!(app.toggle_residue(e, 4));
        assert!(app.is_residue_selected(e, 4));
        assert!(!app.is_residue_selected(e, 3));
    }

    #[test]
    fn selected_entities_enumerates_only_nonempty() {
        let mut app = fresh_app();
        let ids = mint_ids(3);
        app.select_residue(ids[0], 0);
        app.select_residue(ids[1], 0);
        app.select_residue(ids[2], 0);
        app.deselect_residue(ids[1], 0);
        let ents: Vec<_> = app.selected_entities().collect();
        // BTreeMap key order is by `EntityId`'s `Ord`, which for the
        // molex newtype is the underlying u32 order. The allocator
        // hands out ids in sequence so ids[0] < ids[1] < ids[2]; after
        // removing ids[1], `selected_entities` enumerates ids[0],
        // ids[2] in that order.
        assert_eq!(ents, vec![ids[0], ids[2]]);
    }

    #[test]
    fn handle_set_selection_clears_on_empty_input() {
        let mut app = fresh_app();
        let ids = mint_ids(1);
        app.select_residue(ids[0], 7);
        assert!(!app.selection_is_empty());
        // Empty entries: clear (`clear_selection` always runs first; no
        // entry loop body) — independent of whether the empty store
        // could even resolve a raw id.
        app.handle_set_selection(Vec::new());
        assert!(app.selection_is_empty());
    }

    #[test]
    fn handle_set_selection_drops_unknown_entity_ids() {
        let mut app = fresh_app();
        let ids = mint_ids(1);
        // Seed a non-empty selection so we can prove the clear ran.
        app.select_residue(ids[0], 9);
        // The test stub has no loaded structure, so `self.store.ids()`
        // is empty and every raw id is unresolvable. The mutator clears
        // the existing selection and drops the unknown entries.
        app.handle_set_selection(vec![
            foldit_gui::EntitySelection {
                entity_id: 0,
                residues: vec![1, 2, 3],
            },
            foldit_gui::EntitySelection {
                entity_id: 999,
                residues: vec![5],
            },
        ]);
        assert!(app.selection_is_empty());
    }
}
