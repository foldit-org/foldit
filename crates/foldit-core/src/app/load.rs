use foldit_gui::AppPhase;

use crate::app::App;
use super::startup::StartupCamera;

impl App {
    /// Free-form file load (Scientist mode). Ingest entities, set
    /// metadata, then hand to bring-up.
    pub(in crate::app) fn handle_load_structure(&mut self, path: &str) {
        self.set_app_phase(AppPhase::LoadingSession);
        // Drop any prior plugin sessions so the armed bring-up re-`Init`s each plugin.
        self.runner_client.reset_for_new_structure();
        let loaded = match crate::structure_io::load_file_as_entities(path) {
            Ok((entities, name)) => {
                log::info!("Loaded structure via IPC: {name}");
                self.store.reset();
                let _ = self.store.seed_history_with_entities(
                    entities,
                    std::path::PathBuf::new(),
                    &name,
                );

                self.store.start(name, None);
                #[cfg(not(target_arch = "wasm32"))]
                if self.gui.view_touched() {
                    self.reapply_view_options_to_engine();
                } else {
                    self.apply_view_preset_to_session("Default");
                }
                true
            }
            Err(e) => {
                log::error!("Failed to load structure '{path}': {e}");
                false
            }
        };

        self.arm_session_bringup(loaded);
    }

    /// Tutorial / campaign puzzle load (Game mode). Ingest entities and
    /// metadata, then hand to bring-up.
    pub(in crate::app) fn handle_load_puzzle(&mut self, puzzle_id: u32) {
        let data = crate::puzzle_load::load_puzzle_structure(puzzle_id);
        self.load_puzzle_from_data(puzzle_id, data);
    }

    /// Load an arbitrary puzzle directory (user-chosen via Load Session).
    pub(in crate::app) fn handle_load_puzzle_dir(&mut self, dir: &str) {
        let data = crate::puzzle_load::load_puzzle_data_from_dir(std::path::Path::new(dir));
        self.load_puzzle_from_data(stable_dir_id(dir), data);
    }

    fn load_puzzle_from_data(
        &mut self,
        puzzle_id: u32,
        data: Result<crate::puzzle_load::PuzzleData, String>,
    ) {
        self.set_app_phase(AppPhase::LoadingSession);
        self.viz.reset();
        self.projectors.render.reset_baselines();
        self.scores.clear_filter_bonus();
        self.pending_dispatches.clear();
        self.runner_client.reset_for_new_structure();

        if let Some(engine) = self.harness.engine.as_mut() {
            engine.clear_scores();
            engine.clear_all_appearance();
        }

        let loaded = match data {
            Ok(puzzle_data) => {
                let render = self.store.load_puzzle(puzzle_id, puzzle_data);

                // Overlay the puzzle's weight patch onto the base map.
                #[cfg(not(target_arch = "wasm32"))]
                {
                    let patch = self.store.puzzle().and_then(|p| p.weight_patch.clone());
                    let base = match crate::scores::load_default_term_weights() {
                        Ok(base) => base,
                        Err(e) => {
                            log::error!(
                                "[App] puzzle {puzzle_id}: failed to reload base \
                                 score-term weights for patch overlay: {e}"
                            );
                            self.scores.term_weights().clone()
                        }
                    };
                    let weights = crate::app::score_coordinator::ScoreCoordinator::overlay_weights(
                        base,
                        patch.as_ref(),
                    );
                    self.scores.set_term_weights(weights);
                }

                // A puzzle may pin its own view preset; otherwise fall back to
                // the Default preset
                #[cfg(not(target_arch = "wasm32"))]
                if self.gui.view_touched() {
                    self.reapply_view_options_to_engine();
                } else {
                    self.apply_view_preset_to_session(
                        render.view_preset.as_deref().unwrap_or("Default"),
                    );
                }

                self.bringup.camera = StartupCamera::PuzzlePose {
                    eye: render.cam_eye,
                    up: render.cam_up,
                };
                self.bringup.ss_override = render.ss_override;

                true
            }
            Err(e) => {
                log::error!("Failed to load puzzle {puzzle_id}: {e}");
                false
            }
        };

        self.arm_session_bringup(loaded);
    }
}

/// Deterministic non-zero puzzle id for a user-chosen dir, high bit forced so
/// the id sits in `[2^31, 2^32)`, disjoint from campaign ids.
fn stable_dir_id(dir: &str) -> u32 {
    use std::hash::Hasher;
    let canonical = std::fs::canonicalize(dir)
        .ok()
        .map_or_else(|| dir.to_owned(), |p| p.to_string_lossy().into_owned());
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    hasher.write(canonical.as_bytes());
    #[allow(clippy::cast_possible_truncation)]
    let low = hasher.finish() as u32;
    low | 0x8000_0000
}
