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
//!   V - Toggle view mode (tube/ribbon)
//!   Q - Toggle backbone quality (high/low)
//!   H - Toggle visibility of designed structures
//!   Tab - Cycle focus (Session -> Structure 1 -> ... -> Session)
//!   Delete - Remove last added structure
//!   Esc - Cancel operation / clear selection
//!   Mouse - Rotate/zoom camera

use foldit_rs::action_manager::{ActionManager, ActionType};
use foldit_rs::ml_runner::{MLResult, MLRunner, MLTask, IntermediateUpdate};
use foldit_rs::rosetta::{RosettaExecutor, RosettaUpdate, RosettaSessionState, RosettaStructureId};
use foldit_rs::scene::{Scene, Structure, StructureId, CombinedCoordsResult};
use foldit_rs::session::Session;
use foldit_rs::visual_effects::VisualEffect;
use std::collections::HashMap;
use foldit_conv::coords::{
    align_coords_bytes, extract_ca_from_chains, kabsch_alignment_with_scale,
};
use glam::Vec3;

use foldit_render::animation::AnimationAction;
use foldit_render::engine::ProteinRenderEngine;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Window, WindowId};

/// Main application state
struct App {
    window: Option<Arc<Window>>,
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
    last_frame: Instant,
    last_mouse_pos: (f32, f32),
    pdb_path: String,
    /// Pending animation action (None = no update needed)
    pending_action: Option<AnimationAction>,
}

impl App {
    fn new(pdb_path: String) -> Self {
        Self {
            window: None,
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
            last_frame: Instant::now(),
            last_mouse_pos: (0.0, 0.0),
            pdb_path,
            pending_action: None,
        }
    }

    /// Update engine from scene data with appropriate animation action.
    fn sync_engine_with_scene(&mut self) {
        let Some(action) = self.pending_action.take() else {
            return;
        };

        if let Some(engine) = &mut self.engine {
            let data = self.scene.aggregated();

            // Trigger animation with the appropriate action type
            engine.animate_to_full_pose_with_action(
                &data.backbone_chains,
                &data.sidechain_positions,
                &data.sidechain_bonds,
                &data.sidechain_hydrophobicity,
                &data.sidechain_residue_indices,
                &data.backbone_sidechain_bonds,
                action,
            );

            // Update renderers with target positions
            engine.update_from_aggregated(
                &data.backbone_chains,
                &data.sidechain_positions,
                &data.sidechain_hydrophobicity,
                &data.sidechain_residue_indices,
                &data.sidechain_bonds,
                &data.backbone_sidechain_bonds,
                &data.all_positions,
                false, // Don't fit camera on every update
            );
        }
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

            // Update progress effect
            if let VisualEffect::Progress { .. } = &mut self.effect {
                self.effect.update_progress(update.step, update.total_steps);
            }

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
                                        structure.coords_bytes = new_data.coords_bytes;
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
                    }

                    MLResult::Error(error) => {
                        log::error!("ML error: {}", error);
                        // Unlock any locked structures on error
                        for lock_id in self.action_manager.locked_structures() {
                            self.action_manager.unlock(lock_id);
                        }
                        self.effect = VisualEffect::None;
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
                                structure.coords_bytes = new_structure.coords_bytes;
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
                            structure.coords_bytes = new_structure.coords_bytes;
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
    fn handle_key(&mut self, key: KeyCode) {
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
            }

            KeyCode::KeyV => {
                // Toggle view mode (tube vs ribbon)
                if let Some(engine) = &mut self.engine {
                    engine.toggle_view_mode();
                    log::info!("View mode: {:?}", engine.view_mode);
                }
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
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_none() {
            // Create window
            let window = Arc::new(
                event_loop
                    .create_window(
                        Window::default_attributes()
                            .with_title("Foldit ML Render")
                            .with_inner_size(winit::dpi::LogicalSize::new(1280, 720)),
                    )
                    .expect("Failed to create window"),
            );

            // Create render engine with the specified molecule path
            let mut engine =
                pollster::block_on(ProteinRenderEngine::new_with_path(window.clone(), &self.pdb_path));

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
                        &data.sidechain_bonds,
                        &data.backbone_sidechain_bonds,
                        &data.all_positions,
                        true, // Fit camera on initial load
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

            // Initialize Rosetta executor (uses action-based API: ActionCartGlobalWiggle, etc.)
            let (rosetta_executor, rosetta_updates) = RosettaExecutor::new();
            self.rosetta_executor = Some(rosetta_executor);
            self.rosetta_updates = Some(rosetta_updates);

            window.request_redraw();
            self.window = Some(window);
            self.engine = Some(engine);
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                // Shutdown runners before exit
                if let Some(ref runner) = self.ml_runner {
                    runner.shutdown();
                }
                if let Some(ref executor) = self.rosetta_executor {
                    executor.shutdown();
                }
                event_loop.exit();
            }

            WindowEvent::Resized(newsize) => {
                if let Some(engine) = &mut self.engine {
                    engine.resize(newsize);
                }
            }

            WindowEvent::ScaleFactorChanged { .. } => {
                if let (Some(window), Some(engine)) = (&self.window, &mut self.engine) {
                    let newsize = window.inner_size();
                    engine.resize(newsize); // Full resize including camera aspect ratio
                }
            }

            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                if let PhysicalKey::Code(key) = event.physical_key {
                    self.handle_key(key);
                }
            }

            WindowEvent::RedrawRequested => {
                let now = Instant::now();
                let dt = now.duration_since(self.last_frame);
                self.last_frame = now;

                // Process ML updates
                self.process_ml_updates();

                // Process Rosetta updates (wiggle)
                self.process_rosetta_updates();

                // Sync engine with scene if dirty
                self.sync_engine_with_scene();

                // Update visual effect
                let _intensity = self.effect.tick(dt.as_secs_f32());
                // TODO: Pass intensity to shader for pulsing effect

                // Update camera animation
                if let Some(engine) = &mut self.engine {
                    engine.update_camera_animation(dt.as_secs_f32());
                }

                // Render
                if let Some(engine) = &mut self.engine {
                    let _ = engine.render();
                }

                // Request next frame
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }

            WindowEvent::MouseInput { button, state, .. } => {
                if let Some(engine) = &mut self.engine {
                    let pressed = state == ElementState::Pressed;
                    engine.handle_mouse_button(button, pressed);

                    // Handle selection logic on left button release
                    if button == winit::event::MouseButton::Left && !pressed {
                        engine.handle_mouse_up();
                    }
                }
            }

            WindowEvent::CursorMoved { position, .. } => {
                let delta_x = position.x as f32 - self.last_mouse_pos.0;
                let delta_y = position.y as f32 - self.last_mouse_pos.1;

                if let Some(engine) = &mut self.engine {
                    engine.handle_mouse_move(delta_x, delta_y);
                    engine.handle_mouse_position(position.x as f32, position.y as f32);
                }

                self.last_mouse_pos = (position.x as f32, position.y as f32);

                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }

            WindowEvent::MouseWheel { delta, .. } => {
                if let Some(engine) = &mut self.engine {
                    match delta {
                        MouseScrollDelta::LineDelta(_, y) => engine.handle_mouse_wheel(y),
                        MouseScrollDelta::PixelDelta(pos) => {
                            engine.handle_mouse_wheel(pos.y as f32 * 0.01)
                        }
                    }
                }

                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }

            WindowEvent::ModifiersChanged(modifiers) => {
                if let Some(engine) = &mut self.engine {
                    engine.update_modifiers(modifiers.state());
                }
            }

            _ => (),
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

fn main() {
    // Initialize logging
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // Get PDB ID or path from command line
    let input = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "1bfe".to_string());

    log::info!("Foldit ML Render starting...");

    // Resolve to actual file path (downloading if needed)
    let pdb_path = match resolve_structure_path(&input) {
        Ok(path) => path,
        Err(e) => {
            log::error!("{}", e);
            std::process::exit(1);
        }
    };

    log::info!("Loading structure from: {}", pdb_path);
    log::info!("Controls:");
    log::info!("  W - Wiggle (Rosetta minimize, toggle on/off)");
    log::info!("  S - Shake (Rosetta repack sidechains, toggle on/off)");
    log::info!("  P - Predict (SimpleFold structure prediction)");
    log::info!("  M - MPNN (design sequence for structure)");
    log::info!("  R - RFDiffusion3 (design new structure)");
    log::info!("  V - Toggle view mode (tube/ribbon)");
    log::info!("  Q - Toggle backbone quality (high/low)");
    log::info!("  H - Toggle visibility of designed structures");
    log::info!("  Tab - Cycle focus (Session -> Structure 1 -> ... -> Session)");
    log::info!("  Delete - Remove last added structure");
    log::info!("  Esc - Cancel operation / clear selection");
    log::info!("  Mouse - Rotate/zoom camera");

    let mut app = App::new(pdb_path);
    let event_loop = EventLoop::new().expect("Failed to create event loop");

    event_loop.set_control_flow(ControlFlow::Poll);
    event_loop.run_app(&mut app).expect("Event loop error");
}
