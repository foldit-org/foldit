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
use foldit_frontend::DirtyFlags;
use foldit_runner::Orchestrator;
use foldit_rs::shared_state::SharedState;
use viso::renderer::molecular::band::BandRenderInfo;
use viso::engine::ProteinRenderEngine;
use viso::renderer::molecular::pull::PullRenderInfo;
use std::sync::Arc;
use winit::event::MouseScrollDelta;
use winit::keyboard::ModifiersState;
use winit::window::Window;

/// Main application state — thin glue connecting the render engine and action router.
pub(crate) struct App {
    engine: Option<ProteinRenderEngine>,
    router: ActionRouter,
    shared_state: SharedState,
    pdb_path: String,
    latest_score: Option<f64>,
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
            router: ActionRouter::new(),
            shared_state: SharedState::new(),
            pdb_path,
            latest_score: None,
        }
    }

    // ── Engine-only delegation (no router interaction) ──

    /// Sync the engine's renderers with the scene if dirty.
    fn sync_engine(&mut self) {
        if let Some(engine) = &mut self.engine {
            engine.sync_scene_to_renderers(None);
        }
    }

    /// Apply any completed scene from the background scene processor.
    fn apply_pending_scene(&mut self) {
        if let Some(engine) = &mut self.engine {
            engine.apply_pending_scene();
        }
    }

    pub(crate) fn resize(&mut self, width: u32, height: u32) {
        if let Some(engine) = &mut self.engine {
            engine.resize(width, height);
        }
    }

    pub(crate) fn set_surface_scale(&mut self, scale_factor: f64) {
        if let Some(ref mut engine) = self.engine {
            engine.context.render_scale = if scale_factor < 2.0 { 2 } else { 1 };
        }
    }

    pub(crate) fn update_camera_animation(&mut self, dt: f32) {
        if let Some(engine) = &mut self.engine {
            engine.camera_controller.update_animation(dt);
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
            for (_group_id, update) in updates {
                backend_handler::handle_backend_update(
                    engine,
                    &mut self.shared_state,
                    &mut self.router.orchestrator,
                    &mut self.router.ui_dirty,
                    &mut self.latest_score,
                    update,
                );
            }
        }
    }

    // ── Keybinding dispatch (engine + router) ──

    pub(crate) fn handle_keybinding(&mut self, key: winit::keyboard::KeyCode) -> bool {
        use viso::input::KeyAction;

        let Some(engine) = &mut self.engine else { return false };
        let key_str = format!("{key:?}");
        let Some(action) = engine.options.keybindings.lookup(&key_str) else {
            return false;
        };

        // Actions that need foldit-specific pre/post processing
        match action {
            KeyAction::ToggleTrajectory => {
                if engine.trajectory_player.is_some() {
                    engine.toggle_trajectory();
                } else if let Some(path) = action_router::trajectory_path_from_args() {
                    engine.load_trajectory(std::path::Path::new(&path));
                } else {
                    log::info!("No trajectory loaded. Pass --trajectory <path.dcd> to load one.");
                }
            }
            KeyAction::Cancel => {
                self.router.cancel_operations(engine, &mut self.shared_state);
            }
            KeyAction::CycleFocus | KeyAction::ResetFocus => {
                action.execute(engine);
                log::info!("Focus: {}", engine.scene.focus_description());
                self.router.update_rosetta_locks(engine, &self.shared_state);
                self.router.ui_dirty |= DirtyFlags::SELECTION | DirtyFlags::UI;
            }
            // All other actions: delegate entirely to viso
            other => other.execute(engine),
        }
        true
    }

    // ── Viewport input (from webview) ──

    pub(crate) fn handle_viewport_input(&mut self, input: foldit_frontend::ViewportInput) {
        use foldit_frontend::ViewportInput;
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
                engine.camera_controller.shift_pressed = shift;
                self.router.handle_native_cursor_moved(engine, x, y);
                self.router.handle_native_mouse_input(engine, winit_button, true);
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
                engine.camera_controller.shift_pressed = shift;
                self.router.handle_native_cursor_moved(engine, x, y);
                self.router.handle_native_mouse_input(engine, winit_button, false);
            }
            ViewportInput::PointerMove { x, y, shift, .. } => {
                engine.camera_controller.shift_pressed = shift;
                self.router.handle_native_cursor_moved(engine, x, y);
            }
            ViewportInput::Scroll { delta } => {
                engine.camera_controller.zoom(delta);
            }
            ViewportInput::Key { code, pressed } => {
                // Key strings from JS match the keybinding format directly
                if pressed {
                    use viso::input::KeyAction;
                    if let Some(action) = engine.options.keybindings.lookup(&code) {
                        match action {
                            KeyAction::ToggleTrajectory => {
                                if engine.trajectory_player.is_some() {
                                    engine.toggle_trajectory();
                                } else if let Some(path) = action_router::trajectory_path_from_args() {
                                    engine.load_trajectory(std::path::Path::new(&path));
                                }
                            }
                            KeyAction::Cancel => {
                                self.router.cancel_operations(engine, &mut self.shared_state);
                            }
                            KeyAction::CycleFocus | KeyAction::ResetFocus => {
                                action.execute(engine);
                                self.router.update_rosetta_locks(engine, &self.shared_state);
                                self.router.ui_dirty |= DirtyFlags::SELECTION | DirtyFlags::UI;
                            }
                            other => other.execute(engine),
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

    pub(crate) fn handle_trigger_action(&mut self, action: foldit_frontend::ActionId) {
        if let Some(engine) = &mut self.engine {
            if let Some(pa) = self.router.handle_trigger_action(engine, &mut self.shared_state, action) {
                self.handle_parameterized_action(pa);
            }
        }
    }

    pub(crate) fn handle_parameterized_action(
        &mut self,
        action: foldit_frontend::ParameterizedAction,
    ) {
        use foldit_frontend::ParameterizedAction;
        let Some(engine) = &mut self.engine else { return };

        match action {
            ParameterizedAction::LoadStructure { path } => {
                match action_router::load_file_as_entities(&path) {
                    Ok((entities, name)) => {
                        log::info!("Loaded structure via IPC: {}", name);
                        let backbone_ca = action_router::entities_backbone_ca(&entities);
                        let id = engine.load_entities(entities, &name, true);
                        self.shared_state.register_loaded(id, backbone_ca);
                        self.router.ui_dirty |= DirtyFlags::LOADING | DirtyFlags::ACTIONS | DirtyFlags::SCORE;
                    }
                    Err(e) => {
                        log::error!("Failed to load structure '{}': {}", path, e);
                    }
                }
                self.router.ui_dirty |= DirtyFlags::LOADING | DirtyFlags::SCORE | DirtyFlags::SELECTION;
            }
            ParameterizedAction::LoadPuzzle { puzzle_id } => {
                use viso::animation::AnimationAction;

                engine.scene.clear();
                self.router.reset_for_new_structure(&mut self.shared_state);

                match foldit_rs::puzzle::load_puzzle_structure(puzzle_id) {
                    Ok(puzzle_data) => {
                        if let Some(preset_name) = &puzzle_data.view_preset {
                            let presets_dir = std::path::Path::new("assets/view_presets");
                            engine.load_preset(preset_name, presets_dir);
                        }

                        let backbone_ca = action_router::entities_backbone_ca(&puzzle_data.entities);
                        let mut ss_override = puzzle_data.ss_override;
                        let id = engine.load_entities(
                            puzzle_data.entities,
                            &puzzle_data.name,
                            true,
                        );
                        if let Some(ss) = ss_override.take() {
                            if let Some(group) = engine.scene.group_mut(id) {
                                group.ss_override = Some(ss);
                            }
                        }
                        self.shared_state.register_loaded(id, backbone_ca);
                        engine.sync_scene_to_renderers(Some(AnimationAction::Load));
                    }
                    Err(e) => log::error!("Failed to load puzzle {}: {}", puzzle_id, e),
                }
                self.router.ui_dirty |= DirtyFlags::LOADING
                    | DirtyFlags::SCORE
                    | DirtyFlags::SELECTION
                    | DirtyFlags::ACTIONS;
            }
            ParameterizedAction::CreateBand { .. } => {
                log::info!("CreateBand via IPC not yet wired");
            }
            ParameterizedAction::RemoveBand { .. } => {
                log::info!("RemoveBand via IPC not yet wired");
            }
            ParameterizedAction::SetViewOptions { options } => {
                match serde_json::from_value::<viso::options::Options>(options) {
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
                    &mut self.router, &self.shared_state, engine,
                    temperature, num_sequences,
                );
            }
            ParameterizedAction::RunStructureDesign { length, num_steps, contig } => {
                Self::run_structure_design(
                    &mut self.router, &self.shared_state, engine,
                    &length, num_steps, contig,
                );
            }
            ParameterizedAction::RunPrediction { entity_ids } => {
                Self::run_prediction_for_entities(
                    &mut self.router, &self.shared_state, engine,
                    &entity_ids,
                );
            }
        }
    }

    // ── ML operations (parameterized) ──

    /// Build entity context data from a pre-collected slice of entities.
    fn build_entity_context(
        entities: &[foldit_conv::coords::entity::MoleculeEntity],
        shared: &SharedState,
        group_id: viso::scene::GroupId,
    ) -> Option<foldit_runner::orchestrator::EntityContextData> {
        use foldit_conv::coords::entity::MoleculeType;
        use foldit_runner::orchestrator::{EntityContextData, EntityInfoData};

        let assembly_coords = crate::backend_handler::entities_to_assembly_bytes(entities)?;
        let meta = shared.entity_meta(group_id);

        let entity_info = entities.iter().map(|e| {
            let mol_str = match e.molecule_type {
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
            let mut res_nums: Vec<i32> = e.coords.res_nums.iter().copied().collect();
            res_nums.dedup();
            let residue_count = res_nums.len() as u32;

            let chain_id = e.coords.chain_ids.first()
                .map(|&c| String::from(c as char))
                .unwrap_or_default();

            let is_protein = e.molecule_type == MoleculeType::Protein;
            let is_ambient = matches!(e.molecule_type,
                MoleculeType::Water | MoleculeType::Ion | MoleculeType::Solvent);

            EntityInfoData {
                entity_id: e.entity_id,
                molecule_type: mol_str.to_string(),
                chain_id,
                residue_count,
                designable: is_protein && meta.map_or(true, |m| m.role.designable),
                foldable: is_protein && meta.map_or(true, |m| m.role.foldable),
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
        shared: &SharedState,
        engine: &mut ProteinRenderEngine,
        temperature: f32,
        num_sequences: u32,
    ) {
        use foldit_runner::orchestrator::{EntityContextData, EntityInfoData, EntityId, OpType};
        use foldit_conv::coords::entity::MoleculeType;
        use viso::scene::Focus;

        let focus = *engine.scene.focus();
        log::info!("MPNN: focus = {:?}", focus);

        // Collect entities based on focus (single entity, group, or session fallback).
        let fallback = shared.loaded_entity().or(shared.lock_target(&focus));
        let Some((target_id, entities)) =
            backend_handler::collect_ml_entities(engine, &focus, fallback)
        else {
            log::warn!("No structure available for sequence design");
            return;
        };

        let Some(assembly_bytes) = backend_handler::entities_to_assembly_bytes(&entities) else {
            log::warn!("No coords available for sequence design");
            return;
        };

        let total_atoms: usize = entities.iter().map(|e| e.coords.num_atoms).sum();
        let group_name = engine.scene.group(target_id).map(|g| g.name().to_string());
        log::info!(
            "MPNN: target_id={:?}, group='{}', {} entities, {} total atoms, {} assembly bytes",
            target_id,
            group_name.as_deref().unwrap_or("?"),
            entities.len(),
            total_atoms,
            assembly_bytes.len(),
        );

        // Role validation: target must be designable
        if let Some(meta) = shared.entity_meta(target_id) {
            if meta.role.ambient {
                log::warn!("Cannot run sequence design on ambient entity group (water/ion)");
                return;
            }
            if !meta.role.designable {
                log::warn!("Target entity group is not designable");
                return;
            }
        }

        // Build focus-aware entity context:
        // - Focus::Entity → only that entity's chains are designable
        // - Focus::Group/Session → all protein chains are designable
        let focused_entity_id = match focus {
            Focus::Entity(eid) => Some(eid),
            _ => None,
        };

        let entity_info: Vec<EntityInfoData> = entities.iter().map(|e| {
            let is_protein = e.molecule_type == MoleculeType::Protein;
            let is_ambient = matches!(e.molecule_type,
                MoleculeType::Water | MoleculeType::Ion | MoleculeType::Solvent);
            let mol_str = match e.molecule_type {
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
            let chain_id = e.coords.chain_ids.first()
                .map(|&c| String::from(c as char))
                .unwrap_or_default();

            let mut res_nums: Vec<i32> = e.coords.res_nums.iter().copied().collect();
            res_nums.dedup();

            // Focus-aware designability: when focusing on a specific entity,
            // only that entity's protein chains are designable
            let designable = if let Some(focused_eid) = focused_entity_id {
                is_protein && e.entity_id == focused_eid
            } else {
                is_protein
            };
            let fixed = if let Some(focused_eid) = focused_entity_id {
                // Other proteins become fixed context, non-protein non-ambient are fixed
                (is_protein && e.entity_id != focused_eid) || (!is_protein && !is_ambient)
            } else {
                !is_protein && !is_ambient
            };

            EntityInfoData {
                entity_id: e.entity_id,
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

        if orch.is_locked(EntityId(target_id.0)) {
            let op = orch.get_op_type(EntityId(target_id.0));
            log::warn!("Structure is locked by {:?}, cannot start sequence design", op);
            return;
        }

        if orch.try_lock(EntityId(target_id.0), OpType::MLSequenceDesign).is_none() {
            log::warn!("Failed to acquire lock for sequence design");
            return;
        }

        let target_name = engine
            .scene.group(target_id)
            .map(|g| g.name().to_string())
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
            orch.unlock(EntityId(target_id.0));
            return;
        }

        router.ui_dirty |= DirtyFlags::ACTIONS | DirtyFlags::LOADING;
    }

    fn run_structure_design(
        router: &mut ActionRouter,
        shared: &SharedState,
        engine: &ProteinRenderEngine,
        length: &str,
        num_steps: u32,
        contig: Option<String>,
    ) {
        use foldit_runner::orchestrator::{EntityId, OpType};

        // For structure design, always use the loaded protein as context —
        // the user's contig references the original protein's chains/residues,
        // not whatever group happens to be focused.
        let loaded_group = shared.loaded_entity();
        let Some((target_id, entities)) = loaded_group
            .and_then(|gid| engine.scene.group(gid))
            .map(|group| (group.id, group.entities().to_vec()))
        else {
            log::warn!("No structure available for structure design");
            return;
        };

        // Role validation: target must be foldable
        if let Some(meta) = shared.entity_meta(target_id) {
            if meta.role.ambient {
                log::warn!("Cannot run structure design on ambient entity group (water/ion)");
                return;
            }
            if !meta.role.foldable {
                log::warn!("Target entity group is not foldable");
                return;
            }
        }

        // Filter out ambient entities (water, ion, solvent) — they share chain/residue
        // IDs with protein atoms and confuse the foundry parser.
        use foldit_conv::coords::entity::MoleculeType;
        let entities: Vec<_> = entities.into_iter().filter(|e| {
            !matches!(e.molecule_type, MoleculeType::Water | MoleculeType::Ion | MoleculeType::Solvent)
        }).collect();

        let total_atoms: usize = entities.iter().map(|e| e.coords.num_atoms).sum();
        log::info!(
            "RFD3 structure design: source={:?}, {} entities, {} total atoms",
            target_id, entities.len(), total_atoms,
        );

        // Build entity context from collected entities
        let entity_context = Self::build_entity_context(&entities, shared, target_id);

        let Some(ref mut orch) = router.orchestrator else {
            log::warn!("Orchestrator not initialized");
            return;
        };

        if orch.is_locked(EntityId(target_id.0)) {
            let op = orch.get_op_type(EntityId(target_id.0));
            log::warn!("Structure is locked by {:?}, cannot start structure design", op);
            return;
        }

        if orch.try_lock(EntityId(target_id.0), OpType::MLStructureDesign).is_none() {
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
            orch.unlock(EntityId(target_id.0));
            return;
        }

        router.ui_dirty |= DirtyFlags::ACTIONS | DirtyFlags::LOADING;
    }

    fn run_prediction_for_entities(
        router: &mut ActionRouter,
        shared: &SharedState,
        engine: &mut ProteinRenderEngine,
        entity_ids: &[u32],
    ) {
        use foldit_runner::orchestrator::{EntityId, OpType};

        // Collect entities matching the given IDs from all groups
        let mut collected = Vec::new();
        let mut target_id = None;
        for gid in engine.scene.group_ids() {
            if let Some(group) = engine.scene.group(gid) {
                for entity in group.entities() {
                    if entity_ids.contains(&entity.entity_id) {
                        collected.push(entity.clone());
                        if target_id.is_none() {
                            target_id = Some(gid);
                        }
                    }
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

        let total_atoms: usize = collected.iter().map(|e| e.coords.num_atoms).sum();
        log::info!(
            "RF3 prediction (entity picker): {} entities, {} total atoms",
            collected.len(), total_atoms,
        );

        let entity_context = Self::build_entity_context(&collected, shared, target_id);

        let Some(ref mut orch) = router.orchestrator else {
            log::warn!("Orchestrator not initialized");
            return;
        };

        if orch.is_locked(EntityId(target_id.0)) {
            let op = orch.get_op_type(EntityId(target_id.0));
            log::warn!("Structure is locked by {:?}, cannot start prediction", op);
            return;
        }

        orch.stop_rosetta();
        orch.clear_session();

        if orch.try_lock(EntityId(target_id.0), OpType::MLPredict).is_none() {
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
            orch.unlock(EntityId(target_id.0));
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
            self.router.handle_native_mouse_input(engine, button, pressed);
            update_all_visualizations(engine, &self.router);
        }
    }

    pub(crate) fn handle_native_cursor_moved(&mut self, x: f32, y: f32) {
        if let Some(engine) = &mut self.engine {
            self.router.handle_native_cursor_moved(engine, x, y);
            update_all_visualizations(engine, &self.router);
        }
    }

    pub(crate) fn handle_native_mouse_wheel(&mut self, delta: MouseScrollDelta) {
        if let Some(engine) = &mut self.engine {
            match delta {
                MouseScrollDelta::LineDelta(_, y) => engine.camera_controller.zoom(y),
                MouseScrollDelta::PixelDelta(pos) => engine.camera_controller.zoom(pos.y as f32 * 0.01),
            }
        }
    }

    pub(crate) fn handle_native_modifiers(&mut self, state: ModifiersState) {
        if let Some(engine) = &mut self.engine {
            engine.camera_controller.shift_pressed = state.shift_key();
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

    pub(crate) fn populate_frontend(&mut self, frontend: &mut foldit_frontend::FrontendState) {
        let engine = match &self.engine {
            Some(e) => e,
            None => return,
        };

        // FPS and selected count change every frame — always push them
        frontend.set_fps(engine.frame_timing.fps());
        frontend.ui.selected_count = engine.picking.selected_residues.len();

        let app_dirty = self.router.take_ui_dirty();
        if app_dirty.is_empty() {
            return;
        }

        if app_dirty.contains(DirtyFlags::SCORE) {
            if let Some(score) = self.latest_score {
                frontend.set_score(score, false);
            }
        }
        if app_dirty.contains(DirtyFlags::ACTIONS) {
            frontend.set_actions(action_router::build_actions_list(&self.router.orchestrator));
        }
        if app_dirty.contains(DirtyFlags::LOADING) {
            frontend.set_loading_progress(None);
        }
        if app_dirty.contains(DirtyFlags::VIEW) {
            frontend.view.options = serde_json::to_value(&engine.options).unwrap_or_default();

            // Schema is static — only set once
            if frontend.view.options_schema.is_null() {
                frontend.view.options_schema =
                    serde_json::to_value(viso::options::Options::json_schema())
                        .unwrap_or_default();
            }

            let presets_dir = std::path::Path::new("assets/view_presets");
            frontend.view.available_presets =
                viso::options::Options::list_presets(presets_dir);
            frontend.view.active_preset = engine.active_preset.clone();
        }
        if app_dirty.contains(DirtyFlags::SELECTION) {
            frontend.mark_dirty(DirtyFlags::SELECTION);
        }
        if app_dirty.contains(DirtyFlags::UI) {
            frontend.mark_dirty(DirtyFlags::UI);
        }
        if app_dirty.contains(DirtyFlags::LOADING) || app_dirty.contains(DirtyFlags::SELECTION) {
            use foldit_conv::coords::entity::MoleculeType;
            let mut scene_entities = Vec::new();
            for gid in engine.scene.group_ids() {
                if let Some(group) = engine.scene.group(gid) {
                    for entity in group.entities() {
                        let mol_str = match entity.molecule_type {
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
                        scene_entities.push(foldit_frontend::SceneEntityInfo {
                            entity_id: entity.entity_id,
                            label: entity.label(),
                            molecule_type: mol_str.to_string(),
                            atom_count: entity.coords.num_atoms,
                            residue_count: entity.residue_count(),
                        });
                    }
                }
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
        let mut engine = pollster::block_on(ProteinRenderEngine::new_with_path(
            window.clone(),
            (size.width, size.height),
            scale,
            &self.pdb_path,
        ));

        // Load default view preset if available
        let presets_dir = std::path::Path::new("assets/view_presets");
        engine.load_preset("default", presets_dir);

        engine.context.render_scale = if scale < 2.0 { 2 } else { 1 };

        if let Some(&first_id) = engine.scene.group_ids().first() {
            let name = engine.scene.group(first_id).map(|g| g.name()).unwrap_or("?");
            log::info!("Loaded structure: {}", name);
            let backbone_ca = engine
                .scene.group(first_id)
                .map(|g| action_router::entities_backbone_ca(g.entities()))
                .unwrap_or_default();
            log::info!(
                "Stored {} original CA positions for alignment",
                backbone_ca.len()
            );
            self.shared_state.register_loaded(first_id, backbone_ca);
        } else {
            log::error!("Engine has no groups after loading '{}'", self.pdb_path);
        }

        let mut orchestrator = Orchestrator::new();

        // Register entity triple buffers for each loaded group
        let group_ids = engine.scene.group_ids();
        for gid in &group_ids {
            let reader = orchestrator.register_entity(gid.0);
            self.shared_state.register_entity(*gid, reader);
        }
        // Set first group as the active update target
        if let Some(first_id) = group_ids.first() {
            orchestrator.set_update_target(first_id.0);
        }

        self.router.orchestrator = Some(orchestrator);

        self.engine = Some(engine);

        if self.router.ensure_rosetta_session(self.engine.as_mut().unwrap()) {
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
            engine.scene_processor.shutdown();
        }
    }
}

// ---------------------------------------------------------------------------
// Visualization helpers (free functions for split-borrow friendliness)
// ---------------------------------------------------------------------------

/// Update all drag/pull/band visualizations from router state.
fn update_all_visualizations(engine: &mut ProteinRenderEngine, router: &ActionRouter) {
    // Build band render infos
    let mut band_infos = action_router::build_band_render_infos(engine, router.active_bands());

    // Add band drag preview if active
    if let Some((start_pos, target_pos, residue_idx)) = router.band_drag_preview(engine) {
        band_infos.push(BandRenderInfo {
            endpoint_a: start_pos,
            endpoint_b: target_pos,
            is_pull: true,
            residue_idx,
            is_space_pull: false,
            ..Default::default()
        });
    }

    // Update bands
    if band_infos.is_empty() {
        engine.band_renderer.clear();
    } else {
        engine.update_bands(&band_infos);
    }

    // Update pull visualization
    if let Some((atom_pos, target_pos, residue_idx)) = router.pull_drag_info() {
        engine.update_pull(Some(&PullRenderInfo {
            atom_pos,
            target_pos,
            residue_idx,
        }));
    } else {
        engine.pull_renderer.clear();
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
    window::run(app, foldit_frontend::FrontendState::new(), log_buffer);
}
