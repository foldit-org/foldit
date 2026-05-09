//! Action Router: translates user input into orchestrator/backend commands.
//!
//! Owns routing state (session, orchestrator, bands, pull) and dispatches
//! user actions to the appropriate backend operations. Does NOT handle
//! backend output processing, rendering, or frontend state sync.

use foldit_gui::DirtyFlags;
use crate::entity_store::{EntityRole, EntityStore};
use crate::focus;
use crate::history::{CheckpointKind, WiggleMask};
use viso::{AtomRef, BandInfo, BandTarget, InputEvent, InputProcessor, MouseButton, VisoCommand, VisoEngine};
use foldit_runner::orchestrator::{EntityContextData, EntityId as RunnerEntityId, OpType};
use foldit_runner::Orchestrator;
use glam::{Vec2, Vec3};

/// Request to start a backend ML operation.
///
/// Centralizes the protocol shared by predict / sequence-design /
/// structure-design: lock check → optional rosetta-stop → try_lock →
/// op-specific kickoff → optional preview-mirror → ui_dirty.
pub struct BackendOpRequest {
    /// Entity to lock.
    pub target: RunnerEntityId,
    /// Op type for the lock.
    pub op_type: OpType,
    /// Owned entity context, consumed by the kickoff closure.
    pub entity_context: EntityContextData,
    /// True for Predict: stop the live Rosetta thread and clear the
    /// session before locking, since prediction will rebuild topology.
    pub stop_rosetta_session: bool,
    /// True for Predict: mirror the loaded entity into a preview
    /// (`Predicting...`), hide the loaded entity, and store the
    /// preview id on the router so the streaming/result paths know
    /// where to write.
    pub create_preview_mirror: bool,
    /// CA positions to remember for result alignment (Predict only).
    pub pending_reference_ca: Option<Vec<Vec3>>,
    /// Op-specific submission. Receives the orchestrator + the owned
    /// entity context (which carries the assembly).
    pub kickoff: Box<
        dyn FnOnce(&mut Orchestrator, EntityContextData) -> Result<(), String>,
    >,
}

/// Information about an active band for UI tracking
#[derive(Debug, Clone)]
pub struct ActiveBand {
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
    atom_name: String,
    start_pos: Vec3,
    target_pos: Vec3,
    initial_mouse_pos: (f32, f32),
    is_active: bool,
}

/// Minimum drag distance in pixels to activate a pull (vs treating as click)
const PULL_DRAG_THRESHOLD: f32 = 5.0;

/// Central mediator for action dispatch, owning all routing state.
pub struct ActionRouter {
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
    /// ID of the mirror preview entity backing the active ML op, if any.
    ///
    /// RF3: created at op kickoff (clone of the loaded entity, loaded
    /// hidden); streaming and final paths mutate the preview instead of
    /// the loaded entity, so undo never sees intermediate frames.
    /// RFD3: created lazily on the first streaming frame.
    /// Cleared on final result / cancel.
    pub pending_preview_id: Option<molex::entity::molecule::id::EntityId>,
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
            pending_preview_id: None,
        }
    }

    // ── Helpers ──

    pub fn take_ui_dirty(&mut self) -> DirtyFlags {
        let flags = self.ui_dirty;
        self.ui_dirty = DirtyFlags::empty();
        flags
    }

    fn _get_structure_chains(&self, engine: &VisoEngine, store: &mut EntityStore) -> Vec<(String, String)> {
        let focus = engine.focus();
        let structure_id = focus::operation_target(&focus)
            .map(|raw| store.mint_id(raw))
            .or_else(|| store.loaded_entity());
        if let Some(id) = structure_id {
            if let Some(entity) = store.entity(id) {
                return foldit_runner::orchestrator::chains_from_entities(std::slice::from_ref(entity));
            }
        }
        vec![]
    }

    // ── Read-only accessors for App ──

    pub fn active_bands(&self) -> &std::collections::HashMap<u32, ActiveBand> {
        &self.active_bands
    }

    /// Return pull drag screen-space info for viso's PullInfo.
    /// Returns (residue_index, atom_name, (screen_x, screen_y)) if a pull
    /// is active.
    pub fn pull_drag_info_for_viso(&self) -> Option<(u32, String, (f32, f32))> {
        self.pull_drag.as_ref().and_then(|pull| {
            if pull.is_active {
                Some((
                    pull.residue as u32,
                    pull.atom_name.clone(),
                    self.last_mouse_pos,
                ))
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
                if let Some(current) = engine
                    .resolve_atom_position(pull.residue as u32, &pull.atom_name)
                {
                    pull.start_pos = current;
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
        store: &mut EntityStore,
        action: foldit_gui::ActionId,
    ) -> Option<foldit_gui::ParameterizedAction> {
        use foldit_gui::ActionId;
        let parameterized = match action {
            ActionId::ToggleWiggle => { self.toggle_wiggle(engine, store); None }
            ActionId::ToggleShake => { self.toggle_shake(engine, store); None }
            ActionId::RunPrediction => {
                #[cfg(not(target_arch = "wasm32"))]
                self.run_prediction(engine, store);
                #[cfg(target_arch = "wasm32")]
                { let _ = (engine, store); }
                None
            }
            ActionId::RunMPNN => {
                // Default params — frontend can use ParameterizedAction for custom values
                Some(foldit_gui::ParameterizedAction::RunSequenceDesign {
                    temperature: 0.1,
                    num_sequences: 4,
                })
            }
            ActionId::RunDiffusion => {
                Some(foldit_gui::ParameterizedAction::RunStructureDesign {
                    length: "100-100".to_string(),
                    num_steps: 50,
                    contig: None,
                })
            }
            ActionId::Undo | ActionId::Redo => {
                // Routed by App::handle_trigger_action — main.rs intercepts
                // these so it can call store.undo / store.redo with the
                // engine handle. Reaching this branch means the router
                // got the action without the App-level intercept; that's
                // a bug.
                log::error!("Undo/Redo reached the router; should be handled at App level");
                None
            }
        };
        self.ui_dirty |= DirtyFlags::SCORE | DirtyFlags::ACTIONS | DirtyFlags::UI;
        parameterized
    }

    pub fn cancel_operations(&mut self, engine: &mut VisoEngine, store: &mut EntityStore) {
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
        // Continuous Rosetta actions: cancel = commit. The user keeps
        // whatever the wiggle/shake had reached; the tentative Solution
        // becomes a permanent undo entry. ML preview ops use a different
        // surface (`is_preview`) and are torn down below.
        if store.has_ongoing_action() {
            if let Err(e) = store.commit_action() {
                log::warn!("cancel_operations: commit_action refused: {e}");
            }
        }
        let preview_ids: Vec<molex::entity::molecule::id::EntityId> =
            store.preview_ids().collect();
        if !preview_ids.is_empty() {
            for id in &preview_ids {
                store.remove_preview(*id);
            }
            store.publish_to(engine);
            log::info!("Removed {} in-progress preview entities", preview_ids.len());
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

    fn toggle_wiggle(&mut self, engine: &mut VisoEngine, store: &mut EntityStore) {
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

                    // Toggle-off: commit the ongoing action so all the
                    // intermediate cycles collapse into one undo entry.
                    if store.has_ongoing_action() {
                        if let Err(e) = store.commit_action() {
                            log::warn!("toggle_wiggle: commit_action refused: {e}");
                        }
                    }

                    self.ui_dirty |= DirtyFlags::ACTIONS;
                }
                return;
            }
        }

        let focus = engine.focus();
        let Some(lock_id) =
            focus::lock_target(&focus, store.loaded_entity().map(|id| id.raw()))
        else {
            log::warn!("No structure available for wiggle");
            return;
        };

        let Some(combined) = store.combined_assembly_for_backend() else {
            log::warn!("No assembly available for wiggle");
            return;
        };
        let assembly = combined.assembly.clone();
        let session_entity_ids: Vec<molex::entity::molecule::id::EntityId> =
            combined.entity_ids.clone();

        if !self.ensure_rosetta_session(store) {
            log::warn!("Failed to ensure Rosetta session for wiggle");
            return;
        }

        self.update_rosetta_locks(engine, store);

        let target_desc = if focus::is_session_mode(&focus) {
            format!("full session ({} entities)", store.count())
        } else {
            focus::operation_target(&focus)
                .and_then(|id| {
                    let eid = store.mint_id(id);
                    store.metadata(eid).map(|m| m.name.clone())
                })
                .unwrap_or_default()
        };

        let orch = self.orchestrator.as_mut().unwrap();
        if orch.try_lock(RunnerEntityId(u64::from(lock_id)), OpType::RosettaWiggle).is_some() {
            log::info!(
                "Starting wiggle on {} ({} entities)...",
                target_desc,
                assembly.entities().len()
            );
            if let Err(e) = orch.start_wiggle(assembly) {
                log::error!("Failed to start wiggle: {}", e);
                orch.unlock(RunnerEntityId(u64::from(lock_id)));
                return;
            }

            // Toggle-on: begin an ongoing action on the lock target.
            // The new History API's begin_action takes the entity from
            // the kind; cycle writebacks land via action_update; commit
            // happens on toggle-off or Esc.
            let target_eid = store.mint_id(lock_id);
            log::info!(
                "toggle_wiggle: begin_action on entity {} (session has {} eids)",
                target_eid.raw(),
                session_entity_ids.len(),
            );
            match store.begin_action(
                CheckpointKind::Wiggle {
                    entity: target_eid,
                    mask: WiggleMask {
                        backbone: true,
                        sidechains: true,
                    },
                    duration_ms: 0,
                },
                format!("Wiggle {target_desc}"),
            ) {
                Ok(id) => log::info!("toggle_wiggle: begin_action Ok, tentative {:?}", id),
                Err(e) => log::warn!("toggle_wiggle: begin_action refused: {e}"),
            }

            self.ui_dirty |= DirtyFlags::ACTIONS;
        } else {
            log::warn!("Structure is already locked by another operation");
        }
    }

    fn toggle_shake(&mut self, engine: &mut VisoEngine, store: &mut EntityStore) {
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

                    if store.has_ongoing_action() {
                        if let Err(e) = store.commit_action() {
                            log::warn!("toggle_shake: commit_action refused: {e}");
                        }
                    }

                    self.ui_dirty |= DirtyFlags::ACTIONS;
                }
                return;
            }
        }

        let focus = engine.focus();
        let Some(lock_id) =
            focus::lock_target(&focus, store.loaded_entity().map(|id| id.raw()))
        else {
            log::warn!("No structure available for shake");
            return;
        };

        let Some(combined) = store.combined_assembly_for_backend() else {
            log::warn!("No assembly available for shake");
            return;
        };
        let assembly = combined.assembly.clone();
        let session_entity_ids: Vec<molex::entity::molecule::id::EntityId> =
            combined.entity_ids.clone();

        if !self.ensure_rosetta_session(store) {
            log::warn!("Failed to ensure Rosetta session for shake");
            return;
        }

        self.update_rosetta_locks(engine, store);

        let target_desc = if focus::is_session_mode(&focus) {
            format!("full session ({} entities)", store.count())
        } else {
            focus::operation_target(&focus)
                .and_then(|id| {
                    let eid = store.mint_id(id);
                    store.metadata(eid).map(|m| m.name.clone())
                })
                .unwrap_or_default()
        };

        let orch = self.orchestrator.as_mut().unwrap();
        if orch.try_lock(RunnerEntityId(u64::from(lock_id)), OpType::RosettaShake).is_some() {
            log::info!(
                "Starting shake on {} ({} entities)...",
                target_desc,
                assembly.entities().len()
            );
            if let Err(e) = orch.start_shake(assembly) {
                log::error!("Failed to start shake: {}", e);
                orch.unlock(RunnerEntityId(u64::from(lock_id)));
                return;
            }

            let target_eid = store.mint_id(lock_id);
            let _ = session_entity_ids; // session covers more, but the
                                        // running action targets one entity
                                        // (Shake is per-entity by kind).
            if let Err(e) = store.begin_action(
                CheckpointKind::Shake {
                    entity: target_eid,
                    duration_ms: 0,
                },
                format!("Shake {target_desc}"),
            ) {
                log::warn!("begin_action(Shake) refused: {e}");
            }

            self.ui_dirty |= DirtyFlags::ACTIONS;
        } else {
            log::warn!("Structure is already locked by another operation");
        }
    }

    // ── Pull (drag-driven Rosetta minimization) ──

    fn start_rosetta_pull(&mut self, engine: &VisoEngine, store: &mut EntityStore) {
        let (residue_idx, residue_1indexed, atom_name, target) = {
            let Some(pull) = self.pull_drag.as_ref() else { return };
            let res_u32 = pull.residue as u32;
            (
                res_u32,
                res_u32 + 1,
                pull.atom_name.clone(),
                [pull.target_pos.x, pull.target_pos.y, pull.target_pos.z],
            )
        };
        let is_sidechain = !molex::chemistry::is_protein_backbone_atom_name(&atom_name);

        if self.orchestrator.is_none() {
            log::warn!("Orchestrator not initialized; cannot start pull");
            return;
        }
        if self.orchestrator.as_ref().unwrap().is_rosetta_running() {
            log::warn!("Rosetta op already running; ignoring pull");
            return;
        }

        let focus = engine.focus();
        let Some(lock_id) =
            focus::lock_target(&focus, store.loaded_entity().map(|id| id.raw()))
        else {
            log::warn!("No structure available for pull");
            return;
        };

        let Some(combined) = store.combined_assembly_for_backend() else {
            log::warn!("No assembly available for pull");
            return;
        };
        let assembly = combined.assembly.clone();

        if !self.ensure_rosetta_session(store) {
            log::warn!("Failed to ensure Rosetta session for pull");
            return;
        }
        self.update_rosetta_locks(engine, store);

        let runner_id = RunnerEntityId(u64::from(lock_id));
        let orch = self.orchestrator.as_mut().unwrap();
        if orch.try_lock(runner_id, OpType::RosettaPull).is_none() {
            log::warn!("Failed to lock entity for pull");
            return;
        }
        let kickoff = if is_sidechain {
            orch.start_pull_sidechain(assembly, residue_1indexed, atom_name.clone(), target)
        } else {
            orch.start_pull(assembly, residue_1indexed, target)
        };
        if let Err(e) = kickoff {
            log::error!("Failed to start pull: {}", e);
            orch.unlock(runner_id);
            return;
        }

        let target_eid = store.mint_id(lock_id);
        let label = if is_sidechain {
            format!("Pull sidechain {residue_1indexed}.{atom_name}")
        } else {
            format!("Pull residue {residue_1indexed}")
        };
        if let Err(e) = store.begin_action(
            CheckpointKind::ManualMove {
                entity: target_eid,
                residues: residue_idx..residue_idx + 1,
            },
            label,
        ) {
            log::warn!("begin_action(ManualMove) refused: {e}");
        }

        self.ui_dirty |= DirtyFlags::ACTIONS;
        if is_sidechain {
            log::info!("Started sidechain pull on residue {residue_1indexed}.{atom_name}");
        } else {
            log::info!("Started backbone pull on residue {residue_1indexed} ({atom_name})");
        }
    }

    fn finish_rosetta_pull(&mut self, store: &mut EntityStore) {
        if let Some(ref mut orch) = self.orchestrator {
            let pull_locks: Vec<RunnerEntityId> = orch
                .locked_entities()
                .into_iter()
                .filter(|&eid| matches!(orch.get_op_type(eid), Some(OpType::RosettaPull)))
                .collect();
            for eid in &pull_locks {
                orch.cancel_entity(*eid);
            }
            orch.cancel_rosetta();
            for eid in pull_locks {
                orch.unlock(eid);
            }
        }

        if store.has_ongoing_action() {
            if let Err(e) = store.commit_action() {
                log::warn!("commit_action(pull) refused: {e}");
            }
        }

        self.ui_dirty |= DirtyFlags::ACTIONS;
    }

    // ── ML operations ──

    /// Run the shared kickoff protocol for an ML backend op.
    ///
    /// Order: lock check → (optional) stop rosetta + clear session →
    /// try_lock → kickoff → (optional) preview mirror → ui_dirty.
    pub fn start_op(
        &mut self,
        request: BackendOpRequest,
        engine: &mut VisoEngine,
        store: &mut EntityStore,
    ) {
        let Some(ref mut orch) = self.orchestrator else {
            log::warn!("Orchestrator not initialized");
            return;
        };

        if orch.is_locked(request.target) {
            let op = orch.get_op_type(request.target);
            log::warn!(
                "Structure is locked by {:?}, cannot start {:?}",
                op, request.op_type,
            );
            return;
        }

        if request.stop_rosetta_session {
            orch.stop_rosetta();
            orch.clear_session();
        }

        if orch.try_lock(request.target, request.op_type).is_none() {
            log::warn!("Failed to acquire lock for {:?}", request.op_type);
            return;
        }

        if let Some(ca) = request.pending_reference_ca {
            self.pending_prediction_reference = Some(ca);
        }

        if let Err(e) = (request.kickoff)(orch, request.entity_context) {
            log::error!("Failed to submit {:?}: {}", request.op_type, e);
            orch.unlock(request.target);
            return;
        }

        if request.create_preview_mirror {
            if let Some(loaded_id) = store.loaded_entity() {
                let mirror = store.entity(loaded_id).cloned().zip(
                    store.metadata(loaded_id).map(|m| m.origin.clone()),
                );
                if let Some((mirror, origin)) = mirror {
                    let preview_id = store.insert_preview(
                        mirror,
                        "Predicting...".to_string(),
                        origin,
                        EntityRole { foldable: false, designable: false, ambient: false },
                    );
                    engine.set_entity_visible(loaded_id.raw(), false);
                    self.pending_preview_id = Some(preview_id);
                    store.publish_to(engine);
                    log::info!(
                        "Created RF3 preview {} mirroring loaded entity {}",
                        preview_id.raw(), loaded_id.raw(),
                    );
                }
            }
        }

        self.ui_dirty |= DirtyFlags::ACTIONS | DirtyFlags::LOADING;
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn run_prediction(&mut self, engine: &mut VisoEngine, store: &mut EntityStore) {
        use crate::backend_results;

        let focus = engine.focus();
        let fallback = store.loaded_entity();
        let Some((target_id, entities)) =
            backend_results::collect_ml_entities(store, &focus, fallback)
        else {
            log::warn!("No structure available for prediction");
            return;
        };

        let chains = foldit_runner::orchestrator::chains_from_entities(&entities);
        if chains.is_empty() {
            log::warn!("No sequence/chains available");
            return;
        }

        let total_atoms: usize = entities.iter().map(|e| e.atom_count()).sum();
        log::info!(
            "RF3 prediction: focus={:?}, {} entities, {} total atoms",
            focus, entities.len(), total_atoms,
        );

        let total_residues: usize = chains.iter().map(|(_, s)| s.len()).sum();
        log::info!(
            "Starting RoseTTAFold3 prediction for {} residues...",
            total_residues,
        );

        let pending_ca = molex::ops::codec::ca_positions(&entities);
        let entity_context = build_entity_context(entities, store, target_id, None);

        log::info!(
            "Passing assembly context ({} entities, {} entities in assembly)",
            entity_context.entities.len(),
            entity_context.assembly.entities().len(),
        );

        self.start_op(
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
                    .map(|(id, _)| RosettaStructureId(u64::from(id.raw())))
                    .collect();
                let residue_counts_rosetta: HashMap<RosettaStructureId, usize> = visible
                    .iter()
                    .map(|(id, count)| (RosettaStructureId(u64::from(id.raw())), *count))
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
                .map(|id| RosettaStructureId(u64::from(id.raw())))
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

    pub fn update_rosetta_locks(&mut self, engine: &VisoEngine, _store: &EntityStore) {
        let focus = engine.focus();
        let new_focus = focus::operation_target(&focus)
            .map(|id| RunnerEntityId(u64::from(id)));

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
        store: &mut EntityStore,
        button: MouseButton,
        pressed: bool,
    ) {
        match button {
            MouseButton::Left => {
                self.left_mouse_pressed = pressed;

                if pressed {
                    let hovered = engine.hovered_target();
                    let hovered_residue = hovered.as_residue_i32();
                    if hovered_residue >= 0 {
                        let atom_name = engine
                            .closest_atom_in_residue(
                                hovered_residue as u32,
                                self.last_mouse_pos,
                            )
                            .unwrap_or_else(|| "CA".to_string());
                        if let Some(start_pos) = engine.resolve_atom_position(
                            hovered_residue as u32,
                            &atom_name,
                        ) {
                            let click_world_pos = engine.screen_to_world_at_depth(
                                Vec2::new(self.last_mouse_pos.0, self.last_mouse_pos.1),
                                start_pos,
                            );

                            self.pull_drag = Some(PullDragState {
                                residue: hovered_residue,
                                atom_name: atom_name.clone(),
                                start_pos,
                                target_pos: click_world_pos,
                                initial_mouse_pos: self.last_mouse_pos,
                                is_active: false,
                            });
                            log::debug!(
                                "Potential pull on residue {}.{} at {:?}",
                                hovered_residue,
                                atom_name,
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
                            self.finish_rosetta_pull(store);
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
            MouseButton::Right => {
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
        store: &mut EntityStore,
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
                if let Some(current) = engine
                    .resolve_atom_position(pull.residue as u32, &pull.atom_name)
                {
                    pull.start_pos = current;
                }
                pull.target_pos = engine.screen_to_world_at_depth(
                    Vec2::new(x, y),
                    pull.start_pos,
                );
            }
        }

        // Start Rosetta pull when pull becomes active
        if pull_became_active {
            self.start_rosetta_pull(engine, store);
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
pub fn entities_backbone_ca(
    entities: &[molex::MoleculeEntity],
) -> Vec<Vec3> {
    molex::ops::codec::ca_positions(entities)
}

/// Build BandInfo from active bands using AtomRef endpoints.
pub fn build_band_infos(
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
pub fn build_actions_list(
    orchestrator: &Option<Orchestrator>,
) -> Vec<foldit_gui::state::ActionInfo> {
    use foldit_gui::state::ActionInfo;

    let orch = match orchestrator {
        Some(o) => o,
        None => return vec![],
    };

    let locked: Vec<RunnerEntityId> = orch.locked_entities();
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
pub fn trajectory_path_from_args() -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.windows(2).find_map(|w| {
        if w[0] == "--trajectory" {
            Some(w[1].clone())
        } else {
            None
        }
    })
}

/// Build the per-entity context bundle that ML-side ops (predict, MPNN,
/// RFD3) require. Pure helper — no router/app state. Originally lived as
/// `impl App::build_entity_context` in foldit-desktop; relocated here to
/// keep the call site host-agnostic.
pub fn build_entity_context(
    entities: Vec<molex::MoleculeEntity>,
    store: &EntityStore,
    entity_id: molex::entity::molecule::id::EntityId,
    focused_entity_id: Option<u32>,
) -> foldit_runner::orchestrator::EntityContextData {
    use foldit_runner::orchestrator::{EntityContextData, EntityRoleHint};
    let target_role = store.entity_meta(entity_id).map(|(_, r)| r.clone());
    EntityContextData::from_entities(entities, focused_entity_id, |raw_id| {
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
