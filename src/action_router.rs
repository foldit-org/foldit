//! Action Router: translates user input into orchestrator/backend commands.
//!
//! Owns routing state (session, orchestrator, bands, pull) and dispatches
//! user actions to the appropriate backend operations. Does NOT handle
//! backend output processing, rendering, or frontend state sync.

use foldit_frontend::DirtyFlags;
use foldit_rs::shared_state::SharedState;
use viso::renderer::molecular::band::BandRenderInfo;
use viso::engine::scene::Focus;
use foldit_runner::orchestrator::{EntityId, OpType};
use foldit_runner::Orchestrator;
use glam::Vec3;

use viso::animation::AnimationAction;
use viso::engine::core::ProteinRenderEngine;

/// Information about an active band for UI tracking
#[derive(Debug, Clone)]
pub(crate) struct ActiveBand {
    band_id: u32,
    res1: u32,
    atom1_name: String,
    res2: u32,
    atom2_name: String,
    length: f64,
    strength: f64,
    is_pull: bool,
    is_push: bool,
    is_disabled: bool,
}

/// State for band creation via right-click drag
#[derive(Debug, Clone)]
struct BandDragState {
    start_residue: i32,
    start_atom_name: String,
    start_atom_pos: Vec3,
    current_mouse_pos: (f32, f32),
}

/// State for pull action via left-click drag
#[derive(Debug, Clone)]
struct PullDragState {
    residue: i32,
    start_pos: Vec3,
    target_pos: Vec3,
    initial_mouse_pos: (f32, f32),
    is_active: bool,
}

/// Minimum drag distance in pixels to activate a pull (vs treating as click)
const PULL_DRAG_THRESHOLD: f32 = 5.0;

/// Central mediator for action dispatch, owning all routing state.
pub(crate) struct ActionRouter {
    pub orchestrator: Option<Orchestrator>,
    active_bands: std::collections::HashMap<u32, ActiveBand>,
    band_drag: Option<BandDragState>,
    right_mouse_pressed: bool,
    pull_drag: Option<PullDragState>,
    left_mouse_pressed: bool,
    pub ui_dirty: DirtyFlags,
    pub last_mouse_pos: (f32, f32),
}

/// Extract (chain_id, sequence) pairs from a slice of entities.
/// Merges entity coordinates, filters to protein residues, then extracts sequences.
pub(crate) fn extract_chains_from_entities_pub(
    entities: &[foldit_conv::coords::entity::MoleculeEntity],
) -> Vec<(String, String)> {
    extract_chains_from_entities(entities)
}

fn extract_chains_from_entities(
    entities: &[foldit_conv::coords::entity::MoleculeEntity],
) -> Vec<(String, String)> {
    let merged = foldit_conv::coords::entity::merge_entities(entities);
    let coords = foldit_conv::coords::protein_only(&merged);
    let (_full_seq, chain_sequences) = foldit_conv::coords::render::extract_sequences(&coords);
    if !chain_sequences.is_empty() {
        chain_sequences
            .iter()
            .map(|(cid, seq)| (format!("{}", *cid as char), seq.clone()))
            .collect()
    } else {
        let full_seq = _full_seq;
        if !full_seq.is_empty() {
            vec![("A".to_string(), full_seq)]
        } else {
            vec![]
        }
    }
}

impl ActionRouter {
    pub fn new() -> Self {
        Self {
            orchestrator: None,
            active_bands: std::collections::HashMap::new(),
            band_drag: None,
            right_mouse_pressed: false,
            pull_drag: None,
            left_mouse_pressed: false,
            ui_dirty: DirtyFlags::empty(),
            last_mouse_pos: (0.0, 0.0),
        }
    }

    // ── Helpers ──

    fn focus(&self, engine: &ProteinRenderEngine) -> Focus {
        *engine.focus()
    }

    pub fn take_ui_dirty(&mut self) -> DirtyFlags {
        let flags = self.ui_dirty;
        self.ui_dirty = DirtyFlags::empty();
        flags
    }

    fn get_structure_chains(&self, engine: &ProteinRenderEngine, shared: &SharedState) -> Vec<(String, String)> {
        let focus = self.focus(engine);
        let structure_id = SharedState::operation_target(&focus)
            .or(shared.loaded_entity());
        if let Some(id) = structure_id {
            if let Some(group) = engine.group(id) {
                if let Some(protein_coords) = group.protein_coords() {
                    let coords = foldit_conv::coords::protein_only(&protein_coords);
                    let (sequence, chain_sequences) =
                        foldit_conv::coords::render::extract_sequences(&coords);
                    if !chain_sequences.is_empty() {
                        return chain_sequences
                            .iter()
                            .map(|(cid, seq)| (format!("{}", *cid as char), seq.clone()))
                            .collect();
                    }
                    if !sequence.is_empty() {
                        return vec![("A".to_string(), sequence)];
                    }
                }
            }
        }
        vec![]
    }

    // ── Read-only accessors for App ──

    pub fn active_bands(&self) -> &std::collections::HashMap<u32, ActiveBand> {
        &self.active_bands
    }

    /// Return pull drag render info if an active pull is in progress.
    pub fn pull_drag_info(&self) -> Option<(Vec3, Vec3, u32)> {
        self.pull_drag.as_ref().and_then(|pull| {
            if pull.is_active {
                Some((pull.start_pos, pull.target_pos, pull.residue as u32))
            } else {
                None
            }
        })
    }

    /// Return band drag preview info (start_pos, current_target, residue_idx).
    pub fn band_drag_preview(&self, engine: &ProteinRenderEngine) -> Option<(Vec3, Vec3, u32)> {
        self.band_drag.as_ref().map(|drag| {
            let target_pos = engine.screen_to_world_at_depth(
                drag.current_mouse_pos.0,
                drag.current_mouse_pos.1,
                drag.start_atom_pos,
            );
            (drag.start_atom_pos, target_pos, drag.start_residue as u32)
        })
    }

    /// Update pull start position from current engine state.
    pub fn refresh_pull_position(&mut self, engine: &ProteinRenderEngine) {
        if let Some(ref mut pull) = self.pull_drag {
            if pull.is_active {
                if let Some(current_ca) = engine.get_residue_ca_position(pull.residue as usize) {
                    pull.start_pos = current_ca;
                }
            }
        }
    }

    /// Reset band state and orchestrator for puzzle loading.
    pub fn reset_for_new_structure(&mut self, shared: &mut SharedState) {
        shared.reset_entities();
        self.active_bands.clear();
        if let Some(ref mut orch) = self.orchestrator {
            for eid in orch.locked_entities() {
                orch.unlock(eid);
            }
            orch.clear_session();
        }
    }

    // ── Action dispatch ──

    pub fn handle_trigger_action(
        &mut self,
        engine: &mut ProteinRenderEngine,
        shared: &mut SharedState,
        action: foldit_frontend::ActionId,
    ) -> Option<foldit_frontend::ParameterizedAction> {
        use foldit_frontend::ActionId;
        let parameterized = match action {
            ActionId::ToggleWiggle => { self.toggle_wiggle(engine, shared); None }
            ActionId::ToggleShake => { self.toggle_shake(engine, shared); None }
            ActionId::RunPrediction => { self.run_prediction(engine, shared); None }
            ActionId::RunMPNN => {
                // Default params — frontend can use ParameterizedAction for custom values
                Some(foldit_frontend::ParameterizedAction::RunSequenceDesign {
                    temperature: 0.1,
                    num_sequences: 4,
                })
            }
            ActionId::RunDiffusion => {
                Some(foldit_frontend::ParameterizedAction::RunStructureDesign {
                    length: "100-100".to_string(),
                    num_steps: 50,
                    contig: None,
                })
            }
            ActionId::Undo | ActionId::Redo => {
                log::warn!("Undo/Redo not yet implemented");
                None
            }
        };
        self.ui_dirty |= DirtyFlags::SCORE | DirtyFlags::ACTIONS | DirtyFlags::UI;
        parameterized
    }

    pub fn cancel_operations(&mut self, engine: &mut ProteinRenderEngine, shared: &mut SharedState) {
        log::info!("Cancelling current operation");
        engine.picking.clear_selection();
        if let Some(ref mut orch) = self.orchestrator {
            let locked_ids = orch.locked_entities();
            for eid in &locked_ids {
                orch.cancel_entity(*eid);
            }
            orch.stop_rosetta();
            for eid in locked_ids {
                orch.unlock(eid);
                log::info!("Stopped operation on entity {:?}", eid);
            }
        }
        if let Some(anim_id) = shared.animation() {
            if engine.remove_group(anim_id).is_some() {
                log::info!("Removed in-progress animation structure");
                shared.remove_animation();
                engine.sync_scene_to_renderers(Some(AnimationAction::Load));
            }
        }

        if !self.active_bands.is_empty() {
            if let Some(ref orch) = self.orchestrator {
                let _ = orch.clear_all_bands();
            }
            log::info!("Cleared {} bands", self.active_bands.len());
            self.active_bands.clear();
            engine.clear_bands();
        }
        self.band_drag = None;
        self.ui_dirty |= DirtyFlags::ACTIONS | DirtyFlags::SELECTION | DirtyFlags::LOADING;
    }

    // ── Rosetta operations ──

    fn toggle_wiggle(&mut self, engine: &mut ProteinRenderEngine, shared: &SharedState) {
        if self.orchestrator.is_none() {
            log::warn!("Orchestrator not initialized");
            return;
        }

        // Check if already running — if so, stop
        {
            let orch = self.orchestrator.as_mut().unwrap();
            let locked_ids = orch.locked_entities();
            if !locked_ids.is_empty() {
                let has_rosetta_op = locked_ids.iter().any(|&id| {
                    matches!(
                        orch.get_op_type(id),
                        Some(OpType::RosettaWiggle) | Some(OpType::RosettaShake)
                    )
                });
                if has_rosetta_op {
                    log::info!("Stopping Rosetta operation...");
                    for eid in &locked_ids {
                        if matches!(
                            orch.get_op_type(*eid),
                            Some(OpType::RosettaWiggle) | Some(OpType::RosettaShake)
                        ) {
                            orch.cancel_entity(*eid);
                            orch.unlock(*eid);
                        }
                    }
                    orch.stop_rosetta();

                    self.ui_dirty |= DirtyFlags::ACTIONS;
                }
                return;
            }
        }

        let focus = self.focus(engine);
        let Some(lock_id) = shared.lock_target(&focus) else {
            log::warn!("No structure available for wiggle");
            return;
        };

        let Some(combined) = engine.combined_coords_for_backend() else {
            log::warn!("No coords available for wiggle");
            return;
        };
        let coords = combined.bytes.clone();

        if !self.ensure_rosetta_session(engine) {
            log::warn!("Failed to ensure Rosetta session for wiggle");
            return;
        }

        self.update_rosetta_locks(engine, shared);

        let target_desc = if SharedState::is_session_mode(&focus) {
            format!("full session ({} structures)", engine.group_count())
        } else {
            SharedState::operation_target(&focus)
                .and_then(|id| engine.group(id))
                .map(|g| g.name().to_string())
                .unwrap_or_default()
        };

        let orch = self.orchestrator.as_mut().unwrap();
        if orch.try_lock(EntityId(lock_id.0), OpType::RosettaWiggle).is_some() {
            log::info!(
                "Starting wiggle on {} ({} bytes)...",
                target_desc,
                coords.len()
            );
            if let Err(e) = orch.start_wiggle(coords) {
                log::error!("Failed to start wiggle: {}", e);
                orch.unlock(EntityId(lock_id.0));
                return;
            }

            self.ui_dirty |= DirtyFlags::ACTIONS;
        } else {
            log::warn!("Structure is already locked by another operation");
        }
    }

    fn toggle_shake(&mut self, engine: &mut ProteinRenderEngine, shared: &SharedState) {
        if self.orchestrator.is_none() {
            log::warn!("Orchestrator not initialized");
            return;
        }

        {
            let orch = self.orchestrator.as_mut().unwrap();
            let locked_ids = orch.locked_entities();
            if !locked_ids.is_empty() {
                let has_rosetta_op = locked_ids.iter().any(|&id| {
                    matches!(
                        orch.get_op_type(id),
                        Some(OpType::RosettaWiggle) | Some(OpType::RosettaShake)
                    )
                });
                if has_rosetta_op {
                    log::info!("Stopping Rosetta operation...");
                    for eid in &locked_ids {
                        if matches!(
                            orch.get_op_type(*eid),
                            Some(OpType::RosettaWiggle) | Some(OpType::RosettaShake)
                        ) {
                            orch.cancel_entity(*eid);
                            orch.unlock(*eid);
                        }
                    }
                    orch.stop_rosetta();

                    self.ui_dirty |= DirtyFlags::ACTIONS;
                }
                return;
            }
        }

        let focus = self.focus(engine);
        let Some(lock_id) = shared.lock_target(&focus) else {
            log::warn!("No structure available for shake");
            return;
        };

        let Some(combined) = engine.combined_coords_for_backend() else {
            log::warn!("No coords available for shake");
            return;
        };
        let coords = combined.bytes.clone();

        if !self.ensure_rosetta_session(engine) {
            log::warn!("Failed to ensure Rosetta session for shake");
            return;
        }

        self.update_rosetta_locks(engine, shared);

        let target_desc = if SharedState::is_session_mode(&focus) {
            format!("full session ({} structures)", engine.group_count())
        } else {
            SharedState::operation_target(&focus)
                .and_then(|id| engine.group(id))
                .map(|g| g.name().to_string())
                .unwrap_or_default()
        };

        let orch = self.orchestrator.as_mut().unwrap();
        if orch.try_lock(EntityId(lock_id.0), OpType::RosettaShake).is_some() {
            log::info!(
                "Starting shake on {} ({} bytes)...",
                target_desc,
                coords.len()
            );
            if let Err(e) = orch.start_shake(coords) {
                log::error!("Failed to start shake: {}", e);
                orch.unlock(EntityId(lock_id.0));
                return;
            }

            self.ui_dirty |= DirtyFlags::ACTIONS;
        } else {
            log::warn!("Structure is already locked by another operation");
        }
    }

    // ── ML operations ──

    fn run_prediction(&mut self, engine: &mut ProteinRenderEngine, shared: &SharedState) {
        use crate::backend_handler;

        let focus = self.focus(engine);
        let fallback = shared.loaded_entity();
        let Some((target_id, entities)) =
            backend_handler::collect_ml_entities(engine, &focus, fallback)
        else {
            log::warn!("No structure available for prediction");
            return;
        };

        if self.orchestrator.is_none() {
            log::warn!("Orchestrator not initialized");
            return;
        }

        {
            let orch = self.orchestrator.as_ref().unwrap();
            if orch.is_locked(EntityId(target_id.0)) {
                let op = orch.get_op_type(EntityId(target_id.0));
                log::warn!(
                    "Structure is locked by {:?}, cannot start RoseTTAFold3",
                    op
                );
                return;
            }
        }

        {
            let orch = self.orchestrator.as_mut().unwrap();
            orch.stop_rosetta();
            orch.clear_session();
        }

        // Extract chains from collected entities
        let chains = extract_chains_from_entities(&entities);
        if chains.is_empty() {
            log::warn!("No sequence/chains available");
            return;
        }

        let total_atoms: usize = entities.iter().map(|e| e.coords.num_atoms).sum();
        log::info!(
            "RF3 prediction: focus={:?}, {} entities, {} total atoms",
            focus, entities.len(), total_atoms,
        );

        // Build entity context from the collected entities
        let entity_context = crate::App::build_entity_context(&entities, shared, target_id);

        let orch = self.orchestrator.as_mut().unwrap();
        if orch
            .try_lock(EntityId(target_id.0), OpType::MLPredict)
            .is_none()
        {
            log::warn!("Failed to acquire lock for RoseTTAFold3");
            return;
        }

        let total_residues: usize = chains.iter().map(|(_, s)| s.len()).sum();
        log::info!(
            "Starting RoseTTAFold3 prediction for {} residues...",
            total_residues
        );

        let result = if let Some(ctx) = entity_context {
            log::info!(
                "Passing assembly context ({} entities, {} bytes)",
                ctx.entities.len(),
                ctx.assembly_coords.len(),
            );
            orch.predict_with_context(None, chains, 3, ctx)
        } else {
            orch.predict(None, chains, 3)
        };

        if let Err(e) = result {
            log::error!("Failed to submit prediction task: {}", e);
            orch.unlock(EntityId(target_id.0));
            return;
        }

        self.ui_dirty |= DirtyFlags::ACTIONS | DirtyFlags::LOADING;
    }

    // ── Rosetta session management ──

    pub fn ensure_rosetta_session(&mut self, engine: &mut ProteinRenderEngine) -> bool {
        use foldit_runner::backends::rosetta::session_state::{
            RosettaSessionState, StructureId as RosettaStructureId,
        };

        let combined = match engine.combined_coords_for_backend() {
            Some(c) => c,
            None => return false,
        };

        if self.orchestrator.is_none() {
            return false;
        }

        let needs_recreation = match self.orchestrator.as_ref().unwrap().session() {
            None => true,
            Some(state) => {
                let visible = engine.visible_residue_counts();
                let visible_rosetta_ids: Vec<RosettaStructureId> = visible
                    .iter()
                    .map(|(id, _)| RosettaStructureId(id.0))
                    .collect();
                let residue_counts_rosetta: std::collections::HashMap<RosettaStructureId, usize> = visible
                    .iter()
                    .map(|(id, count)| (RosettaStructureId(id.0), *count))
                    .collect();
                state.topology_changed(&visible_rosetta_ids, &residue_counts_rosetta)
            }
        };

        if needs_recreation {
            log::info!("Recreating Rosetta session (topology changed)");
            let orch = self.orchestrator.as_mut().unwrap();
            if let Err(e) = orch.recreate_session(combined.bytes.clone()) {
                log::error!("Failed to recreate Rosetta session: {}", e);
                return false;
            }

            let chain_ids_per_structure: Vec<(RosettaStructureId, Vec<u8>)> = combined
                .chain_ids_per_group
                .iter()
                .map(|(id, chains)| (RosettaStructureId(id.0), chains.clone()))
                .collect();
            let residue_ranges: std::collections::HashMap<RosettaStructureId, (usize, usize)> = combined
                .residue_ranges
                .iter()
                .map(|(id, range)| (RosettaStructureId(id.0), *range))
                .collect();

            let state = RosettaSessionState::new(chain_ids_per_structure, residue_ranges);
            log::info!(
                "Session created with {} structures, {} total residues",
                combined.chain_ids_per_group.len(),
                state.total_residues
            );
            orch.set_session(state);
        }

        true
    }

    pub(crate) fn update_rosetta_locks(&mut self, engine: &ProteinRenderEngine, _shared: &SharedState) {
        let focus = self.focus(engine);
        let new_focus = SharedState::operation_target(&focus)
            .map(|id| EntityId(id.0));

        if let Some(ref mut orch) = self.orchestrator {
            orch.update_focus_locks(new_focus);
        }
    }

    // ── Band creation ──

    fn create_band_with_atoms(
        &mut self,
        engine: &mut ProteinRenderEngine,
        start_residue: i32,
        start_pos: Vec3,
        start_atom_name: &str,
        end_residue: i32,
        end_pos: Vec3,
        end_atom_name: &str,
    ) {
        if !self.ensure_rosetta_session(engine) {
            log::warn!("No Rosetta session available for band creation");
            return;
        }

        if self.orchestrator.is_none() {
            log::warn!("No orchestrator available");
            return;
        }

        let res1 = (start_residue + 1) as u32;
        let res2 = (end_residue + 1) as u32;

        let atom1 = 2u32;
        let atom2 = 2u32;

        let length = start_pos.distance(end_pos) as f64;
        let strength = 1.0;

        let orch = self.orchestrator.as_ref().unwrap();
        match orch.add_band(res1, atom1, res2, atom2, length, strength) {
            Ok(band_id) => {
                log::info!(
                    "Created band {} between {}:{} and {}:{} (length: {:.1}\u{00c5})",
                    band_id,
                    res1,
                    start_atom_name,
                    res2,
                    end_atom_name,
                    length
                );

                self.active_bands.insert(
                    band_id,
                    ActiveBand {
                        band_id,
                        res1,
                        atom1_name: start_atom_name.to_string(),
                        res2,
                        atom2_name: end_atom_name.to_string(),
                        length,
                        strength,
                        is_pull: true,
                        is_push: false,
                        is_disabled: false,
                    },
                );
            }
            Err(e) => {
                log::error!("Failed to create band: {}", e);
            }
        }
    }

    // ── Mouse / input handlers ──

    pub fn handle_native_mouse_input(
        &mut self,
        engine: &mut ProteinRenderEngine,
        button: winit::event::MouseButton,
        pressed: bool,
    ) {
        use foldit_conv::coords::{get_closest_atom_for_residue, get_closest_atom_with_name};

        match button {
            winit::event::MouseButton::Left => {
                self.left_mouse_pressed = pressed;

                if pressed {
                    let hovered = engine.hovered_residue();
                    if hovered >= 0 {
                        if let Some(ca_pos) = engine.get_residue_ca_position(hovered as usize) {
                            let click_world_pos = engine.screen_to_world_at_depth(
                                self.last_mouse_pos.0,
                                self.last_mouse_pos.1,
                                ca_pos,
                            );
                            let agg = engine.scene.aggregated().clone();
                            let start_pos = get_closest_atom_for_residue(
                                &agg.backbone_chains,
                                &agg.sidechain_positions,
                                &agg.sidechain_residue_indices,
                                hovered as usize,
                                click_world_pos,
                            )
                            .unwrap_or(ca_pos);

                            self.pull_drag = Some(PullDragState {
                                residue: hovered,
                                start_pos,
                                target_pos: click_world_pos,
                                initial_mouse_pos: self.last_mouse_pos,
                                is_active: false,
                            });
                            log::debug!(
                                "Potential pull on residue {} at {:?}",
                                hovered,
                                start_pos
                            );
                        }
                    }

                    engine.handle_mouse_button(button, pressed);
                } else {
                    // Left button released
                    if let Some(pull) = self.pull_drag.take() {
                        engine.handle_mouse_button(button, false);

                        if pull.is_active {
                            log::info!(
                                "Pull released - residue {} pulled to {:?}",
                                pull.residue,
                                pull.target_pos
                            );
                            if let Some(ref orch) = self.orchestrator {
                                orch.cancel_rosetta();
                            }
                        } else {
                            engine.handle_mouse_up();
                        }
                    } else {
                        engine.handle_mouse_button(button, false);
                        engine.handle_mouse_up();
                    }
                }
            }
            winit::event::MouseButton::Right => {
                self.right_mouse_pressed = pressed;

                if pressed {
                    let hovered = engine.hovered_residue();
                    if hovered >= 0 {
                        if let Some(ca_pos) = engine.get_residue_ca_position(hovered as usize) {
                            let click_world_pos = engine.screen_to_world_at_depth(
                                self.last_mouse_pos.0,
                                self.last_mouse_pos.1,
                                ca_pos,
                            );
                            let agg = engine.scene.aggregated().clone();
                            let (start_atom_pos, start_atom_name) = get_closest_atom_with_name(
                                &agg.backbone_chains,
                                &agg.sidechain_positions,
                                &agg.sidechain_residue_indices,
                                &agg.sidechain_atom_names,
                                hovered as usize,
                                click_world_pos,
                            )
                            .unwrap_or((ca_pos, "CA".to_string()));

                            self.band_drag = Some(BandDragState {
                                start_residue: hovered,
                                start_atom_pos,
                                start_atom_name,
                                current_mouse_pos: self.last_mouse_pos,
                            });
                            log::info!(
                                "Started band drag from residue {} at {:?}",
                                hovered,
                                start_atom_pos
                            );
                        }
                    }
                } else {
                    if let Some(drag) = self.band_drag.take() {
                        let end_residue = engine.hovered_residue();
                        if end_residue >= 0 && end_residue != drag.start_residue {
                            if let Some(ca_pos) =
                                engine.get_residue_ca_position(end_residue as usize)
                            {
                                let click_world_pos = engine.screen_to_world_at_depth(
                                    self.last_mouse_pos.0,
                                    self.last_mouse_pos.1,
                                    ca_pos,
                                );
                                let agg = engine.scene.aggregated().clone();
                                let (end_atom_pos, end_atom_name) = get_closest_atom_with_name(
                                    &agg.backbone_chains,
                                    &agg.sidechain_positions,
                                    &agg.sidechain_residue_indices,
                                    &agg.sidechain_atom_names,
                                    end_residue as usize,
                                    click_world_pos,
                                )
                                .unwrap_or((ca_pos, "CA".to_string()));

                                self.create_band_with_atoms(
                                    engine,
                                    drag.start_residue,
                                    drag.start_atom_pos,
                                    &drag.start_atom_name,
                                    end_residue,
                                    end_atom_pos,
                                    &end_atom_name,
                                );
                            }
                        } else if end_residue == drag.start_residue {
                            log::info!("Band drag ended on same residue - cancelled");
                        } else {
                            log::info!("Band drag ended on background - cancelled");
                        }
                    }
                }
            }
            _ => {}
        }
    }

    pub fn handle_native_cursor_moved(
        &mut self,
        engine: &mut ProteinRenderEngine,
        x: f32,
        y: f32,
    ) {
        let delta_x = x - self.last_mouse_pos.0;
        let delta_y = y - self.last_mouse_pos.1;

        // Handle pull drag movement
        let mut pull_became_active = false;
        if let Some(ref mut pull) = self.pull_drag {
            if !pull.is_active {
                let dx = x - pull.initial_mouse_pos.0;
                let dy = y - pull.initial_mouse_pos.1;
                let distance = (dx * dx + dy * dy).sqrt();
                if distance > PULL_DRAG_THRESHOLD {
                    pull.is_active = true;
                    pull_became_active = true;
                    log::info!(
                        "Pull activated on residue {} (moved {} pixels)",
                        pull.residue,
                        distance
                    );
                }
            }

            if pull.is_active {
                if let Some(current_ca) = engine.get_residue_ca_position(pull.residue as usize) {
                    pull.start_pos = current_ca;
                }
                pull.target_pos = engine.screen_to_world_at_depth(x, y, pull.start_pos);
            }
        }

        // Start Rosetta pull when pull becomes active
        if pull_became_active {
            if let Some(ref pull) = self.pull_drag {
                if let Some(ref orch) = self.orchestrator {
                    if let Some(combined) = engine.combined_coords_for_backend() {
                        let residue_1indexed = (pull.residue + 1) as u32;
                        let target = [pull.target_pos.x, pull.target_pos.y, pull.target_pos.z];
                        if let Err(e) = orch.start_pull(combined.bytes, residue_1indexed, target) {
                            log::warn!("Failed to start pull: {}", e);
                        } else {
                            log::info!(
                                "Started Rosetta pull operation for residue {}",
                                residue_1indexed
                            );
                        }
                    }
                }
            }
        }

        // Handle mouse movement for camera/hovering
        if let Some(ref pull) = self.pull_drag {
            if pull.is_active {
                let target = [pull.target_pos.x, pull.target_pos.y, pull.target_pos.z];
                if let Some(ref orch) = self.orchestrator {
                    let _ = orch.update_pull_target(target);
                }
                engine.handle_mouse_position(x, y);
            } else {
                engine.handle_mouse_move(delta_x, delta_y);
                engine.handle_mouse_position(x, y);
            }
        } else {
            engine.handle_mouse_move(delta_x, delta_y);
            engine.handle_mouse_position(x, y);
        }

        self.last_mouse_pos = (x, y);

        // Update band drag state during right-click drag
        if let Some(ref mut drag) = self.band_drag {
            drag.current_mouse_pos = self.last_mouse_pos;
        }
    }

    // ── Shutdown ──

    pub fn shutdown(&self) {
        if let Some(ref orch) = self.orchestrator {
            orch.shutdown();
        }
    }
}

// ---------------------------------------------------------------------------
// Free functions (used by App)
// ---------------------------------------------------------------------------

/// Extract CA positions from entities (for Kabsch alignment).
pub(crate) fn entities_backbone_ca(
    entities: &[foldit_conv::coords::entity::MoleculeEntity],
) -> Vec<Vec3> {
    let mut cas = Vec::new();
    for entity in entities {
        if entity.molecule_type == foldit_conv::coords::entity::MoleculeType::Protein {
            for i in 0..entity.coords.num_atoms {
                let name = std::str::from_utf8(&entity.coords.atom_names[i])
                    .unwrap_or("")
                    .trim();
                if name == "CA" {
                    cas.push(Vec3::new(
                        entity.coords.atoms[i].x,
                        entity.coords.atoms[i].y,
                        entity.coords.atoms[i].z,
                    ));
                }
            }
        }
    }
    cas
}

/// Load a file (PDB/CIF/BCIF) and return entities + name.
pub(crate) fn load_file_as_entities(
    path: &str,
) -> Result<(Vec<foldit_conv::coords::entity::MoleculeEntity>, String), String> {
    let p = std::path::Path::new(path);
    let name = p
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Unknown")
        .to_string();

    let coords = foldit_rs::puzzle::load_coords_from_file(p)?;
    let entities = foldit_conv::coords::split_into_entities(&coords);
    Ok((entities, name))
}

/// Check if a string looks like a PDB ID (4 alphanumeric characters)
fn is_pdb_id(s: &str) -> bool {
    s.len() == 4 && s.chars().all(|c| c.is_ascii_alphanumeric())
}

/// Resolve a PDB ID or path to an actual file path, downloading if necessary
pub(crate) fn resolve_structure_path(input: &str) -> Result<String, String> {
    if std::path::Path::new(input).exists() {
        return Ok(input.to_string());
    }

    if is_pdb_id(input) {
        let pdb_id = input.to_lowercase();
        let models_dir = std::path::Path::new("assets/models");
        let local_path = models_dir.join(format!("{}.cif", pdb_id));

        if local_path.exists() {
            log::info!("Found local copy: {}", local_path.display());
            return Ok(local_path.to_string_lossy().to_string());
        }

        if !models_dir.exists() {
            std::fs::create_dir_all(models_dir)
                .map_err(|e| format!("Failed to create models directory: {}", e))?;
        }

        let url = format!("https://files.rcsb.org/download/{}.cif", pdb_id);
        log::info!("Downloading {} from RCSB...", pdb_id.to_uppercase());

        let response = reqwest::blocking::get(&url)
            .map_err(|e| format!("Failed to download {}: {}", pdb_id, e))?;

        if !response.status().is_success() {
            return Err(format!(
                "Failed to download {}: HTTP {}",
                pdb_id,
                response.status()
            ));
        }

        let content = response
            .text()
            .map_err(|e| format!("Failed to read response: {}", e))?;

        std::fs::write(&local_path, &content)
            .map_err(|e| format!("Failed to save CIF file: {}", e))?;

        log::info!("Downloaded to {}", local_path.display());
        return Ok(local_path.to_string_lossy().to_string());
    }

    Err(format!("File not found: {}", input))
}

/// Build BandRenderInfo from active bands and engine state.
pub(crate) fn build_band_render_infos(
    engine: &ProteinRenderEngine,
    active_bands: &std::collections::HashMap<u32, ActiveBand>,
) -> Vec<BandRenderInfo> {
    active_bands
        .values()
        .filter_map(|band| {
            let idx1 = (band.res1 as usize).checked_sub(1)?;
            let idx2 = (band.res2 as usize).checked_sub(1)?;

            let pos1 = engine.get_atom_position_by_name(idx1, &band.atom1_name)?;
            let pos2 = engine.get_atom_position_by_name(idx2, &band.atom2_name)?;

            Some(BandRenderInfo {
                endpoint_a: pos1,
                endpoint_b: pos2,
                is_pull: band.is_pull,
                is_push: band.is_push,
                is_disabled: band.is_disabled,
                strength: band.strength as f32,
                target_length: band.length as f32,
                residue_idx: idx1 as u32,
                is_space_pull: false,
                ..Default::default()
            })
        })
        .collect()
}

/// Build actions list from orchestrator state.
pub(crate) fn build_actions_list(
    orchestrator: &Option<Orchestrator>,
) -> Vec<foldit_frontend::state::ActionInfo> {
    use foldit_frontend::state::ActionInfo;

    let orch = match orchestrator {
        Some(o) => o,
        None => return vec![],
    };

    let locked: Vec<EntityId> = orch.locked_entities();
    let has_any_lock = !locked.is_empty();
    let has_rosetta_op = locked.iter().any(|&id| {
        matches!(
            orch.get_op_type(id),
            Some(OpType::RosettaWiggle) | Some(OpType::RosettaShake)
        )
    });
    let has_ml_op = locked.iter().any(|&id| {
        matches!(
            orch.get_op_type(id),
            Some(OpType::MLPredict)
                | Some(OpType::MLSequenceDesign)
                | Some(OpType::MLStructureDesign)
        )
    });

    vec![
        ActionInfo {
            id: 0,
            name: "Wiggle".into(),
            enabled: !has_ml_op,
            active: has_rosetta_op
                && locked
                    .iter()
                    .any(|&id| orch.get_op_type(id) == Some(OpType::RosettaWiggle)),
        },
        ActionInfo {
            id: 1,
            name: "Shake".into(),
            enabled: !has_ml_op,
            active: has_rosetta_op
                && locked
                    .iter()
                    .any(|&id| orch.get_op_type(id) == Some(OpType::RosettaShake)),
        },
        ActionInfo {
            id: 2,
            name: "Predict".into(),
            enabled: !has_any_lock,
            active: locked
                .iter()
                .any(|&id| orch.get_op_type(id) == Some(OpType::MLPredict)),
        },
        ActionInfo {
            id: 3,
            name: "MPNN".into(),
            enabled: !has_any_lock,
            active: locked
                .iter()
                .any(|&id| orch.get_op_type(id) == Some(OpType::MLSequenceDesign)),
        },
        ActionInfo {
            id: 4,
            name: "Diffusion".into(),
            enabled: !has_any_lock,
            active: locked
                .iter()
                .any(|&id| orch.get_op_type(id) == Some(OpType::MLStructureDesign)),
        },
    ]
}

/// Get the trajectory path from command-line arguments.
pub(crate) fn trajectory_path_from_args() -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.windows(2).find_map(|w| {
        if w[0] == "--trajectory" {
            Some(w[1].clone())
        } else {
            None
        }
    })
}

/// Convert a winit KeyCode to the string format used in keybinding options.
pub(crate) fn key_code_to_string(key: winit::keyboard::KeyCode) -> String {
    use winit::keyboard::KeyCode;
    match key {
        KeyCode::KeyA => "KeyA".into(),
        KeyCode::KeyB => "KeyB".into(),
        KeyCode::KeyC => "KeyC".into(),
        KeyCode::KeyD => "KeyD".into(),
        KeyCode::KeyE => "KeyE".into(),
        KeyCode::KeyF => "KeyF".into(),
        KeyCode::KeyG => "KeyG".into(),
        KeyCode::KeyH => "KeyH".into(),
        KeyCode::KeyI => "KeyI".into(),
        KeyCode::KeyJ => "KeyJ".into(),
        KeyCode::KeyK => "KeyK".into(),
        KeyCode::KeyL => "KeyL".into(),
        KeyCode::KeyM => "KeyM".into(),
        KeyCode::KeyN => "KeyN".into(),
        KeyCode::KeyO => "KeyO".into(),
        KeyCode::KeyP => "KeyP".into(),
        KeyCode::KeyQ => "KeyQ".into(),
        KeyCode::KeyR => "KeyR".into(),
        KeyCode::KeyS => "KeyS".into(),
        KeyCode::KeyT => "KeyT".into(),
        KeyCode::KeyU => "KeyU".into(),
        KeyCode::KeyV => "KeyV".into(),
        KeyCode::KeyW => "KeyW".into(),
        KeyCode::KeyX => "KeyX".into(),
        KeyCode::KeyY => "KeyY".into(),
        KeyCode::KeyZ => "KeyZ".into(),
        KeyCode::Digit0 => "Digit0".into(),
        KeyCode::Digit1 => "Digit1".into(),
        KeyCode::Digit2 => "Digit2".into(),
        KeyCode::Digit3 => "Digit3".into(),
        KeyCode::Digit4 => "Digit4".into(),
        KeyCode::Digit5 => "Digit5".into(),
        KeyCode::Digit6 => "Digit6".into(),
        KeyCode::Digit7 => "Digit7".into(),
        KeyCode::Digit8 => "Digit8".into(),
        KeyCode::Digit9 => "Digit9".into(),
        KeyCode::Escape => "Escape".into(),
        KeyCode::Tab => "Tab".into(),
        KeyCode::Space => "Space".into(),
        KeyCode::Enter => "Enter".into(),
        KeyCode::Backspace => "Backspace".into(),
        KeyCode::Delete => "Delete".into(),
        KeyCode::ArrowUp => "ArrowUp".into(),
        KeyCode::ArrowDown => "ArrowDown".into(),
        KeyCode::ArrowLeft => "ArrowLeft".into(),
        KeyCode::ArrowRight => "ArrowRight".into(),
        KeyCode::F1 => "F1".into(),
        KeyCode::F2 => "F2".into(),
        KeyCode::F3 => "F3".into(),
        KeyCode::F4 => "F4".into(),
        KeyCode::F5 => "F5".into(),
        KeyCode::F6 => "F6".into(),
        KeyCode::F7 => "F7".into(),
        KeyCode::F8 => "F8".into(),
        KeyCode::F9 => "F9".into(),
        KeyCode::F10 => "F10".into(),
        KeyCode::F11 => "F11".into(),
        KeyCode::F12 => "F12".into(),
        other => format!("{:?}", other),
    }
}
