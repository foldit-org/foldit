//! Rosetta runner for background Rosetta operations
//!
//! Runs Rosetta operations (wiggle, shake) in a background thread
//! with streaming updates for real-time visualization.
//!
//! Based on the original Foldit implementations:
//! - Wiggle = Pure minimization, converges when score change < 0.0002
//! - Shake = Pure packing (rotamer optimization), processes residues in batches

use crate::rosetta_ffi::{self, Pose, ScoreFunction};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use tokio::sync::mpsc;

/// Convergence threshold for wiggle (from original Foldit)
const WIGGLE_CONVERGENCE_THRESHOLD: f64 = 0.0002;

/// Intermediate update from Rosetta during operations
#[derive(Debug, Clone)]
pub struct RosettaUpdate {
    /// COORDS bytes of current structure state
    pub coords_bytes: Vec<u8>,
    /// Current Rosetta score
    pub score: f64,
    /// Cycle/iteration number
    pub cycle: u32,
    /// Optional message
    pub message: Option<String>,
    /// Whether the operation has converged/completed
    pub converged: bool,
}

/// Rosetta task types
#[derive(Debug, Clone)]
pub enum RosettaTask {
    /// Start wiggle (pure minimization - no packing)
    /// Runs until score change < 0.0002 or stopped
    StartWiggle {
        /// COORDS bytes of the structure to wiggle
        coords: Vec<u8>,
    },
    /// Start shake (pure packing - no minimization)
    /// Optimizes sidechain rotamers
    StartShake {
        /// COORDS bytes of the structure to shake
        coords: Vec<u8>,
    },
    /// Apply a new sequence and pack rotamers (one-shot operation)
    /// Used after MPNN designs a new sequence
    ApplySequenceAndPack {
        /// COORDS bytes of the structure
        coords: Vec<u8>,
        /// New sequence to apply (one-letter codes)
        sequence: String,
    },
    /// Stop the current operation
    Stop,
}

/// Background Rosetta runner for wiggle and shake operations
pub struct RosettaRunner {
    /// Channel to send tasks
    task_tx: mpsc::Sender<RosettaTask>,
    /// Flag to signal current operation should stop
    stop_flag: Arc<AtomicBool>,
    /// Flag tracking if an operation is currently running
    running: Arc<AtomicBool>,
    /// Flag for full shutdown
    shutdown: Arc<AtomicBool>,
}

impl RosettaRunner {
    /// Create a new Rosetta runner
    ///
    /// Returns the runner and a receiver for intermediate updates
    pub fn new() -> (Self, mpsc::Receiver<RosettaUpdate>) {
        let (update_tx, update_rx) = mpsc::channel(32);
        let (task_tx, mut task_rx) = mpsc::channel::<RosettaTask>(4);
        let stop_flag = Arc::new(AtomicBool::new(false));
        let running = Arc::new(AtomicBool::new(false));
        let shutdown = Arc::new(AtomicBool::new(false));

        let stop_clone = stop_flag.clone();
        let running_clone = running.clone();
        let shutdown_clone = shutdown.clone();

        // Spawn background thread for Rosetta operations
        thread::spawn(move || {
            // Find database path relative to executable
            let db_path = std::env::current_exe()
                .ok()
                .and_then(|exe| exe.parent().map(|p| p.join("rosetta_database")))
                .map(|p| p.to_string_lossy().to_string());

            // Initialize Rosetta with database path
            if let Err(e) = rosetta_ffi::init(db_path.as_deref()) {
                log::error!("Failed to initialize Rosetta: {:?}", e);
                return;
            }
            log::info!("Rosetta initialized successfully (db: {:?})", db_path);

            // Create score function once (reuse for all operations)
            let mut sfxn = match ScoreFunction::ref2015() {
                Some(s) => s,
                None => {
                    log::error!("Failed to create score function");
                    return;
                }
            };

            // Use a simple blocking loop (no async needed for Rosetta)
            while !shutdown_clone.load(Ordering::Relaxed) {
                // Try to receive a task (blocking)
                match task_rx.blocking_recv() {
                    Some(RosettaTask::StartWiggle { coords }) => {
                        log::info!("Starting wiggle ({} bytes)", coords.len());
                        stop_clone.store(false, Ordering::Relaxed);
                        running_clone.store(true, Ordering::Relaxed);

                        // Create pose from coords
                        let mut pose = match Pose::from_coords(&coords) {
                            Some(p) => p,
                            None => {
                                log::error!("Failed to create pose from coords");
                                running_clone.store(false, Ordering::Relaxed);
                                continue;
                            }
                        };

                        // First, repack once to fix any atoms that Rosetta added but we didn't have coords for
                        // This puts them in reasonable positions so minimization can work
                        log::info!("Repacking once to fix missing atom positions...");
                        if let Err(e) = pose.pack_rotamers(&mut sfxn) {
                            log::warn!("Initial repack failed: {:?}", e);
                        }

                        let mut cycle = 0u32;
                        let mut prev_score = pose.score(&mut sfxn);
                        let mut converged = false;

                        log::info!("Starting minimization, initial score: {:.2}", prev_score);

                        // Wiggle loop - pure minimization, runs until converged or stopped
                        // Based on ActionCartGlobalWiggle: converges when |dscore| < 0.0002
                        while !stop_clone.load(Ordering::Relaxed)
                            && !shutdown_clone.load(Ordering::Relaxed)
                            && !converged
                        {
                            cycle += 1;

                            // Pure minimization (no packing) - this is the faithful wiggle
                            if let Err(e) = pose.minimize(&mut sfxn, 0) {
                                log::warn!("Minimize failed: {:?}", e);
                            }

                            // Get current score
                            let score = pose.score(&mut sfxn);
                            let dscore = (score - prev_score).abs();

                            // Check convergence (from original Foldit: fabs(dscore) < 0.0002)
                            if dscore < WIGGLE_CONVERGENCE_THRESHOLD {
                                converged = true;
                                log::info!("Wiggle converged at cycle {} (dscore: {:.6})", cycle, dscore);
                            }

                            prev_score = score;

                            // Export coords
                            let coords_bytes = match pose.to_coords() {
                                Some(c) => c,
                                None => {
                                    log::warn!("Failed to export coords at cycle {}", cycle);
                                    continue;
                                }
                            };

                            // Send update
                            let update = RosettaUpdate {
                                coords_bytes,
                                score,
                                cycle,
                                message: Some(format!(
                                    "Wiggle cycle {} (score: {:.1}, dscore: {:.4})",
                                    cycle, score, dscore
                                )),
                                converged,
                            };

                            if update_tx.blocking_send(update).is_err() {
                                log::warn!("Update receiver dropped, stopping wiggle");
                                break;
                            }

                            log::debug!("Wiggle cycle {} complete, score: {:.2}, dscore: {:.6}", cycle, score, dscore);
                        }

                        running_clone.store(false, Ordering::Relaxed);
                        log::info!("Wiggle stopped after {} cycles (converged: {})", cycle, converged);
                    }

                    Some(RosettaTask::StartShake { coords }) => {
                        log::info!("Starting shake ({} bytes)", coords.len());
                        stop_clone.store(false, Ordering::Relaxed);
                        running_clone.store(true, Ordering::Relaxed);

                        // Create pose from coords
                        let mut pose = match Pose::from_coords(&coords) {
                            Some(p) => p,
                            None => {
                                log::error!("Failed to create pose from coords");
                                running_clone.store(false, Ordering::Relaxed);
                                continue;
                            }
                        };

                        let mut cycle = 0u32;

                        // Shake loop - pure packing (rotamer optimization), no minimization
                        // Based on ActionCartShakeMutate: packs rotamers for sidechains
                        while !stop_clone.load(Ordering::Relaxed)
                            && !shutdown_clone.load(Ordering::Relaxed)
                        {
                            cycle += 1;

                            // Pure packing (no minimization) - this is the faithful shake
                            if let Err(e) = pose.pack_rotamers(&mut sfxn) {
                                log::warn!("Pack failed: {:?}", e);
                            }

                            // Get current score
                            let score = pose.score(&mut sfxn);

                            // Export coords
                            let coords_bytes = match pose.to_coords() {
                                Some(c) => c,
                                None => {
                                    log::warn!("Failed to export coords at cycle {}", cycle);
                                    continue;
                                }
                            };

                            // Send update
                            let update = RosettaUpdate {
                                coords_bytes,
                                score,
                                cycle,
                                message: Some(format!("Shake cycle {} (score: {:.1})", cycle, score)),
                                converged: false, // Shake doesn't have convergence, runs until stopped
                            };

                            if update_tx.blocking_send(update).is_err() {
                                log::warn!("Update receiver dropped, stopping shake");
                                break;
                            }

                            log::debug!("Shake cycle {} complete, score: {:.2}", cycle, score);
                        }

                        running_clone.store(false, Ordering::Relaxed);
                        log::info!("Shake stopped after {} cycles", cycle);
                    }

                    Some(RosettaTask::ApplySequenceAndPack { coords, sequence }) => {
                        log::info!("Applying sequence ({} residues) and packing...", sequence.len());
                        stop_clone.store(false, Ordering::Relaxed);
                        running_clone.store(true, Ordering::Relaxed);

                        // Create pose from coords
                        let mut pose = match Pose::from_coords(&coords) {
                            Some(p) => p,
                            None => {
                                log::error!("Failed to create pose from coords");
                                running_clone.store(false, Ordering::Relaxed);
                                continue;
                            }
                        };

                        // Idealize backbone geometry - essential for RFD3-generated structures
                        // which may have unrealistic bond lengths/angles
                        log::info!("Idealizing backbone geometry...");
                        if let Err(e) = pose.idealize() {
                            log::warn!("Idealization failed: {:?}", e);
                        }

                        // Get current sequence to compare
                        let current_seq = pose.sequence().unwrap_or_default();
                        log::info!("Current sequence: {}", current_seq);
                        log::info!("Target sequence:  {}", sequence);

                        // Mutate residues that differ
                        let mut mutations = 0;
                        for (i, (current, target)) in current_seq.chars().zip(sequence.chars()).enumerate() {
                            if current != target {
                                let res_idx = (i + 1) as u32; // 1-indexed
                                if let Err(e) = pose.mutate_residue(res_idx, target) {
                                    log::warn!("Failed to mutate residue {} from {} to {}: {:?}", res_idx, current, target, e);
                                } else {
                                    mutations += 1;
                                }
                            }
                        }
                        log::info!("Applied {} mutations", mutations);

                        // Pack rotamers to optimize sidechains
                        log::info!("Packing rotamers for new sidechains...");
                        if let Err(e) = pose.pack_rotamers(&mut sfxn) {
                            log::warn!("Pack failed: {:?}", e);
                        }

                        // Minimize to fix geometry after mutations
                        log::info!("Minimizing to fix geometry...");
                        let mut minimize_cycles = 0;
                        let mut prev_score = pose.score(&mut sfxn);
                        loop {
                            minimize_cycles += 1;
                            if let Err(e) = pose.minimize(&mut sfxn, 0) {
                                log::warn!("Minimize failed: {:?}", e);
                                break;
                            }
                            let new_score = pose.score(&mut sfxn);
                            let dscore = (new_score - prev_score).abs();
                            log::info!("Minimize cycle {}: score {:.1} (dscore: {:.4})", minimize_cycles, new_score, dscore);

                            // Stop when converged or after max cycles
                            if dscore < WIGGLE_CONVERGENCE_THRESHOLD || minimize_cycles >= 20 {
                                break;
                            }
                            prev_score = new_score;
                        }
                        log::info!("Minimization done after {} cycles", minimize_cycles);

                        // Get score and export coords
                        let score = pose.score(&mut sfxn);
                        let coords_bytes = match pose.to_coords() {
                            Some(c) => c,
                            None => {
                                log::error!("Failed to export coords after sequence application");
                                running_clone.store(false, Ordering::Relaxed);
                                continue;
                            }
                        };

                        // Send update
                        let update = RosettaUpdate {
                            coords_bytes,
                            score,
                            cycle: 1,
                            message: Some(format!("Applied {} mutations, packed (score: {:.1})", mutations, score)),
                            converged: true, // One-shot operation, always "converged"
                        };

                        if update_tx.blocking_send(update).is_err() {
                            log::warn!("Update receiver dropped");
                        }

                        running_clone.store(false, Ordering::Relaxed);
                        log::info!("Sequence application complete (score: {:.2})", score);
                    }

                    Some(RosettaTask::Stop) => {
                        log::info!("Stop requested");
                        stop_clone.store(true, Ordering::Relaxed);
                    }

                    None => {
                        // Channel closed
                        break;
                    }
                }
            }

            rosetta_ffi::shutdown();
            log::info!("Rosetta runner shutdown");
        });

        (
            Self {
                task_tx,
                stop_flag,
                running,
                shutdown,
            },
            update_rx,
        )
    }

    /// Start wiggling a structure (pure minimization)
    /// Wiggle converges when score change < 0.0002
    pub fn start_wiggle(&self, coords: Vec<u8>) -> Result<(), String> {
        self.stop_flag.store(false, Ordering::Relaxed);
        self.task_tx
            .blocking_send(RosettaTask::StartWiggle { coords })
            .map_err(|e| format!("Failed to start wiggle: {}", e))
    }

    /// Start shaking a structure (pure packing/rotamer optimization)
    /// Shake runs continuously until stopped
    pub fn start_shake(&self, coords: Vec<u8>) -> Result<(), String> {
        self.stop_flag.store(false, Ordering::Relaxed);
        self.task_tx
            .blocking_send(RosettaTask::StartShake { coords })
            .map_err(|e| format!("Failed to start shake: {}", e))
    }

    /// Apply a new sequence to a structure and pack rotamers (one-shot)
    /// Used after MPNN designs a new sequence
    pub fn apply_sequence_and_pack(&self, coords: Vec<u8>, sequence: String) -> Result<(), String> {
        self.task_tx
            .blocking_send(RosettaTask::ApplySequenceAndPack { coords, sequence })
            .map_err(|e| format!("Failed to apply sequence: {}", e))
    }

    /// Stop the current operation (wiggle or shake)
    pub fn stop(&self) {
        self.stop_flag.store(true, Ordering::Relaxed);
        // Also send stop task in case the thread is waiting on recv
        let _ = self.task_tx.blocking_send(RosettaTask::Stop);
    }

    /// Check if an operation is currently running
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    /// Shutdown the runner
    pub fn shutdown(&self) {
        self.stop_flag.store(true, Ordering::Relaxed);
        self.shutdown.store(true, Ordering::Relaxed);
    }
}

impl Drop for RosettaRunner {
    fn drop(&mut self) {
        self.shutdown();
    }
}
