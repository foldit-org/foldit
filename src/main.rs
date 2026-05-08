//! Foldit-RS: A reimagined Foldit
//!
//! Decoupled architecture with GUI, render engine, and backends
//! for Rosetta and ML-powered structure prediction and design.
//!
//! Controls:
//!   W - Wiggle (Rosetta minimize, toggle on/off)
//!   S - Shake (Rosetta repack sidechains, toggle on/off)
//!   P - Predict (RoseTTAFold3 structure prediction)
//!   M - MPNN (design sequence for structure)
//!   R - Toggle auto-rotate (turntable spin)
//!   I - Toggle water and ion visibility
//!   Q - Recenter camera on focused entity
//!   T - Toggle trajectory playback (load with --trajectory <path.dcd>)
//!   Tab - Cycle focus (Session -> Structure 1 -> ... -> Session)
//!   ` (backtick) - Reset focus to full scene
//!   Esc - Cancel operation / clear selection / clear bands
//!   Left-drag on residue - Pull (coming soon)
//!   Right-drag residue to residue - Create band
//!   Mouse - Rotate/zoom camera

mod action_router;
mod backend_results;
mod tee_logger;
mod window;

use action_router::ActionRouter;
use foldit_gui::{
    CheckpointInfo, CheckpointKindTag, DirtyFlags, FilterStatus, HistoryCommand, HistoryLiveUpdate,
    HistorySection, ScoringMode, WireId,
};
use foldit_runner::Orchestrator;
use foldit::entity_store::{EntityStore, EntityStoreError, EntityOrigin, EntityRole};
use foldit::history::{CheckpointKind, FilterStatus as HistoryFilterStatus, History};
use molex::entity::molecule::id::EntityId;
use viso::{BandInfo, BandTarget, AtomRef, Focus, InputEvent, InputProcessor, PullInfo, VisoEngine, VisoCommand};
use std::sync::Arc;
use std::time::{Instant, UNIX_EPOCH};
use winit::event::MouseScrollDelta;
use winit::keyboard::ModifiersState;
use winit::window::Window;

/// Pick the score on a checkpoint that matches the active `ScoringMode`
/// (G7: no place in the *write* path consults the mode; the read /
/// projection path picks).
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

fn timestamp_ms(t: std::time::SystemTime) -> f64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0)
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
fn project_history(store: &EntityStore) -> HistorySection {
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
fn head_score(store: &EntityStore, mode: ScoringMode) -> Option<f64> {
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
    store: &mut EntityStore,
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
    let id = store.insert_preview(
        entity,
        name.clone(),
        EntityOrigin::Loaded,
        EntityRole {
            foldable: !is_ambient && !zero_residue,
            designable: !is_ambient && !zero_residue,
            ambient: is_ambient,
        },
    );
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

/// Main application state — thin glue connecting the render engine and action router.
pub(crate) struct App {
    engine: Option<VisoEngine>,
    input: InputProcessor,
    store: EntityStore,
    router: ActionRouter,
    pdb_path: String,
    /// `Game` for tutorial/campaign/server puzzles, `Scientist` for CLI /
    /// drag-drop loads. Drives which score representation reaches the GUI.
    scoring_mode: ScoringMode,
    /// Puzzle metadata from the active toml. Zero/empty in Scientist mode.
    puzzle_id: u32,
    puzzle_title: String,
    starting_score: f64,
    target_score: f64,
    /// Last `History::topology_version()` that was pushed to the frontend.
    /// `None` forces an initial push (G5: no `u64::MAX` sentinel).
    last_history_topology: Option<u64>,
    /// Last `History::live_version()` pushed; mid-action score updates
    /// only — full-graph reproject is gated on `last_history_topology`.
    last_history_live: Option<u64>,
    /// Wall-clock of the last live history push. Gates the 50ms (20Hz)
    /// debounce so per-cycle Rosetta updates don't saturate the IPC.
    last_history_live_push_at: Option<Instant>,
}

impl App {
    /// Get a display title derived from the PDB path (e.g. "1BFE" from ".../1bfe.cif")
    pub(crate) fn structure_title(&self) -> String {
        std::path::Path::new(&self.pdb_path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("Unknown")
            .to_uppercase()
    }

    pub(crate) fn new(pdb_path: String) -> Self {
        Self {
            engine: None,
            input: InputProcessor::new(),
            store: EntityStore::new(),
            router: ActionRouter::new(),
            pdb_path,
            // CLI bootstrap defaults to scientist; LoadPuzzle flips to Game
            // when an intro/campaign puzzle is loaded.
            scoring_mode: ScoringMode::Scientist,
            puzzle_id: 0,
            puzzle_title: String::new(),
            starting_score: 0.0,
            target_score: 0.0,
            last_history_topology: None,
            last_history_live: None,
            last_history_live_push_at: None,
        }
    }

    /// True once the Rosetta backend has delivered its first score update
    /// for the current session. Replaces the old `latest_score`
    /// shadow-field check; the truth source is now the head checkpoint.
    pub(crate) fn has_initial_score(&self) -> bool {
        head_score(&self.store, self.scoring_mode).is_some()
    }

    // ── Engine-only delegation (no router interaction) ──

    pub(crate) fn resize(&mut self, width: u32, height: u32) {
        if let Some(engine) = &mut self.engine {
            engine.resize(width, height);
        }
    }

    pub(crate) fn set_surface_scale(&mut self, scale_factor: f64) {
        if let Some(ref mut engine) = self.engine {
            engine.set_render_scale(if scale_factor < 2.0 { 2 } else { 1 });
        }
    }

    pub(crate) fn update_engine(&mut self, dt: f32) {
        if let Some(engine) = &mut self.engine {
            engine.update(dt);
        }
    }

    pub(crate) fn render(&mut self) {
        if let Some(engine) = &mut self.engine {
            if let Err(e) = engine.render() {
                log::error!("Render error: {:?}", e);
            }
        }
    }

    // ── Backend update processing ──

    pub(crate) fn apply_backend_updates(&mut self) {
        // 1. Tell orchestrator to pump internal channels → triple buffers
        //    and drain readers in one pass.
        let updates = match self.router.orchestrator.as_mut() {
            Some(orch) => {
                orch.pump_updates();
                orch.drain_updates()
            }
            None => return,
        };
        if updates.is_empty() {
            return;
        }

        // 2. Process each update via backend_results free functions
        if let Some(engine) = &mut self.engine {
            for (_entity_id, update) in updates {
                backend_results::apply_backend_update(
                    engine,
                    &mut self.store,
                    &mut self.router.orchestrator,
                    &mut self.router.ui_dirty,
                    &mut self.router.pending_prediction_reference,
                    &mut self.router.pending_preview_id,
                    self.scoring_mode,
                    update,
                );
            }
        }
    }

    // ── Keybinding dispatch (engine + router) ──

    pub(crate) fn handle_keybinding(&mut self, key: winit::keyboard::KeyCode) -> bool {
        let Some(engine) = &mut self.engine else { return false };
        let key_str = format!("{key:?}");
        let Some(cmd) = self.input.handle_key_press(&key_str) else {
            return false;
        };

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
                self.router.cancel_operations(engine, &mut self.store);
            }
            VisoCommand::CycleFocus | VisoCommand::ResetFocus => {
                engine.execute(cmd);
                log::info!("Focus: {}", self.store.focus_description(&engine.focus()));
                self.router.update_rosetta_locks(engine, &self.store);
                self.router.ui_dirty |= DirtyFlags::SELECTION | DirtyFlags::UI;
            }
            // All other commands: delegate entirely to viso
            other => { engine.execute(other); }
        }
        true
    }

    // ── Viewport input (from webview) ──

    pub(crate) fn handle_viewport_input(&mut self, input: foldit_gui::ViewportInput) {
        use foldit_gui::ViewportInput;
        let Some(engine) = &mut self.engine else { return };

        match input {
            ViewportInput::PointerDown {
                x, y, button, shift, ..
            } => {
                let winit_button = match button {
                    0 => winit::event::MouseButton::Left,
                    2 => winit::event::MouseButton::Right,
                    1 => winit::event::MouseButton::Middle,
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
                self.router.handle_native_mouse_input(engine, &mut self.input, &mut self.store, winit_button, true);
            }
            ViewportInput::PointerUp {
                x, y, button, shift, ..
            } => {
                let winit_button = match button {
                    0 => winit::event::MouseButton::Left,
                    2 => winit::event::MouseButton::Right,
                    1 => winit::event::MouseButton::Middle,
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
                self.router.handle_native_mouse_input(engine, &mut self.input, &mut self.store, winit_button, false);
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
                            VisoCommand::ToggleTrajectory => {
                                if engine.has_trajectory() {
                                    engine.execute(VisoCommand::ToggleTrajectory);
                                } else if let Some(path) = action_router::trajectory_path_from_args() {
                                    engine.load_trajectory(std::path::Path::new(&path));
                                }
                            }
                            VisoCommand::ClearSelection => {
                                self.router.cancel_operations(engine, &mut self.store);
                            }
                            VisoCommand::CycleFocus | VisoCommand::ResetFocus => {
                                engine.execute(cmd);
                                self.router.update_rosetta_locks(engine, &self.store);
                                self.router.ui_dirty |= DirtyFlags::SELECTION | DirtyFlags::UI;
                            }
                            other => { engine.execute(other); }
                        }
                    } else {
                        log::debug!("Unhandled key code from frontend: {}", code);
                    }
                }
            }
            ViewportInput::Resize { .. } => {
                // Ignored: JS sends CSS pixels (logical) which are wrong on HiDPI.
            }
        }

        self.router.ui_dirty |= DirtyFlags::UI;

        // Update drag/pull/band visualizations after input
        update_all_visualizations(engine, &self.router);
    }

    pub(crate) fn handle_trigger_action(&mut self, action: foldit_gui::ActionId) {
        // Undo / Redo intercept the dispatch chain because they need
        // &mut self to call store + engine; the router can't reach
        // those fields.
        match action {
            foldit_gui::ActionId::Undo => {
                self.handle_undo();
                return;
            }
            foldit_gui::ActionId::Redo => {
                self.handle_redo();
                return;
            }
            _ => {}
        }
        if let Some(engine) = &mut self.engine {
            if let Some(pa) = self.router.handle_trigger_action(engine, &mut self.store, action) {
                self.handle_parameterized_action(pa);
            }
        }
    }

    pub(crate) fn handle_parameterized_action(
        &mut self,
        action: foldit_gui::ParameterizedAction,
    ) {
        use foldit_gui::ParameterizedAction;

        // History-side commands take &mut self (no engine borrow held).
        if let ParameterizedAction::History { cmd } = action {
            self.run_history_command(cmd);
            return;
        }

        let title = self.structure_title();
        let Some(engine) = &mut self.engine else { return };

        match action {
            ParameterizedAction::LoadStructure { path } => {
                match foldit::puzzle::load_file_as_entities(&path) {
                    Ok((entities, name)) => {
                        log::info!("Loaded structure via IPC: {}", name);
                        let backbone_ca = action_router::entities_backbone_ca(&entities);
                        let mut ids = Vec::new();
                        for entity in entities {
                            if let Some(id) = load_entity_into_history(&mut self.store, entity, name.clone()) {
                                ids.push(id);
                            }
                        }
                        self.store.publish_to(engine);
                        engine.fit_camera_to_focus();
                        if let Some(&first_id) = ids.first() {
                            self.store.register_loaded(first_id, backbone_ca);
                        }
                        // Free-form file load → scientist mode.
                        self.scoring_mode = ScoringMode::Scientist;
                        self.puzzle_id = 0;
                        self.puzzle_title = name;
                        self.starting_score = 0.0;
                        self.target_score = 0.0;
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
                self.router.reset_for_new_structure();

                match foldit::puzzle::load_puzzle_structure(puzzle_id) {
                    Ok(puzzle_data) => {
                        // Capture mode + puzzle metadata for the GUI.
                        self.scoring_mode = ScoringMode::Game;
                        self.puzzle_id = puzzle_id;
                        self.puzzle_title = puzzle_data.name.clone();
                        self.starting_score = puzzle_data.start_energy;
                        self.target_score = puzzle_data.completion_score;

                        if let Some(preset_name) = &puzzle_data.view_preset {
                            let presets_dir = std::path::Path::new("assets/view_presets");
                            engine.load_preset(preset_name, presets_dir);
                        }

                        let backbone_ca = action_router::entities_backbone_ca(&puzzle_data.entities);
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
                        self.store.replace_in(engine);

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

                        if let Some(&first_id) = ids.first() {
                            self.store.register_loaded(first_id, backbone_ca);
                        }

                        // Recreate Rosetta session for the new topology so cycle-0
                        // scoring fires immediately and per-residue colors land
                        // without waiting for a user action.
                        if !self.router.ensure_rosetta_session(&self.store) {
                            log::warn!("Failed to create Rosetta session for puzzle {}", puzzle_id);
                        }
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
                let presets_dir = std::path::Path::new("assets/view_presets");
                engine.load_preset(&name, presets_dir);
                self.router.ui_dirty |= DirtyFlags::VIEW;
            }
            ParameterizedAction::SaveViewPreset { name } => {
                let presets_dir = std::path::Path::new("assets/view_presets");
                engine.save_preset(&name, presets_dir);
                self.router.ui_dirty |= DirtyFlags::VIEW;
            }
            ParameterizedAction::RunSequenceDesign { temperature, num_sequences } => {
                Self::run_sequence_design(
                    &mut self.router, &mut self.store, engine,
                    temperature, num_sequences,
                );
            }
            ParameterizedAction::RunStructureDesign { length, num_steps, contig } => {
                Self::run_structure_design(
                    &mut self.router, &mut self.store, engine,
                    &length, num_steps, contig,
                );
            }
            ParameterizedAction::RunPrediction { entity_ids } => {
                Self::run_prediction_for_entities(
                    &mut self.router, &mut self.store, engine,
                    &entity_ids,
                );
            }
            ParameterizedAction::History { .. } => {
                // Handled in the early-return block above. The match is
                // exhaustive over `ParameterizedAction` (G10): a new
                // variant without a handler is a compile error.
            }
        }
    }

    // ── History navigation (Undo / Redo / Jump / Pin) ──

    pub(crate) fn handle_undo(&mut self) {
        self.run_history_command(HistoryCommand::Undo);
    }

    pub(crate) fn handle_redo(&mut self) {
        self.run_history_command(HistoryCommand::Redo { branch: None });
    }

    /// Common tail for undo / redo / jump_checkpoint: republish to viso
    /// via `replace_in` (unconditional rederive — the snapshot swap
    /// installs an Assembly with stale generation that `set_assembly`
    /// would otherwise skip), clear cached per-residue scores (the
    /// values were computed against the *previous* head and become
    /// meaningless on a head move; v1 just blanks them so the structure
    /// renders neutral instead of "gray", v2 will async-reeval), and
    /// mark UI dirty. Score is no longer cached in `App`; the GUI
    /// projection reads it off the new head checkpoint on the next
    /// `populate_frontend` (G1).
    fn after_head_move(&mut self) {
        if let Some(engine) = self.engine.as_mut() {
            self.store.replace_in(engine);
            let ids: Vec<EntityId> = self.store.ids().collect();
            for eid in ids {
                engine.set_per_residue_scores(eid.raw(), None);
            }
        }

        // Push the new pose to Rosetta and trigger a cycle-0 re-score.
        // Without this, the head move installs the right coordinates in
        // viso but the Rosetta session keeps the *previous* head's pose
        // — `head_score` (which now reads off the head checkpoint)
        // returns the snapshot's stamped score, frozen in time, and
        // `set_per_residue_scores` above stays cleared (gray
        // structure). `recreate_session` rebuilds Rosetta's pose from
        // the current `head_assembly()`; the cycle-0 init score lands
        // back through the normal `BackendUpdate::RosettaCoords` path,
        // which `apply_ongoing_update`'s idle branch then stamps via
        // `set_head_scores`. Per-residue colors restore via
        // `cache_per_residue_scores`. Same mechanism as load-time
        // scoring — just retriggered on every head move.
        if let Some(combined) = self.store.combined_assembly_for_backend() {
            if let Some(orch) = self.router.orchestrator.as_ref() {
                if let Err(e) = orch.recreate_session(combined.assembly.clone()) {
                    log::warn!(
                        "after_head_move: failed to recreate Rosetta session: {e}"
                    );
                }
            }
        }

        self.router.ui_dirty |= DirtyFlags::SCORE | DirtyFlags::ACTIONS | DirtyFlags::SCENE;
    }

    /// Dispatch a [`HistoryCommand`] from the GUI to the matching
    /// `EntityStore` method. Refusals are logged; the GUI surface
    /// shows the result by virtue of the head not moving (no separate
    /// toast / error channel — single-client mode never produces a
    /// `LockedByClient` refusal, and `HistoryError::EntityLocked` only
    /// fires while the user's own action is still running, where the
    /// running indicator is the natural feedback). The match is
    /// exhaustive (G10): adding a variant without a handler is a
    /// compile error.
    fn run_history_command(&mut self, cmd: HistoryCommand) {
        if self.engine.is_none() {
            return;
        }
        let result: Result<HistoryOutcome, EntityStoreError> = match cmd {
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

    // ── ML operations (parameterized) ──

    /// Build entity context data from a pre-collected slice of entities.
    ///
    /// `focused_entity_id` switches the runner into focus-aware mode, used
    /// by MPNN: only the focused protein is designable; every other
    /// non-ambient entity is fixed.
    fn build_entity_context(
        entities: Vec<molex::MoleculeEntity>,
        store: &EntityStore,
        entity_id: EntityId,
        focused_entity_id: Option<u32>,
    ) -> foldit_runner::orchestrator::EntityContextData {
        use foldit_runner::orchestrator::{EntityContextData, EntityRoleHint};

        let target_role = store.entity_meta(entity_id).map(|(_, r)| r.clone());
        EntityContextData::from_entities(entities, focused_entity_id, |raw_id| {
            // Generic case (no focus) uses the *target's* role for every
            // entity in the slice — preserves the original
            // `App::build_entity_context` behavior, which always read
            // `entity_meta(entity_id)` regardless of which entity was
            // being described.
            target_role.as_ref().map(|r| {
                let _ = raw_id;
                EntityRoleHint {
                    designable: r.designable,
                    foldable: r.foldable,
                    ambient: r.ambient,
                }
            })
        })
    }

    fn run_sequence_design(
        router: &mut ActionRouter,
        store: &mut EntityStore,
        engine: &mut VisoEngine,
        temperature: f32,
        num_sequences: u32,
    ) {
        use foldit_runner::orchestrator::{EntityId as RunnerEntityId, OpType};
        use crate::action_router::BackendOpRequest;

        let focus = engine.focus();
        log::info!("MPNN: focus = {:?}", focus);

        let loaded = store.loaded_entity();
        let Some((target_id, entities)) =
            store.collect_ml_entities(&focus, loaded)
        else {
            log::warn!("No structure available for sequence design");
            return;
        };

        let total_atoms: usize = entities.iter().map(|e| e.atom_count()).sum();
        let entity_name = store.metadata(target_id).map(|m| m.name.clone());
        log::info!(
            "MPNN: target_id={}, entity='{}', {} entities, {} total atoms",
            target_id.raw(),
            entity_name.as_deref().unwrap_or("?"),
            entities.len(),
            total_atoms,
        );

        // Role validation: target must be designable
        if let Some((_, role)) = store.entity_meta(target_id) {
            if role.ambient {
                log::warn!("Cannot run sequence design on ambient entity group (water/ion)");
                return;
            }
            if !role.designable {
                log::warn!("Target entity group is not designable");
                return;
            }
        }

        let focused_entity_id: Option<u32> = match focus {
            Focus::Entity(eid) => Some(eid.raw()),
            _ => None,
        };

        let entity_context = Self::build_entity_context(
            entities, store, target_id, focused_entity_id,
        );
        let assembly = entity_context.assembly.clone();

        let designed: Vec<&str> = entity_context.entities.iter()
            .filter(|e| e.designable)
            .map(|e| e.chain_id.as_str())
            .collect();
        let fixed_chains: Vec<&str> = entity_context.entities.iter()
            .filter(|e| e.fixed)
            .map(|e| e.chain_id.as_str())
            .collect();
        log::info!(
            "MPNN entity context: designed={:?}, fixed={:?}",
            designed, fixed_chains,
        );

        let target_name = store.metadata(target_id)
            .map(|m| m.name.clone())
            .unwrap_or_default();
        log::info!(
            "Starting sequence design on '{}' ({} entities, T={}, n={})...",
            target_name, assembly.entities().len(), temperature, num_sequences,
        );

        router.start_op(
            BackendOpRequest {
                target: RunnerEntityId(u64::from(target_id.raw())),
                op_type: OpType::MLSequenceDesign,
                entity_context,
                stop_rosetta_session: false,
                create_preview_mirror: false,
                pending_reference_ca: None,
                kickoff: Box::new(move |orch, ctx| {
                    orch.design_sequence_with_context(
                        assembly, temperature, num_sequences, ctx,
                    )
                }),
            },
            engine,
            store,
        );
    }

    fn run_structure_design(
        router: &mut ActionRouter,
        store: &mut EntityStore,
        engine: &mut VisoEngine,
        length: &str,
        num_steps: u32,
        contig: Option<String>,
    ) {
        use foldit_runner::orchestrator::{EntityId as RunnerEntityId, OpType};
        use crate::action_router::BackendOpRequest;

        let Some(target_id) = store.loaded_entity() else {
            log::warn!("No structure available for structure design");
            return;
        };
        let Some(entity) = store.entity(target_id).cloned() else {
            log::warn!("No structure available for structure design");
            return;
        };

        // Role validation: target must be foldable
        if let Some((_, role)) = store.entity_meta(target_id) {
            if role.ambient {
                log::warn!("Cannot run structure design on ambient entity (water/ion)");
                return;
            }
            if !role.foldable {
                log::warn!("Target entity is not foldable");
                return;
            }
        }

        use molex::MoleculeType;
        let entities: Vec<molex::MoleculeEntity> = vec![entity].into_iter().filter(|e| {
            !matches!(e.molecule_type(), MoleculeType::Water | MoleculeType::Ion | MoleculeType::Solvent)
        }).collect();

        let total_atoms: usize = entities.iter().map(|e| e.atom_count()).sum();
        log::info!(
            "RFD3 structure design: source={}, {} entities, {} total atoms",
            target_id.raw(), entities.len(), total_atoms,
        );

        let entity_context = Self::build_entity_context(entities, store, target_id, None);
        let contig_str = contig.unwrap_or_default();

        log::info!(
            "Starting structure design (length={}, steps={}, contig='{}')...",
            length, num_steps, contig_str,
        );
        log::info!(
            "Passing assembly context ({} info entries, {} entities in assembly)",
            entity_context.entities.len(),
            entity_context.assembly.entities().len(),
        );

        let length_owned = length.to_string();
        router.start_op(
            BackendOpRequest {
                target: RunnerEntityId(u64::from(target_id.raw())),
                op_type: OpType::MLStructureDesign,
                entity_context,
                stop_rosetta_session: false,
                create_preview_mirror: false,
                pending_reference_ca: None,
                kickoff: Box::new(move |orch, ctx| {
                    orch.design_structure_with_context(
                        length_owned, num_steps, contig_str, ctx,
                    )
                }),
            },
            engine,
            store,
        );
    }

    fn run_prediction_for_entities(
        router: &mut ActionRouter,
        store: &mut EntityStore,
        engine: &mut VisoEngine,
        entity_ids: &[u32],
    ) {
        use foldit_runner::orchestrator::{EntityId as RunnerEntityId, OpType};
        use crate::action_router::BackendOpRequest;

        // Resolve raw u32 ids (from the GUI's entity-picker payload) to
        // typed EntityIds via the store's allocator. `mint_id` advances
        // the allocator past `raw` so future allocations don't collide;
        // for ids the store already knows, that's a no-op.
        let mut resolved: Vec<EntityId> = Vec::new();
        let mut collected: Vec<molex::MoleculeEntity> = Vec::new();
        for raw in entity_ids {
            let id = store.mint_id(*raw);
            if let Some(entity) = store.entity(id) {
                collected.push(entity.clone());
                resolved.push(id);
            }
        }
        let target_id = match resolved.first().copied() {
            Some(id) => id,
            None => {
                log::warn!("No matching entities found for prediction");
                return;
            }
        };

        if collected.is_empty() {
            log::warn!("No entities selected for prediction");
            return;
        }

        let chains = foldit_runner::orchestrator::chains_from_entities(&collected);
        if chains.is_empty() {
            log::warn!("No protein chains found in selected entities");
            return;
        }

        let total_atoms: usize = collected.iter().map(|e| e.atom_count()).sum();
        log::info!(
            "RF3 prediction (entity picker): {} entities, {} total atoms",
            collected.len(), total_atoms,
        );

        let total_residues: usize = chains.iter().map(|(_, s)| s.len()).sum();
        log::info!("Starting RoseTTAFold3 prediction for {} residues...", total_residues);

        let pending_ca = molex::ops::codec::ca_positions(&collected);
        let entity_context = Self::build_entity_context(collected, store, target_id, None);

        router.start_op(
            BackendOpRequest {
                target: RunnerEntityId(u64::from(target_id.raw())),
                op_type: OpType::MLPredict,
                entity_context,
                stop_rosetta_session: true,
                create_preview_mirror: true,
                pending_reference_ca: Some(pending_ca),
                kickoff: Box::new(move |orch, ctx| {
                    orch.predict_with_context(None, chains, 3, ctx)
                }),
            },
            engine,
            store,
        );
    }

    // ── Native input (when webview is not ready) ──

    pub(crate) fn handle_native_mouse_input(
        &mut self,
        button: winit::event::MouseButton,
        pressed: bool,
    ) {
        if let Some(engine) = &mut self.engine {
            self.router.handle_native_mouse_input(engine, &mut self.input, &mut self.store, button, pressed);
            update_all_visualizations(engine, &self.router);
        }
    }

    pub(crate) fn handle_native_cursor_moved(&mut self, x: f32, y: f32) {
        if let Some(engine) = &mut self.engine {
            self.router.handle_native_cursor_moved(engine, &self.input, &mut self.store, x, y);
            update_all_visualizations(engine, &self.router);
        }
    }

    pub(crate) fn handle_native_mouse_wheel(&mut self, delta: MouseScrollDelta) {
        if let Some(engine) = &mut self.engine {
            let scroll_delta = match delta {
                MouseScrollDelta::LineDelta(_, y) => y,
                MouseScrollDelta::PixelDelta(pos) => pos.y as f32 * 0.01,
            };
            if let Some(cmd) = self.input.handle_event(InputEvent::Scroll { delta: scroll_delta }, engine.hovered_target()) {
                engine.execute(cmd);
            }
        }
    }

    pub(crate) fn handle_native_modifiers(&mut self, state: ModifiersState) {
        if let Some(engine) = &mut self.engine {
            if let Some(cmd) = self.input.handle_event(InputEvent::ModifiersChanged {
                shift: state.shift_key(),
            }, engine.hovered_target()) {
                engine.execute(cmd);
            }
        }
    }

    // ── Per-frame visual updates ──

    pub(crate) fn update_frame_visuals(&mut self) {
        let Some(engine) = &mut self.engine else { return };

        // Refresh pull drag position from current atom positions
        self.router.refresh_pull_position(engine);

        // Update all visualizations (bands, pull, band preview)
        update_all_visualizations(engine, &self.router);
    }

    // ── Frontend state sync ──

    pub(crate) fn populate_frontend(&mut self, frontend: &mut foldit_gui::FrontendState) {
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
            match self.scoring_mode {
                ScoringMode::Game => frontend.set_puzzle_game(
                    self.puzzle_id,
                    self.puzzle_title.clone(),
                    self.starting_score,
                    self.target_score,
                ),
                ScoringMode::Scientist => frontend.set_puzzle_scientist(
                    if self.puzzle_title.is_empty() {
                        self.structure_title()
                    } else {
                        self.puzzle_title.clone()
                    },
                ),
            }
        }
        if app_dirty.contains(DirtyFlags::SCORE) {
            if let Some(score) = head_score(&self.store, self.scoring_mode) {
                frontend.set_score(score, false);
                // Victory check: in Game mode, latch puzzle as complete the
                // first time current_score crosses the toml target. Higher
                // game score = better fold (game-score formula negates),
                // so the comparison is `>=`.
                if self.scoring_mode == ScoringMode::Game
                    && self.target_score > 0.0
                    && score >= self.target_score
                {
                    frontend.mark_puzzle_complete();
                }
            }
        }
        if app_dirty.contains(DirtyFlags::ACTIONS) {
            frontend.set_actions(action_router::build_actions_list(&self.router.orchestrator));
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

            let presets_dir = std::path::Path::new("assets/view_presets");
            frontend.view.available_presets =
                viso::options::VisoOptions::list_presets(presets_dir);
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
        let topology_changed = self.last_history_topology != Some(topology);
        let live_changed = self.last_history_live != Some(live);

        if topology_changed {
            frontend.set_history(project_history(&self.store));
            self.last_history_topology = Some(topology);
            self.last_history_live = Some(live);
            self.last_history_live_push_at = Some(Instant::now());
        } else if live_changed {
            let now = Instant::now();
            let debounced = self
                .last_history_live_push_at
                .map_or(false, |t| now.duration_since(t).as_millis() < 50);
            if !debounced {
                if let Some(update) = project_history_live(self.store.history()) {
                    frontend.set_history_live(update);
                    self.last_history_live = Some(live);
                    self.last_history_live_push_at = Some(now);
                }
            }
        }

    }

    // ── Complex methods (touch both engine and router) ──

    /// Phase 1: create the wgpu RenderContext and empty VisoEngine. This must
    /// run BEFORE the wry WebView is attached as a child of the window — on
    /// macOS, `wgpu::Instance::create_surface` calls `setLayer:` on the
    /// contentView, replacing it with a CAMetalLayer; if the WKWebView is
    /// already a subview at that point its backing layer never recovers and
    /// only `toggleFullScreen` heals it (Apple Forums 124688, wry#1335).
    /// Matches the canonical wry/examples/wgpu.rs ordering.
    pub(crate) fn create_render_context(&mut self, window: Arc<Window>) {
        let size = window.inner_size();
        let scale = window.scale_factor();
        log::info!(
            "create_render_context: inner_size={}x{}, scale_factor={}",
            size.width,
            size.height,
            scale
        );

        let context = match pollster::block_on(viso::RenderContext::new(window, (size.width, size.height))) {
            Ok(ctx) => ctx,
            Err(e) => {
                log::error!("Failed to initialize GPU render context: {:?}", e);
                return;
            }
        };

        let mut engine = match VisoEngine::new(context, viso::options::VisoOptions::default()) {
            Ok(e) => e,
            Err(e) => {
                log::error!("Failed to initialize engine: {:?}", e);
                return;
            }
        };

        // Load default view preset if available
        let presets_dir = std::path::Path::new("assets/view_presets");
        engine.load_preset("default", presets_dir);

        engine.set_render_scale(if scale < 2.0 { 2 } else { 1 });

        self.engine = Some(engine);
    }

    /// Phase 2: load the initial structure, register entities, and create the
    /// initial Rosetta session. Runs AFTER the webview's loading screen is
    /// visible so the user has feedback during the (potentially slow) load.
    /// Requires `create_render_context` to have run first.
    pub(crate) fn load_initial_structure(&mut self) {
        // Take engine out so we can hold a `&mut engine` alongside `&mut self.store`
        // etc. without borrow-checker grief; restored at end.
        let Some(mut engine) = self.engine.take() else {
            log::error!("load_initial_structure called before create_render_context");
            return;
        };

        // Parse entities from file
        match foldit::puzzle::load_file_as_entities(&self.pdb_path) {
            Ok((entities, name)) => {
                let backbone_ca = action_router::entities_backbone_ca(&entities);

                let mut ids: Vec<EntityId> = Vec::new();
                for entity in entities {
                    if let Some(id) = load_entity_into_history(&mut self.store, entity, name.clone()) {
                        ids.push(id);
                    }
                }

                // Push to viso (viso inherits our IDs). update(0.0)
                // drains the pending Assembly so scene.current is
                // populated before fit_camera reads it.
                self.store.publish_to(&mut engine);
                engine.update(0.0);
                engine.fit_camera_to_focus();

                if let Some(&first_id) = ids.first() {
                    log::info!("Loaded structure: {}", name);
                    log::info!(
                        "Stored {} original CA positions for alignment",
                        backbone_ca.len()
                    );
                    self.store.register_loaded(first_id, backbone_ca);
                }

                let mut orchestrator = Orchestrator::new();

                // Register entity triple buffers for each loaded entity
                for &eid in &ids {
                    orchestrator.register_entity(u64::from(eid.raw()));
                }
                // Set first entity as the active update target
                if let Some(&first_id) = ids.first() {
                    orchestrator.set_update_target(u64::from(first_id.raw()));
                }

                self.router.orchestrator = Some(orchestrator);
            }
            Err(e) => {
                log::error!("Failed to load structure '{}': {}", self.pdb_path, e);
                self.router.orchestrator = Some(Orchestrator::new());
            }
        }

        self.engine = Some(engine);

        if self.router.ensure_rosetta_session(&self.store) {
            log::info!("Rosetta session created, will receive score asynchronously");
        } else {
            log::warn!("Failed to create initial Rosetta session");
        }

        self.router.ui_dirty |= DirtyFlags::VIEW;
    }

    /// Shut down backends and scene processor.
    pub(crate) fn shutdown(&mut self) {
        self.router.shutdown();
        if let Some(engine) = &mut self.engine {
            engine.shutdown();
        }
    }
}

// ---------------------------------------------------------------------------
// Visualization helpers (free functions for split-borrow friendliness)
// ---------------------------------------------------------------------------

/// Update all drag/pull/band visualizations from router state.
fn update_all_visualizations(engine: &mut VisoEngine, router: &ActionRouter) {
    // Build band render infos using AtomRef-based BandInfo
    let mut band_infos = action_router::build_band_infos(router.active_bands());

    // Add band drag preview if active
    if let Some((start_residue, start_atom_name, target_pos)) = router.band_drag_preview(engine) {
        band_infos.push(BandInfo {
            anchor_a: AtomRef { residue: start_residue, atom_name: start_atom_name },
            anchor_b: BandTarget::Position(target_pos),
            is_pull: true,
            is_push: false,
            is_disabled: false,
            strength: 1.0,
            target_length: 0.0,
            band_type: None,
            from_script: false,
        });
    }

    // Update bands
    engine.update_bands(band_infos);

    // Update pull visualization
    if let Some((residue, atom_name, screen_target)) = router.pull_drag_info_for_viso() {
        engine.update_pull(Some(PullInfo {
            atom: AtomRef { residue, atom_name },
            screen_target,
        }));
    } else {
        engine.update_pull(None);
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------
fn main() {
    let default_filter = "info,wgpu_hal::vulkan::instance=off,naga=warn";
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| default_filter.to_string());
    let log_buffer = tee_logger::init(&filter);

    let input = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "1bfe".to_string());

    // Install signal handlers that kill ML worker process groups on
    // SIGINT/SIGTERM, preventing orphaned Python subprocesses.
    foldit_runner::install_cleanup_signal_handlers();

    log::info!("Foldit starting...");

    let pdb_path = match foldit::puzzle::resolve_structure_path(&input) {
        Ok(path) => path,
        Err(e) => {
            log::error!("{}", e);
            std::process::exit(1);
        }
    };

    log::info!("Loading structure from: {}", pdb_path);

    let app = App::new(pdb_path);
    window::run(app, foldit_gui::FrontendState::new(), log_buffer);
}
