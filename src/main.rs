//! Foldit-RS: A reimagined Foldit
//!
//! Decoupled architecture with GUI, render engine, and backends
//! for Rosetta and ML-powered structure prediction and design.
//!
//! Controls:
//!   W - Wiggle (Rosetta minimize, toggle on/off)
//!   S - Shake (Rosetta repack sidechains, toggle on/off)
//!   P - Predict (SimpleFold structure prediction)
//!   M - MPNN (design sequence for structure)
//!   R - RFDiffusion3 (design new structure)
//!   I - Toggle water and ion visibility
//!   Q - Recenter camera on focused entity
//!   T - Toggle trajectory playback (load with --trajectory <path.dcd>)
//!   Tab - Cycle focus (Session -> Structure 1 -> ... -> Session)
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
use viso::engine::core::ProteinRenderEngine;
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
            engine.set_scale_factor(scale_factor);
            engine.context.set_surface_scale(scale_factor);
        }
    }

    pub(crate) fn update_camera_animation(&mut self, dt: f32) {
        if let Some(engine) = &mut self.engine {
            engine.update_camera_animation(dt);
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
        let Some(engine) = &mut self.engine else { return false };
        let key_str = action_router::key_code_to_string(key);
        let action = engine
            .options()
            .keybindings
            .lookup(&key_str)
            .map(|s| s.to_string());
        if let Some(action) = action {
            match action.as_str() {
                "recenter_camera" => {
                    engine.fit_camera_to_focus();
                    log::info!("Recentered on {}", engine.scene.focus_description());
                }
                "toggle_trajectory" => {
                    if engine.has_trajectory() {
                        engine.toggle_trajectory();
                    } else if let Some(path) = action_router::trajectory_path_from_args() {
                        engine.load_trajectory(std::path::Path::new(&path));
                    } else {
                        log::info!("No trajectory loaded. Pass --trajectory <path.dcd> to load one.");
                    }
                }
                "toggle_ions" => {
                    engine.toggle_ions();
                    log::info!("Ions: {}", if engine.options.display.show_ions { "visible" } else { "hidden" });
                }
                "toggle_waters" => {
                    engine.toggle_waters();
                    log::info!("Waters: {}", if engine.options.display.show_waters { "visible" } else { "hidden" });
                }
                "toggle_solvent" => {
                    engine.toggle_solvent();
                    log::info!("Solvent: {}", if engine.options.display.show_solvent { "visible" } else { "hidden" });
                }
                "toggle_lipids" => {
                    engine.toggle_lipids();
                    log::info!("Lipids: {}", engine.options.display.lipid_mode);
                }
                "cycle_focus" => {
                    engine.cycle_focus();
                    let focus_name = engine.scene.focus_description();
                    log::info!("Focus: {}", focus_name);
                    self.router.update_rosetta_locks(engine, &self.shared_state);
                    engine.fit_camera_to_focus();
                    self.router.ui_dirty |= DirtyFlags::SELECTION | DirtyFlags::UI;
                }
                "cancel" => self.router.cancel_operations(engine, &mut self.shared_state),
                other => {
                    log::debug!("Unknown keybinding action: {}", other);
                    return false;
                }
            }
            true
        } else {
            false
        }
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
                engine.set_shift_pressed(shift);
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
                engine.set_shift_pressed(shift);
                self.router.handle_native_cursor_moved(engine, x, y);
                self.router.handle_native_mouse_input(engine, winit_button, false);
            }
            ViewportInput::PointerMove { x, y, shift, .. } => {
                engine.set_shift_pressed(shift);
                self.router.handle_native_cursor_moved(engine, x, y);
            }
            ViewportInput::Scroll { delta } => {
                engine.handle_mouse_wheel(delta);
            }
            ViewportInput::Key { code, pressed } => {
                if pressed {
                    match code.as_str() {
                        "KeyQ" => {
                            engine.fit_camera_to_focus();
                        }
                        "KeyT" => {
                            if engine.has_trajectory() {
                                engine.toggle_trajectory();
                            } else if let Some(path) = action_router::trajectory_path_from_args() {
                                engine.load_trajectory(std::path::Path::new(&path));
                            }
                        }
                        "KeyI" => { engine.toggle_ions(); }
                        "KeyU" => { engine.toggle_waters(); }
                        "KeyO" => { engine.toggle_solvent(); }
                        "KeyL" => { engine.toggle_lipids(); }
                        "Tab" => {
                            engine.cycle_focus();
                            self.router.update_rosetta_locks(engine, &self.shared_state);
                            engine.fit_camera_to_focus();
                            self.router.ui_dirty |= DirtyFlags::SELECTION | DirtyFlags::UI;
                        }
                        "Escape" => {
                            self.router.cancel_operations(engine, &mut self.shared_state);
                        }
                        _ => {
                            log::debug!("Unhandled key code from frontend: {}", code);
                        }
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
            self.router.handle_trigger_action(engine, &mut self.shared_state, action);
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

                engine.clear_scene();
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
                            if let Some(group) = engine.group_mut(id) {
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
            ParameterizedAction::SetViewOption { key, value } => {
                engine.apply_view_option(&key, &value);
                self.router.ui_dirty |= DirtyFlags::VIEW;
            }
            ParameterizedAction::LoadViewPreset { name } => {
                let presets_dir = std::path::Path::new("assets/view_presets");
                engine.load_preset(&name, presets_dir);
                self.router.ui_dirty |= DirtyFlags::VIEW;
            }
            ParameterizedAction::SaveViewPreset { name } => {
                let presets_dir = std::path::Path::new("assets/view_presets");
                if engine.save_preset(&name, presets_dir) {
                    engine.active_preset = Some(name);
                }
                self.router.ui_dirty |= DirtyFlags::VIEW;
            }
        }
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
                MouseScrollDelta::LineDelta(_, y) => engine.handle_mouse_wheel(y),
                MouseScrollDelta::PixelDelta(pos) => engine.handle_mouse_wheel(pos.y as f32 * 0.01),
            }
        }
    }

    pub(crate) fn handle_native_modifiers(&mut self, state: ModifiersState) {
        if let Some(engine) = &mut self.engine {
            engine.update_modifiers(state);
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

        // FPS changes every frame — always push it
        frontend.set_fps(engine.frame_timing.fps());

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
            frontend.set_view_mode(foldit_frontend::state::ViewMode::Ribbon);

            frontend.view.options = serde_json::to_value(engine.options()).unwrap_or_default();

            let presets_dir = std::path::Path::new("assets/view_presets");
            frontend.view.available_presets =
                viso::util::options::Options::list_presets(presets_dir);
            frontend.view.active_preset = engine.active_preset.clone();
        }
        if app_dirty.contains(DirtyFlags::SELECTION) {
            frontend.mark_dirty(DirtyFlags::SELECTION);
        }
        if app_dirty.contains(DirtyFlags::UI) {
            frontend.mark_dirty(DirtyFlags::UI);
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

        engine.context.set_surface_scale(scale);

        if let Some(&first_id) = engine.group_ids().first() {
            let name = engine.group(first_id).map(|g| g.name()).unwrap_or("?");
            log::info!("Loaded structure: {}", name);
            let backbone_ca = engine
                .group(first_id)
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
        let group_ids = engine.group_ids();
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
            engine.shutdown_scene_processor();
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
        engine.clear_bands();
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
        engine.clear_pull();
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
