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
mod backend_handler;
mod tee_logger;
mod window;

use action_router::ActionRouter;
use foldit_gui::{DirtyFlags, ScoringMode};
use foldit_runner::Orchestrator;
use foldit::entity_store::{EntityStore, EntityOrigin, EntityRole};
use foldit::shared_state::SharedState;
use viso::{BandInfo, BandTarget, AtomRef, Focus, InputEvent, InputProcessor, PullInfo, VisoEngine, VisoCommand};
use std::sync::Arc;
use winit::event::MouseScrollDelta;
use winit::keyboard::ModifiersState;
use winit::window::Window;

/// Main application state — thin glue connecting the render engine and action router.
pub(crate) struct App {
    engine: Option<VisoEngine>,
    input: InputProcessor,
    store: EntityStore,
    router: ActionRouter,
    shared_state: SharedState,
    pdb_path: String,
    latest_score: Option<f64>,
    /// `Game` for tutorial/campaign/server puzzles, `Scientist` for CLI /
    /// drag-drop loads. Drives which score representation reaches the GUI.
    scoring_mode: ScoringMode,
    /// Puzzle metadata from the active toml. Zero/empty in Scientist mode.
    puzzle_id: u32,
    puzzle_title: String,
    starting_score: f64,
    target_score: f64,
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
            shared_state: SharedState::new(),
            pdb_path,
            latest_score: None,
            // CLI bootstrap defaults to scientist; LoadPuzzle flips to Game
            // when an intro/campaign puzzle is loaded.
            scoring_mode: ScoringMode::Scientist,
            puzzle_id: 0,
            puzzle_title: String::new(),
            starting_score: 0.0,
            target_score: 0.0,
        }
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
        if let Some(ref mut orch) = self.router.orchestrator {
            orch.pump_updates();
        }

        // 2. Read latest from all entity buffers
        let updates = self.shared_state.drain_updates();
        if updates.is_empty() {
            return;
        }

        // 3. Process each update via backend_handler free functions
        if let Some(engine) = &mut self.engine {
            for (_entity_id, update) in updates {
                backend_handler::handle_backend_update(
                    engine,
                    &mut self.store,
                    &mut self.shared_state,
                    &mut self.router.orchestrator,
                    &mut self.router.ui_dirty,
                    &mut self.router.pending_prediction_reference,
                    &mut self.latest_score,
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
                self.router.cancel_operations(engine, &mut self.store, &mut self.shared_state);
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
                self.router.handle_native_cursor_moved(engine, &self.input, x, y);
                self.router.handle_native_mouse_input(engine, &mut self.input, &self.store, winit_button, true);
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
                self.router.handle_native_cursor_moved(engine, &self.input, x, y);
                self.router.handle_native_mouse_input(engine, &mut self.input, &self.store, winit_button, false);
            }
            ViewportInput::PointerMove { x, y, shift, .. } => {
                if let Some(cmd) = self.input.handle_event(InputEvent::ModifiersChanged { shift }, engine.hovered_target()) {
                    engine.execute(cmd);
                }
                engine.set_cursor_pos(x, y);
                if let Some(cmd) = self.input.handle_event(InputEvent::CursorMoved { x, y }, engine.hovered_target()) {
                    engine.execute(cmd);
                }
                self.router.handle_native_cursor_moved(engine, &self.input, x, y);
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
                                self.router.cancel_operations(engine, &mut self.store, &mut self.shared_state);
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
        if let Some(engine) = &mut self.engine {
            if let Some(pa) = self.router.handle_trigger_action(engine, &self.store, action) {
                self.handle_parameterized_action(pa);
            }
        }
    }

    pub(crate) fn handle_parameterized_action(
        &mut self,
        action: foldit_gui::ParameterizedAction,
    ) {
        use foldit_gui::ParameterizedAction;
        let title = self.structure_title();
        let Some(engine) = &mut self.engine else { return };

        match action {
            ParameterizedAction::LoadStructure { path } => {
                match action_router::load_file_as_entities(&path) {
                    Ok((entities, name)) => {
                        log::info!("Loaded structure via IPC: {}", name);
                        let backbone_ca = action_router::entities_backbone_ca(&entities);
                        // Insert into store first, then push to viso
                        let mut ids = Vec::new();
                        for entity in entities {
                            let id = self.store.insert(
                                entity,
                                name.clone(),
                                EntityOrigin::Loaded,
                                EntityRole { foldable: true, designable: true, ambient: false },
                            );
                            ids.push(id);
                        }
                        // Push to viso
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
                self.store.clear();
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

                        let mut ids = Vec::new();
                        for entity in puzzle_data.entities {
                            let id = self.store.insert(
                                entity,
                                title.clone(),
                                EntityOrigin::Loaded,
                                EntityRole { foldable: true, designable: true, ambient: false },
                            );
                            ids.push(id);
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
                                engine.set_ss_override(first_id, ss);
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
                    &mut self.router, &self.store, engine,
                    temperature, num_sequences,
                );
            }
            ParameterizedAction::RunStructureDesign { length, num_steps, contig } => {
                Self::run_structure_design(
                    &mut self.router, &self.store, engine,
                    &length, num_steps, contig,
                );
            }
            ParameterizedAction::RunPrediction { entity_ids } => {
                Self::run_prediction_for_entities(
                    &mut self.router, &self.store, engine,
                    &entity_ids,
                );
            }
        }
    }

    // ── ML operations (parameterized) ──

    /// Build entity context data from a pre-collected slice of entities.
    fn build_entity_context(
        entities: &[molex::MoleculeEntity],
        store: &EntityStore,
        entity_id: u32,
    ) -> Option<foldit_runner::orchestrator::EntityContextData> {
        use molex::MoleculeType;
        use foldit_runner::orchestrator::{EntityContextData, EntityInfoData};

        let assembly_coords = crate::backend_handler::entities_to_assembly_bytes(entities)?;
        let meta = store.entity_meta(entity_id);

        let entity_info = entities.iter().map(|e| {
            let mol_str = match e.molecule_type() {
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

            // Count residues (unique res_nums)
            let coords = e.to_coords();
            let mut res_nums: Vec<i32> = coords.res_nums.iter().copied().collect();
            res_nums.dedup();
            let residue_count = res_nums.len() as u32;

            let chain_id = coords.chain_ids.first()
                .map(|&c| String::from(c as char))
                .unwrap_or_default();

            let is_protein = e.molecule_type() == MoleculeType::Protein;
            let is_ambient = matches!(e.molecule_type(),
                MoleculeType::Water | MoleculeType::Ion | MoleculeType::Solvent);

            EntityInfoData {
                entity_id: e.id().raw(),
                molecule_type: mol_str.to_string(),
                chain_id,
                residue_count,
                designable: is_protein && meta.map_or(true, |(_, role)| role.designable),
                foldable: is_protein && meta.map_or(true, |(_, role)| role.foldable),
                fixed: !is_protein && !is_ambient,
            }
        }).collect();

        Some(EntityContextData {
            entities: entity_info,
            assembly_coords,
        })
    }

    fn run_sequence_design(
        router: &mut ActionRouter,
        store: &EntityStore,
        engine: &mut VisoEngine,
        temperature: f32,
        num_sequences: u32,
    ) {
        use foldit_runner::orchestrator::{EntityContextData, EntityInfoData, EntityId, OpType};
        use molex::MoleculeType;

        let focus = engine.focus();
        log::info!("MPNN: focus = {:?}", focus);

        // Collect entities based on focus (single entity, group, or session fallback).
        let fallback = store.loaded_entity().or_else(|| SharedState::lock_target(&focus, store.loaded_entity()));
        let Some((target_id, entities)) =
            store.collect_ml_entities(&focus, fallback)
        else {
            log::warn!("No structure available for sequence design");
            return;
        };

        let Some(assembly_bytes) = backend_handler::entities_to_assembly_bytes(&entities) else {
            log::warn!("No coords available for sequence design");
            return;
        };

        let total_atoms: usize = entities.iter().map(|e| e.atom_count()).sum();
        let entity_name = store.get(target_id).map(|te| te.name.clone());
        log::info!(
            "MPNN: target_id={}, entity='{}', {} entities, {} total atoms, {} assembly bytes",
            target_id,
            entity_name.as_deref().unwrap_or("?"),
            entities.len(),
            total_atoms,
            assembly_bytes.len(),
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

        // Build focus-aware entity context
        let focused_entity_id: Option<u32> = match focus {
            Focus::Entity(eid) => Some(eid.raw()),
            _ => None,
        };

        let entity_info: Vec<EntityInfoData> = entities.iter().map(|e| {
            let is_protein = e.molecule_type() == MoleculeType::Protein;
            let is_ambient = matches!(e.molecule_type(),
                MoleculeType::Water | MoleculeType::Ion | MoleculeType::Solvent);
            let mol_str = match e.molecule_type() {
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
            let coords = e.to_coords();
            let chain_id = coords.chain_ids.first()
                .map(|&c| String::from(c as char))
                .unwrap_or_default();

            let mut res_nums: Vec<i32> = coords.res_nums.iter().copied().collect();
            res_nums.dedup();

            let raw_id = e.id().raw();
            let designable = if let Some(focused_eid) = focused_entity_id {
                is_protein && raw_id == focused_eid
            } else {
                is_protein
            };
            let fixed = if let Some(focused_eid) = focused_entity_id {
                (is_protein && raw_id != focused_eid) || (!is_protein && !is_ambient)
            } else {
                !is_protein && !is_ambient
            };

            EntityInfoData {
                entity_id: raw_id,
                molecule_type: mol_str.to_string(),
                chain_id,
                residue_count: res_nums.len() as u32,
                designable,
                foldable: is_protein,
                fixed,
            }
        }).collect();

        let designed: Vec<&str> = entity_info.iter()
            .filter(|e| e.designable)
            .map(|e| e.chain_id.as_str())
            .collect();
        let fixed: Vec<&str> = entity_info.iter()
            .filter(|e| e.fixed)
            .map(|e| e.chain_id.as_str())
            .collect();
        log::info!(
            "MPNN entity context: designed={:?}, fixed={:?}",
            designed, fixed,
        );

        let entity_context = Some(EntityContextData {
            entities: entity_info,
            assembly_coords: assembly_bytes.clone(),
        });

        let Some(ref mut orch) = router.orchestrator else {
            log::warn!("Orchestrator not initialized");
            return;
        };

        if orch.is_locked(EntityId(u64::from(target_id))) {
            let op = orch.get_op_type(EntityId(u64::from(target_id)));
            log::warn!("Structure is locked by {:?}, cannot start sequence design", op);
            return;
        }

        if orch.try_lock(EntityId(u64::from(target_id)), OpType::MLSequenceDesign).is_none() {
            log::warn!("Failed to acquire lock for sequence design");
            return;
        }

        let target_name = store.get(target_id)
            .map(|te| te.name.clone())
            .unwrap_or_default();

        log::info!(
            "Starting sequence design on '{}' ({} bytes, T={}, n={})...",
            target_name, assembly_bytes.len(), temperature, num_sequences
        );

        let result = if let Some(ctx) = entity_context {
            orch.design_sequence_with_context(assembly_bytes, temperature, num_sequences, ctx)
        } else {
            orch.design_sequence(assembly_bytes, temperature, num_sequences)
        };

        if let Err(e) = result {
            log::error!("Failed to submit sequence design: {}", e);
            orch.unlock(EntityId(u64::from(target_id)));
            return;
        }

        router.ui_dirty |= DirtyFlags::ACTIONS | DirtyFlags::LOADING;
    }

    fn run_structure_design(
        router: &mut ActionRouter,
        store: &EntityStore,
        _engine: &VisoEngine,
        length: &str,
        num_steps: u32,
        contig: Option<String>,
    ) {
        use foldit_runner::orchestrator::{EntityId, OpType};

        let Some(target_id) = store.loaded_entity() else {
            log::warn!("No structure available for structure design");
            return;
        };
        let Some(te) = store.get(target_id) else {
            log::warn!("No structure available for structure design");
            return;
        };
        let entity = te.entity.clone();

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
        let entities: Vec<_> = vec![entity].into_iter().filter(|e| {
            !matches!(e.molecule_type(), MoleculeType::Water | MoleculeType::Ion | MoleculeType::Solvent)
        }).collect();

        let total_atoms: usize = entities.iter().map(|e| e.atom_count()).sum();
        log::info!(
            "RFD3 structure design: source={:?}, {} entities, {} total atoms",
            target_id, entities.len(), total_atoms,
        );

        let entity_context = Self::build_entity_context(&entities, store, target_id);

        let Some(ref mut orch) = router.orchestrator else {
            log::warn!("Orchestrator not initialized");
            return;
        };

        if orch.is_locked(EntityId(u64::from(target_id))) {
            let op = orch.get_op_type(EntityId(u64::from(target_id)));
            log::warn!("Structure is locked by {:?}, cannot start structure design", op);
            return;
        }

        if orch.try_lock(EntityId(u64::from(target_id)), OpType::MLStructureDesign).is_none() {
            log::warn!("Failed to acquire lock for structure design");
            return;
        }

        let contig_str = contig.unwrap_or_default();
        log::info!("Starting structure design (length={}, steps={}, contig='{}')...", length, num_steps, contig_str);

        let result = if let Some(ctx) = entity_context {
            log::info!(
                "Passing assembly context ({} entities, {} bytes)",
                ctx.entities.len(),
                ctx.assembly_coords.len(),
            );
            orch.design_structure_with_context(length.to_string(), num_steps, contig_str, ctx)
        } else {
            orch.design_structure(length.to_string(), num_steps, contig_str)
        };

        if let Err(e) = result {
            log::error!("Failed to submit structure design: {}", e);
            orch.unlock(EntityId(u64::from(target_id)));
            return;
        }

        router.ui_dirty |= DirtyFlags::ACTIONS | DirtyFlags::LOADING;
    }

    fn run_prediction_for_entities(
        router: &mut ActionRouter,
        store: &EntityStore,
        _engine: &mut VisoEngine,
        entity_ids: &[u32],
    ) {
        use foldit_runner::orchestrator::{EntityId, OpType};

        // Collect entities matching the given IDs
        let mut collected = Vec::new();
        let mut target_id = None;
        for id in entity_ids {
            if let Some(te) = store.get(*id) {
                collected.push(te.entity.clone());
                if target_id.is_none() {
                    target_id = Some(*id);
                }
            }
        }

        let Some(target_id) = target_id else {
            log::warn!("No matching entities found for prediction");
            return;
        };

        if collected.is_empty() {
            log::warn!("No entities selected for prediction");
            return;
        }

        let chains = action_router::extract_chains_from_entities_pub(&collected);
        if chains.is_empty() {
            log::warn!("No protein chains found in selected entities");
            return;
        }

        let total_atoms: usize = collected.iter().map(|e| e.atom_count()).sum();
        log::info!(
            "RF3 prediction (entity picker): {} entities, {} total atoms",
            collected.len(), total_atoms,
        );

        // Snapshot the submitted entities' CAs so the predicted result
        // can be aligned back to the same frame. RF3 emits chains in
        // the order they were submitted, matching `ca_positions`.
        router.pending_prediction_reference =
            Some(molex::ops::codec::ca_positions(&collected));

        let entity_context = Self::build_entity_context(&collected, store, target_id);

        let Some(ref mut orch) = router.orchestrator else {
            log::warn!("Orchestrator not initialized");
            return;
        };

        if orch.is_locked(EntityId(u64::from(target_id))) {
            let op = orch.get_op_type(EntityId(u64::from(target_id)));
            log::warn!("Structure is locked by {:?}, cannot start prediction", op);
            return;
        }

        orch.stop_rosetta();
        orch.clear_session();

        if orch.try_lock(EntityId(u64::from(target_id)), OpType::MLPredict).is_none() {
            log::warn!("Failed to acquire lock for prediction");
            return;
        }

        let total_residues: usize = chains.iter().map(|(_, s)| s.len()).sum();
        log::info!("Starting RoseTTAFold3 prediction for {} residues...", total_residues);

        let result = if let Some(ctx) = entity_context {
            orch.predict_with_context(None, chains, 3, ctx)
        } else {
            orch.predict(None, chains, 3)
        };

        if let Err(e) = result {
            log::error!("Failed to submit prediction task: {}", e);
            orch.unlock(EntityId(u64::from(target_id)));
            return;
        }

        router.ui_dirty |= DirtyFlags::ACTIONS | DirtyFlags::LOADING;
    }

    // ── Native input (when webview is not ready) ──

    pub(crate) fn handle_native_mouse_input(
        &mut self,
        button: winit::event::MouseButton,
        pressed: bool,
    ) {
        if let Some(engine) = &mut self.engine {
            self.router.handle_native_mouse_input(engine, &mut self.input, &self.store, button, pressed);
            update_all_visualizations(engine, &self.router);
        }
    }

    pub(crate) fn handle_native_cursor_moved(&mut self, x: f32, y: f32) {
        if let Some(engine) = &mut self.engine {
            self.router.handle_native_cursor_moved(engine, &self.input, x, y);
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
            if let Some(score) = self.latest_score {
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
            for (_, te) in self.store.iter() {
                let entity = &te.entity;
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
        }
    }

    // ── Complex methods (touch both engine and router) ──

    /// Initialize domain state once a window is available.
    pub(crate) fn initialize_with_window(&mut self, window: Arc<Window>) {
        let size = window.inner_size();
        let scale = window.scale_factor();
        log::info!(
            "initialize_with_window: inner_size={}x{}, scale_factor={}",
            size.width,
            size.height,
            scale
        );

        // Create render context and empty engine
        let context = match pollster::block_on(viso::RenderContext::new(window.clone(), (size.width, size.height))) {
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

        // Parse entities from file
        match action_router::load_file_as_entities(&self.pdb_path) {
            Ok((entities, name)) => {
                let backbone_ca = action_router::entities_backbone_ca(&entities);

                // Insert into store (assigns IDs)
                let mut ids = Vec::new();
                for entity in entities {
                    let id = self.store.insert(
                        entity,
                        name.clone(),
                        EntityOrigin::Loaded,
                        EntityRole { foldable: true, designable: true, ambient: false },
                    );
                    ids.push(id);
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
                    let reader = orchestrator.register_entity(u64::from(eid));
                    self.shared_state.register_entity(eid, reader);
                }
                // Set first entity as the active update target
                if let Some(&first_id) = ids.first() {
                    orchestrator.set_update_target(u64::from(first_id));
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
    if let Some((residue, screen_target)) = router.pull_drag_info_for_viso() {
        engine.update_pull(Some(PullInfo {
            atom: AtomRef { residue, atom_name: "CA".to_string() },
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
    let log_buffer = tee_logger::init(
        "info,wgpu_hal::vulkan::instance=off,naga=warn",
    );

    let input = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "1bfe".to_string());

    // Install signal handlers that kill ML worker process groups on
    // SIGINT/SIGTERM, preventing orphaned Python subprocesses.
    foldit_runner::install_cleanup_signal_handlers();

    log::info!("Foldit starting...");

    let pdb_path = match action_router::resolve_structure_path(&input) {
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
