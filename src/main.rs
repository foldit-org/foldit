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
//!   H - Toggle visibility of designed structures
//!   Delete - Remove last added structure
//!   Esc - Cancel current operation
//!   Mouse - Rotate/zoom camera

use foldit_rs::ml_runner::{MLResult, MLRunner, MLTask, IntermediateUpdate};
use foldit_rs::rosetta_runner::{RosettaRunner, RosettaUpdate};
use foldit_rs::scene::{Scene, Structure, StructureId};
use foldit_rs::visual_effects::VisualEffect;
use glam::Vec3;

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
    original_structure_id: Option<StructureId>,
    /// Structure ID for in-flight ML animation (created on first intermediate, updated on each)
    animation_structure_id: Option<StructureId>,
    /// Structure ID for MPNN design (separate from original)
    mpnn_structure_id: Option<StructureId>,
    /// Structure ID for RFD3 design (the backbone we want to design sequence for)
    rfd3_design_id: Option<StructureId>,
    /// Pending MPNN structure waiting for Rosetta to finish applying sequence
    mpnn_pending: bool,
    ml_runner: Option<MLRunner>,
    ml_updates: Option<mpsc::Receiver<IntermediateUpdate>>,
    ml_results: Option<mpsc::Receiver<MLResult>>,
    rosetta_runner: Option<RosettaRunner>,
    rosetta_updates: Option<mpsc::Receiver<RosettaUpdate>>,
    effect: VisualEffect,
    last_frame: Instant,
    last_mouse_pos: (f32, f32),
    pdb_path: String,
    scene_dirty: bool,
}

impl App {
    fn new(pdb_path: String) -> Self {
        Self {
            window: None,
            engine: None,
            scene: Scene::new(),
            original_structure_id: None,
            animation_structure_id: None,
            mpnn_structure_id: None,
            rfd3_design_id: None,
            mpnn_pending: false,
            ml_runner: None,
            ml_updates: None,
            ml_results: None,
            rosetta_runner: None,
            rosetta_updates: None,
            effect: VisualEffect::None,
            last_frame: Instant::now(),
            last_mouse_pos: (0.0, 0.0),
            pdb_path,
            scene_dirty: false,
        }
    }

    /// Update engine from scene data
    fn sync_engine_with_scene(&mut self) {
        if !self.scene_dirty {
            return;
        }

        if let Some(engine) = &mut self.engine {
            let data = self.scene.aggregated();
            engine.update_from_aggregated(
                &data.backbone_chains,
                &data.sidechain_positions,
                &data.sidechain_hydrophobicity,
                &data.sidechain_bonds,
                &data.backbone_sidechain_bonds,
                &data.all_positions,
                false, // Don't fit camera on every update
            );
        }

        self.scene_dirty = false;
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

    /// Update animation structure with intermediate ML positions
    fn update_animation_structure(&mut self, update: &IntermediateUpdate) {
        log::debug!(
            "update_animation_structure: step {}/{}, has_coords={}, backbone_positions={}",
            update.step,
            update.total_steps,
            update.coords_bytes.is_some(),
            update.backbone_positions.len()
        );

        // Get backbone chains from either coords_bytes (SimpleFold) or backbone_positions (RFD3)
        let backbone_chains = if let Some(ref coords_bytes) = update.coords_bytes {
            // SimpleFold: extract backbone from full COORDS
            Self::coords_to_backbone_chains(coords_bytes)
        } else if !update.backbone_positions.is_empty() {
            // RFD3: convert flat backbone positions
            Self::positions_to_backbone_chains(&update.backbone_positions)
        } else {
            log::warn!("No coordinates in update, skipping");
            return;
        };

        log::debug!(
            "Converted to {} chains, first chain has {} points",
            backbone_chains.len(),
            backbone_chains.first().map(|c| c.len()).unwrap_or(0)
        );

        if backbone_chains.is_empty() || backbone_chains[0].is_empty() {
            log::warn!("Empty backbone chains, skipping update");
            return;
        }

        // Create or update the animation structure
        if let Some(anim_id) = self.animation_structure_id {
            // Update existing animation structure
            if let Some(structure) = self.scene.get_mut(anim_id) {
                structure.backbone_chains = backbone_chains;
                structure.name = format!("Predicting... ({}/{})", update.step, update.total_steps);
                log::info!("Updated animation frame {}/{}", update.step, update.total_steps);
            }
        } else {
            // Create new animation structure
            let structure = Structure::from_backbone_design(
                format!("Predicting... ({}/{})", update.step, update.total_steps),
                backbone_chains,
                update.confidence,
            );
            let id = self.scene.add(structure);
            self.animation_structure_id = Some(id);
            log::info!("Created animation structure {:?}", id);
        }

        self.scene_dirty = true;
    }

    /// Extract backbone chains from COORDS bytes (for SimpleFold intermediates)
    fn coords_to_backbone_chains(coords_bytes: &[u8]) -> Vec<Vec<Vec3>> {
        use foldit_conv::coords::binary::deserialize;

        let coords = match deserialize(coords_bytes) {
            Ok(c) => c,
            Err(e) => {
                log::warn!("Failed to parse COORDS: {:?}", e);
                return vec![];
            }
        };

        let mut chains: Vec<Vec<Vec3>> = Vec::new();
        let mut current_chain: Vec<Vec3> = Vec::new();
        let mut last_chain_id: Option<u8> = None;
        let mut last_res_num: Option<i32> = None;

        for i in 0..coords.num_atoms {
            let atom_name = std::str::from_utf8(&coords.atom_names[i])
                .unwrap_or("")
                .trim();

            // Only include N, CA, C for backbone spline (skip O and sidechains)
            if atom_name != "N" && atom_name != "CA" && atom_name != "C" {
                continue;
            }

            let chain_id = coords.chain_ids[i];
            let res_num = coords.res_nums[i];
            let pos = Vec3::new(coords.atoms[i].x, coords.atoms[i].y, coords.atoms[i].z);

            // Check for chain break
            let is_chain_break = last_chain_id.map_or(false, |c| c != chain_id);
            let is_sequence_gap = last_res_num.map_or(false, |r| (res_num - r).abs() > 1);

            if (is_chain_break || is_sequence_gap) && !current_chain.is_empty() {
                chains.push(std::mem::take(&mut current_chain));
            }

            current_chain.push(pos);
            last_chain_id = Some(chain_id);

            if atom_name == "CA" {
                last_res_num = Some(res_num);
            }
        }

        if !current_chain.is_empty() {
            chains.push(current_chain);
        }

        chains
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

                        // Create structure from COORDS (includes sidechains)
                        match Structure::from_coords_bytes(
                            format!("SimpleFold ({:.0}%)", confidence * 100.0),
                            &coords_bytes,
                            confidence,
                        ) {
                            Ok(structure) => {
                                log::info!(
                                    "Created predicted structure: {} residues, {} sidechain atoms",
                                    structure.sequence.len(),
                                    structure.sidechain_atoms.len()
                                );

                                // Remove the animation structure if it exists (replace with final)
                                if let Some(anim_id) = self.animation_structure_id.take() {
                                    if self.scene.remove(anim_id).is_some() {
                                        log::info!("Removed intermediate animation structure");
                                    }
                                }

                                // Add the predicted structure
                                let id = self.scene.add(structure);
                                log::info!("Added predicted structure {:?} to scene", id);
                                self.scene_dirty = true;
                            }
                            Err(e) => {
                                log::error!("Failed to create structure from prediction: {}", e);
                            }
                        }

                        self.effect = VisualEffect::None;
                    }

                    MLResult::SequenceDesign { sequences, scores } => {
                        log::info!("Sequence design complete!");
                        for (i, (seq, score)) in sequences.iter().zip(scores.iter()).enumerate() {
                            log::info!("  {}: {} (score: {:.3})", i + 1, seq, score);
                        }

                        // Find best sequence (highest score, or first if all equal)
                        let best_idx = scores
                            .iter()
                            .enumerate()
                            .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
                            .map(|(i, _)| i)
                            .unwrap_or(0);

                        if let Some(best_seq) = sequences.get(best_idx) {
                            log::info!("Using sequence {} (score: {:.3})", best_idx + 1, scores[best_idx]);

                            // Get coords from the RFD3 design if it exists, otherwise original
                            let target_id = self.rfd3_design_id.or(self.original_structure_id);
                            let coords = target_id
                                .and_then(|id| self.scene.get(id))
                                .and_then(|s| s.get_coords_bytes());

                            if self.rfd3_design_id.is_some() {
                                log::info!("Using RFD3 design structure for MPNN");
                            } else {
                                log::info!("Using original structure for MPNN");
                            }

                            if let Some(coords) = coords {
                                // Apply the sequence and pack rotamers via Rosetta
                                // This will create a NEW structure (not modify original)
                                if let Some(ref runner) = self.rosetta_runner {
                                    // Stop any running operation and drain stale updates
                                    runner.stop();
                                    if let Some(ref mut updates) = self.rosetta_updates {
                                        while updates.try_recv().is_ok() {
                                            // Drain stale updates
                                        }
                                    }

                                    log::info!("Applying designed sequence via Rosetta and packing sidechains...");
                                    self.mpnn_pending = true;
                                    if let Err(e) = runner.apply_sequence_and_pack(coords, best_seq.clone()) {
                                        log::error!("Failed to apply sequence: {}", e);
                                        self.mpnn_pending = false;
                                    }
                                } else {
                                    log::warn!("No Rosetta runner available for sidechain placement");
                                }
                            } else {
                                log::warn!("No coords available from original structure");
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

                        // If we have an animation structure, update it to the final result
                        // Otherwise create a new one
                        if let Some(anim_id) = self.animation_structure_id.take() {
                            if let Some(structure) = self.scene.get_mut(anim_id) {
                                structure.name = format!("RFD3 Design ({:.0}%)", confidence * 100.0);
                                structure.backbone_chains = backbone_chains;
                                log::info!("Updated animation structure {:?} to final result", anim_id);
                            }
                            // Track this as the RFD3 design for MPNN
                            self.rfd3_design_id = Some(anim_id);
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
                            self.rfd3_design_id = Some(id);
                        }

                        self.scene_dirty = true;
                        self.effect = VisualEffect::None;
                    }

                    MLResult::Error(error) => {
                        log::error!("ML error: {}", error);
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

            // Check if this is an MPNN result (create new structure) or wiggle/shake (update existing)
            if self.mpnn_pending && update.converged {
                // MPNN apply completed - create a new structure for the design
                self.mpnn_pending = false;

                log::info!("MPNN update received: {} bytes", update.coords_bytes.len());
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
                        // Remove old MPNN structure if it exists
                        if let Some(old_id) = self.mpnn_structure_id.take() {
                            self.scene.remove(old_id);
                        }
                        // Hide only the RFD3 design (backbone-only) to avoid overlap
                        // Never hide the original structure
                        if let Some(rfd3_id) = self.rfd3_design_id {
                            if let Some(rfd3) = self.scene.get_mut(rfd3_id) {
                                rfd3.visible = false;
                                log::info!("Hiding RFD3 design {:?} to show MPNN design", rfd3_id);
                            }
                        }
                        // Add the new MPNN design as a separate structure
                        let id = self.scene.add(new_structure);
                        self.mpnn_structure_id = Some(id);
                        log::info!("Created MPNN design structure {:?}", id);
                        self.scene_dirty = true;
                    }
                    Err(e) => {
                        log::error!("Failed to create MPNN structure: {}", e);
                    }
                }
            } else if let Some(id) = self.mpnn_structure_id.or(self.original_structure_id) {
                // Wiggle/shake update - update the active structure (MPNN if exists, else original)
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
                        self.scene_dirty = true;
                    }
                    Err(e) => {
                        log::warn!("Failed to update structure from Rosetta: {}", e);
                    }
                }
            }

            // Stop visual effect when operation converges
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
                // Converges when score change < 0.0002
                let rosetta_runner = match &self.rosetta_runner {
                    Some(r) => r,
                    None => {
                        log::warn!("Rosetta runner not initialized");
                        return;
                    }
                };

                // Toggle wiggle on/off
                if rosetta_runner.is_running() {
                    log::info!("Stopping operation...");
                    rosetta_runner.stop();
                    self.effect = VisualEffect::None;
                } else {
                    // Get coords from active structure (MPNN design if exists, else original)
                    let structure_id = self.mpnn_structure_id.or(self.original_structure_id);
                    let coords_bytes = match structure_id {
                        Some(id) => self.scene.get(id).and_then(|s| s.get_coords_bytes()),
                        None => None,
                    };

                    match coords_bytes {
                        Some(coords) => {
                            let target = if self.mpnn_structure_id.is_some() { "MPNN design" } else { "original" };
                            log::info!("Starting wiggle on {} ({} bytes)...", target, coords.len());
                            if let Err(e) = rosetta_runner.start_wiggle(coords) {
                                log::error!("Failed to start wiggle: {}", e);
                                return;
                            }
                            self.effect = VisualEffect::pulsing();
                        }
                        None => {
                            log::warn!("No structure available for wiggle");
                        }
                    }
                }
            }

            KeyCode::KeyS => {
                // Shake: pure packing (rotamer optimization, no minimization)
                // Runs continuously until stopped
                let rosetta_runner = match &self.rosetta_runner {
                    Some(r) => r,
                    None => {
                        log::warn!("Rosetta runner not initialized");
                        return;
                    }
                };

                // Toggle shake on/off
                if rosetta_runner.is_running() {
                    log::info!("Stopping operation...");
                    rosetta_runner.stop();
                    self.effect = VisualEffect::None;
                } else {
                    // Get coords from active structure (MPNN design if exists, else original)
                    let structure_id = self.mpnn_structure_id.or(self.original_structure_id);
                    let coords_bytes = match structure_id {
                        Some(id) => self.scene.get(id).and_then(|s| s.get_coords_bytes()),
                        None => None,
                    };

                    match coords_bytes {
                        Some(coords) => {
                            let target = if self.mpnn_structure_id.is_some() { "MPNN design" } else { "original" };
                            log::info!("Starting shake on {} ({} bytes)...", target, coords.len());
                            if let Err(e) = rosetta_runner.start_shake(coords) {
                                log::error!("Failed to start shake: {}", e);
                                return;
                            }
                            self.effect = VisualEffect::pulsing();
                        }
                        None => {
                            log::warn!("No structure available for shake");
                        }
                    }
                }
            }

            KeyCode::KeyP => {
                // SimpleFold: predict structure from sequence
                let sequence = self.get_original_sequence();
                if sequence.is_empty() {
                    log::warn!("No sequence available");
                    return;
                }

                let ml_runner = match &self.ml_runner {
                    Some(r) => r,
                    None => {
                        log::warn!("ML runner not initialized");
                        return;
                    }
                };

                log::info!(
                    "Starting SimpleFold prediction for {} residues...",
                    sequence.len()
                );

                if let Err(e) = ml_runner.submit(MLTask::Predict {
                    sequence,
                    num_recycles: 3,
                }) {
                    log::error!("Failed to submit prediction task: {}", e);
                    return;
                }

                self.effect = VisualEffect::pulsing();
            }

            KeyCode::KeyM => {
                // MPNN: design sequence for current structure
                let ml_runner = match &self.ml_runner {
                    Some(r) => r,
                    None => {
                        log::warn!("ML runner not initialized");
                        return;
                    }
                };

                // Get the structure to design sequence for
                // Use the most recently added design, or the original structure
                let structure_id = self.animation_structure_id
                    .or(self.original_structure_id);

                let coords_bytes = match structure_id {
                    Some(id) => self.scene.get(id).and_then(|s| s.get_coords_bytes()),
                    None => None,
                };

                match coords_bytes {
                    Some(coords) => {
                        log::info!("Starting MPNN sequence design ({} bytes)...", coords.len());

                        if let Err(e) = ml_runner.submit(MLTask::SequenceDesign {
                            coords,
                            temperature: 0.1,
                            num_sequences: 4,
                        }) {
                            log::error!("Failed to submit sequence design task: {}", e);
                            return;
                        }

                        self.effect = VisualEffect::pulsing();
                    }
                    None => {
                        log::warn!("No structure available for sequence design");
                    }
                }
            }

            KeyCode::KeyR => {
                // RFDiffusion3: design new structure
                let ml_runner = match &self.ml_runner {
                    Some(r) => r,
                    None => {
                        log::warn!("ML runner not initialized");
                        return;
                    }
                };

                log::info!("Starting RFDiffusion3 structure design...");

                if let Err(e) = ml_runner.submit(MLTask::StructureDesign {
                    length: "100-100".to_string(),
                    num_steps: 50,
                }) {
                    log::error!("Failed to submit structure design task: {}", e);
                    return;
                }

                self.effect = VisualEffect::design_highlight(Vec::new());
            }

            KeyCode::KeyH => {
                // Toggle visibility of designed structures (all except original)
                let ids: Vec<StructureId> = self.scene.structure_ids().to_vec();
                for id in ids {
                    if Some(id) != self.original_structure_id {
                        // Extract info before mutable borrow
                        let toggle_info = self.scene.get(id).map(|s| (s.name.clone(), !s.visible));
                        if let Some((name, new_visible)) = toggle_info {
                            self.scene.set_visible(id, new_visible);
                            log::info!("Set {} visibility to {}", name, new_visible);
                        }
                    }
                }
                self.scene_dirty = true;
            }

            KeyCode::Delete | KeyCode::Backspace => {
                // Remove last added structure (keep original)
                if self.scene.len() > 1 {
                    let ids: Vec<StructureId> = self.scene.structure_ids().to_vec();
                    if let Some(&last_id) = ids.last() {
                        if Some(last_id) != self.original_structure_id {
                            if let Some(removed) = self.scene.remove(last_id) {
                                log::info!("Removed structure: {}", removed.name);
                                self.scene_dirty = true;
                            }
                        }
                    }
                }
            }

            KeyCode::Escape => {
                // Cancel current operation
                log::info!("Cancelling current operation");

                // Stop Rosetta operation if running
                if let Some(ref runner) = self.rosetta_runner {
                    if runner.is_running() {
                        runner.stop();
                        log::info!("Stopped Rosetta operation");
                    }
                }

                // Remove animation structure if one exists
                if let Some(anim_id) = self.animation_structure_id.take() {
                    if self.scene.remove(anim_id).is_some() {
                        log::info!("Removed in-progress animation structure");
                        self.scene_dirty = true;
                    }
                }

                self.effect = VisualEffect::None;
            }

            _ => {}
        }
    }

    /// Get the sequence from the original structure
    fn get_original_sequence(&self) -> String {
        if let Some(id) = self.original_structure_id {
            if let Some(structure) = self.scene.get(id) {
                return structure.sequence.clone();
            }
        }
        String::new()
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
                    let id = self.scene.add(structure);
                    self.original_structure_id = Some(id);

                    // Initial sync with engine
                    let data = self.scene.aggregated();
                    engine.update_from_aggregated(
                        &data.backbone_chains,
                        &data.sidechain_positions,
                        &data.sidechain_hydrophobicity,
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

            // Initialize Rosetta runner
            let (rosetta_runner, rosetta_updates) = RosettaRunner::new();
            self.rosetta_runner = Some(rosetta_runner);
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
                if let Some(ref runner) = self.rosetta_runner {
                    runner.shutdown();
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
                    engine.context.resize(newsize);
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
                    engine.handle_mouse_button(button, state == ElementState::Pressed);
                }
            }

            WindowEvent::CursorMoved { position, .. } => {
                let delta_x = position.x as f32 - self.last_mouse_pos.0;
                let delta_y = position.y as f32 - self.last_mouse_pos.1;

                if let Some(engine) = &mut self.engine {
                    engine.handle_mouse_move(delta_x, delta_y);
                    engine.handle_mouse_position((position.x as f32, position.y as f32));
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
        .unwrap_or_else(|| "6ta1".to_string());

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
    log::info!("  H - Toggle visibility of designed structures");
    log::info!("  Delete - Remove last added structure");
    log::info!("  Esc - Cancel current operation");
    log::info!("  Mouse - Rotate/zoom camera");

    let mut app = App::new(pdb_path);
    let event_loop = EventLoop::new().expect("Failed to create event loop");

    event_loop.set_control_flow(ControlFlow::Poll);
    event_loop.run_app(&mut app).expect("Event loop error");
}
