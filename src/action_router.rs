//! Action Router: translates user input into orchestrator/backend commands.
//!
//! Owns routing state (session, orchestrator, bands, pull) and dispatches
//! user actions to the appropriate backend operations. Does NOT handle
//! backend output processing, rendering, or frontend state sync.

use foldit_frontend::DirtyFlags;
use foldit_rs::entity_store::EntityStore;
use foldit_rs::shared_state::SharedState;
use viso::{AtomRef, BandInfo, BandTarget, InputEvent, InputProcessor, MouseButton, VisoCommand, VisoEngine};
use foldit_runner::orchestrator::{EntityId, OpType};
use foldit_runner::Orchestrator;
use glam::{Vec2, Vec3};

/// Information about an active band for UI tracking
#[derive(Debug, Clone)]
pub(crate) struct ActiveBand {
    _band_id: u32,
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
    /// CA positions of the protein chains submitted to the most recent
    /// RF3 prediction, in submission (chain) order. Used by
    /// `handle_ml_predict_result` / `handle_ml_intermediate` as the
    /// alignment reference so the predicted structure stays anchored to
    /// the entities the user actually sent — works for multi-entity
    /// predictions where the loaded entity's stored `reference_ca` no
    /// longer matches. Cleared once the final result has been applied.
    pub pending_prediction_reference: Option<Vec<Vec3>>,
}

/// 3-letter PDB residue code → one-letter amino acid code. Unknown
/// codes map to `X`.
fn residue_three_to_one(name: &[u8; 3]) -> char {
    match name {
        b"ALA" => 'A', b"ARG" => 'R', b"ASN" => 'N', b"ASP" => 'D',
        b"CYS" => 'C', b"GLN" => 'Q', b"GLU" => 'E', b"GLY" => 'G',
        b"HIS" => 'H', b"ILE" => 'I', b"LEU" => 'L', b"LYS" => 'K',
        b"MET" => 'M', b"PHE" => 'F', b"PRO" => 'P', b"SER" => 'S',
        b"THR" => 'T', b"TRP" => 'W', b"TYR" => 'Y', b"VAL" => 'V',
        _ => 'X',
    }
}

/// Extract (chain_id, sequence) pairs from a slice of entities.
/// Each protein entity contributes its own chain; non-protein entities
/// are skipped.
pub(crate) fn extract_chains_from_entities_pub(
    entities: &[molex::MoleculeEntity],
) -> Vec<(String, String)> {
    extract_chains_from_entities(entities)
}

fn extract_chains_from_entities(
    entities: &[molex::MoleculeEntity],
) -> Vec<(String, String)> {
    let mut by_chain: indexmap::IndexMap<u8, String> = indexmap::IndexMap::new();
    for entity in entities {
        if let molex::MoleculeEntity::Protein(p) = entity {
            let seq: String = p.residues.iter()
                .map(|r| residue_three_to_one(&r.name))
                .collect();
            by_chain.entry(p.pdb_chain_id).or_default().push_str(&seq);
        }
    }
    by_chain.into_iter()
        .map(|(cid, seq)| (format!("{}", cid as char), seq))
        .collect()
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
            pending_prediction_reference: None,
        }
    }

    // ── Helpers ──

    pub fn take_ui_dirty(&mut self) -> DirtyFlags {
        let flags = self.ui_dirty;
        self.ui_dirty = DirtyFlags::empty();
        flags
    }

    fn _get_structure_chains(&self, engine: &VisoEngine, store: &EntityStore) -> Vec<(String, String)> {
        let focus = engine.focus();
        let structure_id = SharedState::operation_target(&focus)
            .or(store.loaded_entity());
        if let Some(id) = structure_id {
            if let Some(te) = store.get(id) {
                return extract_chains_from_entities(std::slice::from_ref(&te.entity));
            }
        }
        vec![]
    }

    // ── Read-only accessors for App ──

    pub fn active_bands(&self) -> &std::collections::HashMap<u32, ActiveBand> {
        &self.active_bands
    }

    /// Return pull drag screen-space info for viso's PullInfo.
    /// Returns (residue_index, (screen_x, screen_y)) if a pull is active.
    pub fn pull_drag_info_for_viso(&self) -> Option<(u32, (f32, f32))> {
        self.pull_drag.as_ref().and_then(|pull| {
            if pull.is_active {
                Some((pull.residue as u32, self.last_mouse_pos))
            } else {
                None
            }
        })
    }

    /// Return band drag preview info (start_residue, start_atom_name, target_pos).
    pub fn band_drag_preview(&self, engine: &VisoEngine) -> Option<(u32, String, Vec3)> {
        self.band_drag.as_ref().map(|drag| {
            let target_pos = engine.screen_to_world_at_depth(
                Vec2::new(drag.current_mouse_pos.0, drag.current_mouse_pos.1),
                drag.start_atom_pos,
            );
            (drag.start_residue as u32, drag.start_atom_name.clone(), target_pos)
        })
    }

    /// Update pull start position from current engine state.
    pub fn refresh_pull_position(&mut self, engine: &VisoEngine) {
        if let Some(ref mut pull) = self.pull_drag {
            if pull.is_active {
                if let Some(current_ca) = engine.resolve_atom_position(pull.residue as u32, "CA") {
                    pull.start_pos = current_ca;
                }
            }
        }
    }

    /// Reset band state and orchestrator for puzzle loading.
    pub fn reset_for_new_structure(&mut self) {
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
        engine: &mut VisoEngine,
        store: &EntityStore,
        action: foldit_frontend::ActionId,
    ) -> Option<foldit_frontend::ParameterizedAction> {
        use foldit_frontend::ActionId;
        let parameterized = match action {
            ActionId::ToggleWiggle => { self.toggle_wiggle(engine, store); None }
            ActionId::ToggleShake => { self.toggle_shake(engine, store); None }
            ActionId::RunPrediction => { self.run_prediction(engine, store); None }
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

    pub fn cancel_operations(&mut self, engine: &mut VisoEngine, store: &mut EntityStore, _shared: &mut SharedState) {
        log::info!("Cancelling current operation");
        engine.execute(VisoCommand::ClearSelection);
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
        if let Some(_anim_id) = store.animation() {
            store.remove_animation();
            store.publish_to(engine);
            log::info!("Removed in-progress animation structure");
        }

        if !self.active_bands.is_empty() {
            if let Some(ref orch) = self.orchestrator {
                let _ = orch.clear_all_bands();
            }
            log::info!("Cleared {} bands", self.active_bands.len());
            self.active_bands.clear();
            engine.update_bands(vec![]);
        }
        self.band_drag = None;
        self.ui_dirty |= DirtyFlags::ACTIONS | DirtyFlags::SELECTION | DirtyFlags::LOADING;
    }

    // ── Rosetta operations ──

    fn toggle_wiggle(&mut self, engine: &mut VisoEngine, store: &EntityStore) {
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

        let focus = engine.focus();
        let Some(lock_id) = SharedState::lock_target(&focus, store.loaded_entity()) else {
            log::warn!("No structure available for wiggle");
            return;
        };

        let Some(combined) = store.combined_assembly_for_backend() else {
            log::warn!("No assembly available for wiggle");
            return;
        };
        let assembly = combined.assembly.clone();

        if !self.ensure_rosetta_session(store) {
            log::warn!("Failed to ensure Rosetta session for wiggle");
            return;
        }

        self.update_rosetta_locks(engine, store);

        let target_desc = if SharedState::is_session_mode(&focus) {
            format!("full session ({} entities)", store.count())
        } else {
            SharedState::operation_target(&focus)
                .and_then(|id| store.get(id))
                .map(|te| te.name.clone())
                .unwrap_or_default()
        };

        let orch = self.orchestrator.as_mut().unwrap();
        if orch.try_lock(EntityId(u64::from(lock_id)), OpType::RosettaWiggle).is_some() {
            log::info!(
                "Starting wiggle on {} ({} entities)...",
                target_desc,
                assembly.entities().len()
            );
            if let Err(e) = orch.start_wiggle(assembly) {
                log::error!("Failed to start wiggle: {}", e);
                orch.unlock(EntityId(u64::from(lock_id)));
                return;
            }

            self.ui_dirty |= DirtyFlags::ACTIONS;
        } else {
            log::warn!("Structure is already locked by another operation");
        }
    }

    fn toggle_shake(&mut self, engine: &mut VisoEngine, store: &EntityStore) {
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

        let focus = engine.focus();
        let Some(lock_id) = SharedState::lock_target(&focus, store.loaded_entity()) else {
            log::warn!("No structure available for shake");
            return;
        };

        let Some(combined) = store.combined_assembly_for_backend() else {
            log::warn!("No assembly available for shake");
            return;
        };
        let assembly = combined.assembly.clone();

        if !self.ensure_rosetta_session(store) {
            log::warn!("Failed to ensure Rosetta session for shake");
            return;
        }

        self.update_rosetta_locks(engine, store);

        let target_desc = if SharedState::is_session_mode(&focus) {
            format!("full session ({} entities)", store.count())
        } else {
            SharedState::operation_target(&focus)
                .and_then(|id| store.get(id))
                .map(|te| te.name.clone())
                .unwrap_or_default()
        };

        let orch = self.orchestrator.as_mut().unwrap();
        if orch.try_lock(EntityId(u64::from(lock_id)), OpType::RosettaShake).is_some() {
            log::info!(
                "Starting shake on {} ({} entities)...",
                target_desc,
                assembly.entities().len()
            );
            if let Err(e) = orch.start_shake(assembly) {
                log::error!("Failed to start shake: {}", e);
                orch.unlock(EntityId(u64::from(lock_id)));
                return;
            }

            self.ui_dirty |= DirtyFlags::ACTIONS;
        } else {
            log::warn!("Structure is already locked by another operation");
        }
    }

    // ── ML operations ──

    fn run_prediction(&mut self, engine: &mut VisoEngine, store: &EntityStore) {
        use crate::backend_handler;

        let focus = engine.focus();
        let fallback = store.loaded_entity();
        let Some((target_id, entities)) =
            backend_handler::collect_ml_entities(store, &focus, fallback)
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
            if orch.is_locked(EntityId(u64::from(target_id))) {
                let op = orch.get_op_type(EntityId(u64::from(target_id)));
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

        let total_atoms: usize = entities.iter().map(|e| e.atom_count()).sum();
        log::info!(
            "RF3 prediction: focus={:?}, {} entities, {} total atoms",
            focus, entities.len(), total_atoms,
        );

        // Snapshot the submitted entities' CAs in chain order so the
        // prediction result can be aligned back to the same frame.
        // `extract_chains_from_entities` walks proteins in the same
        // order as RF3's per-chain output.
        self.pending_prediction_reference =
            Some(molex::ops::codec::ca_positions(&entities));

        // Build entity context from the collected entities
        let entity_context = crate::App::build_entity_context(&entities, store, target_id);

        let orch = self.orchestrator.as_mut().unwrap();
        if orch
            .try_lock(EntityId(u64::from(target_id)), OpType::MLPredict)
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
            orch.unlock(EntityId(u64::from(target_id)));
            return;
        }

        self.ui_dirty |= DirtyFlags::ACTIONS | DirtyFlags::LOADING;
    }

    // ── Rosetta session management ──

    pub fn ensure_rosetta_session(&mut self, store: &EntityStore) -> bool {
        use foldit_runner::backends::rosetta::session_state::{
            RosettaSessionState, StructureId as RosettaStructureId,
        };
        use std::collections::HashMap;

        let combined = match store.combined_assembly_for_backend() {
            Some(c) => c,
            None => return false,
        };

        if self.orchestrator.is_none() {
            return false;
        }

        let needs_recreation = match self.orchestrator.as_ref().unwrap().session() {
            None => true,
            Some(state) => {
                let visible = store.visible_residue_counts();
                let visible_rosetta_ids: Vec<RosettaStructureId> = visible
                    .iter()
                    .map(|(id, _)| RosettaStructureId(u64::from(*id)))
                    .collect();
                let residue_counts_rosetta: HashMap<RosettaStructureId, usize> = visible
                    .iter()
                    .map(|(id, count)| (RosettaStructureId(u64::from(*id)), *count))
                    .collect();
                state.topology_changed(&visible_rosetta_ids, &residue_counts_rosetta)
            }
        };

        if needs_recreation {
            log::info!("Recreating Rosetta session (topology changed)");
            let orch = self.orchestrator.as_mut().unwrap();
            if let Err(e) = orch.recreate_session(combined.assembly.clone()) {
                log::error!("Failed to recreate Rosetta session: {}", e);
                return false;
            }

            let structure_ids: Vec<RosettaStructureId> = combined
                .entity_ids
                .iter()
                .map(|id| RosettaStructureId(u64::from(*id)))
                .collect();
            let residue_ranges: HashMap<RosettaStructureId, (usize, usize)> = structure_ids
                .iter()
                .copied()
                .zip(combined.residue_ranges.iter().copied())
                .collect();

            let state = RosettaSessionState::new(structure_ids, residue_ranges);
            log::info!(
                "Session created with {} entities, {} total residues",
                combined.entity_ids.len(),
                state.total_residues
            );
            orch.set_session(state);
        }

        true
    }

    pub(crate) fn update_rosetta_locks(&mut self, engine: &VisoEngine, _store: &EntityStore) {
        let focus = engine.focus();
        let new_focus = SharedState::operation_target(&focus)
            .map(|id| EntityId(u64::from(id)));

        if let Some(ref mut orch) = self.orchestrator {
            orch.update_focus_locks(new_focus);
        }
    }

    // ── Band creation ──

    fn create_band_with_atoms(
        &mut self,
        store: &EntityStore,
        start_residue: i32,
        start_pos: Vec3,
        start_atom_name: &str,
        end_residue: i32,
        end_pos: Vec3,
        end_atom_name: &str,
    ) {
        if !self.ensure_rosetta_session(store) {
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
                        _band_id: band_id,
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
        engine: &mut VisoEngine,
        input: &mut InputProcessor,
        store: &EntityStore,
        button: winit::event::MouseButton,
        pressed: bool,
    ) {
        match button {
            winit::event::MouseButton::Left => {
                self.left_mouse_pressed = pressed;

                if pressed {
                    let hovered = engine.hovered_target();
                    let hovered_residue = hovered.as_residue_i32();
                    if hovered_residue >= 0 {
                        if let Some(ca_pos) = engine.resolve_atom_position(hovered_residue as u32, "CA") {
                            let click_world_pos = engine.screen_to_world_at_depth(
                                Vec2::new(self.last_mouse_pos.0, self.last_mouse_pos.1),
                                ca_pos,
                            );
                            // Use CA as the start position for simplicity
                            let start_pos = ca_pos;

                            self.pull_drag = Some(PullDragState {
                                residue: hovered_residue,
                                start_pos,
                                target_pos: click_world_pos,
                                initial_mouse_pos: self.last_mouse_pos,
                                is_active: false,
                            });
                            log::debug!(
                                "Potential pull on residue {} at {:?}",
                                hovered_residue,
                                start_pos
                            );
                        }
                    }

                    // Record mouse down via unified input
                    if let Some(cmd) = input.handle_event(InputEvent::MouseButton {
                        button: MouseButton::Left,
                        pressed: true,
                    }, hovered) {
                        engine.execute(cmd);
                    }
                } else {
                    // Left button released
                    if let Some(pull) = self.pull_drag.take() {
                        if pull.is_active {
                            // Pull was active — release mouse state without
                            // click detection
                            input.release_mouse_state();
                            log::info!(
                                "Pull released - residue {} pulled to {:?}",
                                pull.residue,
                                pull.target_pos
                            );
                            if let Some(ref orch) = self.orchestrator {
                                orch.cancel_rosetta();
                            }
                        } else {
                            // Pull not activated — process as normal click
                            let hovered = engine.hovered_target();
                            if let Some(cmd) = input.handle_event(InputEvent::MouseButton {
                                button: MouseButton::Left,
                                pressed: false,
                            }, hovered) {
                                engine.execute(cmd);
                            }
                        }
                    } else {
                        // No pull drag — process as normal click
                        let hovered = engine.hovered_target();
                        if let Some(cmd) = input.handle_event(InputEvent::MouseButton {
                            button: MouseButton::Left,
                            pressed: false,
                        }, hovered) {
                            engine.execute(cmd);
                        }
                    }
                }
            }
            winit::event::MouseButton::Right => {
                self.right_mouse_pressed = pressed;

                if pressed {
                    let hovered = engine.hovered_target();
                    let hovered_residue = hovered.as_residue_i32();
                    if hovered_residue >= 0 {
                        if let Some(ca_pos) = engine.resolve_atom_position(hovered_residue as u32, "CA") {
                            self.band_drag = Some(BandDragState {
                                start_residue: hovered_residue,
                                start_atom_pos: ca_pos,
                                start_atom_name: "CA".to_string(),
                                current_mouse_pos: self.last_mouse_pos,
                            });
                            log::info!(
                                "Started band drag from residue {} at {:?}",
                                hovered_residue,
                                ca_pos
                            );
                        }
                    }
                } else {
                    if let Some(drag) = self.band_drag.take() {
                        let hovered = engine.hovered_target();
                        let end_residue = hovered.as_residue_i32();
                        if end_residue >= 0 && end_residue != drag.start_residue {
                            if let Some(ca_pos) =
                                engine.resolve_atom_position(end_residue as u32, "CA")
                            {
                                let end_atom_pos = ca_pos;
                                let end_atom_name = "CA".to_string();

                                self.create_band_with_atoms(
                                    store,
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
        engine: &mut VisoEngine,
        _input: &InputProcessor,
        x: f32,
        y: f32,
    ) {
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
                if let Some(current_ca) = engine.resolve_atom_position(pull.residue as u32, "CA") {
                    pull.start_pos = current_ca;
                }
                pull.target_pos = engine.screen_to_world_at_depth(
                    Vec2::new(x, y),
                    pull.start_pos,
                );
            }
        }

        // Start Rosetta pull when pull becomes active
        if pull_became_active {
            if let Some(ref pull) = self.pull_drag {
                let residue_1indexed = (pull.residue + 1) as u32;
                // Note: pull start_pull needs coords from store, which is
                // not available here. The caller (App) should wire this up.
                log::info!(
                    "Pull activated for residue {} — Rosetta pull requires session coords",
                    residue_1indexed
                );
            }
        }

        // Update pull target during active drag
        if let Some(ref pull) = self.pull_drag {
            if pull.is_active {
                let target = [pull.target_pos.x, pull.target_pos.y, pull.target_pos.z];
                if let Some(ref orch) = self.orchestrator {
                    let _ = orch.update_pull_target(target);
                }
            }
        }

        // Update cursor position on engine for picking
        engine.set_cursor_pos(x, y);

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
    entities: &[molex::MoleculeEntity],
) -> Vec<Vec3> {
    molex::ops::codec::ca_positions(entities)
}

/// Load a file (PDB/CIF/BCIF) and return entities + name.
pub(crate) fn load_file_as_entities(
    path: &str,
) -> Result<(Vec<molex::MoleculeEntity>, String), String> {
    let p = std::path::Path::new(path);
    let name = p
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Unknown")
        .to_string();

    let entities = foldit_rs::puzzle::load_entities_from_file(p)?;
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

/// Build BandInfo from active bands using AtomRef endpoints.
pub(crate) fn build_band_infos(
    active_bands: &std::collections::HashMap<u32, ActiveBand>,
) -> Vec<BandInfo> {
    active_bands
        .values()
        .filter_map(|band| {
            let idx1 = (band.res1 as usize).checked_sub(1)?;
            let idx2 = (band.res2 as usize).checked_sub(1)?;

            Some(BandInfo {
                anchor_a: AtomRef { residue: idx1 as u32, atom_name: band.atom1_name.clone() },
                anchor_b: BandTarget::Atom(AtomRef { residue: idx2 as u32, atom_name: band.atom2_name.clone() }),
                is_pull: band.is_pull,
                is_push: band.is_push,
                is_disabled: band.is_disabled,
                strength: band.strength as f32,
                target_length: band.length as f32,
                band_type: None,
                from_script: false,
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
