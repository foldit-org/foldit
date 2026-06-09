use foldit_gui::AppPhase;
use molex::entity::molecule::id::EntityId;
use viso::VisoEngine;

use crate::app::App;
use crate::session::Puzzle;
#[cfg(not(target_arch = "wasm32"))]
use foldit_gui::DirtyFlags;
#[cfg(not(target_arch = "wasm32"))]
use crate::history::CheckpointKind;
use crate::render_projector::RenderProjector;
#[cfg(not(target_arch = "wasm32"))]
use crate::runner_client::DispatchIntent;

impl App {
    pub fn handle_app_command(&mut self, command: foldit_gui::AppCommand) {
        use foldit_gui::AppCommand;

        // History-side commands take &mut self (no engine borrow held).
        if let AppCommand::History { cmd } = command {
            self.run_history_command(&cmd);
            return;
        }

        // Bubble cursor advance is engine-independent.
        if let AppCommand::AdvanceBubble { back } = command {
            self.advance_bubble(back);
            return;
        }

        if self.engine.is_none() {
            return;
        }

        // Engine borrow is taken per-arm now (LoadStructure / LoadPuzzle
        // need to release the borrow before `self.tick(0.0)`, which is
        // how the render projector republishes after a load).
        match command {
            AppCommand::LoadStructure { path } => self.handle_load_structure(&path),
            AppCommand::LoadPuzzle { puzzle_id } => self.handle_load_puzzle(puzzle_id),
            AppCommand::SetViewOptions { options } => {
                // The session is the source of truth: store the options and
                // let the tick apply them to the engine (+ raise VIEW) off
                // the emitted `ViewOptionsChanged`.
                match serde_json::from_value::<viso::options::VisoOptions>(options) {
                    Ok(opts) => self.store.set_view_options(opts),
                    Err(e) => log::error!("Failed to deserialize view options: {e}"),
                }
            }
            AppCommand::LoadViewPreset { name } => {
                #[cfg(not(target_arch = "wasm32"))]
                if let Some(dir) = self.host.view_presets_dir() {
                    if let Some(engine) = self.engine.as_mut() {
                        // Use the engine to read the preset file off disk,
                        // then record it as the active preset on the session
                        // (the source of truth). The tick re-applies the
                        // options to the engine + raises VIEW.
                        engine.load_preset(&name, dir);
                        let opts = engine.options().clone();
                        self.store.apply_preset(name, opts);
                    }
                }
                #[cfg(target_arch = "wasm32")]
                let _ = name;
            }
            AppCommand::SaveViewPreset { name } => {
                // Writes to the preset *library* on disk; it does not change
                // the active view options, only the available-presets list.
                // No `SessionUpdate` carries a disk-library change, so refresh
                // just that list onto the frontend at-site (the same read the
                // VIEW arm of the GUI consumer does) rather than re-pushing
                // the whole VIEW section.
                #[cfg(not(target_arch = "wasm32"))]
                // Own the dir so the `&self.host` borrow is released before
                // the disjoint `&mut self.engine` / `&mut self.frontend`
                // borrows below.
                if let Some(dir) = self.host.view_presets_dir().map(std::path::Path::to_path_buf) {
                    if let Some(engine) = self.engine.as_mut() {
                        engine.save_preset(&name, &dir);
                    }
                    self.frontend.view.available_presets =
                        viso::options::VisoOptions::list_presets(&dir);
                    self.frontend.mark_dirty(DirtyFlags::VIEW);
                }
                #[cfg(target_arch = "wasm32")]
                let _ = name;
            }
            AppCommand::History { .. } | AppCommand::AdvanceBubble { .. } => {
                // Handled in the early-return block above. The match is
                // exhaustive over `AppCommand`: a new variant
                // without a handler is a compile error.
            }
        }
    }

    /// Free-form file load (Scientist mode). Ingest entities, set
    /// metadata, then tick + fit the camera (tick is how the render
    /// projector republishes - the `SessionUpdate` stream carries `PreviewAdded`s and
    /// `HeadMoved`s from `load_entity_into_history`).
    fn handle_load_structure(&mut self, path: &str) {
        self.set_app_phase(AppPhase::LoadingSession);
        match crate::puzzle::load_file_as_entities(path) {
            Ok((entities, name)) => {
                log::info!("Loaded structure via IPC: {name}");
                for entity in entities {
                    let _ = self.store.load_entity_into_history(entity, &name);
                }
                // Free-form load: set the title and drop any puzzle
                // objective + tutorial bubbles through the create seam.
                // `start` emits `PuzzleChanged` (via `clear_puzzle`) when
                // there was a puzzle to clear, which the tick turns into
                // PUZZLE dirty. A scientist→scientist reload where
                // `clear_puzzle` is a no-op emits nothing, so the puzzle
                // panel's title refresh rides the full populate below.
                self.store.start(name, None);

                // Publish + fit. tick(0.0) drains the `SessionUpdate` stream, publishes
                // via the render projector, and runs engine.update(0.0)
                // so fit_camera_to_focus has bounding-radius to read.
                self.tick(0.0);
                if let Some(engine) = self.engine.as_mut() {
                    engine.fit_camera_to_focus();
                }
            }
            Err(e) => {
                log::error!("Failed to load structure '{path}': {e}");
            }
        }
        // Stamp the first per-residue score synchronously, then drain its
        // `ScoresChanged` through the render projector with a tick(0.0) so the
        // backbone is already colored on the first rendered frame (no gray
        // flash). `score_head_now` no-ops when there is no scoring plugin.
        self.score_head_now();
        self.tick(0.0);
        // Loading is done: flip into InSession (clears the loading screen) and
        // raise the one-shot full populate so the next tick's GUI consumer
        // pushes every section, covering the puzzle-panel title and the
        // not-scored-yet gauge / catalog that no batch variant carries on a
        // free-form load.
        self.enter_session();
    }

    /// Tutorial / campaign puzzle load (Game mode). Ingest entities and
    /// metadata, then tick + snap + apply the puzzle's saved pose.
    fn handle_load_puzzle(&mut self, puzzle_id: u32) {
        self.set_app_phase(AppPhase::LoadingSession);
        // Entity display name for the loaded molecules: the outgoing
        // session title (captured before `reset`, which leaves it intact).
        let title = self.store.title().to_owned();
        self.store.reset();
        self.runner_client.reset_for_new_structure();
        // Topology swap: `Session::reset` already cleared the selection
        // (entity ids from the outgoing assembly can collide numerically
        // with the incoming ones without referring to the same entities).
        // Emit `SelectionChanged` explicitly so the tick re-pushes the
        // now-empty highlight to viso and raises SELECTION dirty; `reset`
        // itself only emits `HeadMoved`.
        self.store.clear_selection();
        // viso's own per-entity score map has an id-reuse hole:
        // replace_assembly now preserves scores across a swap (so a
        // settling preview doesn't flash the survivors gray), reconciling
        // membership by id. A puzzle reload restarts the entity allocator,
        // so the new puzzle's ids collide with the outgoing ones and would
        // inherit their colors; clear viso scores explicitly here.
        if let Some(engine) = self.engine.as_mut() {
            engine.clear_scores();
        }

        match crate::puzzle::load_puzzle_structure(puzzle_id) {
            Ok(puzzle_data) => {
                // Install the puzzle (title + objective + tutorial bubbles)
                // through the create seam. The tutorial sequence and its
                // cursor move together: a non-empty sequence starts at index
                // 0, an empty sequence is `None`. `start` emits
                // `PuzzleChanged`, which the tick turns into PUZZLE dirty
                // (the PUZZLE arm also pushes the current bubble).
                let bubbles = if puzzle_data.bubbles.is_empty() {
                    None
                } else {
                    Some(puzzle_data.bubbles)
                };
                let current_bubble = bubbles.as_ref().map(|_| 0);
                self.store.start(
                    puzzle_data.name.clone(),
                    Some(Puzzle {
                        id: puzzle_id,
                        start_energy: puzzle_data.start_energy,
                        completion_energy: puzzle_data.completion_score,
                        bubbles,
                        current_bubble,
                    }),
                );

                #[cfg(not(target_arch = "wasm32"))]
                if let Some(preset_name) = &puzzle_data.view_preset {
                    if let Some(dir) = self.host.view_presets_dir() {
                        if let Some(engine) = self.engine.as_mut() {
                            // Read the preset off disk via the engine, then
                            // record it as the active preset on the session.
                            // The tick(0.0) below drains the emitted
                            // `ViewOptionsChanged` and re-applies the options.
                            engine.load_preset(preset_name, dir);
                            let opts = engine.options().clone();
                            self.store.apply_preset(preset_name.clone(), opts);
                        }
                    }
                }

                let ss_override = puzzle_data.ss_override;
                let cam = &puzzle_data.camera;
                // GPU camera is f32; puzzle coords are f64.
                #[allow(clippy::cast_possible_truncation)]
                let cam_eye =
                    glam::Vec3::new(cam.eye[0] as f32, cam.eye[1] as f32, cam.eye[2] as f32);
                // GPU camera is f32; puzzle coords are f64.
                #[allow(clippy::cast_possible_truncation)]
                let cam_up = glam::Vec3::new(cam.up[0] as f32, cam.up[1] as f32, cam.up[2] as f32);

                let mut ids: Vec<EntityId> = Vec::new();
                for entity in puzzle_data.entities {
                    if let Some(id) =
                        self.store.load_entity_into_history(entity, &title)
                    {
                        ids.push(id);
                    }
                }

                // Topology swap rides the `SessionUpdate` stream - tick's render
                // projector picks `replace_assembly` because the id set
                // differs from the last publish (post-reset = empty).
                self.tick(0.0);

                if let Some(engine) = self.engine.as_mut() {
                    // Snap so bounding_radius reflects molecule extent
                    // (fog driver), then override the pose with the
                    // puzzle's saved eye/up but anchor the orbit
                    // center on the protein centroid.
                    engine.snap_camera_to_focus();
                    if let Some(centroid) = engine.focus_centroid() {
                        engine.set_camera_pose(centroid, cam_eye, cam_up);
                    }

                    if let Some(ss) = ss_override {
                        if let Some(&first_id) = ids.first() {
                            engine.set_ss_override(first_id.raw(), ss);
                        }
                    }
                }
            }
            Err(e) => log::error!("Failed to load puzzle {puzzle_id}: {e}"),
        }
        // Stamp the first per-residue score synchronously, then drain its
        // `ScoresChanged` through the render projector with a tick(0.0) so the
        // backbone is already colored on the first rendered frame (no gray
        // flash). `score_head_now` no-ops when there is no scoring plugin.
        self.score_head_now();
        self.tick(0.0);
        // PUZZLE rides the `start` emit drained by the inner `tick(0.0)`
        // above (its PUZZLE arm also pushes the current bubble), and the
        // topology swap re-pushes the entity list via the batch. Loading is
        // done: flip into InSession (clears the loading screen) and raise the
        // one-shot full populate to push the not-scored-yet gauge / catalog
        // the batch does not carry on a reload.
        self.enter_session();
    }

    // ── Tutorial-bubble cursor ──

    /// Step the tutorial-bubble cursor on the session. The cursor lives on
    /// `Session` now; this forwards and the emitted `BubbleChanged` is
    /// turned into `TEXT_BUBBLE` dirty by the tick, which re-pushes the new
    /// head. Forward saturates one past the end (the GUI then clears);
    /// back saturates at 0.
    fn advance_bubble(&mut self, back: bool) {
        self.store.advance_bubble(back);
    }
    // ── Complex lifecycle (engine attach + non-blocking startup) ──

    /// Attach a host-built `VisoEngine` to this App. Hosts are
    /// responsible for constructing the wgpu `RenderContext` against
    /// their own surface (winit window on desktop, `<canvas>` on web)
    /// and applying any preset / render-scale tweaks they want before
    /// handing it over.
    pub fn attach_engine(&mut self, engine: VisoEngine) {
        self.engine = Some(engine);
    }

    /// Begin app bring-up: the one host trigger that arms the non-blocking
    /// startup state-machine `advance_startup` drives one step per frame.
    /// Runs AFTER the webview's loading screen is visible so the user
    /// has feedback during the (potentially slow) plugin warm + session
    /// init. Requires `create_render_context` to have run first.
    ///
    /// Does the one-shot non-blocking setup (fresh orchestrator, default
    /// score-term weights, plugin discovery + warm kick) and then branches on
    /// the host bootstrap path
    /// (`HostResources::initial_structure_path`):
    /// - `None`: no structure to load; settle the user at `Landing` while
    ///   the warms finish in the background.
    /// - `Some(path)`: enter `LoadingSession` and stash the path for the
    ///   `Warming` step to parse once every plugin has connected.
    ///
    /// Never blocks: the worker round-trips (warm connect, `Init`,
    /// normalize, first score) are all kicked here / by `advance_startup` and
    /// polled on later frames, so the host renders the loading screen
    /// throughout.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn begin_startup(&mut self) {
        if self.engine.is_none() {
            log::error!("begin_startup called before create_render_context");
            return;
        }

        // A fresh orchestrator restarts request ids at 1, so drop any
        // stale composition targets before a new edit can reuse an old id.
        self.runner_client.init_orchestrator();
        self.score_targets.clear();

        // Load the default score-term weights once, before the first score.
        // `reset` leaves `term_weights` untouched, so a single load here
        // carries across reloads; the empty-check makes a re-arm a no-op.
        // On failure, log and proceed degraded (every weight then resolves
        // to 0.0, so scores read 0 until a valid map lands -- the app stays
        // up rather than crashing on a missing asset).
        if self.store.term_weights().is_empty() {
            match crate::scores::load_default_term_weights() {
                Ok(weights) => {
                    log::info!(
                        "[App] loaded {} default score-term weights",
                        weights.len()
                    );
                    self.store.set_term_weights(weights);
                }
                Err(e) => log::error!("[App] failed to load default score-term weights: {e}"),
            }
        }

        // Discover + KICK a warm for every plugin under the runtime plugin
        // root. The connects finish on later frames via `poll_warms`; the
        // returned ids are the set the `Warming` step tallies completions
        // against. A missing plugins root degrades to viewer-only (empty
        // expected set, so the machine falls straight through to the parse).
        let expected: std::collections::BTreeSet<String> = if let Some(root) = locate_plugins_root()
        {
            log::info!("[App] discovering + warming plugins under {}", root.display());
            self.runner_client.kick_warms(&root).into_iter().collect()
        } else {
            log::warn!(
                "[App] no plugins root found (set FOLDIT_PLUGINS_ROOT or run \
                 from a workspace checkout); plugins disabled"
            );
            std::collections::BTreeSet::new()
        };

        match self.host.initial_structure_path() {
            None => {
                // No initial structure: the user lands in the menus. The
                // warms still finish in the background so a later file-load
                // finds the workers connected.
                self.set_app_phase(AppPhase::Landing);
                self.startup = StartupPhase::WarmingForLanding {
                    expected,
                    connected: std::collections::BTreeSet::new(),
                };
            }
            Some(path) => {
                self.set_app_phase(AppPhase::LoadingSession);
                self.startup = StartupPhase::Warming {
                    expected,
                    connected: std::collections::BTreeSet::new(),
                    path,
                };
            }
        }
    }

    /// Advance the non-blocking startup state-machine by one step. Called
    /// near the top of [`Self::tick`], before the `SessionUpdate` drain, so a
    /// publish a step triggers (the structure parse, a normalize commit) is
    /// drained + projected the same frame. Each step polls for whatever
    /// worker replies arrived since the last frame, folds them into the
    /// in-flight accumulator, and on completeness kicks the next phase. No
    /// step blocks. Inert in `Idle` / `Done`.
    /// True once the startup state-machine has reached its terminal state.
    /// The machine drives the first score itself (the post-normalize kick);
    /// the tick's at-rest auto-rescore must hold off until then so it does
    /// not fire a `score` query into the pose-less window between a plugin's
    /// Init (session registered) and its normalize (pose built), which comes
    /// back empty.
    #[cfg(not(target_arch = "wasm32"))]
    pub(in crate::app) const fn startup_settled(&self) -> bool {
        matches!(self.startup, StartupPhase::Done)
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(in crate::app) fn advance_startup(&mut self) {
        match std::mem::replace(&mut self.startup, StartupPhase::Idle) {
            StartupPhase::Idle => {}
            StartupPhase::Done => self.startup = StartupPhase::Done,
            StartupPhase::Warming {
                expected,
                mut connected,
                path,
            } => {
                for (plugin_id, _ok) in self.runner_client.poll_warms() {
                    connected.insert(plugin_id);
                }
                if connected.is_superset(&expected) {
                    self.startup = self.warms_done_load_and_kick_inits(&path);
                } else {
                    self.startup = StartupPhase::Warming {
                        expected,
                        connected,
                        path,
                    };
                }
            }
            StartupPhase::Initializing {
                expected,
                mut adopted,
            } => {
                for (plugin_id, init_bytes) in self.runner_client.poll_inits() {
                    self.apply_post_init(&plugin_id, &init_bytes, "_init_normalize", "Init");
                    adopted.insert(plugin_id);
                }
                if adopted.is_superset(&expected) {
                    self.startup = self.inits_done_kick_normalizes(&adopted);
                } else {
                    self.startup = StartupPhase::Initializing { expected, adopted };
                }
            }
            StartupPhase::Normalizing {
                expected,
                mut applied,
            } => {
                for (plugin_id, normalized_bytes) in self.runner_client.poll_normalizes() {
                    // Adopt the normalized assembly the same way the Init
                    // bytes are adopted: a committed `PluginOp` checkpoint
                    // (NOT a pending edit), so the head carries the
                    // full-atom pose before the first score stamps it. The
                    // op-id labels the checkpoint; re-read it from the
                    // manifest (the poll drops the dispatch's request_id /
                    // op-id, and the manifest read is stable).
                    let op_id = self
                        .runner_client
                        .normalize_op_for(&plugin_id)
                        .unwrap_or_else(|| String::from("_init_normalize"));
                    self.apply_post_init(&plugin_id, &normalized_bytes, &op_id, "Init");
                    applied.insert(plugin_id);
                }
                if applied.is_superset(&expected) {
                    // Normalize commits are done (committed above), so the
                    // first score now stamps the normalized head. Kick it;
                    // tick's at-rest gate may also fire, but `request_scores`
                    // coalesces, so this overlap is harmless.
                    self.startup = self.kick_first_score_then_phase();
                } else {
                    self.startup = StartupPhase::Normalizing { expected, applied };
                }
            }
            StartupPhase::Scoring => {
                // Watch the head-breakdown predicate: tick step-2's
                // `poll_async_scores` stamps it; we only flip into the
                // session once it lands. No edit is open during startup, so
                // the breakdown reads the committed head.
                if self.store.current_composition_breakdown().is_some() {
                    self.enter_session_from_startup();
                    self.startup = StartupPhase::Done;
                } else {
                    self.startup = StartupPhase::Scoring;
                }
            }
            StartupPhase::WarmingForLanding {
                expected,
                mut connected,
            } => {
                // Keep draining `poll_warms` so the workers' deferred accepts
                // complete even though the user is already at Landing; do not
                // skip to Done without finishing the connects.
                for (plugin_id, _ok) in self.runner_client.poll_warms() {
                    connected.insert(plugin_id);
                }
                if connected.is_superset(&expected) {
                    self.startup = StartupPhase::Done;
                } else {
                    self.startup = StartupPhase::WarmingForLanding { expected, connected };
                }
            }
        }
    }

    /// Kick the first score and pick the next phase. If the kick put a query
    /// in flight (a scorer exists), wait in `Scoring` for the head breakdown
    /// to stamp. If nothing went in flight (no scorer queued anything), the
    /// breakdown predicate would never flip, so enter the session immediately
    /// rather than hang the loading screen. A scorer that replies with an
    /// empty / degraded report and never stamps still waits in `Scoring` by
    /// design; only the no-scorer case takes the immediate-enter path.
    ///
    /// The immediate-enter branch mirrors the `Scoring` arm's stamp terminal
    /// (`enter_session` + `InSession` + `Done`) so both paths clear the
    /// loading screen identically.
    #[cfg(not(target_arch = "wasm32"))]
    fn kick_first_score_then_phase(&mut self) -> StartupPhase {
        self.request_scores();
        if self.runner_client.has_pending_score_queries() {
            StartupPhase::Scoring
        } else {
            self.enter_session_from_startup();
            StartupPhase::Done
        }
    }

    /// Enter the session from the startup state-machine: flip into the
    /// session and frame the camera on the settled (post-normalize) geometry.
    ///
    /// The camera fit lives here rather than in `enter_session` because that
    /// method is shared with the in-session reload handlers, which fit the
    /// camera themselves; folding the fit in there would double-fit on
    /// reload. By the time any with-structure startup path reaches this seam,
    /// the normalize-adopt has published the final geometry and the tick has
    /// synced it, so `fit_camera_to_focus` frames what is actually displayed.
    /// The Landing (no-structure) path never reaches here.
    #[cfg(not(target_arch = "wasm32"))]
    fn enter_session_from_startup(&mut self) {
        self.enter_session();
        self.set_app_phase(AppPhase::InSession);
        if let Some(engine) = self.engine.as_mut() {
            engine.fit_camera_to_focus();
            // Force a per-residue color re-push now that viso has fully synced
            // the final (post-normalize) geometry. The first score may have
            // pushed before viso created the entity's scene-local state, in
            // which case that push was silently dropped and the backbone would
            // render gray. Disjoint field borrow: `render_projector` and `store`
            // are separate fields from `engine`, mirroring the tick consume seam.
            RenderProjector::reproject_scores(&self.store, engine);
            // Re-push alone only updates the separate residue-color buffer; the
            // cartoon tube's color is baked into the mesh at build time and the
            // startup geometry baked gray (it published before the first score
            // arrived). Force a full-rebuild republish now that the scores are
            // populated so the backbone mesh re-bakes colored.
            self.render_projector.rebake_geometry(&self.store, engine);
        }
    }

    /// Warms complete: parse + publish the bootstrap structure
    /// synchronously, snapshot it, and KICK each warm plugin's `Init`
    /// against it. Returns the next phase (`Initializing` over the kicked
    /// init set). The surrounding tick publishes the freshly parsed assembly
    /// this frame, so no inline `tick(0.0)` is taken here.
    #[cfg(not(target_arch = "wasm32"))]
    fn warms_done_load_and_kick_inits(&mut self, path: &str) -> StartupPhase {
        match crate::puzzle::load_file_as_entities(path) {
            Ok((entities, name)) => {
                for entity in entities {
                    let _ = self.store.load_entity_into_history(entity, &name);
                }
                // Free-form initial load: set the title and ensure the
                // free-form (no-puzzle) session through the create seam. The
                // scientist puzzle panel + title reach the GUI at the
                // InSession gate's full populate.
                self.store.start(name.clone(), None);
                log::info!("Loaded structure: {name}");
            }
            Err(e) => {
                // No session was created; the warmed orchestrator and its
                // workers stay as-is. Skip straight to the InSession flip so
                // the loading screen still clears (degraded, viewer-only).
                log::error!("Failed to load structure '{path}': {e}");
                self.enter_session_from_startup();
                return StartupPhase::Done;
            }
        }

        // Snapshot the just-loaded (pre-normalization) assembly and KICK each
        // warm plugin's `Init` against it. Session-init uses this one
        // snapshot for every plugin, so adopting rosetta's post-Init result
        // later does not change what other plugins init against.
        let initial_assembly = {
            let head_before = self.store.head_assembly();
            match molex::ops::wire::serialize_assembly(&head_before) {
                Ok(b) => b,
                Err(e) => {
                    log::warn!(
                        "[App] failed to serialize initial assembly for plugin \
                         session init: {e:?}; plugins disabled"
                    );
                    // Nothing to init against; jump to the first score so the
                    // load still completes (viewer-only on the plugin side).
                    return self.kick_first_score_then_phase();
                }
            }
        };

        let expected: std::collections::BTreeSet<String> = self
            .runner_client
            .kick_inits(&initial_assembly)
            .into_iter()
            .collect();
        if expected.is_empty() {
            // No plugin sessions to bring up: go straight to the first score.
            return self.kick_first_score_then_phase();
        }
        StartupPhase::Initializing {
            expected,
            adopted: std::collections::BTreeSet::new(),
        }
    }

    /// Inits complete: for each adopted plugin that declares a load-time
    /// normalize op, KICK a whole-structure normalize. Returns the next
    /// phase (`Normalizing` over the kicked normalize set, or `Scoring` with
    /// the first score already kicked when no plugin normalizes).
    ///
    /// Whole-structure normalize: empty selection / no focus / no params
    /// (the bridge ignores selection for normalize). The dispatch's own
    /// `request_id` / scope are discarded; `apply_post_init` re-derives its
    /// target entities and mints its own checkpoint when the reply lands.
    #[cfg(not(target_arch = "wasm32"))]
    fn inits_done_kick_normalizes(
        &mut self,
        adopted: &std::collections::BTreeSet<String>,
    ) -> StartupPhase {
        let mut expected = std::collections::BTreeSet::new();
        for plugin_id in adopted {
            let Some(op_id) = self.runner_client.normalize_op_for(plugin_id) else {
                continue;
            };
            let intent = DispatchIntent {
                selection: std::collections::BTreeMap::new(),
                focused_entity_id: None,
                op_id,
                params: std::collections::HashMap::new(),
            };
            let store = &self.store;
            self.runner_client
                .kick_normalize(intent, plugin_id, |id| store.entity_type(id));
            expected.insert(plugin_id.clone());
        }
        if expected.is_empty() {
            // No plugin declares a normalize op: a pose-less Init already
            // seeded the head (or there is no structural plugin), so go
            // straight to the first score.
            return self.kick_first_score_then_phase();
        }
        StartupPhase::Normalizing {
            expected,
            applied: std::collections::BTreeSet::new(),
        }
    }

    /// Apply a plugin's post-Init normalized assembly (full-atom pose) so
    /// the host's canonical assembly matches the plugin's internal pose
    /// before any user action runs. Every entity the normalized assembly
    /// touches that has a committed lane in the store is normalized inside
    /// a single multi-lane edit, so a multi-chain session no longer drops
    /// every entity past the first.
    #[cfg(not(target_arch = "wasm32"))]
    fn apply_post_init(
        &mut self,
        plugin_id: &str,
        post_init_bytes: &[u8],
        op_id: &str,
        display: &str,
    ) {
        if post_init_bytes.is_empty() {
            log::warn!(
                "[App] {plugin_id} post-Init returned no normalized assembly; \
                 first user action will likely snap because scene.positions \
                 stays at the pre-Init atom count."
            );
            return;
        }
        let normalized = match molex::ops::wire::deserialize_assembly(post_init_bytes) {
            Ok(a) => a,
            Err(e) => {
                log::warn!(
                    "[App] {plugin_id} post-Init assembly decode failed: {e:?}; \
                     skipping normalization apply"
                );
                return;
            }
        };
        // Every entity the normalized assembly names that has a committed
        // lane in the store. A protein has a lane (loaded into history);
        // ambient / zero-residue stubs stay transient and have none, so
        // they're skipped here.
        let target_entities: Vec<EntityId> = normalized
            .entities()
            .iter()
            .map(|e| e.id())
            .filter(|id| self.store.history().lane(*id).is_some())
            .collect();
        if target_entities.is_empty() {
            log::warn!(
                "[App] {plugin_id} post-Init: no store entity matches the \
                 normalized assembly; skipping normalization apply"
            );
            return;
        }
        let kind = CheckpointKind::PluginOp {
            plugin_id: String::from(plugin_id),
            op_id: String::from(op_id),
            display: String::from(display),
        };
        // Host-internal action: no dispatch happened, so draw the edit's
        // request_id straight from the orchestrator (the single id
        // authority).
        let Some(request_id) = self.runner_client.alloc_request_id() else {
            log::warn!(
                "[App] {plugin_id} post-Init: no orchestrator to allocate a \
                 request id; skipping normalization apply"
            );
            return;
        };
        if let Err(e) =
            self.store
                .begin_action(target_entities, kind, String::from(display), request_id)
        {
            log::warn!(
                "[App] {plugin_id} post-Init begin_action failed: {e}; \
                 skipping normalization apply"
            );
            return;
        }
        let applied = self.store.apply_streaming_assembly(&normalized, None, request_id);
        if !applied {
            log::warn!(
                "[App] {plugin_id} post-Init apply_streaming_assembly did not \
                 update any entity; rolling back tentative. This usually means \
                 the {plugin_id}-returned entity ID does not match any store \
                 entity ID."
            );
            let _ = self.store.commit_action(request_id);
            return;
        }
        if let Err(e) = self.store.commit_action(request_id) {
            log::warn!("[App] {plugin_id} post-Init commit_action failed: {e}");
            return;
        }
        log::info!(
            "[App] {plugin_id} post-Init assembly applied ({} bytes)",
            post_init_bytes.len()
        );
        // Republish is stream-driven: the HeadMoved from commit_action
        // rides through the next tick's render projector.
    }

    /// Shut down backends and scene processor.
    pub fn shutdown(&mut self) {
        self.runner_client.shutdown();
        if let Some(engine) = &mut self.engine {
            engine.shutdown();
        }
    }
}

/// Non-blocking startup phase, advanced once per frame by
/// [`App::advance_startup`]. The machine accumulates per-plugin worker
/// completions across frames (the polls are stateless: each returns only
/// what completed THIS frame) against an `expected` set, so the host
/// renders the loading screen while bring-up proceeds.
#[cfg(not(target_arch = "wasm32"))]
pub(in crate::app) enum StartupPhase {
    /// Not started; the host has not called `begin_startup` yet.
    Idle,
    /// Full session bring-up: warming plugins, with the bootstrap structure
    /// path stashed for the parse once every warm has connected.
    Warming {
        expected: std::collections::BTreeSet<String>,
        connected: std::collections::BTreeSet<String>,
        path: String,
    },
    /// Plugin `Init` sessions in flight; accumulating adopted plugin ids.
    Initializing {
        expected: std::collections::BTreeSet<String>,
        adopted: std::collections::BTreeSet<String>,
    },
    /// Per-plugin load-time normalize invokes in flight; accumulating
    /// applied plugin ids.
    Normalizing {
        expected: std::collections::BTreeSet<String>,
        applied: std::collections::BTreeSet<String>,
    },
    /// First score requested; waiting for the head breakdown to stamp.
    Scoring,
    /// No bootstrap structure: Landing is already shown and the warms are
    /// finishing in the background so a later file-load finds connected
    /// workers.
    WarmingForLanding {
        expected: std::collections::BTreeSet<String>,
        connected: std::collections::BTreeSet<String>,
    },
    /// Bring-up complete; the machine is inert.
    Done,
}

/// Locate the runtime plugins directory.
///
/// Resolution order:
///   1. `FOLDIT_PLUGINS_ROOT` environment override (production /
///      bundled deployments point this at the bundle's plugins dir).
///   2. `<exe_dir>/plugins/` if it exists (bundle layout).
///   3. Walk up from `current_exe()` looking for
///      `crates/foldit-runner/plugins/` (dev workflow under cargo).
///
/// Returns `None` if none of these resolve. The caller logs and skips
/// plugin discovery in that case -- the desktop app degrades to viewer-
/// only mode rather than failing the load.
#[cfg(not(target_arch = "wasm32"))]
#[must_use]
pub fn locate_plugins_root() -> Option<std::path::PathBuf> {
    if let Some(env) = std::env::var_os("FOLDIT_PLUGINS_ROOT") {
        let p = std::path::PathBuf::from(env);
        if p.is_dir() {
            return Some(p);
        }
    }
    let exe = std::env::current_exe().ok()?;
    if let Some(dir) = exe.parent() {
        let bundle = dir.join("plugins");
        if bundle.is_dir() {
            return Some(bundle);
        }
    }
    let mut cursor = exe.parent()?.to_path_buf();
    loop {
        let candidate = cursor.join("crates/foldit-runner/plugins");
        if candidate.is_dir() {
            return Some(candidate);
        }
        if !cursor.pop() {
            break;
        }
    }
    None
}
