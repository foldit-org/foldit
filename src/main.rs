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
//!   V - Cycle sheet style (Ribbon/ClassicRibbon/RMF)
//!   Q - Toggle backbone quality (high/low)
//!   H - Toggle visibility of designed structures
//!   Tab - Cycle focus (Session -> Structure 1 -> ... -> Session)
//!   Delete - Remove last added structure
//!   Esc - Cancel operation / clear selection / clear bands
//!   Left-drag on residue - Pull (coming soon)
//!   Right-drag residue to residue - Create band
//!   Mouse - Rotate/zoom camera

mod window;

use foldit_rs::action_manager::{ActionManager, ActionType};
use foldit_frontend::DirtyFlags;
use foldit_rs::ml_runner::{MLResult, MLRunner, MLTask, IntermediateUpdate};
use foldit_rs::rosetta::{RosettaExecutor, RosettaUpdate, RosettaSessionState, RosettaStructureId};
use foldit_rs::scene::{Scene, Structure, StructureId, CombinedCoordsResult};
use foldit_rs::session::Session;
use foldit_rs::visual_effects::VisualEffect;
use std::collections::HashMap;
use foldit_render::band_renderer::BandRenderInfo;
use foldit_render::pull_renderer::PullRenderInfo;
use foldit_conv::coords::{
    align_coords_bytes, extract_ca_from_chains, get_closest_atom_for_residue,
    get_closest_atom_with_name, kabsch_alignment_with_scale,
};
use glam::Vec3;

use foldit_render::animation::AnimationAction;
use foldit_render::engine::ProteinRenderEngine;
use foldit_rs::render_snapshot::{self, RenderSnapshot, SnapshotWriter, SnapshotReader};
use std::sync::Arc;
use tokio::sync::mpsc;
use winit::event::MouseScrollDelta;
use winit::keyboard::{KeyCode, ModifiersState};
use winit::window::Window;

/// Information about an active band for UI tracking
#[derive(Debug, Clone)]
struct ActiveBand {
    band_id: u32,
    res1: u32,
    /// Atom name for first endpoint (e.g., "CA", "CB", "CG")
    atom1_name: String,
    res2: u32,
    /// Atom name for second endpoint
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
    /// Residue where drag started (0-indexed)
    start_residue: i32,
    /// Name of the closest atom at drag start (e.g., "CA", "CB")
    start_atom_name: String,
    /// World position of closest atom at drag start
    start_atom_pos: Vec3,
    /// Current mouse position during drag
    current_mouse_pos: (f32, f32),
}

/// State for pull action via left-click drag
#[derive(Debug, Clone)]
struct PullDragState {
    /// Residue being pulled (0-indexed)
    residue: i32,
    /// Starting position (closest backbone atom to click)
    start_pos: Vec3,
    /// Current pull target position (world space)
    target_pos: Vec3,
    /// Initial mouse position when drag started (for absolute positioning)
    initial_mouse_pos: (f32, f32),
    /// Whether the pull is active (user dragged past threshold)
    /// If false, this is just a potential pull that may become a click
    is_active: bool,
}

/// Minimum drag distance in pixels to activate a pull (vs treating as click)
const PULL_DRAG_THRESHOLD: f32 = 5.0;

/// Main application state
pub(crate) struct App {
    engine: Option<ProteinRenderEngine>,
    scene: Scene,
    /// Session tracks structure relationships and operation targeting
    session: Session,
    ml_runner: Option<MLRunner>,
    ml_updates: Option<mpsc::Receiver<IntermediateUpdate>>,
    ml_results: Option<mpsc::Receiver<MLResult>>,
    rosetta_executor: Option<RosettaExecutor>,
    rosetta_updates: Option<mpsc::Receiver<RosettaUpdate>>,
    /// Per-structure action locking (used for BOTH Rosetta AND ML operations)
    action_manager: ActionManager,
    /// Unified Rosetta session state: tracks structures, residue ranges, and focus locks
    rosetta_state: Option<RosettaSessionState>,
    effect: VisualEffect,
    last_mouse_pos: (f32, f32),
    pdb_path: String,
    /// Pending animation action (None = no update needed)
    pending_action: Option<AnimationAction>,
    /// Active bands for visualization and management
    active_bands: HashMap<u32, ActiveBand>,
    /// Right-click drag state for band creation
    band_drag: Option<BandDragState>,
    /// Whether right mouse button is currently pressed
    right_mouse_pressed: bool,
    /// Left-click drag state for pull action
    pull_drag: Option<PullDragState>,
    /// Whether left mouse button is currently pressed
    left_mouse_pressed: bool,
    /// Triple-buffer writer for render snapshots (mutation side)
    snapshot_writer: SnapshotWriter,
    /// Triple-buffer reader for render snapshots (render side)
    snapshot_reader: SnapshotReader,
    /// Accumulated dirty flags from mutations
    ui_dirty: DirtyFlags,
    /// Latest score from Rosetta updates (tracked for frontend push)
    latest_score: Option<f64>,
    /// Whether the next snapshot should trigger a camera fit
    pending_fit_camera: bool,
}

impl App {
    pub(crate) fn new(pdb_path: String) -> Self {
        let (snapshot_writer, snapshot_reader) = render_snapshot::create_snapshot_buffer();
        Self {
            engine: None,
            scene: Scene::new(),
            session: Session::new(),
            ml_runner: None,
            ml_updates: None,
            ml_results: None,
            rosetta_executor: None,
            rosetta_updates: None,
            action_manager: ActionManager::new(),
            rosetta_state: None,
            effect: VisualEffect::None,
            last_mouse_pos: (0.0, 0.0),
            pdb_path,
            pending_action: None,
            active_bands: HashMap::new(),
            band_drag: None,
            right_mouse_pressed: false,
            pull_drag: None,
            left_mouse_pressed: false,
            snapshot_writer,
            snapshot_reader,
            ui_dirty: DirtyFlags::empty(),
            latest_score: None,
            pending_fit_camera: false,
        }
    }

    /// Build a RenderSnapshot from current App state and flush to the triple-buffer.
    ///
    /// Call this after processing mutations (ML updates, Rosetta updates, etc.).
    /// The snapshot captures scene geometry + animation action + band/pull state.
    fn flush_render_snapshot(&mut self) {
        // Check if there's a scene geometry change (pending_action) or
        // band/pull changes that need to be communicated to the render path.
        let action = self.pending_action.take();
        let has_geometry_change = action.is_some();

        if !has_geometry_change && !self.snapshot_writer.is_dirty() {
            return;
        }

        // Build scene data only when geometry changed
        let (backbone_chains, sidechain_positions, sidechain_hydrophobicity,
             sidechain_residue_indices, sidechain_atom_names,
             sidechain_bonds, backbone_sidechain_bonds, all_positions,
             ss_types) = if has_geometry_change {
            let data = self.scene.aggregated();
            (
                data.backbone_chains.clone(),
                data.sidechain_positions.clone(),
                data.sidechain_hydrophobicity.clone(),
                data.sidechain_residue_indices.clone(),
                data.sidechain_atom_names.clone(),
                data.sidechain_bonds.clone(),
                data.backbone_sidechain_bonds.clone(),
                data.all_positions.clone(),
                data.ss_types.clone(),
            )
        } else {
            Default::default()
        };

        let snapshot = RenderSnapshot {
            pending_action: action,
            backbone_chains,
            sidechain_positions,
            sidechain_hydrophobicity,
            sidechain_residue_indices,
            sidechain_atom_names,
            sidechain_bonds,
            backbone_sidechain_bonds,
            all_positions,
            ss_types,
            bands: Vec::new(),
            bands_dirty: false,
            pull: None,
            pull_dirty: false,
            fit_camera: std::mem::take(&mut self.pending_fit_camera),
            generation: 0, // set by writer
        };

        self.snapshot_writer.force_write(snapshot);
    }

    /// Read the latest render snapshot and apply it to the engine.
    ///
    /// This is the render-side consumption of the triple-buffer. The engine
    /// receives scene geometry via the snapshot rather than reading App state
    /// directly, establishing the decoupling pattern.
    fn apply_pending_snapshot(&mut self) {
        let snapshot = match self.snapshot_reader.try_read() {
            Some(snap) => snap,
            None => return,
        };

        let engine = match &mut self.engine {
            Some(e) => e,
            None => return,
        };

        // Apply scene geometry change
        if let Some(action) = snapshot.pending_action {
            engine.animate_to_full_pose_with_action(
                &snapshot.backbone_chains,
                &snapshot.sidechain_positions,
                &snapshot.sidechain_bonds,
                &snapshot.sidechain_hydrophobicity,
                &snapshot.sidechain_residue_indices,
                &snapshot.sidechain_atom_names,
                &snapshot.backbone_sidechain_bonds,
                action,
            );

            // Apply SS override if present
            if let Some(ref ss) = snapshot.ss_types {
                engine.set_ss_override(ss);
            }
        }

        // Fit camera to all positions when requested (e.g., structure load)
        if snapshot.fit_camera {
            engine.fit_camera_to_positions(&snapshot.all_positions);
        }
    }

    /// Convenience: flush snapshot and immediately apply (single-threaded path).
    ///
    /// Used in the single-threaded path where mutations and rendering
    /// happen on the same thread. When rendering moves to a separate thread,
    /// flush and apply will be called independently.
    fn sync_engine_with_scene(&mut self) {
        self.flush_render_snapshot();
        self.apply_pending_snapshot();
    }

    /// Take and clear accumulated UI dirty flags from mutations.
    fn take_ui_dirty(&mut self) -> DirtyFlags {
        let flags = self.ui_dirty;
        self.ui_dirty = DirtyFlags::empty();
        flags
    }

    /// Build the current actions list from the action manager state.
    /// Each action reports whether it's enabled (can be started) and active (running).
    fn build_actions_list(&self) -> Vec<foldit_frontend::state::ActionInfo> {
        use foldit_frontend::state::ActionInfo;

        let locked = self.action_manager.locked_structures();
        let has_any_lock = !locked.is_empty();
        let has_rosetta_op = locked.iter().any(|&id| {
            matches!(
                self.action_manager.get_action_type(id),
                Some(ActionType::RosettaWiggle) | Some(ActionType::RosettaShake)
            )
        });
        let has_ml_op = locked.iter().any(|&id| {
            matches!(
                self.action_manager.get_action_type(id),
                Some(ActionType::MLPredict) | Some(ActionType::MLSequenceDesign) | Some(ActionType::MLStructureDesign)
            )
        });

        vec![
            ActionInfo {
                id: 0, // ToggleWiggle
                name: "Wiggle".into(),
                enabled: !has_ml_op, // Can toggle off if Rosetta is running
                active: has_rosetta_op && locked.iter().any(|&id|
                    self.action_manager.get_action_type(id) == Some(ActionType::RosettaWiggle)),
            },
            ActionInfo {
                id: 1, // ToggleShake
                name: "Shake".into(),
                enabled: !has_ml_op,
                active: has_rosetta_op && locked.iter().any(|&id|
                    self.action_manager.get_action_type(id) == Some(ActionType::RosettaShake)),
            },
            ActionInfo {
                id: 2, // RunPrediction
                name: "Predict".into(),
                enabled: !has_any_lock,
                active: locked.iter().any(|&id|
                    self.action_manager.get_action_type(id) == Some(ActionType::MLPredict)),
            },
            ActionInfo {
                id: 3, // RunMPNN
                name: "MPNN".into(),
                enabled: !has_any_lock,
                active: locked.iter().any(|&id|
                    self.action_manager.get_action_type(id) == Some(ActionType::MLSequenceDesign)),
            },
            ActionInfo {
                id: 4, // RunDiffusion
                name: "Diffusion".into(),
                enabled: !has_any_lock,
                active: locked.iter().any(|&id|
                    self.action_manager.get_action_type(id) == Some(ActionType::MLStructureDesign)),
            },
            ActionInfo {
                id: 5, // ToggleViewMode
                name: "View Mode".into(),
                enabled: true,
                active: false,
            },
            ActionInfo {
                id: 7, // ToggleDesignedStructures
                name: "Toggle Designs".into(),
                enabled: self.scene.len() > 1,
                active: false,
            },
            ActionInfo {
                id: 8, // CycleFocus
                name: "Cycle Focus".into(),
                enabled: self.scene.len() > 1,
                active: false,
            },
            ActionInfo {
                id: 9, // RemoveStructure
                name: "Remove".into(),
                enabled: self.scene.len() > 1,
                active: false,
            },
            ActionInfo {
                id: 10, // Cancel
                name: "Cancel".into(),
                enabled: has_any_lock || !self.active_bands.is_empty(),
                active: false,
            },
        ]
    }

    /// Convert flat backbone positions (N, CA, C, O per residue) to backbone chains
    fn positions_to_backbone_chains(positions: &[Vec3]) -> Vec<Vec<Vec3>> {
        if positions.is_empty() {
            return vec![];
        }

        // RFD3 outputs 4 atoms per residue: N, CA, C, O
        // We want N, CA, C for the spline (skip O)
        let mut chain: Vec<Vec3> = Vec::new();

        for chunk in positions.chunks(4) {
            // Add N, CA, C (indices 0, 1, 2), skip O (index 3)
            for (i, &pos) in chunk.iter().enumerate() {
                if i < 3 {
                    chain.push(pos);
                }
            }
        }

        if chain.is_empty() {
            vec![]
        } else {
            vec![chain]
        }
    }

    /// Update structure with intermediate ML positions
    fn update_animation_structure(&mut self, update: &IntermediateUpdate) {
        log::debug!(
            "update_animation_structure: step {}/{}, has_coords={}, backbone_positions={}",
            update.step,
            update.total_steps,
            update.coords_bytes.is_some(),
            update.backbone_positions.len()
        );

        // SimpleFold: update full structure including sidechains, with scale correction
        if let Some(ref coords_bytes) = update.coords_bytes {
            if let Some(orig_id) = self.session.original {
                // Parse full structure first
                match Structure::from_coords_bytes(
                    format!("Predicting... ({}/{})", update.step, update.total_steps),
                    coords_bytes,
                    update.confidence,
                ) {
                    Ok(mut new_data) => {
                        // Compute scale + alignment from CA positions
                        if let Some(ref original_ca) = self.session.original_backbone_ca {
                            let predicted_ca = extract_ca_from_chains(&new_data.backbone_chains);
                            if let Some((rotation, translation, scale)) = kabsch_alignment_with_scale(original_ca, &predicted_ca) {
                                // Apply scale, rotation, translation to backbone
                                for chain in &mut new_data.backbone_chains {
                                    for pos in chain.iter_mut() {
                                        *pos = rotation * (*pos * scale) + translation;
                                    }
                                }
                                // Apply to sidechain atoms
                                for atom in &mut new_data.sidechain_atoms {
                                    atom.position = rotation * (atom.position * scale) + translation;
                                }
                                // Apply to backbone-sidechain bond CA positions
                                for bond in &mut new_data.backbone_sidechain_bonds {
                                    bond.ca_position = rotation * (bond.ca_position * scale) + translation;
                                }
                                log::debug!("Applied Kabsch+scale ({:.3}) for frame {}", scale, update.step);
                            }
                        }

                        if let Some(structure) = self.scene.get_mut(orig_id) {
                            structure.backbone_chains = new_data.backbone_chains;
                            structure.sidechain_atoms = new_data.sidechain_atoms;
                            structure.sidechain_bonds = new_data.sidechain_bonds;
                            structure.backbone_sidechain_bonds = new_data.backbone_sidechain_bonds;
                            structure.name = new_data.name;
                            log::info!("Updated frame {}/{} ({} sidechains)",
                                update.step, update.total_steps, structure.sidechain_atoms.len());
                        }
                        self.pending_action = Some(AnimationAction::Diffusion);
                    }
                    Err(e) => {
                        log::warn!("Failed to parse intermediate: {}", e);
                    }
                }
            }
            return;
        }

        // RFD3: uses backbone_positions and needs animation structure (new design)
        if !update.backbone_positions.is_empty() {
            let backbone_chains = Self::positions_to_backbone_chains(&update.backbone_positions);
            if backbone_chains.is_empty() || backbone_chains[0].is_empty() {
                log::warn!("Empty backbone chains, skipping update");
                return;
            }

            if let Some(anim_id) = self.session.animation_structure {
                if let Some(structure) = self.scene.get_mut(anim_id) {
                    structure.backbone_chains = backbone_chains;
                    structure.name = format!("Designing... ({}/{})", update.step, update.total_steps);
                    log::info!("Updated animation frame {}/{}", update.step, update.total_steps);
                }
            } else {
                let structure = Structure::from_backbone_design(
                    format!("Designing... ({}/{})", update.step, update.total_steps),
                    backbone_chains,
                    update.confidence,
                );
                let id = self.scene.add(structure);
                self.session.on_animation_structure_created(id);
                log::info!("Created animation structure {:?}", id);

                // Smoothly animate camera to show all structures
                if let Some(engine) = &mut self.engine {
                    let data = self.scene.aggregated();
                    engine.fit_camera_to_positions_animated(&data.all_positions);
                }
            }
            self.pending_action = Some(AnimationAction::Diffusion);
            return;
        }

        log::warn!("No coordinates in update, skipping");
    }

    /// Process pending ML updates (non-blocking)
    fn process_ml_updates(&mut self) {
        // Collect intermediate updates first (to avoid borrow issues)
        let pending_updates: Vec<IntermediateUpdate> = self
            .ml_updates
            .as_mut()
            .map(|updates| {
                let mut collected = Vec::new();
                while let Ok(update) = updates.try_recv() {
                    collected.push(update);
                }
                collected
            })
            .unwrap_or_default();

        // Process collected updates
        for update in pending_updates {
            let has_data = update.coords_bytes.is_some() || !update.backbone_positions.is_empty();
            log::info!(
                "ML update: step {}/{}, confidence {:.2}, has_coords={}, backbone_positions={}",
                update.step,
                update.total_steps,
                update.confidence,
                update.coords_bytes.is_some(),
                update.backbone_positions.len()
            );

            // Update progress effect and loading state for frontend
            if let VisualEffect::Progress { .. } = &mut self.effect {
                self.effect.update_progress(update.step, update.total_steps);
            }
            self.ui_dirty |= DirtyFlags::LOADING;

            // Animate intermediate structure if we have data
            if has_data {
                self.update_animation_structure(&update);
            }
        }

        // Check for final results
        if let Some(ref mut results) = self.ml_results {
            while let Ok(result) = results.try_recv() {
                match result {
                    MLResult::Predict { coords_bytes, confidence } => {
                        log::info!("Prediction complete! Confidence: {:.2}", confidence);

                        // Remove animation structure if exists
                        if let Some(anim_id) = self.session.animation_structure {
                            self.scene.remove(anim_id);
                            self.session.on_animation_structure_removed();
                        }

                        // Update original structure in place with predicted coords
                        if let Some(orig_id) = self.session.original {
                            // Apply Kabsch alignment to the raw COORDS before parsing
                            // This ensures coords_bytes and visual representation are in sync
                            let aligned_coords_bytes = if let Some(ref original_ca) = self.session.original_backbone_ca {
                                match align_coords_bytes(&coords_bytes, original_ca) {
                                    Ok(aligned) => {
                                        log::info!("Applied Kabsch alignment to COORDS data");
                                        aligned
                                    }
                                    Err(e) => {
                                        log::warn!("Failed to align coords: {}, using original", e);
                                        coords_bytes.clone()
                                    }
                                }
                            } else {
                                coords_bytes.clone()
                            };

                            match Structure::from_coords_bytes(
                                format!("SimpleFold ({:.0}%)", confidence * 100.0),
                                &aligned_coords_bytes,
                                confidence,
                            ) {
                                Ok(new_data) => {
                                    if let Some(structure) = self.scene.get_mut(orig_id) {
                                        structure.name = new_data.name;
                                        structure.backbone_chains = new_data.backbone_chains;
                                        structure.sidechain_atoms = new_data.sidechain_atoms;
                                        structure.sidechain_bonds = new_data.sidechain_bonds;
                                        structure.backbone_sidechain_bonds = new_data.backbone_sidechain_bonds;
                                        structure.sequence = new_data.sequence;
                                        structure.coords = new_data.coords;
                                        structure.visible = true;
                                        log::info!(
                                            "Updated structure with prediction: {} residues, {} sidechain atoms",
                                            structure.sequence.len(),
                                            structure.sidechain_atoms.len()
                                        );
                                    }
                                    self.pending_action = Some(AnimationAction::Diffusion);
                                }
                                Err(e) => {
                                    log::error!("Failed to parse prediction: {}", e);
                                    // Show original again on error
                                    if let Some(structure) = self.scene.get_mut(orig_id) {
                                        structure.visible = true;
                                    }
                                }
                            }
                            // Unlock the structure now that prediction is complete
                            self.action_manager.unlock(orig_id);
                        }

                        self.effect = VisualEffect::None;
                        self.ui_dirty |= DirtyFlags::LOADING | DirtyFlags::ACTIONS;
                    }

                    MLResult::SequenceDesign { sequences, scores } => {
                        log::info!("Sequence design complete!");
                        for (i, (seq, score)) in sequences.iter().zip(scores.iter()).enumerate() {
                            log::info!("  {}: {} (score: {:.3})", i + 1, seq, score);
                        }

                        // Track the locked structure for potential unlock on failure
                        let locked_target = self.session.mpnn_target();

                        // Find best sequence (highest score, or first if all equal)
                        let best_idx = scores
                            .iter()
                            .enumerate()
                            .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
                            .map(|(i, _)| i)
                            .unwrap_or(0);

                        let mut unlock_needed = true; // Will be set to false if we successfully hand off to Rosetta

                        if let Some(best_seq) = sequences.get(best_idx) {
                            log::info!("Using sequence {} (score: {:.3})", best_idx + 1, scores[best_idx]);

                            // Get coords from the MPNN target (RFD3 design if exists, otherwise original)
                            let target_id = self.session.mpnn_apply_target();
                            let coords = target_id
                                .and_then(|id| self.scene.get(id))
                                .and_then(|s| s.get_coords_bytes());

                            if self.session.rfd3_design.is_some() {
                                log::info!("Using RFD3 design structure for MPNN");
                            } else {
                                log::info!("Using original structure for MPNN");
                            }

                            if let Some(coords) = coords {
                                // Apply the sequence and pack rotamers via Rosetta
                                // This will create a NEW structure (not modify original)
                                if let Some(ref executor) = self.rosetta_executor {
                                    // Stop any running operation and drain stale updates
                                    executor.stop();
                                    if let Some(ref mut updates) = self.rosetta_updates {
                                        while updates.try_recv().is_ok() {
                                            // Drain stale updates
                                        }
                                    }
                                    // Clear session state - sequence change will need fresh session
                                    self.rosetta_state = None;

                                    log::info!("Applying designed sequence via Rosetta and packing sidechains...");
                                    self.session.on_mpnn_start();
                                    if let Err(e) = executor.apply_sequence_and_pack(coords, best_seq.clone()) {
                                        log::error!("Failed to apply sequence: {}", e);
                                        self.session.mpnn_pending = false;
                                    } else {
                                        // Successfully handed off to Rosetta, keep lock until Rosetta completes
                                        unlock_needed = false;
                                    }
                                } else {
                                    log::warn!("No Rosetta executor available for sidechain placement");
                                }
                            } else {
                                log::warn!("No coords available from original structure");
                            }
                        }

                        // Unlock on failure (Rosetta will unlock on success in process_rosetta_updates)
                        if unlock_needed {
                            if let Some(id) = locked_target {
                                self.action_manager.unlock(id);
                            }
                        }

                        self.effect = VisualEffect::None;
                        self.ui_dirty |= DirtyFlags::LOADING | DirtyFlags::ACTIONS;
                    }

                    MLResult::StructureDesign { backbone_chains, confidence } => {
                        log::info!(
                            "Structure design complete! {} chains, confidence: {:.2}",
                            backbone_chains.len(),
                            confidence
                        );

                        // Invalidate Rosetta session - topology has changed with new structure
                        self.rosetta_state = None;

                        // If we have an animation structure, update it to the final result
                        // Otherwise create a new one
                        if let Some(anim_id) = self.session.animation_structure {
                            if let Some(structure) = self.scene.get_mut(anim_id) {
                                structure.name = format!("RFD3 Design ({:.0}%)", confidence * 100.0);
                                structure.backbone_chains = backbone_chains;
                                log::info!("Updated animation structure {:?} to final result", anim_id);
                            }
                            // Track this as the RFD3 design for MPNN
                            self.session.on_rfd3_complete(anim_id);
                        } else {
                            // No animation structure, create new one
                            let structure = Structure::from_backbone_design(
                                format!("RFD3 Design ({:.0}%)", confidence * 100.0),
                                backbone_chains,
                                confidence,
                            );
                            let id = self.scene.add(structure);
                            log::info!("Added designed structure {:?} to scene", id);
                            // Track this as the RFD3 design for MPNN
                            self.session.on_rfd3_complete(id);
                        }

                        // Unlock the original structure now that RFD3 is complete
                        if let Some(orig_id) = self.session.original {
                            self.action_manager.unlock(orig_id);
                        }

                        // Smoothly animate camera to show all structures including new design
                        if let Some(engine) = &mut self.engine {
                            let data = self.scene.aggregated();
                            engine.fit_camera_to_positions_animated(&data.all_positions);
                        }

                        self.pending_action = Some(AnimationAction::Diffusion);
                        self.effect = VisualEffect::None;
                        self.ui_dirty |= DirtyFlags::LOADING | DirtyFlags::ACTIONS;
                    }

                    MLResult::Error(error) => {
                        log::error!("ML error: {}", error);
                        // Unlock any locked structures on error
                        for lock_id in self.action_manager.locked_structures() {
                            self.action_manager.unlock(lock_id);
                        }
                        self.effect = VisualEffect::None;
                        self.ui_dirty |= DirtyFlags::LOADING | DirtyFlags::ACTIONS;
                    }
                }
            }
        }
    }

    /// Process pending Rosetta updates (non-blocking)
    fn process_rosetta_updates(&mut self) {
        let pending_updates: Vec<RosettaUpdate> = self
            .rosetta_updates
            .as_mut()
            .map(|updates| {
                let mut collected = Vec::new();
                while let Ok(update) = updates.try_recv() {
                    collected.push(update);
                }
                collected
            })
            .unwrap_or_default();

        for update in pending_updates {
            log::info!(
                "Rosetta update: cycle {}, score {:.2}, converged: {}",
                update.cycle,
                update.score,
                update.converged
            );

            // Track score and action state for frontend push
            self.latest_score = Some(update.score);
            self.ui_dirty |= DirtyFlags::SCORE | DirtyFlags::ACTIONS;

            // Check if this is an MPNN result or wiggle/shake update
            if self.session.mpnn_pending {
                log::info!("MPNN update received: {} bytes, score: {:.1}",
                    update.coords_bytes.len(), update.score);

                // Target structure is RFD3 design if exists, otherwise original
                let target_id = self.session.mpnn_apply_target();
                // Structure that was locked when MPNN started
                let locked_id = self.session.mpnn_target();

                match Structure::from_coords_bytes(
                    format!("MPNN Design (score {:.1})", update.score),
                    &update.coords_bytes,
                    1.0,
                ) {
                    Ok(new_structure) => {
                        log::info!(
                            "MPNN structure parsed: {} backbone chains, {} sidechain atoms",
                            new_structure.backbone_chains.len(),
                            new_structure.sidechain_atoms.len()
                        );

                        if let Some(id) = target_id {
                            if let Some(structure) = self.scene.get_mut(id) {
                                structure.backbone_chains = new_structure.backbone_chains;
                                structure.sidechain_atoms = new_structure.sidechain_atoms;
                                structure.sidechain_bonds = new_structure.sidechain_bonds;
                                structure.backbone_sidechain_bonds = new_structure.backbone_sidechain_bonds;
                                structure.coords = new_structure.coords;
                                structure.name = format!("MPNN Design (score {:.1})", update.score);
                                log::info!("Updated structure {:?} with MPNN design", id);
                                self.pending_action = Some(AnimationAction::Mutation);
                            }
                            // Track this as the MPNN design
                            self.session.on_mpnn_complete(id);
                        } else {
                            self.session.mpnn_pending = false;
                        }
                        // Unlock the structure now that MPNN is complete
                        if let Some(id) = locked_id {
                            self.action_manager.unlock(id);
                        }
                    }
                    Err(e) => {
                        log::error!("Failed to parse MPNN structure: {}", e);
                        self.session.mpnn_pending = false;
                        // Unlock on error
                        if let Some(id) = locked_id {
                            self.action_manager.unlock(id);
                        }
                    }
                }
            } else if let Some(ref state) = self.rosetta_state {
                // Full session update - apply to all structures using the session mapping
                log::info!("Applying full session update ({} structures, {} bytes)",
                    state.structure_count(), update.coords_bytes.len());

                // Convert RosettaStructureId back to StructureId for scene update
                let chain_ids: Vec<(StructureId, Vec<u8>)> = state.chain_ids_per_structure
                    .iter()
                    .map(|(id, chains)| (StructureId::from_raw(id.0), chains.clone()))
                    .collect();

                match self.scene.apply_combined_update(&update.coords_bytes, &chain_ids) {
                    Ok(()) => {
                        log::info!("Successfully updated all structures in session");
                        self.pending_action = Some(AnimationAction::Wiggle);
                    }
                    Err(e) => {
                        log::warn!("Failed to apply combined update: {}", e);
                    }
                }
            } else if let Some(id) = self.session.operation_target().or(self.session.original) {
                // Single structure update (focused structure or fallback to original)
                let target_name = self.scene.get(id).map(|s| s.name.clone()).unwrap_or_else(|| format!("{:?}", id));
                log::info!("Applying update to '{}' ({} bytes)", target_name, update.coords_bytes.len());

                let name = update
                    .message
                    .clone()
                    .unwrap_or_else(|| format!("Cycle {} (score {:.1})", update.cycle, update.score));

                match Structure::from_coords_bytes(
                    name.clone(),
                    &update.coords_bytes,
                    1.0,
                ) {
                    Ok(new_structure) => {
                        log::info!(
                            "Parsed update: {} backbone chains, {} sidechain atoms",
                            new_structure.backbone_chains.len(),
                            new_structure.sidechain_atoms.len()
                        );
                        if let Some(structure) = self.scene.get_mut(id) {
                            structure.backbone_chains = new_structure.backbone_chains;
                            structure.sidechain_atoms = new_structure.sidechain_atoms;
                            structure.sidechain_bonds = new_structure.sidechain_bonds;
                            structure.backbone_sidechain_bonds = new_structure.backbone_sidechain_bonds;
                            structure.coords = new_structure.coords;
                            // Keep the existing name for wiggle/shake updates
                            if !update.converged {
                                structure.name = name;
                            }
                        }
                        self.pending_action = Some(AnimationAction::Wiggle);
                    }
                    Err(e) => {
                        log::warn!("Failed to update structure from Rosetta: {}", e);
                    }
                }
            }

            // Note: Wiggle/shake never auto-converge (like real Foldit).
            // Operations only stop when user explicitly presses the key again.
            // The converged flag is only true for one-shot operations like MPNN,
            // which don't use the action_manager, so no unlock needed here.
            if update.converged {
                self.effect = VisualEffect::None;
            }
        }
    }

    /// Handle keyboard input for ML operations
    pub(crate) fn handle_key(&mut self, key: KeyCode) {
        match key {
            KeyCode::KeyW => {
                // Wiggle: pure minimization (no packing)
                if self.rosetta_executor.is_none() {
                    log::warn!("Rosetta executor not initialized");
                    return;
                }

                // Check if ANY structure has an active operation
                let locked_ids = self.action_manager.locked_structures();
                if !locked_ids.is_empty() {
                    // Check what type of operation is running
                    let has_rosetta_op = locked_ids.iter().any(|&id| {
                        matches!(
                            self.action_manager.get_action_type(id),
                            Some(ActionType::RosettaWiggle) | Some(ActionType::RosettaShake)
                        )
                    });

                    if has_rosetta_op {
                        // Rosetta operation running - stop it (toggle behavior)
                        log::info!("Stopping Rosetta operation...");
                        for lock_id in locked_ids {
                            if matches!(
                                self.action_manager.get_action_type(lock_id),
                                Some(ActionType::RosettaWiggle) | Some(ActionType::RosettaShake)
                            ) {
                                self.action_manager.request_cancel(lock_id);
                                self.action_manager.unlock(lock_id);
                            }
                        }
                        if let Some(ref executor) = self.rosetta_executor {
                            executor.stop();
                        }
                        // Don't clear rosetta_state - topology hasn't changed
                        self.effect = VisualEffect::None;
                        self.ui_dirty |= DirtyFlags::ACTIONS;
                    } else {
                        // ML operation running - can't start wiggle
                        let action = locked_ids.first()
                            .and_then(|&id| self.action_manager.get_action_type(id));
                        log::warn!("Cannot start wiggle: {:?} is running", action);
                    }
                } else {
                    // Use session's lock target (original in session mode, focused in single mode)
                    let Some(lock_id) = self.session.lock_target() else {
                        log::warn!("No structure available for wiggle");
                        return;
                    };

                    // Get combined coords - used for session creation AND operations
                    // The combined session approach uses locks to control which residues move
                    let Some(combined) = self.scene.get_combined_coords_bytes() else {
                        log::warn!("No coords available for wiggle");
                        return;
                    };
                    let coords = combined.bytes.clone();

                    // Ensure session exists with correct topology
                    if self.ensure_rosetta_session().is_none() {
                        log::warn!("Failed to ensure Rosetta session for wiggle");
                        return;
                    }

                    // Update locks based on current focus
                    self.update_rosetta_locks();

                    if let Some(_cancel_flag) = self.action_manager.try_lock(lock_id, ActionType::RosettaWiggle) {
                        let target_desc = if self.session.is_session_mode() {
                            format!("full session ({} structures)", self.scene.len())
                        } else {
                            self.session.operation_target()
                                .and_then(|id| self.scene.get(id))
                                .map(|s| s.name.clone())
                                .unwrap_or_default()
                        };
                        log::info!("Starting wiggle on {} ({} bytes)...", target_desc, coords.len());
                        // Get executor reference in this scope where we need it
                        if let Some(ref executor) = self.rosetta_executor {
                            if let Err(e) = executor.start_wiggle(coords) {
                                log::error!("Failed to start wiggle: {}", e);
                                self.action_manager.unlock(lock_id);
                                return;
                            }
                        }
                        self.effect = VisualEffect::pulsing();
                        self.ui_dirty |= DirtyFlags::ACTIONS;
                    } else {
                        log::warn!("Structure is already locked by another operation");
                    }
                }
            }

            KeyCode::KeyS => {
                // Shake: pure packing (rotamer optimization, no minimization)
                if self.rosetta_executor.is_none() {
                    log::warn!("Rosetta executor not initialized");
                    return;
                }

                // Check if ANY structure has an active operation
                let locked_ids = self.action_manager.locked_structures();
                if !locked_ids.is_empty() {
                    // Check what type of operation is running
                    let has_rosetta_op = locked_ids.iter().any(|&id| {
                        matches!(
                            self.action_manager.get_action_type(id),
                            Some(ActionType::RosettaWiggle) | Some(ActionType::RosettaShake)
                        )
                    });

                    if has_rosetta_op {
                        // Rosetta operation running - stop it (toggle behavior)
                        log::info!("Stopping Rosetta operation...");
                        for lock_id in locked_ids {
                            if matches!(
                                self.action_manager.get_action_type(lock_id),
                                Some(ActionType::RosettaWiggle) | Some(ActionType::RosettaShake)
                            ) {
                                self.action_manager.request_cancel(lock_id);
                                self.action_manager.unlock(lock_id);
                            }
                        }
                        if let Some(ref executor) = self.rosetta_executor {
                            executor.stop();
                        }
                        // Don't clear rosetta_state - topology hasn't changed
                        self.effect = VisualEffect::None;
                        self.ui_dirty |= DirtyFlags::ACTIONS;
                    } else {
                        // ML operation running - can't start shake
                        let action = locked_ids.first()
                            .and_then(|&id| self.action_manager.get_action_type(id));
                        log::warn!("Cannot start shake: {:?} is running", action);
                    }
                } else {
                    // Use session's lock target (original in session mode, focused in single mode)
                    let Some(lock_id) = self.session.lock_target() else {
                        log::warn!("No structure available for shake");
                        return;
                    };

                    // Get combined coords - used for session creation AND operations
                    // The combined session approach uses locks to control which residues move
                    let Some(combined) = self.scene.get_combined_coords_bytes() else {
                        log::warn!("No coords available for shake");
                        return;
                    };
                    let coords = combined.bytes.clone();

                    // Ensure session exists with correct topology
                    if self.ensure_rosetta_session().is_none() {
                        log::warn!("Failed to ensure Rosetta session for shake");
                        return;
                    }

                    // Update locks based on current focus
                    self.update_rosetta_locks();

                    if let Some(_cancel_flag) = self.action_manager.try_lock(lock_id, ActionType::RosettaShake) {
                        let target_desc = if self.session.is_session_mode() {
                            format!("full session ({} structures)", self.scene.len())
                        } else {
                            self.session.operation_target()
                                .and_then(|id| self.scene.get(id))
                                .map(|s| s.name.clone())
                                .unwrap_or_default()
                        };
                        log::info!("Starting shake on {} ({} bytes)...", target_desc, coords.len());
                        // Get executor reference in this scope where we need it
                        if let Some(ref executor) = self.rosetta_executor {
                            if let Err(e) = executor.start_shake(coords) {
                                log::error!("Failed to start shake: {}", e);
                                self.action_manager.unlock(lock_id);
                                return;
                            }
                        }
                        self.effect = VisualEffect::pulsing();
                        self.ui_dirty |= DirtyFlags::ACTIONS;
                    } else {
                        log::warn!("Structure is already locked by another operation");
                    }
                }
            }

            KeyCode::KeyP => {
                // SimpleFold: predict structure from sequence (multi-chain aware)
                // Target is the original structure (we update it in-place with prediction)
                let Some(target_id) = self.session.original else {
                    log::warn!("No structure loaded for prediction");
                    return;
                };

                // Check if target structure is already locked
                if self.action_manager.is_locked(target_id) {
                    let action = self.action_manager.get_action_type(target_id);
                    log::warn!("Structure is locked by {:?}, cannot start SimpleFold", action);
                    return;
                }

                // Stop any Rosetta operations and clear session (they may affect other structures)
                if let Some(ref executor) = self.rosetta_executor {
                    executor.stop();
                }
                self.rosetta_state = None;

                let chains = self.get_structure_chains();
                if chains.is_empty() {
                    log::warn!("No sequence/chains available");
                    return;
                }

                let ml_runner = match &self.ml_runner {
                    Some(r) => r,
                    None => {
                        log::warn!("ML runner not initialized");
                        return;
                    }
                };

                // Lock the target structure for ML prediction
                if self.action_manager.try_lock(target_id, ActionType::MLPredict).is_none() {
                    log::warn!("Failed to acquire lock for SimpleFold");
                    return;
                }

                let total_residues: usize = chains.iter().map(|(_, s)| s.len()).sum();
                if chains.len() == 1 {
                    log::info!(
                        "Starting SimpleFold prediction for {} residues...",
                        total_residues
                    );
                } else {
                    log::info!(
                        "Starting SimpleFold prediction for {} chains ({} total residues)...",
                        chains.len(),
                        total_residues
                    );
                }

                if let Err(e) = ml_runner.submit(MLTask::Predict {
                    sequence: None,
                    chains,
                    num_recycles: 3,
                }) {
                    log::error!("Failed to submit prediction task: {}", e);
                    self.action_manager.unlock(target_id);
                    return;
                }

                self.effect = VisualEffect::pulsing();
                self.ui_dirty |= DirtyFlags::ACTIONS | DirtyFlags::LOADING;
            }

            KeyCode::KeyM => {
                // MPNN: design sequence for current structure
                // Target is the structure we'll design a sequence for
                let Some(target_id) = self.session.mpnn_target() else {
                    log::warn!("No structure available for sequence design");
                    return;
                };

                // Check if target structure is already locked
                if self.action_manager.is_locked(target_id) {
                    let action = self.action_manager.get_action_type(target_id);
                    log::warn!("Structure is locked by {:?}, cannot start MPNN", action);
                    return;
                }

                // Stop any Rosetta operations and clear session
                if let Some(ref executor) = self.rosetta_executor {
                    executor.stop();
                }
                self.rosetta_state = None;

                let ml_runner = match &self.ml_runner {
                    Some(r) => r,
                    None => {
                        log::warn!("ML runner not initialized");
                        return;
                    }
                };

                let target_name = self.scene.get(target_id).map(|s| s.name.clone()).unwrap_or_default();
                let coords = self.scene.get(target_id).and_then(|s| s.get_coords_bytes());

                match coords {
                    Some(coords) => {
                        // Lock the target structure for ML sequence design
                        if self.action_manager.try_lock(target_id, ActionType::MLSequenceDesign).is_none() {
                            log::warn!("Failed to acquire lock for MPNN");
                            return;
                        }

                        log::info!("Starting MPNN sequence design on '{}' ({} bytes)...", target_name, coords.len());

                        if let Err(e) = ml_runner.submit(MLTask::SequenceDesign {
                            coords,
                            temperature: 0.1,
                            num_sequences: 4,
                        }) {
                            log::error!("Failed to submit sequence design task: {}", e);
                            self.action_manager.unlock(target_id);
                            return;
                        }

                        self.effect = VisualEffect::pulsing();
                        self.ui_dirty |= DirtyFlags::ACTIONS | DirtyFlags::LOADING;
                    }
                    None => {
                        log::warn!("No coords available for sequence design");
                    }
                }
            }

            KeyCode::KeyR => {
                // RFDiffusion3: design new structure
                // Lock the original structure to indicate ML operation in progress
                let Some(lock_id) = self.session.original else {
                    log::warn!("No structure loaded, cannot start RFD3");
                    return;
                };

                // Check if any structure is already locked
                if self.action_manager.is_locked(lock_id) {
                    let action = self.action_manager.get_action_type(lock_id);
                    log::warn!("Structure is locked by {:?}, cannot start RFD3", action);
                    return;
                }

                // Stop any Rosetta operations and clear session
                if let Some(ref executor) = self.rosetta_executor {
                    executor.stop();
                }
                self.rosetta_state = None;

                let ml_runner = match &self.ml_runner {
                    Some(r) => r,
                    None => {
                        log::warn!("ML runner not initialized");
                        return;
                    }
                };

                // Lock the structure for ML structure design
                if self.action_manager.try_lock(lock_id, ActionType::MLStructureDesign).is_none() {
                    log::warn!("Failed to acquire lock for RFD3");
                    return;
                }

                log::info!("Starting RFDiffusion3 structure design...");

                if let Err(e) = ml_runner.submit(MLTask::StructureDesign {
                    length: "100-100".to_string(),
                    num_steps: 50,
                }) {
                    log::error!("Failed to submit structure design task: {}", e);
                    self.action_manager.unlock(lock_id);
                    return;
                }

                self.effect = VisualEffect::design_highlight(Vec::new());
                self.ui_dirty |= DirtyFlags::ACTIONS | DirtyFlags::LOADING;
            }

            KeyCode::KeyH => {
                // Toggle visibility of designed structures (all except original)
                // Stop any running Rosetta operation and invalidate session
                // since changing visibility changes the combined structure set
                if let Some(ref executor) = self.rosetta_executor {
                    executor.stop();
                }
                self.rosetta_state = None;

                let ids: Vec<StructureId> = self.scene.structure_ids().to_vec();
                for id in ids {
                    if Some(id) != self.session.original {
                        // Extract info before mutable borrow
                        let toggle_info = self.scene.get(id).map(|s| (s.name.clone(), !s.visible));
                        if let Some((name, new_visible)) = toggle_info {
                            self.scene.set_visible(id, new_visible);
                            log::info!("Set {} visibility to {}", name, new_visible);
                        }
                    }
                }
                self.pending_action = Some(AnimationAction::Load);
                self.ui_dirty |= DirtyFlags::VIEW;
            }

            KeyCode::Delete | KeyCode::Backspace => {
                // Remove last added structure (keep original)
                if self.scene.len() > 1 {
                    let ids: Vec<StructureId> = self.scene.structure_ids().to_vec();
                    if let Some(&last_id) = ids.last() {
                        if Some(last_id) != self.session.original {
                            // Stop any running Rosetta operation and invalidate session
                            // since removing a structure changes the topology
                            if let Some(ref executor) = self.rosetta_executor {
                                executor.stop();
                            }
                            self.rosetta_state = None;

                            if let Some(removed) = self.scene.remove(last_id) {
                                log::info!("Removed structure: {}", removed.name);
                                // Validate focus in case removed structure was focused
                                self.session.validate_focus(self.scene.structure_ids());
                                self.pending_action = Some(AnimationAction::Load);
                                self.ui_dirty |= DirtyFlags::ACTIONS;
                            }
                        }
                    }
                }
            }

            KeyCode::Escape => {
                // Cancel current operation and clear selection
                log::info!("Cancelling current operation");

                // Clear selection
                if let Some(engine) = &mut self.engine {
                    engine.picking.clear_selection();
                }

                // Stop any locked operations and release locks
                let locked_ids: Vec<StructureId> = self.action_manager.locked_structures();
                for structure_id in locked_ids {
                    self.action_manager.request_cancel(structure_id);
                    if let Some(ref executor) = self.rosetta_executor {
                        executor.stop();
                    }
                    self.action_manager.unlock(structure_id);
                    log::info!("Stopped operation on structure {:?}", structure_id);
                }

                // Don't clear rosetta_state - topology hasn't changed, just operation stopped

                // Remove animation structure if one exists
                if let Some(anim_id) = self.session.animation_structure {
                    if self.scene.remove(anim_id).is_some() {
                        log::info!("Removed in-progress animation structure");
                        self.session.on_animation_structure_removed();
                        self.pending_action = Some(AnimationAction::Load);
                    }
                }

                self.effect = VisualEffect::None;

                // Also clear all bands on Escape
                if !self.active_bands.is_empty() {
                    if let Some(ref executor) = self.rosetta_executor {
                        let _ = executor.clear_all_bands();
                    }
                    log::info!("Cleared {} bands", self.active_bands.len());
                    self.active_bands.clear();
                    self.update_band_visualization();
                }

                // Cancel any band drag in progress
                self.band_drag = None;
                self.ui_dirty |= DirtyFlags::ACTIONS | DirtyFlags::SELECTION | DirtyFlags::LOADING;
            }

            KeyCode::Tab => {
                // Cycle focus: Session -> Structure 1 -> Structure 2 -> ... -> Session
                let structure_ids = self.scene.structure_ids().to_vec();
                self.session.cycle_focus(&structure_ids);
                let focus_name = self.session.focus_description(&self.scene);
                log::info!("Focus: {}", focus_name);

                // Update Rosetta locks to match new focus
                // This only takes effect if there's an active session
                self.update_rosetta_locks();

                // Update camera based on new focus
                if let Some(engine) = &mut self.engine {
                    match self.session.operation_target() {
                        None => {
                            // Session view: fit all structures
                            let data = self.scene.aggregated();
                            engine.fit_camera_to_positions_animated(&data.all_positions);
                        }
                        Some(id) => {
                            // Structure view: fit just this structure
                            if let Some(structure) = self.scene.get(id) {
                                let positions: Vec<Vec3> = structure.backbone_chains
                                    .iter()
                                    .flat_map(|c| c.iter().copied())
                                    .chain(structure.sidechain_atoms.iter().map(|a| a.position))
                                    .collect();
                                engine.fit_camera_to_positions_animated(&positions);
                            }
                        }
                    }
                }
                self.ui_dirty |= DirtyFlags::SELECTION | DirtyFlags::UI;
            }

            _ => {}
        }
    }

    /// Get chains from the focused structure (or original) for multi-chain prediction
    /// Returns Vec<(chain_id, sequence)>
    fn get_structure_chains(&self) -> Vec<(String, String)> {
        // Use focused structure if set, otherwise fall back to original
        let structure_id = self.session.operation_target().or(self.session.original);
        if let Some(id) = structure_id {
            if let Some(structure) = self.scene.get(id) {
                if !structure.chain_sequences.is_empty() {
                    return structure.chain_sequences
                        .iter()
                        .map(|(cid, seq)| (format!("{}", *cid as char), seq.clone()))
                        .collect();
                }
                // Fallback: if no chain_sequences, use the full sequence as chain A
                if !structure.sequence.is_empty() {
                    return vec![("A".to_string(), structure.sequence.clone())];
                }
            }
        }
        vec![]
    }

    /// Get coords for backend operations.
    /// If focused on a structure, returns that structure's coords.
    /// Otherwise returns combined session coords.
    fn get_operation_coords(&self) -> Option<(Vec<u8>, Option<CombinedCoordsResult>)> {
        match self.session.operation_target() {
            Some(id) => {
                // Single structure mode
                self.scene.get(id)
                    .and_then(|s| s.get_coords_bytes())
                    .map(|bytes| (bytes, None))
            }
            None => {
                // Session mode: combined coords
                self.scene.get_combined_coords_bytes()
                    .map(|result| (result.bytes.clone(), Some(result)))
            }
        }
    }

    /// Ensure a Rosetta session exists with current topology.
    /// Creates a new session or returns the existing one if topology matches.
    /// Returns None if no structures are visible or executor is unavailable.
    fn ensure_rosetta_session(&mut self) -> Option<&RosettaSessionState> {
        let combined = self.scene.get_combined_coords_bytes()?;
        let executor = self.rosetta_executor.as_ref()?;

        // Check if we need to recreate the session
        let needs_recreation = match &self.rosetta_state {
            None => true,
            Some(state) => {
                let (visible_ids, residue_counts) = self.scene.get_visible_structure_residue_counts();
                // Convert StructureId to RosettaStructureId for comparison
                let visible_rosetta_ids: Vec<RosettaStructureId> = visible_ids
                    .iter()
                    .map(|id| RosettaStructureId(id.0))
                    .collect();
                let residue_counts_rosetta: HashMap<RosettaStructureId, usize> = residue_counts
                    .iter()
                    .map(|(id, count)| (RosettaStructureId(id.0), *count))
                    .collect();
                state.topology_changed(&visible_rosetta_ids, &residue_counts_rosetta)
            }
        };

        if needs_recreation {
            log::info!("Recreating Rosetta session (topology changed)");
            if let Err(e) = executor.recreate_session(combined.bytes.clone()) {
                log::error!("Failed to recreate Rosetta session: {}", e);
                return None;
            }

            // Build RosettaSessionState from CombinedCoordsResult
            let chain_ids_per_structure: Vec<(RosettaStructureId, Vec<u8>)> = combined
                .chain_ids_per_structure
                .iter()
                .map(|(id, chains)| (RosettaStructureId(id.0), chains.clone()))
                .collect();
            let residue_ranges: HashMap<RosettaStructureId, (usize, usize)> = combined
                .residue_ranges
                .iter()
                .map(|(id, range)| (RosettaStructureId(id.0), *range))
                .collect();

            self.rosetta_state = Some(RosettaSessionState::new(chain_ids_per_structure, residue_ranges));
            log::info!(
                "Session created with {} structures, {} total residues",
                combined.chain_ids_per_structure.len(),
                self.rosetta_state.as_ref().map(|s| s.total_residues).unwrap_or(0)
            );
        }

        self.rosetta_state.as_ref()
    }

    /// Update Rosetta locks to match current focus.
    /// In session mode, all residues are unlocked.
    /// In single structure mode, only the focused structure's residues are unlocked.
    fn update_rosetta_locks(&mut self) {
        let Some(state) = self.rosetta_state.as_ref() else {
            log::debug!("update_rosetta_locks: no rosetta_state, skipping");
            return;
        };

        // Get new focus as RosettaStructureId
        let new_focus = self.session.operation_target()
            .map(|id| RosettaStructureId(id.0));

        // Check if focus has changed
        if state.focused_structure == new_focus {
            log::debug!("update_rosetta_locks: focus unchanged ({:?}), skipping", new_focus);
            return;
        }

        log::info!("update_rosetta_locks: focus changing from {:?} to {:?}",
            state.focused_structure, new_focus);

        let Some(executor) = self.rosetta_executor.as_ref() else {
            return;
        };

        let total_residues = state.total_residues;

        match new_focus {
            None => {
                // Session mode: unlock all residues
                if let Err(e) = executor.clear_all_locks(total_residues) {
                    log::warn!("Failed to clear locks: {}", e);
                } else {
                    log::info!("Cleared all locks (session mode)");
                }
            }
            Some(focus_id) => {
                // Single structure mode: lock residues not in focused structure
                let locked_residues = state.residues_to_lock(focus_id);
                if let Err(e) = executor.set_focus_locks(locked_residues.clone(), total_residues) {
                    log::warn!("Failed to set focus locks: {}", e);
                } else {
                    log::info!(
                        "Locked {} residues (focusing on structure {:?})",
                        locked_residues.len(),
                        focus_id
                    );
                }
            }
        }

        // Update focus in state
        if let Some(state) = self.rosetta_state.as_mut() {
            state.set_focus(new_focus);
        }
    }

    /// Update band visualization based on current active bands.
    /// Uses stored atom names to look up the same atom's interpolated position during animation.
    fn update_band_visualization(&mut self) {
        let Some(engine) = &mut self.engine else {
            return;
        };

        if self.active_bands.is_empty() {
            engine.clear_bands();
            return;
        }

        // Convert active bands to render info using interpolated atom positions
        // Use the stored atom names to reliably track the same atom during animation
        let band_infos: Vec<BandRenderInfo> = self.active_bands
            .values()
            .filter_map(|band| {
                // Convert from 1-indexed Rosetta to 0-indexed
                let idx1 = (band.res1 as usize).checked_sub(1)?;
                let idx2 = (band.res2 as usize).checked_sub(1)?;

                // Look up atoms by name to track them during animation
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
            .collect();

        engine.update_bands(&band_infos);
        log::debug!("Updated {} band visualizations", band_infos.len());
    }

    /// Update visualization with optional pull or band preview during drag.
    /// Pass pull_info for pull preview, band_preview for band preview, or None for both to clear.
    /// Uses interpolated CA positions so bands follow animated atoms.
    fn update_drag_visualization(
        &mut self,
        pull_info: Option<(Vec3, Vec3, u32)>,
        band_preview: Option<(Vec3, Vec3, u32)>,
    ) {
        let Some(engine) = &mut self.engine else {
            return;
        };

        // Get interpolated CA positions from engine (follows animation)
        let ca_positions = engine.get_current_ca_positions();

        // Start with existing bands using interpolated CA positions
        let mut band_infos: Vec<BandRenderInfo> = self.active_bands
            .values()
            .filter_map(|band| {
                // Convert from 1-indexed Rosetta to 0-indexed
                let idx1 = (band.res1 as usize).checked_sub(1)?;
                let idx2 = (band.res2 as usize).checked_sub(1)?;

                // Get interpolated CA positions (bands follow animated atoms)
                let pos1 = ca_positions.get(idx1).copied()?;
                let pos2 = ca_positions.get(idx2).copied()?;

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
            .collect();

        // Add band preview if active (shows during right-drag)
        if let Some((start_pos, target_pos, residue_idx)) = band_preview {
            band_infos.push(BandRenderInfo {
                endpoint_a: start_pos,
                endpoint_b: target_pos,
                is_pull: true,
                residue_idx,
                is_space_pull: false,
                ..Default::default()
            });
        }

        engine.update_bands(&band_infos);

        // Update pull visualization separately (uses PullRenderer with cone at mouse end)
        if let Some((atom_pos, target_pos, residue_idx)) = pull_info {
            engine.update_pull(Some(&PullRenderInfo {
                atom_pos,
                target_pos,
                residue_idx,
            }));
        } else {
            engine.clear_pull();
        }
    }

    /// Update pull visualization during drag.
    /// Pass None to clear the pull visualization.
    fn update_pull_visualization(&mut self, pull_info: Option<(Vec3, Vec3, u32)>) {
        self.update_drag_visualization(pull_info, None);
    }

    /// Update band preview visualization during right-drag.
    /// Pass None to clear the band preview.
    fn update_band_preview(&mut self, band_preview: Option<(Vec3, Vec3, u32)>) {
        self.update_drag_visualization(None, band_preview);
    }

    /// Create a band between two residues via right-click drag (legacy, uses CA atoms).
    /// Both residue indices are 0-indexed (from the render engine).
    #[allow(dead_code)]
    fn create_band(&mut self, start_residue: i32, end_residue: i32) {
        // Get CA positions for visualization
        let data = self.scene.aggregated();
        let pos1 = foldit_conv::coords::get_ca_position_from_chains(
            &data.backbone_chains,
            start_residue as usize,
        );
        let pos2 = foldit_conv::coords::get_ca_position_from_chains(
            &data.backbone_chains,
            end_residue as usize,
        );

        match (pos1, pos2) {
            (Some(p1), Some(p2)) => {
                self.create_band_with_atoms(start_residue, p1, "CA", end_residue, p2, "CA");
            }
            _ => {
                log::warn!("Could not get CA positions for band creation");
            }
        }
    }

    /// Create a band with specific atom positions and names for visualization.
    /// Uses the provided positions to calculate band length and atom names to track during animation.
    fn create_band_with_atoms(
        &mut self,
        start_residue: i32,
        start_pos: Vec3,
        start_atom_name: &str,
        end_residue: i32,
        end_pos: Vec3,
        end_atom_name: &str,
    ) {
        // Ensure we have a Rosetta session
        if self.ensure_rosetta_session().is_none() {
            log::warn!("No Rosetta session available for band creation");
            return;
        }

        if self.rosetta_executor.is_none() {
            log::warn!("No Rosetta executor available");
            return;
        }

        // Convert from 0-indexed to 1-indexed for Rosetta
        let res1 = (start_residue + 1) as u32;
        let res2 = (end_residue + 1) as u32;

        // Use CA atom for Rosetta (atom 2 in 1-indexed)
        // TODO: Could be enhanced to determine actual atom index based on atom name
        let atom1 = 2u32;
        let atom2 = 2u32;

        // Calculate length from the actual clicked atom positions
        let length = start_pos.distance(end_pos) as f64;

        let strength = 1.0;

        let executor = self.rosetta_executor.as_ref().unwrap();
        match executor.add_band(res1, atom1, res2, atom2, length, strength) {
            Ok(band_id) => {
                log::info!(
                    "Created band {} between {}:{} and {}:{} (length: {:.1}Å)",
                    band_id, res1, start_atom_name, res2, end_atom_name, length
                );

                self.active_bands.insert(band_id, ActiveBand {
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
                });

                self.update_band_visualization();
            }
            Err(e) => {
                log::error!("Failed to create band: {}", e);
            }
        }
    }

    /// Calculate distance between CA atoms of two residues.
    /// Residue indices are 0-indexed.
    fn calculate_ca_distance(&mut self, res1: usize, res2: usize) -> Option<f64> {
        let data = self.scene.aggregated();

        // Build CA positions from backbone chains
        let mut ca_positions: Vec<Vec3> = Vec::new();
        for chain in &data.backbone_chains {
            for chunk in chain.chunks(3) {
                if chunk.len() >= 2 {
                    ca_positions.push(chunk[1]); // CA is at index 1
                }
            }
        }

        let pos1 = ca_positions.get(res1)?;
        let pos2 = ca_positions.get(res2)?;

        Some(pos1.distance(*pos2) as f64)
    }

    /// Handle a key press by string name (for IPC).
    /// Maps common key name strings to winit KeyCode values.
    pub fn handle_key_by_name(&mut self, code: &str) {
        let key = match code {
            "KeyW" => KeyCode::KeyW,
            "KeyS" => KeyCode::KeyS,
            "KeyP" => KeyCode::KeyP,
            "KeyM" => KeyCode::KeyM,
            "KeyR" => KeyCode::KeyR,
            "KeyV" => KeyCode::KeyV,
            "KeyQ" => KeyCode::KeyQ,
            "KeyH" => KeyCode::KeyH,
            "Tab" => KeyCode::Tab,
            "Delete" => KeyCode::Delete,
            "Escape" => KeyCode::Escape,
            _ => {
                log::debug!("Unhandled key code from frontend: {}", code);
                return;
            }
        };
        self.handle_key(key);
    }

    /// Load a structure by path or PDB ID (for IPC).
    pub fn handle_load_structure(&mut self, input: &str) {
        let path = match resolve_structure_path(input) {
            Ok(p) => p,
            Err(e) => {
                log::error!("Failed to resolve structure: {}", e);
                return;
            }
        };

        match Structure::from_file(&path) {
            Ok(structure) => {
                log::info!("Loaded structure via IPC: {}", structure.name);
                let backbone_ca = extract_ca_from_chains(&structure.backbone_chains);
                let id = self.scene.add(structure);
                self.session.on_original_loaded(id, backbone_ca);
                self.pending_action = Some(AnimationAction::Load);
                self.ui_dirty |= DirtyFlags::LOADING | DirtyFlags::ACTIONS | DirtyFlags::SCORE;
            }
            Err(e) => {
                log::error!("Failed to load structure '{}': {}", path, e);
            }
        }
    }

    /// Initialize domain state once a window is available (called from AppRunner::resumed).
    pub(crate) fn initialize_with_window(&mut self, window: Arc<Window>) {
        // Create render engine with the specified molecule path
        let size = window.inner_size();
        let scale = window.scale_factor();
        let mut engine = pollster::block_on(ProteinRenderEngine::new_with_path(
            window.clone(),
            (size.width, size.height),
            &self.pdb_path,
        ));

        // Ensure the surface layer's scale matches the display for HiDPI
        engine.context.set_surface_scale(scale);

        // Load initial structure into scene
        match Structure::from_file(&self.pdb_path) {
            Ok(structure) => {
                log::info!(
                    "Loaded structure: {} ({} sidechain atoms, {} backbone chains)",
                    structure.name,
                    structure.sidechain_atoms.len(),
                    structure.backbone_chains.len()
                );

                // Store original CA positions for Kabsch alignment in session
                let backbone_ca = extract_ca_from_chains(&structure.backbone_chains);
                log::info!(
                    "Stored {} original CA positions for alignment",
                    backbone_ca.len()
                );

                let id = self.scene.add(structure);
                self.session.on_original_loaded(id, backbone_ca);

                // Initial sync with engine
                let data = self.scene.aggregated();
                engine.update_from_aggregated(
                    &data.backbone_chains,
                    &data.sidechain_positions,
                    &data.sidechain_hydrophobicity,
                    &data.sidechain_residue_indices,
                    &data.sidechain_atom_names,
                    &data.sidechain_bonds,
                    &data.backbone_sidechain_bonds,
                    &data.all_positions,
                    true, // Fit camera on initial load
                    data.ss_types.as_deref(),
                );
            }
            Err(e) => {
                log::error!("Failed to load structure from '{}': {}", self.pdb_path, e);
            }
        }

        // Initialize ML runner
        let (ml_runner, ml_updates, ml_results) = MLRunner::new();
        self.ml_runner = Some(ml_runner);
        self.ml_updates = Some(ml_updates);
        self.ml_results = Some(ml_results);

        // Initialize Rosetta executor
        let (rosetta_executor, rosetta_updates) = RosettaExecutor::new();
        self.rosetta_executor = Some(rosetta_executor);
        self.rosetta_updates = Some(rosetta_updates);

        self.engine = Some(engine);
    }

    /// Shut down ML runner and Rosetta executor.
    pub(crate) fn shutdown(&self) {
        if let Some(ref runner) = self.ml_runner {
            runner.shutdown();
        }
        if let Some(ref executor) = self.rosetta_executor {
            executor.shutdown();
        }
    }

    /// Resize the render engine surface.
    pub(crate) fn resize(&mut self, width: u32, height: u32) {
        if let Some(engine) = &mut self.engine {
            engine.resize(width, height);
        }
    }

    /// Update surface layer scale factor (e.g. when moving between displays).
    pub(crate) fn set_surface_scale(&self, scale_factor: f64) {
        if let Some(ref engine) = self.engine {
            engine.context.set_surface_scale(scale_factor);
        }
    }

    /// Tick visual effects, returning the current intensity.
    pub(crate) fn tick_effects(&mut self, dt: f32) -> f32 {
        self.effect.tick(dt)
    }

    /// Update camera animation by the given delta time.
    pub(crate) fn update_camera_animation(&mut self, dt: f32) {
        if let Some(engine) = &mut self.engine {
            engine.update_camera_animation(dt);
        }
    }

    /// Update per-frame visuals: band tracking during animation and pull tracking.
    pub(crate) fn update_frame_visuals(&mut self) {
        // Update bands during structure animation so they follow interpolated positions
        if let Some(engine) = &self.engine {
            if engine.needs_band_update() && !self.active_bands.is_empty() {
                self.update_band_visualization();
            }
        }

        // Update pull visualization during animation so it tracks the moving residue
        if let Some(ref mut pull) = self.pull_drag {
            if let Some(engine) = &self.engine {
                if let Some(current_ca) = engine.get_residue_ca_position(pull.residue as usize) {
                    pull.start_pos = current_ca;
                }
            }
        }
        if let Some(ref pull) = self.pull_drag {
            let pull_info = Some((pull.start_pos, pull.target_pos, pull.residue as u32));
            self.update_pull_visualization(pull_info);
        }
    }

    /// Render the current frame.
    pub(crate) fn render(&mut self) {
        if let Some(engine) = &mut self.engine {
            if let Err(e) = engine.render() {
                log::error!("Render error: {:?}", e);
            }
        } else {
            log::warn!("render() called but engine is None");
        }
    }

    /// Populate a FrontendState with current App domain state based on accumulated dirty flags.
    pub(crate) fn populate_frontend(&mut self, frontend: &mut foldit_frontend::FrontendState) {
        let app_dirty = self.take_ui_dirty();
        if app_dirty.is_empty() {
            return;
        }

        if app_dirty.contains(DirtyFlags::SCORE) {
            if let Some(score) = self.latest_score {
                frontend.set_score(score, false);
            }
        }
        if app_dirty.contains(DirtyFlags::ACTIONS) {
            frontend.set_actions(self.build_actions_list());
        }
        if app_dirty.contains(DirtyFlags::LOADING) {
            let progress = self.effect.get_progress_percent().map(|pct| pct / 100.0);
            frontend.set_loading_progress(progress);
        }
        if app_dirty.contains(DirtyFlags::VIEW) {
            if let Some(engine) = &self.engine {
                let mode = match engine.view_mode {
                    foldit_render::engine::ViewMode::Tube => {
                        foldit_frontend::state::ViewMode::Tube
                    }
                    foldit_render::engine::ViewMode::Ribbon => {
                        foldit_frontend::state::ViewMode::Ribbon
                    }
                };
                frontend.set_view_mode(mode);
            }
        }
        if app_dirty.contains(DirtyFlags::SELECTION) {
            frontend.mark_dirty(DirtyFlags::SELECTION);
        }
        if app_dirty.contains(DirtyFlags::UI) {
            frontend.mark_dirty(DirtyFlags::UI);
        }
    }

    /// Handle native mouse input (from winit, not webview).
    pub(crate) fn handle_native_mouse_input(
        &mut self,
        button: winit::event::MouseButton,
        pressed: bool,
    ) {
        match button {
            winit::event::MouseButton::Left => {
                self.left_mouse_pressed = pressed;

                if pressed {
                    // Left button pressed - check if over a residue to start pull
                    if let Some(engine) = &self.engine {
                        let hovered = engine.hovered_residue();
                        if hovered >= 0 {
                            if let Some(ca_pos) =
                                engine.get_residue_ca_position(hovered as usize)
                            {
                                let click_world_pos = engine.screen_to_world_at_depth(
                                    self.last_mouse_pos.0,
                                    self.last_mouse_pos.1,
                                    ca_pos,
                                );
                                let data = self.scene.aggregated();
                                let start_pos = get_closest_atom_for_residue(
                                    &data.backbone_chains,
                                    &data.sidechain_positions,
                                    &data.sidechain_residue_indices,
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
                    }

                    if let Some(engine) = &mut self.engine {
                        engine.handle_mouse_button(button, pressed);
                    }
                } else {
                    // Left button released
                    if let Some(pull) = self.pull_drag.take() {
                        if let Some(engine) = &mut self.engine {
                            engine.handle_mouse_button(button, false);
                        }

                        if pull.is_active {
                            log::info!(
                                "Pull released - residue {} pulled to {:?}",
                                pull.residue,
                                pull.target_pos
                            );
                            self.update_pull_visualization(None);
                            if let Some(rosetta) = &self.rosetta_executor {
                                rosetta.cancel();
                            }
                        } else {
                            if let Some(engine) = &mut self.engine {
                                engine.handle_mouse_up();
                            }
                        }
                    } else {
                        if let Some(engine) = &mut self.engine {
                            engine.handle_mouse_button(button, false);
                            engine.handle_mouse_up();
                        }
                    }
                }
            }
            winit::event::MouseButton::Right => {
                self.right_mouse_pressed = pressed;

                if pressed {
                    if let Some(engine) = &self.engine {
                        let hovered = engine.hovered_residue();
                        if hovered >= 0 {
                            if let Some(ca_pos) =
                                engine.get_residue_ca_position(hovered as usize)
                            {
                                let click_world_pos = engine.screen_to_world_at_depth(
                                    self.last_mouse_pos.0,
                                    self.last_mouse_pos.1,
                                    ca_pos,
                                );
                                let data = self.scene.aggregated();
                                let (start_atom_pos, start_atom_name) =
                                    get_closest_atom_with_name(
                                        &data.backbone_chains,
                                        &data.sidechain_positions,
                                        &data.sidechain_residue_indices,
                                        &data.sidechain_atom_names,
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
                    }
                } else {
                    if let Some(drag) = self.band_drag.take() {
                        self.update_band_preview(None);

                        if let Some(engine) = &self.engine {
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
                                    let data = self.scene.aggregated();
                                    let (end_atom_pos, end_atom_name) =
                                        get_closest_atom_with_name(
                                            &data.backbone_chains,
                                            &data.sidechain_positions,
                                            &data.sidechain_residue_indices,
                                            &data.sidechain_atom_names,
                                            end_residue as usize,
                                            click_world_pos,
                                        )
                                        .unwrap_or((ca_pos, "CA".to_string()));

                                    self.create_band_with_atoms(
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
            }
            _ => {}
        }
    }

    /// Handle native cursor movement (from winit, not webview).
    pub(crate) fn handle_native_cursor_moved(&mut self, x: f32, y: f32) {
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
                if let Some(engine) = &self.engine {
                    if let Some(current_ca) =
                        engine.get_residue_ca_position(pull.residue as usize)
                    {
                        pull.start_pos = current_ca;
                    }
                    pull.target_pos = engine.screen_to_world_at_depth(x, y, pull.start_pos);
                }
            }
        }

        // Start Rosetta pull when pull becomes active
        if pull_became_active {
            if let Some(ref pull) = self.pull_drag {
                if let Some(rosetta) = &self.rosetta_executor {
                    if let Some(combined) = self.scene.get_combined_coords_bytes() {
                        let residue_1indexed = (pull.residue + 1) as u32;
                        let target =
                            [pull.target_pos.x, pull.target_pos.y, pull.target_pos.z];
                        if let Err(e) =
                            rosetta.start_pull(combined.bytes, residue_1indexed, target)
                        {
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

        // Extract pull info for visualization and Rosetta update
        if let Some(ref pull) = self.pull_drag {
            if pull.is_active {
                let pull_info =
                    Some((pull.start_pos, pull.target_pos, pull.residue as u32));
                let target =
                    [pull.target_pos.x, pull.target_pos.y, pull.target_pos.z];

                self.update_pull_visualization(pull_info);

                if let Some(rosetta) = &self.rosetta_executor {
                    let _ = rosetta.update_pull_target(target);
                }

                if let Some(engine) = &mut self.engine {
                    engine.handle_mouse_position(x, y);
                }
            } else {
                if let Some(engine) = &mut self.engine {
                    engine.handle_mouse_move(delta_x, delta_y);
                    engine.handle_mouse_position(x, y);
                }
            }
        } else {
            if let Some(engine) = &mut self.engine {
                engine.handle_mouse_move(delta_x, delta_y);
                engine.handle_mouse_position(x, y);
            }
        }

        self.last_mouse_pos = (x, y);

        // Update band drag state during right-click drag
        if let Some(ref mut drag) = self.band_drag {
            drag.current_mouse_pos = self.last_mouse_pos;

            if let Some(engine) = &self.engine {
                let target_pos =
                    engine.screen_to_world_at_depth(x, y, drag.start_atom_pos);
                let preview =
                    Some((drag.start_atom_pos, target_pos, drag.start_residue as u32));
                self.update_band_preview(preview);
            }
        }
    }

    /// Handle native mouse wheel input (from winit, not webview).
    pub(crate) fn handle_native_mouse_wheel(&mut self, delta: MouseScrollDelta) {
        if let Some(engine) = &mut self.engine {
            match delta {
                MouseScrollDelta::LineDelta(_, y) => engine.handle_mouse_wheel(y),
                MouseScrollDelta::PixelDelta(pos) => {
                    engine.handle_mouse_wheel(pos.y as f32 * 0.01)
                }
            }
        }
    }

    /// Handle native modifier key changes.
    pub(crate) fn handle_native_modifiers(&mut self, state: ModifiersState) {
        if let Some(engine) = &mut self.engine {
            engine.update_modifiers(state);
        }
    }

    /// Handle a ViewportInput message from JS IPC.
    /// Delegates to the same native handlers used by winit so all input logic
    /// (pull drags, band drags, selection, modifiers) works identically.
    pub(crate) fn handle_viewport_input(&mut self, input: foldit_frontend::ViewportInput) {
        use foldit_frontend::ViewportInput;

        match input {
            ViewportInput::PointerDown { x, y, button, shift, .. } => {
                let winit_button = match button {
                    0 => winit::event::MouseButton::Left,
                    2 => winit::event::MouseButton::Right,
                    1 => winit::event::MouseButton::Middle,
                    _ => return,
                };
                if let Some(engine) = &mut self.engine {
                    engine.set_shift_pressed(shift);
                }
                // Update position before button press so handlers see correct coords
                self.handle_native_cursor_moved(x, y);
                self.handle_native_mouse_input(winit_button, true);
            }
            ViewportInput::PointerUp { x, y, button, shift, .. } => {
                let winit_button = match button {
                    0 => winit::event::MouseButton::Left,
                    2 => winit::event::MouseButton::Right,
                    1 => winit::event::MouseButton::Middle,
                    _ => return,
                };
                if let Some(engine) = &mut self.engine {
                    engine.set_shift_pressed(shift);
                }
                self.handle_native_cursor_moved(x, y);
                self.handle_native_mouse_input(winit_button, false);
            }
            ViewportInput::PointerMove { x, y, shift, .. } => {
                if let Some(engine) = &mut self.engine {
                    engine.set_shift_pressed(shift);
                }
                self.handle_native_cursor_moved(x, y);
            }
            ViewportInput::Scroll { delta } => {
                if let Some(engine) = &mut self.engine {
                    engine.handle_mouse_wheel(delta);
                }
            }
            ViewportInput::Key { code, pressed } => {
                if pressed {
                    self.handle_key_by_name(&code);
                }
            }
            ViewportInput::Resize { .. } => {
                // Ignored: JS sends CSS pixels (logical) which are wrong on HiDPI.
                // Window resizes are handled by WindowEvent::Resized (physical pixels).
            }
        }

        self.ui_dirty |= DirtyFlags::UI;
    }

    /// Handle a TriggerAction message from JS IPC.
    pub(crate) fn handle_trigger_action(&mut self, action: foldit_frontend::ActionId) {
        use foldit_frontend::ActionId;
        match action {
            ActionId::ToggleWiggle => self.handle_key(KeyCode::KeyW),
            ActionId::ToggleShake => self.handle_key(KeyCode::KeyS),
            ActionId::RunPrediction => self.handle_key(KeyCode::KeyP),
            ActionId::RunMPNN => self.handle_key(KeyCode::KeyM),
            ActionId::RunDiffusion => self.handle_key(KeyCode::KeyR),
            ActionId::ToggleViewMode => self.handle_key(KeyCode::KeyV),
            ActionId::ToggleBackboneQuality => self.handle_key(KeyCode::KeyQ),
            ActionId::ToggleDesignedStructures => self.handle_key(KeyCode::KeyH),
            ActionId::CycleFocus => self.handle_key(KeyCode::Tab),
            ActionId::RemoveStructure => self.handle_key(KeyCode::Delete),
            ActionId::Cancel => self.handle_key(KeyCode::Escape),
            ActionId::Undo | ActionId::Redo => {
                log::warn!("Undo/Redo not yet implemented");
            }
        }
        self.ui_dirty |= DirtyFlags::SCORE | DirtyFlags::ACTIONS | DirtyFlags::UI;
    }

    /// Handle a ParameterizedAction message from JS IPC.
    pub(crate) fn handle_parameterized_action(&mut self, action: foldit_frontend::ParameterizedAction) {
        use foldit_frontend::ParameterizedAction;
        match action {
            ParameterizedAction::LoadStructure { path } => {
                self.handle_load_structure(&path);
                self.ui_dirty |= DirtyFlags::LOADING | DirtyFlags::SCORE | DirtyFlags::SELECTION;
            }
            ParameterizedAction::LoadPuzzle { puzzle_id } => {
                self.scene.clear();
                self.session = Session::new();
                self.active_bands.clear();
                self.action_manager = ActionManager::new();
                self.rosetta_state = None;
                match foldit_rs::puzzle::load_puzzle_structure(puzzle_id) {
                    Ok(structure) => {
                        let backbone_ca = extract_ca_from_chains(&structure.backbone_chains);
                        let id = self.scene.add(structure);
                        self.session.on_original_loaded(id, backbone_ca);
                        self.pending_action = Some(AnimationAction::Load);
                        self.pending_fit_camera = true;
                    }
                    Err(e) => log::error!("Failed to load puzzle {}: {}", puzzle_id, e),
                }
                self.ui_dirty |= DirtyFlags::LOADING | DirtyFlags::SCORE | DirtyFlags::SELECTION | DirtyFlags::ACTIONS;
            }
            ParameterizedAction::CreateBand { .. } => {
                log::info!("CreateBand via IPC not yet wired");
            }
            ParameterizedAction::RemoveBand { .. } => {
                log::info!("RemoveBand via IPC not yet wired");
            }
            ParameterizedAction::SetViewOption { .. } => {
                log::info!("SetViewOption via IPC not yet wired");
                self.ui_dirty |= DirtyFlags::VIEW;
            }
        }
    }

}
/// Check if a string looks like a PDB ID (4 alphanumeric characters)
fn is_pdb_id(s: &str) -> bool {
    s.len() == 4 && s.chars().all(|c| c.is_ascii_alphanumeric())
}

/// Resolve a PDB ID or path to an actual file path, downloading if necessary
fn resolve_structure_path(input: &str) -> Result<String, String> {
    // If it's a file path that exists, use it directly
    if std::path::Path::new(input).exists() {
        return Ok(input.to_string());
    }

    // If it looks like a PDB ID, try to find or download it
    if is_pdb_id(input) {
        let pdb_id = input.to_lowercase();
        let models_dir = std::path::Path::new("../foldit-render/assets/models");
        let local_path = models_dir.join(format!("{}.cif", pdb_id));

        // Check if already downloaded
        if local_path.exists() {
            log::info!("Found local copy: {}", local_path.display());
            return Ok(local_path.to_string_lossy().to_string());
        }

        // Create models directory if it doesn't exist
        if !models_dir.exists() {
            std::fs::create_dir_all(models_dir)
                .map_err(|e| format!("Failed to create models directory: {}", e))?;
        }

        // Download from RCSB
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

    // Not a PDB ID and file doesn't exist
    Err(format!("File not found: {}", input))
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------
fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let input = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "1bfe".to_string());

    log::info!("Foldit starting...");

    let pdb_path = match resolve_structure_path(&input) {
        Ok(path) => path,
        Err(e) => {
            log::error!("{}", e);
            std::process::exit(1);
        }
    };

    log::info!("Loading structure from: {}", pdb_path);

    let app = App::new(pdb_path);
    window::run(app, foldit_frontend::FrontendState::new());
}

