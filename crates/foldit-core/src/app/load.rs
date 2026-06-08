use foldit_gui::LoadingState;
use molex::entity::molecule::id::EntityId;
use viso::VisoEngine;

use crate::app::App;
use crate::session::Puzzle;
#[cfg(not(target_arch = "wasm32"))]
use foldit_gui::DirtyFlags;
#[cfg(not(target_arch = "wasm32"))]
use crate::history::CheckpointKind;
#[cfg(not(target_arch = "wasm32"))]
use crate::runner_client::{DispatchIntent, OpOutcome};

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
        self.set_loading_state(LoadingState::LoadingSession);
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
        self.set_loading_state(LoadingState::LoadingSession);
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
    // ── Complex lifecycle (engine attach + initial load) ──

    /// Attach a host-built `VisoEngine` to this App. Hosts are
    /// responsible for constructing the wgpu `RenderContext` against
    /// their own surface (winit window on desktop, `<canvas>` on web)
    /// and applying any preset / render-scale tweaks they want before
    /// handing it over.
    pub fn attach_engine(&mut self, engine: VisoEngine) {
        self.engine = Some(engine);
    }

    /// Load the initial structure, register entities, and create the
    /// initial Rosetta session. Runs AFTER the webview's loading screen
    /// is visible so the user has feedback during the (potentially
    /// slow) load. Requires `create_render_context` to have run first.
    ///
    /// Bootstrap path comes from the host (`HostResources::initial_structure_path`);
    /// `None` is a no-op (e.g. the web shell loads structures via a
    /// separate flow rather than a startup path).
    pub fn load_initial_structure(&mut self) {
        if self.engine.is_none() {
            log::error!("load_initial_structure called before create_render_context");
            return;
        }

        let Some(path) = self.host.initial_structure_path() else {
            return;
        };

        self.set_loading_state(LoadingState::LoadingSession);

        // Parse entities from file
        match crate::puzzle::load_file_as_entities(&path) {
            Ok((entities, name)) => {
                for entity in entities {
                    let _ = self.store.load_entity_into_history(entity, &name);
                }
                // Free-form initial load: set the title and ensure the
                // free-form (no-puzzle) session through the create seam. The
                // scientist puzzle panel + title reach the GUI at the
                // InSession gate's `set_puzzle_scientist` push.
                self.store.start(name.clone(), None);

                // Publish + fit. tick(0.0) drains the `SessionUpdate` stream, hands the
                // assembly to the render projector, and runs
                // engine.update(0.0) so the pending Assembly is drained
                // before fit_camera reads bounding-radius.
                self.tick(0.0);
                if let Some(engine) = self.engine.as_mut() {
                    engine.fit_camera_to_focus();
                }

                log::info!("Loaded structure: {name}");

                // Plugins were discovered + warmed at startup
                // (`warm_plugins`), which installed the orchestrator the
                // plugin driver owns. This file-load step only creates the
                // sessions against the just-loaded structure.
                self.bootstrap_plugins();

                // Republish: bootstrap may have committed rosetta's
                // post-Init normalized assembly (full-atom pose) into
                // the store. The HeadMoved emitted by commit_action
                // rides the `SessionUpdate` stream; this tick(0.0) flushes it
                // so the normalized geometry is published before the
                // synchronous first score is stamped below.
                self.tick(0.0);
            }
            Err(e) => {
                log::error!("Failed to load structure '{path}': {e}");
                // No session was created (parse failed before
                // `bootstrap_plugins`), so the startup-warmed orchestrator
                // and its workers stay as-is; nothing to reset here.
            }
        }

        // Stamp the first per-residue score synchronously against the
        // post-bootstrap (normalized) head, then drain its `ScoresChanged`
        // through the render projector with a tick(0.0) so the backbone is
        // already colored on the first rendered frame (no gray flash).
        // `score_head_now` no-ops when the load failed (no session) or there
        // is no scoring plugin.
        self.score_head_now();
        self.tick(0.0);

        // Loading is done: flip into InSession now. The now-populated state
        // reaches the GUI via the one-shot full populate `enter_session`
        // raises (VIEW for the engine options, ACTIONS for the catalog, SCORE
        // for the now-stamped gauge, SCENE for the entity list). The loading
        // screen clears via the `InSession` flip itself, not a dirty flag.
        self.enter_session();
    }

    /// App-lifecycle warm-up (startup, before any structure loads):
    /// install a fresh orchestrator, load the default score-term weights,
    /// discover plugins under the runtime plugin root, and WARM each one
    /// (spawn its worker → backend / database / scoring load, NO session,
    /// NO pose). The session is created later, at file-load, by
    /// `bootstrap_plugins`.
    ///
    /// Errors are logged and dropped: a missing plugin dir / dylib should
    /// degrade the app to viewer-only, not crash startup.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn warm_plugins(&mut self) {
        self.set_loading_state(LoadingState::Initializing);

        // A fresh orchestrator restarts request ids at 1, so drop any
        // stale composition targets before a new edit can reuse an old id.
        self.runner_client.init_orchestrator();
        self.score_targets.clear();

        // Load the default score-term weights once, before the first score.
        // `reset` leaves `term_weights` untouched, so a single load here
        // carries across reloads; the empty-check makes a re-warm a no-op.
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

        let Some(plugins_root) = locate_plugins_root() else {
            log::warn!(
                "[App] no plugins root found (set FOLDIT_PLUGINS_ROOT or run \
                 from a workspace checkout); plugins disabled"
            );
            return;
        };
        log::info!("[App] discovering + warming plugins under {}", plugins_root.display());
        self.runner_client.warm_runner(&plugins_root);
    }

    /// Create plugin sessions for the just-loaded structure (file-load):
    /// snapshot the loaded assembly and run each warm plugin's `Init`
    /// against it, then apply any per-plugin post-Init result. The
    /// plugins were already discovered + warmed at startup by
    /// [`Self::warm_plugins`]; this is the session step that builds the
    /// structure inside each plugin.
    ///
    /// If Rosetta's Init returns a non-empty normalized assembly (full-atom
    /// pose with hydrogens / terminal O / etc. added), it is committed as
    /// a follow-up `PluginOp` checkpoint and republished so that
    /// `scene.positions` is seeded at the normalized atom count before any
    /// user action runs. Without this, the first user op would cross an
    /// atom-set boundary mid-action and snap.
    ///
    /// Errors are logged and dropped: a missing plugin dir / dylib should
    /// degrade the app to viewer-only, not crash the load. Runs while the
    /// `LoadingSession` phase set by the caller is in effect; it does not
    /// re-enter `Initializing`.
    #[cfg(not(target_arch = "wasm32"))]
    fn bootstrap_plugins(&mut self) {
        // Snapshot the loaded assembly under an immutable store borrow so
        // the plugin driver can hand it to `init_plugin_session` for each
        // plugin. Session-init uses this one pre-normalization snapshot for
        // every plugin, so applying rosetta's post-Init result afterward
        // (below) does not change what later plugins init against.
        let initial_assembly = {
            let head_before = self.store.head_assembly();
            match molex::ops::wire::serialize_assembly(&head_before) {
                Ok(b) => b,
                Err(e) => {
                    log::warn!(
                        "[App] failed to serialize initial assembly for plugin \
                         session init: {e:?}; plugins disabled"
                    );
                    return;
                }
            }
        };

        let registered = self
            .runner_client
            .init_runner_sessions(&initial_assembly);

        // Apply each registered plugin's Init reply into the store. A
        // plugin whose Init returns a non-empty assembly is adopted here
        // (its own op-id stamped on the checkpoint); a pose-less Init
        // returns empty bytes, which the empty-bytes guard inside
        // `apply_post_init` no-ops, as do non-structural plugins. The loop
        // stays generic so additional adopting plugins drop in without
        // host-side wiring changes.
        for (plugin_id, post_init_bytes) in &registered {
            self.apply_post_init(plugin_id, post_init_bytes, "_init_normalize", "Init");
        }

        // After Init, give each registered plugin that declares a load-time
        // normalize op a chance to build and adopt its canonicalized
        // structure. A pose-less Init returns an empty assembly (handled
        // above as a no-op), so this dispatch is what actually seeds
        // `scene.positions` at the normalized atom count before any user
        // action runs. Whole-structure normalize: empty selection / no
        // focus / no params (the bridge ignores selection for normalize).
        // The dispatch's own request_id / scope are discarded -
        // `apply_post_init` re-derives its target entities and mints its own
        // checkpoint. The backend lock the dispatch takes is benign at
        // bootstrap (rosetta is the only structural plugin then).
        for (plugin_id, _) in &registered {
            let Some(op_id) = self.runner_client.normalize_op_for(plugin_id) else {
                continue;
            };
            let intent = DispatchIntent {
                selection: std::collections::BTreeMap::new(),
                focused_entity_id: None,
                op_id: op_id.clone(),
                params: std::collections::HashMap::new(),
            };
            let store = &self.store;
            let outcome = self
                .runner_client
                .dispatch_op(intent, plugin_id.clone(), |id| store.entity_type(id));
            match outcome {
                Ok(OpOutcome::Invoke { bytes, .. }) => {
                    self.apply_post_init(plugin_id, &bytes, &op_id, "Init");
                }
                Ok(OpOutcome::Stream { .. }) => {
                    log::warn!(
                        "[App] {plugin_id} normalize op {op_id:?} dispatched as a \
                         stream; expected a synchronous invoke. Skipping adoption."
                    );
                }
                Err(e) => {
                    log::warn!(
                        "[App] {plugin_id} normalize op {op_id:?} dispatch failed: \
                         {e:?}; skipping normalization apply"
                    );
                }
            }
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
