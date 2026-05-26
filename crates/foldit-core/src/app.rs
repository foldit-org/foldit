//! Foldit application state — host-agnostic.
//!
//! `App` owns the `Orchestrator` (in `ActionRouter`), `Document`,
//! `History`, and the cross-cutting bookkeeping (puzzle, scoring mode,
//! viso engine handle, history-version trackers). Both the desktop
//! (`foldit-desktop`) and web (`foldit-web`) builds wrap this in their
//! host-specific lifecycle:
//!
//! - desktop: `window::AppRunner` holds the wry webview + winit window
//!   alongside `App`; winit events are converted to host-agnostic
//!   types before being forwarded to `App`'s methods.
//! - web: `foldit_web::FolditApp` holds `App` plus the canvas and JS
//!   callbacks; DOM events are forwarded as `ViewportInput` JSON.

use web_time::{Instant, UNIX_EPOCH};

use foldit_gui::{
    CheckpointInfo, CheckpointKindTag, DirtyFlags, FilterStatus, HistoryCommand,
    HistoryLiveUpdate, HistorySection, ScoringMode, TextBubbleButton, TextBubblePayload,
    WireId,
};
use foldit_runner::Orchestrator;
use molex::entity::molecule::id::EntityId;
use viso::{
    Focus, InputEvent, InputProcessor, VisoCommand, VisoEngine,
};

use crate::action_router::{self, ActionRouter};
use crate::document::{Document, DocumentError, EntityOrigin};
use crate::gui_projector::GuiProjector;
use crate::history::{CheckpointKind, FilterStatus as HistoryFilterStatus, History};
use crate::plugin_driver::PluginDriver;
#[cfg(not(target_arch = "wasm32"))]
use crate::plugin_driver::{ActiveStreamEntry, OpOutcome};
use crate::render_projector::{self, RenderProjector};

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
fn project_history(store: &Document) -> HistorySection {
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
fn head_score(store: &Document, mode: ScoringMode) -> Option<f64> {
    let head_id = store.history().checkpoints().head();
    let ckpt = store.history().checkpoint(head_id)?;
    score_for_mode(ckpt.raw_score, ckpt.game_score, mode)
}

/// Move one freshly-loaded entity through the preview→promote pipeline
/// so it lands in history with an `AddEntity` checkpoint. Returns the
/// committed [`EntityId`].
///
/// Ambient (water / ion / solvent) and zero-residue entities — the
/// hetatm stubs that the parser emits for cofactors / waters in many
/// PDB files — are kept as previews (transient) so viso still renders
/// them, but they DO NOT push a history checkpoint. They aren't
/// undoable from the user's perspective; pushing one `AddEntity` per
/// stub clutters the history (`1bfe` produced 3 root-level dots: one
/// `Loaded` + two `AddEntity` for chain A and a water).
fn load_entity_into_history(
    store: &mut Document,
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
fn first_protein_entity(store: &Document) -> Option<EntityId> {
    store.proteins().next().map(|(eid, _, _)| eid)
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
    store: &mut Document,
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

/// Main application state — thin glue connecting the render engine and action router.
pub struct App {
    engine: Option<VisoEngine>,
    input: InputProcessor,
    store: Document,
    router: ActionRouter,
    plugin_driver: PluginDriver,
    render_projector: RenderProjector,
    gui_projector: GuiProjector,
    pdb_path: String,
    puzzle: PuzzleSession,
}

/// Objective metadata for the active puzzle. Zero/empty in Scientist
/// mode; populated from the puzzle TOML on a Game load. RX9 moved the
/// GUI-projection fields (scoring mode, tutorial bubbles, bubble
/// cursor) onto [`GuiProjector`]; RX14 will pull these objective
/// fields out as a separate type.
struct PuzzleSession {
    id: u32,
    title: String,
    starting_score: f64,
    target_score: f64,
}

impl App {
    /// Get a display title derived from the PDB path (e.g. "1BFE" from ".../1bfe.cif")
    pub fn structure_title(&self) -> String {
        std::path::Path::new(&self.pdb_path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("Unknown")
            .to_uppercase()
    }

    pub fn new(pdb_path: String) -> Self {
        Self {
            engine: None,
            input: InputProcessor::new(),
            store: Document::new(),
            router: ActionRouter::new(),
            plugin_driver: PluginDriver::new(),
            render_projector: RenderProjector::new(),
            gui_projector: GuiProjector::new(),
            pdb_path,
            // CLI bootstrap zeroes the objective; `LoadPuzzle` fills it
            // in from the puzzle TOML.
            puzzle: PuzzleSession {
                id: 0,
                title: String::new(),
                starting_score: 0.0,
                target_score: 0.0,
            },
        }
    }

    /// True once the Rosetta backend has delivered its first score update
    /// for the current session. Replaces the old `latest_score`
    /// shadow-field check; the truth source is now the head checkpoint.
    pub fn has_initial_score(&self) -> bool {
        head_score(&self.store, self.gui_projector.scoring_mode).is_some()
    }

    // ── Engine-only delegation (no router interaction) ──

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
            // engine right before `render_projector.publish` so the new
            // assembly animates in instead of snapping. Single-action invariant
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
                            if let Some(orch) =
                                self.plugin_driver.orchestrator.as_mut()
                            {
                                refresh_scores(
                                    orch,
                                    &mut self.store,
                                    self.engine.as_mut(),
                                );
                                self.router.ui_dirty |= DirtyFlags::SCORE;
                            }
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
                    // entity *before* set_assembly. The next
                    // engine.update tick picks both up together and
                    // animates from current GPU positions to the new
                    // pose; without the transition, sync_scene_to_renderers
                    // snaps. The entity was captured at update time
                    // (single-action invariant — at most one in flight
                    // at a time).
                    if let Some((entity, kind)) = pending_transition {
                        engine.queue_entity_transition(
                            entity.raw(),
                            resolve_transition(kind),
                        );
                    }
                    self.render_projector.publish(&self.store, engine);
                }
            }
            if had_terminal {
                self.router.ui_dirty |= DirtyFlags::SCORE
                    | DirtyFlags::ACTIONS
                    | DirtyFlags::SCENE
                    | DirtyFlags::HISTORY;
            } else if visual_dirty {
                // Mid-stream visual updates without a terminal event:
                // the scene needs a re-publish but not a full UI sync.
                self.router.ui_dirty |= DirtyFlags::SCENE;
            }
        }
    }

    /// Drain `Document::take_scene_changes` and route the batch to the
    /// `PluginBroadcaster` (Full/Delta plugin fan-out). Call at the end
    /// of every action / keybind / head-move handler; the store emits a
    /// `SceneChange` per observable mutation but holds no projection
    /// logic, and this is the bridge.
    ///
    /// Score refresh is intentionally NOT part of this method (RX10
    /// decision B). Callers run [`Self::poll_plugin_scores`] separately
    /// after the pump to preserve the post-broadcast refresh cadence
    /// the old combined helper provided.
    fn pump_scene_changes(&mut self) {
        let changes = self.store.take_scene_changes();
        if changes.is_empty() {
            return;
        }
        let Some(orch) = self.plugin_driver.orchestrator.as_mut() else {
            // Without an orchestrator we have no plugins to notify;
            // drop the drained changes.
            return;
        };
        // `broadcaster` and `orchestrator` are disjoint fields of
        // `self.plugin_driver`; `store` is a separate field of `self`.
        self.plugin_driver
            .broadcaster
            .broadcast(&changes, &self.store, orch);
    }

    /// Query every plugin's `score` op, merge totals into the head
    /// checkpoint (bumping `live_version` for the GuiProjector to pick
    /// up), and push per-residue scores directly to viso for
    /// color-by-score display modes. Off the `SceneChange` spine
    /// entirely: scores have two consumers (the GuiProjector via
    /// `HistorySyncCursor` and viso via a direct overlay push) and
    /// neither needs to ride the spine (RX10 decision B).
    ///
    /// Called after every `pump_scene_changes` at action / keybind /
    /// head-move boundaries to preserve the cadence the old combined
    /// helper provided. Cadence unweld (refresh on per-tick instead of
    /// per-broadcast) is RX13.
    fn poll_plugin_scores(&mut self) {
        let Some(orch) = self.plugin_driver.orchestrator.as_mut() else {
            return;
        };
        refresh_scores(orch, &mut self.store, self.engine.as_mut());
        self.router.ui_dirty |= DirtyFlags::SCORE | DirtyFlags::HISTORY;
    }

    // ── Keybinding dispatch (engine + router) ──

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
            selection: Vec::new(),
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
        let Some(cmd) = self.input.handle_key_press(key_str) else {
            return self.try_hotkey_dispatch(key_str);
        };
        let Some(engine) = &mut self.engine else { return false };

        // Actions that need foldit-specific pre/post processing
        match cmd {
            VisoCommand::ToggleTrajectory => {
                if engine.has_trajectory() {
                    engine.execute(VisoCommand::ToggleTrajectory);
                } else if let Some(path) = action_router::trajectory_path_from_args() {
                    engine.load_trajectory(std::path::Path::new(&path));
                } else {
                    log::info!("No trajectory loaded. Pass --trajectory <path.dcd> to load one.");
                }
            }
            VisoCommand::ClearSelection => {
                #[cfg(not(target_arch = "wasm32"))]
                self.plugin_driver.cancel_all_active_streams();
                self.cancel_operations();
            }
            VisoCommand::CycleFocus | VisoCommand::ResetFocus => {
                engine.execute(cmd);
                log::info!(
                    "Focus: {}",
                    render_projector::focus_description(&self.store, &engine.focus())
                );
                // Focus-driven lock update lands when the bridge
                // plugin's `update_assembly` + selection-derived locks
                // are wired.
                self.router.ui_dirty |= DirtyFlags::SELECTION | DirtyFlags::UI;
            }
            // All other commands: delegate entirely to viso
            other => { engine.execute(other); }
        }
        self.pump_scene_changes();
        self.poll_plugin_scores();
        true
    }

    /// Cancel the in-flight operation: clear the viso selection, drop any
    /// in-progress preview entities, republish, and flag the GUI dirty.
    /// Stream lock release + commit live in `apply_backend_updates`'
    /// terminal arms; doing them here races a follow-up dispatch that's
    /// quick enough to slip in before the terminal drains. Was
    /// `ActionRouter::cancel_operations`; hoisted to App so the
    /// `RenderProjector` stays a field touched only inside App methods
    /// (the coordination boundary), never threaded as a parameter.
    fn cancel_operations(&mut self) {
        let Some(engine) = self.engine.as_mut() else {
            return;
        };
        log::info!("Cancelling current operation");
        engine.execute(VisoCommand::ClearSelection);
        let preview_ids: Vec<EntityId> = self.store.preview_ids().collect();
        if !preview_ids.is_empty() {
            for id in &preview_ids {
                self.store.remove_preview(*id);
            }
            self.render_projector.publish(&self.store, engine);
            log::info!(
                "Removed {} in-progress preview entities",
                preview_ids.len()
            );
        }
        self.router.ui_dirty |=
            DirtyFlags::ACTIONS | DirtyFlags::SELECTION | DirtyFlags::LOADING;
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
                    if self.input.mouse_pressed()
                        && self.plugin_driver.stream_host.pull_drag.is_none() =>
                {
                    if self.try_begin_pull_drag(*x, *y) {
                        // viso recorded the press; drop its mouse
                        // state so the now-suppressed pointer-up
                        // can't fire a stray click → selection.
                        self.input.release_mouse_state();
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
        // borrow (`self.router`, not `self.engine`); the actual
        // dispatch is deferred to after the match so the `engine`
        // borrow is released before `handle_dispatch_op` takes
        // `&mut self`.
        #[cfg(not(target_arch = "wasm32"))]
        let mut pending_hotkey_op: Option<String> = None;
        // ClearSelection/ESC cancel needs `&mut self`, but `engine` is
        // borrowed for the rest of the match and used again by
        // `update_all_visualizations` after it. Defer the cancel past that
        // last engine use, mirroring the `pending_hotkey_op` deferral.
        let mut pending_cancel = false;

        let Some(engine) = &mut self.engine else { return };

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
                if let Some(cmd) = self.input.handle_event(InputEvent::ModifiersChanged { shift }, engine.hovered_target()) {
                    engine.execute(cmd);
                }
                engine.set_cursor_pos(x, y);
                if let Some(cmd) = self.input.handle_event(InputEvent::CursorMoved { x, y }, engine.hovered_target()) {
                    engine.execute(cmd);
                }
                self.router.handle_native_cursor_moved(engine, &self.input, &mut self.store, x, y);
                self.router.handle_native_mouse_input(engine, &mut self.input, &mut self.store, viso_button, true);
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
                if let Some(cmd) = self.input.handle_event(InputEvent::ModifiersChanged { shift }, engine.hovered_target()) {
                    engine.execute(cmd);
                }
                engine.set_cursor_pos(x, y);
                if let Some(cmd) = self.input.handle_event(InputEvent::CursorMoved { x, y }, engine.hovered_target()) {
                    engine.execute(cmd);
                }
                self.router.handle_native_cursor_moved(engine, &self.input, &mut self.store, x, y);
                self.router.handle_native_mouse_input(engine, &mut self.input, &mut self.store, viso_button, false);
            }
            ViewportInput::PointerMove { x, y, shift, .. } => {
                if let Some(cmd) = self.input.handle_event(InputEvent::ModifiersChanged { shift }, engine.hovered_target()) {
                    engine.execute(cmd);
                }
                engine.set_cursor_pos(x, y);
                if let Some(cmd) = self.input.handle_event(InputEvent::CursorMoved { x, y }, engine.hovered_target()) {
                    engine.execute(cmd);
                }
                self.router.handle_native_cursor_moved(engine, &self.input, &mut self.store, x, y);
            }
            ViewportInput::Scroll { delta } => {
                if let Some(cmd) = self.input.handle_event(InputEvent::Scroll { delta }, engine.hovered_target()) {
                    engine.execute(cmd);
                }
            }
            ViewportInput::Key { code, pressed } => {
                if pressed {
                    if let Some(cmd) = self.input.handle_key_press(&code) {
                        match cmd {
                            // Drop viso's R-binding for turntable auto-rotate;
                            // we don't expose a rotate keybinding in foldit.
                            VisoCommand::ToggleAutoRotate => {}
                            VisoCommand::ToggleTrajectory => {
                                if engine.has_trajectory() {
                                    engine.execute(VisoCommand::ToggleTrajectory);
                                } else if let Some(path) = action_router::trajectory_path_from_args() {
                                    engine.load_trajectory(std::path::Path::new(&path));
                                }
                            }
                            VisoCommand::ClearSelection => {
                                #[cfg(not(target_arch = "wasm32"))]
                                self.plugin_driver
                                    .cancel_all_active_streams();
                                pending_cancel = true;
                            }
                            VisoCommand::CycleFocus | VisoCommand::ResetFocus => {
                                engine.execute(cmd);
                                // (lock update deferred — see comment above)
                                self.router.ui_dirty |= DirtyFlags::SELECTION | DirtyFlags::UI;
                            }
                            other => { engine.execute(other); }
                        }
                    } else {
                        // No viso built-in claims this key — resolve it
                        // against the plugin hotkey catalog. Disjoint
                        // field borrow (`self.router`) so it coexists
                        // with the live `engine` borrow; dispatch is
                        // deferred to after the match.
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
                                                == Some(code.as_str())
                                        })
                                        .map(|e| e.op_id)
                                });
                            if pending_hotkey_op.is_none() {
                                log::debug!(
                                    "Unhandled key code from frontend: {code}"
                                );
                            }
                        }
                        #[cfg(target_arch = "wasm32")]
                        log::debug!("Unhandled key code from frontend: {code}");
                    }
                }
            }
            ViewportInput::Resize { .. } => {
                // Ignored: JS sends CSS pixels (logical) which are wrong on HiDPI.
            }
        }

        self.router.ui_dirty |= DirtyFlags::UI;

        // Update drag/pull/band visualizations after input
        #[cfg(not(target_arch = "wasm32"))]
        let pull = self.plugin_driver.stream_host.pull_drag.as_ref().map(|d| d.pull_info.clone());
        #[cfg(target_arch = "wasm32")]
        let pull: Option<viso::PullInfo> = None;
        update_all_visualizations(engine, &self.router, pull);

        // `engine`'s last use was above — `&mut self` is free again, so
        // the deferred actions below can run. Ordering of the cancel vs.
        // `update_all_visualizations` is immaterial: the latter only sets
        // band/pull overlays, disjoint from cancel's selection-clear +
        // preview-removal + republish.
        if pending_cancel {
            self.cancel_operations();
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
                selection: Vec::new(),
                params: std::collections::HashMap::new(),
            });
        }

        self.pump_scene_changes();
        self.poll_plugin_scores();
    }

    /// Called after the pull-drag interception path. Mirrors the
    /// trailing cleanup the regular `handle_viewport_input` flow does
    /// (visualizations + broadcast pump) without double-running the
    /// match-arm body. Pre-snapshots the pull info so the engine
    /// borrow doesn't overlap with `&self.plugin_driver.stream_host.pull_drag`.
    #[cfg(not(target_arch = "wasm32"))]
    fn finalize_viewport_input(&mut self) {
        self.router.ui_dirty |= DirtyFlags::UI;
        let pull = self.plugin_driver.stream_host.pull_drag.as_ref().map(|d| d.pull_info.clone());
        if let Some(engine) = self.engine.as_mut() {
            update_all_visualizations(engine, &self.router, pull);
        }
        self.pump_scene_changes();
        self.poll_plugin_scores();
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
            SessionContext as RunnerSessionContext, TransitionKind,
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

        let ctx = RunnerSessionContext {
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
            |_| None,
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
        // Document history directly, with `&mut self` to reach
        // both store + engine.
        match action {
            foldit_gui::ActionId::Undo => self.handle_undo(),
            foldit_gui::ActionId::Redo => self.handle_redo(),
        }
        self.pump_scene_changes();
        self.poll_plugin_scores();
    }

    /// Dispatch a plugin op by op-id. Resolves the op against the
    /// orchestrator's `PluginRegistry` to pick Invoke vs Start_stream;
    /// builds a `SessionContext` from the GUI-provided focus +
    /// selection. Op-ids unknown to the registry are logged and
    /// dropped (the catalog couldn't have surfaced them, so this is
    /// either a stale GUI cache or a misrouted message).
    pub fn handle_dispatch_op(&mut self, op: foldit_gui::OpDispatch) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            // Drain pending terminals so rapid follow-up dispatches
            // see released locks.
            self.apply_backend_updates();

            use foldit_runner::orchestrator::{
                EntityId as RunnerEntityId, ResidueRef,
                SessionContext as RunnerSessionContext,
            };

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

            let ctx = RunnerSessionContext {
                focused_entity_id: op.focused_entity_id.map(RunnerEntityId),
                selection: op
                    .selection
                    .iter()
                    .map(|r| ResidueRef {
                        entity_id: RunnerEntityId(r.entity_id),
                        residue_index: r.residue_index,
                    })
                    .collect(),
            };
            let params: std::collections::HashMap<
                String,
                foldit_runner::orchestrator::ParamValue,
            > = op
                .params
                .into_iter()
                .map(|(k, v)| (k, action_router::param_value_from_wire(v)))
                .collect();

            // Drop the orchestrator borrow before reaching back into
            // `self.plugin_driver` for the consolidated dispatch.
            let _ = orch;
            let dispatch_outcome = self.plugin_driver.dispatch_op(
                &op.op_id,
                cached.kind,
                ctx,
                params,
                plugin_id.clone(),
                transition,
                |_| None,
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
            self.router.ui_dirty |=
                DirtyFlags::ACTIONS | DirtyFlags::SCORE | DirtyFlags::UI;
            self.pump_scene_changes();
            self.poll_plugin_scores();
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = op;
        }
    }

    /// Apply the assembly bytes returned by a one-shot `dispatch_invoke`
    /// to the ongoing tentative and commit it. Mirrors the Stream-side
    /// `Final` path; called from `handle_dispatch_op` for `OpKind::Invoke`.
    /// `transition` is the manifest-declared animation preset, queued on
    /// the locked entity right before `render_projector.publish` so the
    /// result eases in rather than snapping.
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
                    engine.queue_entity_transition(
                        eid.raw(),
                        resolve_transition(transition),
                    );
                }
                self.render_projector.publish(&self.store, engine);
            }
            self.router.ui_dirty |=
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
        self.pump_scene_changes();
        self.poll_plugin_scores();
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

        // Bubble cursor advance is engine-independent — handle before
        // the engine borrow so it works whether or not a puzzle's wgpu
        // surface is live.
        if let ParameterizedAction::AdvanceBubble { back } = action {
            self.advance_bubble(back);
            return;
        }

        let title = self.structure_title();
        let Some(engine) = &mut self.engine else { return };

        match action {
            ParameterizedAction::LoadStructure { path } => {
                match crate::puzzle::load_file_as_entities(&path) {
                    Ok((entities, name)) => {
                        log::info!("Loaded structure via IPC: {}", name);
                        for entity in entities {
                            let _ = load_entity_into_history(&mut self.store, entity, name.clone());
                        }
                        self.render_projector.publish(&self.store, engine);
                        engine.fit_camera_to_focus();
                        // Free-form file load → scientist mode.
                        self.gui_projector.scoring_mode = ScoringMode::Scientist;
                        self.puzzle.id = 0;
                        self.puzzle.title = name;
                        self.puzzle.starting_score = 0.0;
                        self.puzzle.target_score = 0.0;
                        self.gui_projector.bubbles.clear();
                        self.gui_projector.current_bubble = 0;
                        self.router.ui_dirty |=
                            DirtyFlags::LOADING | DirtyFlags::ACTIONS | DirtyFlags::SCORE | DirtyFlags::PUZZLE;
                    }
                    Err(e) => {
                        log::error!("Failed to load structure '{}': {}", path, e);
                    }
                }
                self.router.ui_dirty |= DirtyFlags::LOADING | DirtyFlags::SCORE | DirtyFlags::SELECTION;
            }
            ParameterizedAction::LoadPuzzle { puzzle_id } => {
                self.store.reset();
                self.plugin_driver.reset_for_new_structure();

                match crate::puzzle::load_puzzle_structure(puzzle_id) {
                    Ok(puzzle_data) => {
                        // Capture mode + puzzle metadata for the GUI.
                        self.gui_projector.scoring_mode = ScoringMode::Game;
                        self.puzzle.id = puzzle_id;
                        self.puzzle.title = puzzle_data.name.clone();
                        self.puzzle.starting_score = puzzle_data.start_energy;
                        self.puzzle.target_score = puzzle_data.completion_score;
                        self.gui_projector.bubbles = puzzle_data.bubbles;
                        self.gui_projector.current_bubble = 0;

                        #[cfg(not(target_arch = "wasm32"))]
                        if let Some(preset_name) = &puzzle_data.view_preset {
                            let presets_dir = std::path::Path::new("assets/view_presets");
                            engine.load_preset(preset_name, presets_dir);
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

                        // Atomic topology swap: tears down stale scene-local state and
                        // force-syncs so subsequent calls see the new entities.
                        self.render_projector.replace(&self.store, engine);

                        // Snap so bounding_radius reflects molecule extent (fog driver),
                        // then override the pose with the puzzle's saved eye/up but
                        // anchor the orbit center on the protein centroid.
                        engine.snap_camera_to_focus();
                        if let Some(centroid) = engine.focus_centroid() {
                            engine.set_camera_pose(centroid, cam_eye, cam_up);
                        }

                        if let Some(ss) = ss_override {
                            if let Some(&first_id) = ids.first() {
                                engine.set_ss_override(first_id.raw(), ss);
                            }
                        }

                        // Rosetta session init via bridge plugin's
                        // `init` + auto-`update_assembly` fan-out happens
                        // when the orchestrator's ensure_plugin_registered
                        // path is invoked for "rosetta" with the new
                        // assembly (lands when bridge/ wires the eager
                        // sync path, items 64 + 68).
                        let _ = puzzle_id;
                    }
                    Err(e) => log::error!("Failed to load puzzle {}: {}", puzzle_id, e),
                }
                self.router.ui_dirty |= DirtyFlags::LOADING
                    | DirtyFlags::SCORE
                    | DirtyFlags::SELECTION
                    | DirtyFlags::ACTIONS
                    | DirtyFlags::PUZZLE;
            }
            ParameterizedAction::CreateBand { .. } => {
                log::info!("CreateBand via IPC not yet wired");
            }
            ParameterizedAction::RemoveBand { .. } => {
                log::info!("RemoveBand via IPC not yet wired");
            }
            ParameterizedAction::SetViewOptions { options } => {
                match serde_json::from_value::<viso::options::VisoOptions>(options) {
                    Ok(opts) => {
                        engine.set_options(opts);
                        self.router.ui_dirty |= DirtyFlags::VIEW;
                    }
                    Err(e) => log::error!("Failed to deserialize view options: {}", e),
                }
            }
            ParameterizedAction::LoadViewPreset { name } => {
                #[cfg(not(target_arch = "wasm32"))]
                {
                    let presets_dir = std::path::Path::new("assets/view_presets");
                    engine.load_preset(&name, presets_dir);
                    self.router.ui_dirty |= DirtyFlags::VIEW;
                }
                #[cfg(target_arch = "wasm32")]
                { let _ = name; let _ = engine; }
            }
            ParameterizedAction::SaveViewPreset { name } => {
                #[cfg(not(target_arch = "wasm32"))]
                {
                    let presets_dir = std::path::Path::new("assets/view_presets");
                    engine.save_preset(&name, presets_dir);
                    self.router.ui_dirty |= DirtyFlags::VIEW;
                }
                #[cfg(target_arch = "wasm32")]
                { let _ = name; let _ = engine; }
            }
            ParameterizedAction::History { .. }
            | ParameterizedAction::AdvanceBubble { .. } => {
                // Handled in the early-return block above. The match is
                // exhaustive over `ParameterizedAction` (G10): a new
                // variant without a handler is a compile error.
            }
        }
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
        self.router.ui_dirty |= DirtyFlags::TEXT_BUBBLE;
    }

    // ── History navigation (Undo / Redo / Jump / Pin) ──

    pub fn handle_undo(&mut self) {
        self.run_history_command(HistoryCommand::Undo);
        self.pump_scene_changes();
        self.poll_plugin_scores();
    }

    pub fn handle_redo(&mut self) {
        self.run_history_command(HistoryCommand::Redo { branch: None });
        self.pump_scene_changes();
        self.poll_plugin_scores();
    }

    /// Common tail for undo / redo / jump_checkpoint: republish to viso
    /// via `render_projector.replace` (unconditional rederive; the
    /// snapshot swap installs an Assembly that the `set_assembly`
    /// generation gate would otherwise skip), clear cached per-residue scores (the
    /// values were computed against the *previous* head and become
    /// meaningless on a head move; v1 just blanks them so the structure
    /// renders neutral instead of "gray", v2 will async-reeval), and
    /// mark UI dirty. Score is no longer cached in `App`; the GUI
    /// projection reads it off the new head checkpoint on the next
    /// `populate_frontend` (G1).
    fn after_head_move(&mut self) {
        if let Some(engine) = self.engine.as_mut() {
            self.render_projector.replace(&self.store, engine);
            let ids: Vec<EntityId> = self.store.ids().collect();
            for eid in ids {
                engine.set_per_residue_scores(eid.raw(), None);
            }
        }

        self.router.ui_dirty |= DirtyFlags::SCORE | DirtyFlags::ACTIONS | DirtyFlags::SCENE;
    }

    /// Dispatch a [`HistoryCommand`] from the GUI to the matching
    /// `Document` method. Refusals are logged; the GUI surface
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
        let result: Result<HistoryOutcome, DocumentError> = match cmd {
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
                self.router.ui_dirty |= DirtyFlags::ACTIONS;
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
        if let Some(engine) = &mut self.engine {
            self.router.handle_native_mouse_input(engine, &mut self.input, &mut self.store, button, pressed);
            update_all_visualizations(engine, &self.router, None);
        }
    }

    pub fn handle_native_cursor_moved(&mut self, x: f32, y: f32) {
        if let Some(engine) = &mut self.engine {
            self.router.handle_native_cursor_moved(engine, &self.input, &mut self.store, x, y);
            update_all_visualizations(engine, &self.router, None);
        }
    }

    /// Forward a scroll delta in viso "logical scroll units" (winit
    /// `LineDelta(_, y)` passes `y` directly; `PixelDelta(_, y)` should
    /// pass `y * 0.01`). Conversion lives in the host.
    pub fn handle_native_mouse_wheel(&mut self, scroll_delta: f32) {
        if let Some(engine) = &mut self.engine {
            if let Some(cmd) = self.input.handle_event(InputEvent::Scroll { delta: scroll_delta }, engine.hovered_target()) {
                engine.execute(cmd);
            }
        }
    }

    pub fn handle_native_modifiers(&mut self, shift: bool) {
        if let Some(engine) = &mut self.engine {
            if let Some(cmd) = self.input.handle_event(InputEvent::ModifiersChanged {
                shift,
            }, engine.hovered_target()) {
                engine.execute(cmd);
            }
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
        update_all_visualizations(engine, &self.router, pull);
    }

    // ── Frontend state sync ──

    pub fn populate_frontend(&mut self, frontend: &mut foldit_gui::FrontendState) {
        let engine = match &self.engine {
            Some(e) => e,
            None => return,
        };

        // FPS and selected count change every frame — always push them
        frontend.set_fps(engine.fps());
        frontend.ui.selected_count = engine.selected_residues().len();

        let app_dirty = self.router.take_ui_dirty();
        if app_dirty.is_empty() {
            return;
        }

        // PUZZLE before SCORE: a fresh `set_puzzle_*` resets `complete=false`,
        // and then the score check below can latch victory in the same frame
        // without being overwritten.
        if app_dirty.contains(DirtyFlags::PUZZLE) {
            match self.gui_projector.scoring_mode {
                ScoringMode::Game => frontend.set_puzzle_game(
                    self.puzzle.id,
                    self.puzzle.title.clone(),
                    self.puzzle.starting_score,
                    self.puzzle.target_score,
                ),
                ScoringMode::Scientist => frontend.set_puzzle_scientist(
                    if self.puzzle.title.is_empty() {
                        self.structure_title()
                    } else {
                        self.puzzle.title.clone()
                    },
                ),
            }
            // Bubble push on puzzle swap: render the cursor's current
            // bubble (always index 0 right after LoadPuzzle, since the
            // cursor is reset there). Subsequent AdvanceBubble actions
            // re-push via the DirtyFlags::TEXT_BUBBLE arm below.
            frontend.set_text_bubble(
                self.gui_projector
                    .bubbles
                    .get(self.gui_projector.current_bubble)
                    .map(bubble_to_payload),
            );
        }
        if app_dirty.contains(DirtyFlags::TEXT_BUBBLE) {
            frontend.set_text_bubble(
                self.gui_projector
                    .bubbles
                    .get(self.gui_projector.current_bubble)
                    .map(bubble_to_payload),
            );
        }
        if app_dirty.contains(DirtyFlags::SCORE) {
            if let Some(score) = head_score(&self.store, self.gui_projector.scoring_mode) {
                frontend.set_score(score, false);
                // Victory check: in Game mode, latch puzzle as complete the
                // first time current_score crosses the toml target. Higher
                // game score = better fold (game-score formula negates),
                // so the comparison is `>=`.
                if self.gui_projector.scoring_mode == ScoringMode::Game
                    && self.puzzle.target_score > 0.0
                    && score >= self.puzzle.target_score
                {
                    frontend.mark_puzzle_complete();
                }
            }
        }
        if app_dirty.contains(DirtyFlags::ACTIONS) {
            frontend.set_actions(action_router::build_actions_list(&self.plugin_driver.orchestrator));
        }
        if app_dirty.contains(DirtyFlags::LOADING) {
            frontend.set_loading_progress(None);
        }
        if app_dirty.contains(DirtyFlags::VIEW) {
            frontend.view.options = serde_json::to_value(engine.options()).unwrap_or_default();

            // Schema is static — only set once
            if frontend.view.options_schema.is_null() {
                frontend.view.options_schema =
                    serde_json::to_value(viso::options::VisoOptions::json_schema())
                        .unwrap_or_default();
            }

            #[cfg(not(target_arch = "wasm32"))]
            {
                let presets_dir = std::path::Path::new("assets/view_presets");
                frontend.view.available_presets =
                    viso::options::VisoOptions::list_presets(presets_dir);
            }
            frontend.view.active_preset = engine.active_preset().map(String::from);
        }
        if app_dirty.contains(DirtyFlags::SELECTION) {
            frontend.mark_dirty(DirtyFlags::SELECTION);
        }
        if app_dirty.contains(DirtyFlags::UI) {
            frontend.mark_dirty(DirtyFlags::UI);
        }
        if app_dirty.contains(DirtyFlags::LOADING) || app_dirty.contains(DirtyFlags::SELECTION) {
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
            frontend.set_scene_entities(scene_entities);
            let focused = match engine.focus() {
                Focus::Entity(eid) => Some(eid.raw()),
                Focus::Session => None,
            };
            frontend.set_focused_entity(focused);
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
            frontend.set_history(project_history(&self.store));
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
                    frontend.set_history_live(update);
                    cursor.last_live = Some(live);
                    cursor.last_live_push_at = Some(now);
                }
            }
        }

    }

    // ── Complex methods (touch both engine and router) ──

    /// Attach a host-built `VisoEngine` to this App. Hosts are
    /// responsible for constructing the wgpu `RenderContext` against
    /// their own surface (winit window on desktop, `<canvas>` on web)
    /// and applying any preset / render-scale tweaks they want before
    /// handing it over.
    pub fn attach_engine(&mut self, engine: VisoEngine) {
        self.engine = Some(engine);
    }

    /// Take the engine back out for a brief moment (used during
    /// load_initial_structure-style flows that need disjoint borrows).
    pub fn engine_take(&mut self) -> Option<VisoEngine> {
        self.engine.take()
    }

    /// Replace the engine after a `engine_take`/inspection.
    pub fn engine_replace(&mut self, engine: VisoEngine) {
        self.engine = Some(engine);
    }

    /// Load the initial structure, register entities, and create the
    /// initial Rosetta session. Runs AFTER the webview's loading screen
    /// is visible so the user has feedback during the (potentially
    /// slow) load. Requires `create_render_context` to have run first.
    pub fn load_initial_structure(&mut self) {
        // Take engine out so we can hold a `&mut engine` alongside `&mut self.store`
        // etc. without borrow-checker grief; restored at end.
        let Some(mut engine) = self.engine.take() else {
            log::error!("load_initial_structure called before create_render_context");
            return;
        };

        // Parse entities from file
        match crate::puzzle::load_file_as_entities(&self.pdb_path) {
            Ok((entities, name)) => {
                for entity in entities {
                    let _ = load_entity_into_history(&mut self.store, entity, name.clone());
                }

                // Push to viso (viso inherits our IDs). update(0.0)
                // drains the pending Assembly so scene.current is
                // populated before fit_camera reads it.
                self.render_projector.publish(&self.store, &mut engine);
                engine.update(0.0);
                engine.fit_camera_to_focus();

                log::info!("Loaded structure: {}", name);

                // Plugin streaming updates land via plugin_update_rx;
                // canonical state is the Document.
                let mut orch = Orchestrator::new();
                bootstrap_plugins(&mut orch, &mut self.store);
                // Republish: bootstrap may have committed rosetta's
                // post-Init normalized assembly (full-atom pose) into the
                // store. Push it here — after bootstrap, before
                // refresh_scores — the same point the publish formerly ran
                // inside apply_rosetta_post_init.
                self.render_projector.publish(&self.store, &mut engine);
                refresh_scores(
                    &mut orch,
                    &mut self.store,
                    Some(&mut engine),
                );
                self.plugin_driver.orchestrator = Some(orch);
            }
            Err(e) => {
                log::error!("Failed to load structure '{}': {}", self.pdb_path, e);
                self.plugin_driver.orchestrator = Some(Orchestrator::new());
            }
        }

        self.engine = Some(engine);

        // Push the now-populated state to the GUI on the next frame:
        // VIEW for the engine options, ACTIONS so the catalog (wiggle
        // etc.) renders, SCORE so the initial number from
        // refresh_scores reaches the score widget, SCENE for the
        // entity list, LOADING to flip out of the loading screen.
        self.router.ui_dirty |= DirtyFlags::VIEW
            | DirtyFlags::ACTIONS
            | DirtyFlags::SCORE
            | DirtyFlags::SCENE
            | DirtyFlags::LOADING;
    }

    /// Shut down backends and scene processor.
    pub fn shutdown(&mut self) {
        self.plugin_driver.shutdown();
        if let Some(engine) = &mut self.engine {
            engine.shutdown();
        }
    }
}

// ---------------------------------------------------------------------------
// Bridge: Dispatcher trait impl
// ---------------------------------------------------------------------------

impl foldit_gui::Dispatcher for App {
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
                let bytes = std::fs::read(filepath)
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
/// band-router state machine is the next item to come back online).
/// The pull capsule + cone arrow renders whenever the caller hands a
/// `Some(PullInfo)` from a live drag; clears otherwise so a finished
/// or cancelled drag leaves no overlay.
fn update_all_visualizations(
    engine: &mut VisoEngine,
    _router: &ActionRouter,
    pull: Option<viso::PullInfo>,
) {
    engine.update_bands(vec![]);
    engine.update_pull(pull);
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

/// Discover plugins under the runtime plugin root and bring up the
/// always-on Rosetta session with the just-loaded structure as the
/// initial assembly. Errors are logged and dropped: a missing plugin
/// dir / dylib should degrade the app to viewer-only, not crash the
/// load.
///
/// If Rosetta's Init returns a non-empty normalized assembly (full-atom
/// pose with hydrogens / terminal O / etc. added), it is committed as
/// a follow-up `PluginOp` checkpoint and republished so that
/// `scene.positions` is seeded at the normalized atom count before any
/// user action runs. Without this, the first user op would cross an
/// atom-set boundary mid-action and snap.
#[cfg(not(target_arch = "wasm32"))]
fn bootstrap_plugins(
    orch: &mut Orchestrator,
    store: &mut Document,
) {
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
    let discovered = match orch.discover_plugins(&plugins_root) {
        Ok(ids) => ids,
        Err(e) => {
            log::warn!(
                "[App] discover_plugins({}) failed: {e}; plugins disabled",
                plugins_root.display()
            );
            return;
        }
    };
    log::info!("[App] discovered plugins: {discovered:?}");

    let head_before = store.head_assembly();
    let initial_assembly = match molex::ops::wire::serialize_assembly(
        &head_before,
    ) {
        Ok(b) => b,
        Err(e) => {
            log::warn!(
                "[App] failed to serialize initial assembly for plugin \
                 registration: {e:?}; plugins disabled"
            );
            return;
        }
    };

    for plugin_id in &discovered {
        let post_init_bytes = match orch
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
        };
        log::info!("[App] {plugin_id} plugin ready");

        if plugin_id == "rosetta" {
            apply_rosetta_post_init(store, &post_init_bytes);
        }
    }
}

/// Apply rosetta's post-Init normalized assembly (full-atom pose) so the
/// host's canonical assembly matches the plugin's internal pose before
/// any user action runs.
#[cfg(not(target_arch = "wasm32"))]
fn apply_rosetta_post_init(
    store: &mut Document,
    post_init_bytes: &[u8],
) {
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
    let Some(target_entity) = first_protein_entity(store) else {
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
    if let Err(e) =
        store.begin_action(kind, String::from("Init"))
    {
        log::warn!(
            "[App] rosetta post-Init begin_action failed: {e}; \
             skipping normalization apply"
        );
        return;
    }
    let applied = apply_streaming_assembly(store, &normalized, None);
    if !applied {
        log::warn!(
            "[App] rosetta post-Init apply_streaming_assembly did not \
             update any entity; rolling back tentative. This usually means \
             the rosetta-returned entity ID does not match any store \
             entity ID."
        );
        let _ = store.commit_action();
        return;
    }
    if let Err(e) = store.commit_action() {
        log::warn!(
            "[App] rosetta post-Init commit_action failed: {e}"
        );
        return;
    }
    log::info!(
        "[App] rosetta post-Init assembly applied ({} bytes)",
        post_init_bytes.len()
    );
    // Republish is hoisted to `load_initial_structure` (the App method
    // that owns `render_projector`); the projector is never threaded down
    // here.
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
fn refresh_scores(
    orch: &mut Orchestrator,
    store: &mut Document,
    engine: Option<&mut VisoEngine>,
) {
    use foldit_runner::orchestrator::SessionContext;

    let reports = orch.collect_scores(&SessionContext::default());
    if reports.is_empty() {
        return;
    }

    use std::collections::HashMap;

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
        store.set_head_scores(Some(raw), Some(game));
    }

    // Push per-residue scores into the engine so Score / ScoreRelative
    // color schemes have data. Each entity's score Vec is sized to
    // its full residue count; missing residues default to 0.0 (the
    // mid-palette stop in absolute mode, the lower quantile in
    // relative mode -- close enough for a first-pass render).
    let Some(engine) = engine else { return };
    // Build (raw_entity_id -> residue_count) once via head_assembly so
    // we don't need a mut borrow on store to mint molex EntityIds.
    let head = store.head_assembly();
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

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------
