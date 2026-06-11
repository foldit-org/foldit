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

        // Focus is pure session state; no engine borrow needed.
        if let AppCommand::SetFocus { entity_id } = command {
            let focus = match entity_id {
                None => viso::Focus::All,
                Some(raw) => viso::Focus::Entity(EntityId::from_raw(raw)),
            };
            self.store.set_focus(focus);
            return;
        }

        // Per-entity appearance is authoritative on the session; the render
        // projector pushes it into the engine working copy on the emitted
        // `EntityAppearanceChanged`. No engine borrow needed here, so it is
        // handled before the engine-presence guard like focus.
        if let AppCommand::SetEntityAppearance {
            entity_id,
            field,
            value,
        } = command
        {
            self.store
                .set_entity_appearance_field(EntityId::from_raw(entity_id), &field, &value);
            return;
        }

        // Clearing an entity's whole appearance override is likewise pure
        // session state; handled before the engine-presence guard for the
        // same reason as the per-field merge above.
        if let AppCommand::ClearEntityAppearance { entity_id } = command {
            self.store
                .clear_entity_appearance(EntityId::from_raw(entity_id));
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
                // A manual edit: store the App-owned options, clear the active
                // preset (manually-set options no longer match a named preset),
                // and latch the player-touched flag so future loads keep this
                // choice instead of re-seeding from the puzzle/Default preset.
                // The tick applies the options to the engine (+ raises VIEW) off
                // the `ViewOptionsChanged` we note when something actually
                // changed; an idempotent set is silent.
                match serde_json::from_value::<viso::options::VisoOptions>(options) {
                    Ok(opts) => {
                        let changed = self.view_options != opts || self.active_preset.is_some();
                        self.view_options = opts;
                        self.active_preset = None;
                        self.view_settings_touched = true;
                        if changed {
                            self.store.note_view_options_changed();
                        }
                    }
                    Err(e) => log::error!("Failed to deserialize view options: {e}"),
                }
            }
            AppCommand::LoadViewPreset { name } => {
                // An explicit player preset pick: latch the touched flag (so it
                // persists across later loads) and apply the preset now.
                self.view_settings_touched = true;
                #[cfg(not(target_arch = "wasm32"))]
                self.apply_view_preset_to_session(&name);
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
            AppCommand::History { .. }
            | AppCommand::AdvanceBubble { .. }
            | AppCommand::SetFocus { .. }
            | AppCommand::SetEntityAppearance { .. }
            | AppCommand::ClearEntityAppearance { .. } => {
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
                // Seed the view options from the Default preset on a fresh app
                // (so the panel reflects the true coloring and the whole-blob
                // option emit is faithful); once the player has touched any view
                // setting, skip the seed and keep their persisted choice. Either
                // way, push the persisted-or-seeded options to the freshly-reset
                // engine before the publish below. The funnel does the eager set
                // + note itself when it seeds; the touched branch does it from
                // the App-owned options.
                #[cfg(not(target_arch = "wasm32"))]
                if self.view_settings_touched {
                    self.reapply_view_options_to_engine();
                } else {
                    self.apply_view_preset_to_session("Default");
                }

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
        // viso's own per-entity score and appearance maps have an id-reuse
        // hole: replace_assembly now preserves both across a swap (so a
        // settling preview doesn't flash the survivors gray and user-authored
        // per-entity appearance persists), reconciling membership by id. A
        // puzzle reload restarts the entity allocator, so the new puzzle's ids
        // collide with the outgoing ones and would inherit their colors and
        // overrides; clear both viso maps explicitly here.
        if let Some(engine) = self.engine.as_mut() {
            engine.clear_scores();
            engine.clear_all_appearance();
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

                // A puzzle may pin its own view preset; otherwise fall back to
                // the Default preset so the view panel reflects the true state.
                // Seed only on a fresh app; once the player has touched any view
                // setting, skip the seed and keep their persisted choice. Either
                // way the freshly-reset engine is given the persisted-or-seeded
                // options. The tick(0.0) below drains the noted
                // `ViewOptionsChanged` and re-applies them to the engine.
                #[cfg(not(target_arch = "wasm32"))]
                if self.view_settings_touched {
                    self.reapply_view_options_to_engine();
                } else {
                    self.apply_view_preset_to_session(
                        puzzle_data.view_preset.as_deref().unwrap_or("Default"),
                    );
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

    /// Load a named view preset's options off disk and install them as the
    /// App-owned active options + active preset. App owns the view state (so
    /// it persists across a topology swap); the seed paths route through here
    /// so the view panel reflects the true coloring rather than bare defaults
    /// while the engine renders the boot preset, and the panel's whole-blob
    /// option emit does not clobber the engine's coloring. When an engine is
    /// attached it is synced immediately; the tick also re-applies the options
    /// off the noted `ViewOptionsChanged`. A missing presets dir or an
    /// unreadable preset file is logged and left as a no-op (the App keeps its
    /// current options).
    #[cfg(not(target_arch = "wasm32"))]
    fn apply_view_preset_to_session(&mut self, name: &str) {
        let Some(dir) = self.host.view_presets_dir() else {
            return;
        };
        let path = dir.join(format!("{name}.toml"));
        let opts = match viso::options::VisoOptions::load(&path) {
            Ok(opts) => opts,
            Err(e) => {
                log::error!("Failed to load view preset '{name}': {e}");
                return;
            }
        };
        self.view_options = opts.clone();
        self.active_preset = Some(name.to_owned());
        // Eager engine sync so the engine has the options before this load's
        // `tick(0.0)`; the noted `ViewOptionsChanged` re-applies them through
        // the tick seam too (idempotent).
        if let Some(engine) = self.engine.as_mut() {
            engine.set_options(opts);
        }
        self.store.note_view_options_changed();
    }

    /// Push the persisted App-owned view options to the (freshly-reset) engine
    /// and note the change so the tick re-applies them. Used by the load paths
    /// when the player has already touched a view setting, so the preset seed
    /// is skipped but the engine still receives the persisted options on every
    /// load. The eager set mirrors the funnel's own engine sync.
    #[cfg(not(target_arch = "wasm32"))]
    fn reapply_view_options_to_engine(&mut self) {
        let opts = self.view_options.clone();
        if let Some(engine) = self.engine.as_mut() {
            engine.set_options(opts);
        }
        self.store.note_view_options_changed();
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
                // Seed the view options from the Default preset on a fresh app
                // so the panel reflects the true coloring on first paint; once
                // the player has touched any view setting, skip the seed and
                // keep their persisted choice. Either way the freshly-reset
                // engine is given the persisted-or-seeded options. The eager set
                // + note here is drained by the surrounding tick (advance_startup
                // runs before this tick's `SessionUpdate` drain + render seam).
                if self.view_settings_touched {
                    self.reapply_view_options_to_engine();
                } else {
                    self.apply_view_preset_to_session("Default");
                }
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

#[cfg(test)]
#[cfg(not(target_arch = "wasm32"))]
mod preset_tests {
    use crate::HostResources;
    use crate::app::App;
    use std::io;
    use std::path::{Path, PathBuf};
    use viso::options::ColorScheme;

    /// Host stub whose `view_presets_dir` points at the repository's shipped
    /// `assets/view_presets`, so the helper reads the real Default preset.
    struct PresetHost {
        presets_dir: PathBuf,
    }

    impl HostResources for PresetHost {
        fn read_file(&self, _path: &str) -> io::Result<Vec<u8>> {
            Err(io::Error::new(io::ErrorKind::NotFound, "test stub"))
        }
        fn view_presets_dir(&self) -> Option<&Path> {
            Some(&self.presets_dir)
        }
        fn initial_structure_path(&self) -> Option<String> {
            None
        }
    }

    /// A non-default `VisoOptions`, distinguishable from the default by a
    /// single toggle. Used to exercise the change-guard on the App writers.
    fn mk_non_default_options() -> viso::options::VisoOptions {
        let mut opts = viso::options::VisoOptions::default();
        opts.debug.show_normals = true;
        opts
    }

    /// After applying the Default preset through the funnel, the App-owned
    /// view options carry the preset's coloring (Score, not the bare-default
    /// Entity) and record it as the active preset. The App is what the view
    /// panel binds, so the menu shows the true state rather than a stale
    /// default. The engine-attached re-sync and the full GPU load path are
    /// exercised by the parent's runtime confirmation.
    #[test]
    fn default_preset_seeds_view_options() {
        let presets_dir =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/view_presets");
        let mut app = App::new(Box::new(PresetHost { presets_dir }));

        // A fresh app is at the bare default (Entity coloring, no active
        // preset, untouched) - the state the bug left the menu in while the
        // engine rendered the boot preset.
        assert_eq!(
            app.view_options().display.backbone_color_scheme(),
            ColorScheme::Entity,
        );
        assert!(app.active_preset().is_none());
        assert!(!app.view_settings_touched);

        app.apply_view_preset_to_session("Default");

        assert_eq!(
            app.view_options().display.backbone_color_scheme(),
            ColorScheme::Score,
            "Default preset colors by Score, not bare-default Entity",
        );
        assert_eq!(app.active_preset(), Some("Default"));
    }

    /// The funnel records the preset name AND notes a single
    /// `ViewOptionsChanged` on the `SessionUpdate` stream so the tick applies
    /// the options to the engine and refreshes the VIEW panel. Coverage that
    /// moved off the deleted `Session::apply_preset`.
    #[test]
    fn funnel_records_preset_and_notes_one_change() {
        let presets_dir =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/view_presets");
        let mut app = App::new(Box::new(PresetHost { presets_dir }));
        let _ = app.store.take_updates();

        app.apply_view_preset_to_session("Default");

        assert_eq!(app.active_preset(), Some("Default"));
        assert!(
            matches!(
                app.store.take_updates().as_slice(),
                [crate::session::SessionUpdate::ViewOptionsChanged]
            ),
            "the funnel notes exactly one ViewOptionsChanged",
        );
    }

    /// The view options + active preset live on `App` and survive
    /// `Session::reset` (a topology swap). This is the inverted reset
    /// semantics: a player's display choices carry from one structure to the
    /// next instead of zeroing per session. Coverage that moved off the
    /// deleted `Session::reset` view-options block.
    #[test]
    fn view_options_persist_across_session_reset() {
        let presets_dir =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/view_presets");
        let mut app = App::new(Box::new(PresetHost { presets_dir }));

        app.view_options = mk_non_default_options();
        app.active_preset = Some("warm".to_owned());

        app.store.reset();

        assert_eq!(
            app.view_options(),
            &mk_non_default_options(),
            "App view options survive a topology swap",
        );
        assert_eq!(
            app.active_preset(),
            Some("warm"),
            "App active preset survives a topology swap",
        );
    }
}
