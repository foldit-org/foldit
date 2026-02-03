//! Embedded ML runner for background model inference
//!
//! Runs ML models (SimpleFold, MPNN, RFDiffusion3) in background threads
//! with streaming updates for real-time visualization.

use glam::Vec3;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

// Import foldit-runner types
use foldit_runner::{
    MLClient, PredictOptions, DesignOptions, SequenceDesignOptions,
    StreamUpdate as MLStreamUpdate, ChainInput,
};

/// Intermediate update from ML model during inference
#[derive(Debug, Clone)]
pub struct IntermediateUpdate {
    /// Raw COORDS bytes (if available, for full atom info)
    pub coords_bytes: Option<Vec<u8>>,
    /// Backbone-only positions (N, CA, C, O per residue) for RFD3-style updates
    pub backbone_positions: Vec<Vec3>,
    /// Current step in the inference process
    pub step: u32,
    /// Total number of steps
    pub total_steps: u32,
    /// Confidence score (0.0 - 1.0)
    pub confidence: f32,
    /// Optional status message
    pub message: Option<String>,
}

/// ML task types that can be submitted to the runner
#[derive(Debug, Clone)]
pub enum MLTask {
    /// Predict structure from sequence (SimpleFold/ESMFold)
    /// For multi-chain complexes, provide chains instead of sequence
    Predict {
        /// Single sequence (legacy, for single-chain proteins)
        sequence: Option<String>,
        /// Multi-chain: list of (chain_id, sequence) tuples
        chains: Vec<(String, String)>,
        num_recycles: u32,
    },
    /// Design sequence from structure (LigandMPNN)
    /// Note: Requires COORDS bytes - use coords_from_positions() to convert
    SequenceDesign {
        coords: Vec<u8>,
        temperature: f32,
        num_sequences: u32,
    },
    /// Design structure (RFDiffusion3)
    StructureDesign {
        length: String,
        num_steps: u32,
    },
}

/// Result from ML task completion
#[derive(Debug, Clone)]
pub enum MLResult {
    /// Structure prediction result
    Predict {
        /// Raw COORDS bytes (for creating full Structure with sidechains)
        coords_bytes: Vec<u8>,
        /// Confidence score (pLDDT)
        confidence: f32,
    },
    /// Sequence design result
    SequenceDesign {
        sequences: Vec<String>,
        scores: Vec<f32>,
    },
    /// Structure design result (backbone-only from RFDiffusion3)
    StructureDesign {
        /// Backbone chains (each chain is a sequence of N, CA, C positions)
        backbone_chains: Vec<Vec<Vec3>>,
        confidence: f32,
    },
    /// Error during ML operation
    Error(String),
}

/// Background ML runner that manages model inference
pub struct MLRunner {
    /// Channel to send tasks to the worker thread
    task_tx: mpsc::Sender<MLTask>,
    /// Flag to signal shutdown
    shutdown: Arc<AtomicBool>,
}

impl MLRunner {
    /// Create a new ML runner with update channel
    ///
    /// Returns the runner and receivers for intermediate updates and final results.
    pub fn new() -> (Self, mpsc::Receiver<IntermediateUpdate>, mpsc::Receiver<MLResult>) {
        let (update_tx, update_rx) = mpsc::channel(32);
        let (result_tx, result_rx) = mpsc::channel(4);
        let (task_tx, mut task_rx) = mpsc::channel::<MLTask>(4);
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();

        // Spawn background thread for ML tasks
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("Failed to create tokio runtime");

            rt.block_on(async {
                // Initialize ML client
                let client = match MLClient::new(None) {
                    Ok(c) => {
                        log::info!("ML client initialized successfully");
                        Some(c)
                    }
                    Err(e) => {
                        log::error!("Failed to initialize ML client: {}", e);
                        None
                    }
                };

                while !shutdown_clone.load(Ordering::Relaxed) {
                    match task_rx.recv().await {
                        Some(task) => {
                            let result = if let Some(ref client) = client {
                                Self::run_task(task, client, &update_tx).await
                            } else {
                                MLResult::Error("ML client not initialized".to_string())
                            };
                            if result_tx.send(result).await.is_err() {
                                log::warn!("Result receiver dropped");
                                break;
                            }
                        }
                        None => break,
                    }
                }

                // Shutdown client when done
                if let Some(client) = client {
                    client.shutdown();
                }

                log::info!("ML runner shutdown");
            });
        });

        (
            Self {
                task_tx,
                shutdown,
            },
            update_rx,
            result_rx,
        )
    }

    /// Submit a task for background execution
    pub fn submit(&self, task: MLTask) -> Result<(), String> {
        self.task_tx
            .blocking_send(task)
            .map_err(|e| format!("Failed to submit task: {}", e))
    }

    /// Shutdown the ML runner
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }

    /// Run an ML task using the foldit-ml client
    async fn run_task(
        task: MLTask,
        client: &MLClient,
        update_tx: &mpsc::Sender<IntermediateUpdate>,
    ) -> MLResult {
        match task {
            MLTask::Predict { sequence, chains, num_recycles } => {
                // For single chain, use the simpler single-sequence API
                // Multi-chain path is only used when there are 2+ chains
                if chains.len() > 1 {
                    let total_residues: usize = chains.iter().map(|(_, s)| s.len()).sum();
                    log::info!("Running SimpleFold prediction for {} chains ({} total residues)", chains.len(), total_residues);
                    Self::run_predict_chains(client, &chains, num_recycles, update_tx).await
                } else if chains.len() == 1 {
                    // Single chain provided via chains field - extract sequence
                    let seq = &chains[0].1;
                    log::info!("Running SimpleFold prediction for sequence of {} residues", seq.len());
                    Self::run_predict(client, seq, num_recycles, update_tx).await
                } else if let Some(seq) = sequence {
                    log::info!("Running SimpleFold prediction for sequence of {} residues", seq.len());
                    Self::run_predict(client, &seq, num_recycles, update_tx).await
                } else {
                    MLResult::Error("Either sequence or chains must be provided".to_string())
                }
            }
            MLTask::SequenceDesign { coords, temperature, num_sequences } => {
                log::info!("Running LigandMPNN sequence design, T={}, num={}", temperature, num_sequences);
                Self::run_sequence_design(client, &coords, temperature, num_sequences).await
            }
            MLTask::StructureDesign { length, num_steps } => {
                log::info!("Running RFDiffusion3 structure design, length={}, steps={}", length, num_steps);
                Self::run_structure_design(client, &length, num_steps, update_tx).await
            }
        }
    }

    /// Run structure prediction with streaming
    async fn run_predict(
        client: &MLClient,
        sequence: &str,
        num_recycles: u32,
        update_tx: &mpsc::Sender<IntermediateUpdate>,
    ) -> MLResult {
        let options = PredictOptions {
            model: Some("simplefold".to_string()),
            num_recycles,
            stream: true,
            chains: vec![], // Single-chain prediction
        };

        // Clone sender for the callback
        let tx = update_tx.clone();

        let callback: Box<dyn FnMut(MLStreamUpdate) + Send> = Box::new(move |update: MLStreamUpdate| {
            // SimpleFold provides full COORDS with all atoms
            let intermediate = IntermediateUpdate {
                coords_bytes: update.intermediate_coords.clone(),
                backbone_positions: vec![], // SimpleFold uses coords_bytes instead
                step: update.step,
                total_steps: update.total_steps,
                confidence: update.confidence,
                message: Some(format!("{} ({}/{})", update.stage, update.step, update.total_steps)),
            };

            // Try to send, ignore if receiver is full or closed
            let _ = tx.try_send(intermediate);
        });

        match client.predict_streaming(sequence, options, callback) {
            Ok(result) => {
                MLResult::Predict {
                    coords_bytes: result.coords,
                    confidence: result.confidence,
                }
            }
            Err(e) => MLResult::Error(format!("Prediction failed: {}", e)),
        }
    }

    /// Run structure prediction for multi-chain complexes with streaming
    async fn run_predict_chains(
        client: &MLClient,
        chains: &[(String, String)],
        num_recycles: u32,
        update_tx: &mpsc::Sender<IntermediateUpdate>,
    ) -> MLResult {
        let options = PredictOptions {
            model: Some("simplefold".to_string()),
            num_recycles,
            stream: true,
            chains: chains.iter()
                .map(|(id, seq)| ChainInput {
                    chain_id: id.clone(),
                    sequence: seq.clone(),
                })
                .collect(),
        };

        // Clone sender for the callback
        let tx = update_tx.clone();

        let callback: Box<dyn FnMut(MLStreamUpdate) + Send> = Box::new(move |update: MLStreamUpdate| {
            let intermediate = IntermediateUpdate {
                coords_bytes: update.intermediate_coords.clone(),
                backbone_positions: vec![],
                step: update.step,
                total_steps: update.total_steps,
                confidence: update.confidence,
                message: Some(format!("{} ({}/{})", update.stage, update.step, update.total_steps)),
            };

            let _ = tx.try_send(intermediate);
        });

        // For multi-chain, we pass empty string as sequence (chains field takes precedence)
        match client.predict_streaming("", options, callback) {
            Ok(result) => {
                MLResult::Predict {
                    coords_bytes: result.coords,
                    confidence: result.confidence,
                }
            }
            Err(e) => MLResult::Error(format!("Multi-chain prediction failed: {}", e)),
        }
    }

    /// Run sequence design (non-streaming)
    async fn run_sequence_design(
        client: &MLClient,
        coords: &[u8],
        temperature: f32,
        num_sequences: u32,
    ) -> MLResult {
        let options = SequenceDesignOptions {
            temperature,
            num_sequences,
            fixed_positions: vec![],
        };

        match client.sequence_design(coords, options) {
            Ok(result) => MLResult::SequenceDesign {
                sequences: result.sequences,
                scores: result.scores,
            },
            Err(e) => MLResult::Error(format!("Sequence design failed: {}", e)),
        }
    }

    /// Run structure design with streaming
    async fn run_structure_design(
        client: &MLClient,
        length: &str,
        num_steps: u32,
        update_tx: &mpsc::Sender<IntermediateUpdate>,
    ) -> MLResult {
        let options = DesignOptions {
            length: length.to_string(),
            num_steps,
            num_designs: 1,
            use_mps: true, // Use MPS on macOS
            stream: true,
        };

        // Clone sender for the callback
        let tx = update_tx.clone();

        let callback: Box<dyn FnMut(MLStreamUpdate) + Send> = Box::new(move |update: MLStreamUpdate| {
            let has_coords = update.intermediate_coords.is_some();
            let coords_len = update.intermediate_coords.as_ref().map(|c| c.len()).unwrap_or(0);

            // RFD3 provides backbone-only positions (N, CA, C, O per residue)
            let backbone_positions = update.intermediate_coords
                .as_ref()
                .map(|c| coords_to_positions(c))
                .unwrap_or_default();

            log::info!(
                "ML callback: stage={}, step={}/{}, has_coords={}, coords_bytes={}, positions={}",
                update.stage, update.step, update.total_steps,
                has_coords, coords_len, backbone_positions.len()
            );

            let intermediate = IntermediateUpdate {
                coords_bytes: None, // RFD3 uses backbone_positions instead
                backbone_positions,
                step: update.step,
                total_steps: update.total_steps,
                confidence: update.confidence,
                message: Some(format!("{} ({}/{})", update.stage, update.step, update.total_steps)),
            };

            let _ = tx.try_send(intermediate);
        });

        match client.design_streaming(options, callback) {
            Ok(result) => {
                let backbone_chains = coords_to_backbone_chains(&result.coords);
                MLResult::StructureDesign {
                    backbone_chains,
                    confidence: result.confidence,
                }
            }
            Err(e) => MLResult::Error(format!("Structure design failed: {}", e)),
        }
    }
}

impl Default for MLRunner {
    fn default() -> Self {
        Self::new().0
    }
}

impl Drop for MLRunner {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Convert COORDS binary format to Vec3 positions
fn coords_to_positions(coords_bytes: &[u8]) -> Vec<Vec3> {
    if coords_bytes.is_empty() {
        return vec![];
    }

    // Deserialize COORDS binary to Coords struct
    match foldit_conv::coords::binary::deserialize(coords_bytes) {
        Ok(coords) => {
            // Extract positions from all atoms
            coords.atoms.iter()
                .map(|atom| Vec3::new(atom.x, atom.y, atom.z))
                .collect()
        }
        Err(e) => {
            log::warn!("Failed to parse COORDS: {:?}", e);
            vec![]
        }
    }
}

/// Convert COORDS binary format to backbone chains for tube rendering
/// Extracts N, CA, C atoms (in order) grouped by chain, with chain breaks at gaps
fn coords_to_backbone_chains(coords_bytes: &[u8]) -> Vec<Vec<Vec3>> {
    if coords_bytes.is_empty() {
        return vec![];
    }

    match foldit_conv::coords::binary::deserialize(coords_bytes) {
        Ok(coords) => {
            let mut chains: Vec<Vec<Vec3>> = Vec::new();
            let mut current_chain: Vec<Vec3> = Vec::new();
            let mut last_chain_id: Option<u8> = None;
            let mut last_res_num: Option<i32> = None;

            // Collect backbone atoms (N, CA, C) in order
            // RFD3 outputs atoms in order: N, CA, C, O for each residue
            for i in 0..coords.num_atoms {
                let atom_name = std::str::from_utf8(&coords.atom_names[i])
                    .unwrap_or("")
                    .trim();

                // Only include N, CA, C for smooth backbone spline (skip O)
                if atom_name != "N" && atom_name != "CA" && atom_name != "C" {
                    continue;
                }

                let chain_id = coords.chain_ids[i];
                let res_num = coords.res_nums[i];
                let pos = Vec3::new(
                    coords.atoms[i].x,
                    coords.atoms[i].y,
                    coords.atoms[i].z,
                );

                // Check for chain break
                let is_chain_break = last_chain_id.map_or(false, |c| c != chain_id);
                let is_sequence_gap = last_res_num.map_or(false, |r| (res_num - r).abs() > 1);

                if (is_chain_break || is_sequence_gap) && !current_chain.is_empty() {
                    chains.push(std::mem::take(&mut current_chain));
                }

                current_chain.push(pos);
                last_chain_id = Some(chain_id);

                // Only update res_num on CA to track residue changes properly
                if atom_name == "CA" {
                    last_res_num = Some(res_num);
                }
            }

            // Don't forget the last chain
            if !current_chain.is_empty() {
                chains.push(current_chain);
            }

            log::info!(
                "Extracted {} backbone chains with {} total atoms",
                chains.len(),
                chains.iter().map(|c| c.len()).sum::<usize>()
            );

            chains
        }
        Err(e) => {
            log::warn!("Failed to parse COORDS for backbone: {:?}", e);
            vec![]
        }
    }
}
