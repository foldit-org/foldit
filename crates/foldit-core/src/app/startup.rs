//! Non-blocking async-startup state machine for [`App`].
//!
//! These methods arm and advance the per-frame bring-up sequence (plugin
//! warm, `Init`, first score) that runs while the host keeps rendering the
//! loading screen.

use viso::VisoEngine;

use foldit_gui::AppPhase;

use super::plugins::locate_plugins_root;
use super::App;
use crate::render_projector::RenderSources;
use crate::session::{HeadMoveCause, SessionUpdate, SessionUpdateConsumer};

/// The async-startup state App owns as one field: the per-frame phase
/// machine plus the two terminal carryovers (camera intent + puzzle SS
/// override) that `enter_session_from_startup` consumes once the geometry
/// settles.
#[cfg(not(target_arch = "wasm32"))]
pub(in crate::app) struct BringupState {
    pub(in crate::app) phase: StartupPhase,
    pub(in crate::app) camera: StartupCamera,
    pub(in crate::app) ss_override: Option<(u32, Vec<molex::SSType>)>,
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
    /// Full session bring-up: warming plugins
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

/// How the startup-machine terminal ([`App::enter_session_from_startup`])
/// frames the camera once the geometry has settled. The launch and free-form
/// structure paths fit on the focused geometry; a puzzle load stashes its
/// saved pose so the terminal honors it instead of fitting. Consumed (reset
/// to [`StartupCamera::Fit`]) when the terminal runs.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Default)]
pub(in crate::app) enum StartupCamera {
    /// Frame on the focused geometry (launch + free-form structure load).
    #[default]
    Fit,
    /// Apply a puzzle's saved eye/up anchored on the settled centroid;
    /// falls back to a focus fit when no centroid is available.
    PuzzlePose { eye: glam::Vec3, up: glam::Vec3 },
}

impl App {
    /// Attach a host-built `VisoEngine` to this App. Hosts are
    /// responsible for constructing the wgpu `RenderContext` against
    /// their own surface (winit window on desktop, `<canvas>` on web)
    /// and applying any preset / render-scale tweaks they want before
    /// handing it over.
    pub fn attach_engine(&mut self, engine: VisoEngine) {
        self.harness.attach(engine);
    }

    /// Desktop GPU bring-up: create ONE wgpu device against `target`, build
    /// and attach the renderer on it, and cache it for crystallographic GPU
    /// compute so both consumers share a single device.
    ///
    /// The device is created with `required_limits: adapter.limits()` (the
    /// adapter maximum) rather than the wgpu defaults: cubecl reads the handed
    /// device's limits back to size its storage-buffer bindings, and the
    /// default limit (8 storage buffers) is too small for its compute kernels.
    /// The adapter maximum is a guaranteed superset.
    ///
    /// On device-creation failure this falls back to viso self-creating its
    /// own device (renderer only) and leaves the shared device unset, so
    /// density then computes on the CPU. Errors are logged here; the desktop
    /// caller fires-and-forgets.
    #[cfg(not(target_arch = "wasm32"))]
    #[allow(
        clippy::future_not_send,
        reason = "block_on'd on the desktop main thread; the window-handle surface \
                  target and the App's dyn HostResources are inherently not Send"
    )]
    pub async fn init_desktop_gpu(
        &mut self,
        target: impl Into<viso::wgpu::SurfaceTarget<'static>>,
        size: (u32, u32),
        render_scale: u32,
    ) {
        use viso::wgpu;

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            flags: wgpu::InstanceFlags::default().with_env(),
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });

        // Build the shared handles without touching `target`; a failure here
        // leaves `target` free for the self-create fallback below.
        let handles = match instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: None,
                ..Default::default()
            })
            .await
        {
            Ok(adapter) => match adapter
                .request_device(&wgpu::DeviceDescriptor {
                    label: Some("Shared Device"),
                    required_features: wgpu::Features::empty(),
                    required_limits: adapter.limits(),
                    ..Default::default()
                })
                .await
            {
                Ok((device, queue)) => Some((adapter, device, queue)),
                Err(e) => {
                    log::error!("[App] shared device request failed: {e:?}; falling back to renderer-only self-create");
                    None
                }
            },
            Err(e) => {
                log::error!("[App] shared adapter request failed: {e:?}; falling back to renderer-only self-create");
                None
            }
        };

        let Some((adapter, device, queue)) = handles else {
            // Renderer-only fallback: viso creates its own device; the shared
            // device stays unset so density stays on the CPU path.
            match viso::RenderContext::new(target, size).await {
                Ok(ctx) => {
                    let mut engine =
                        match viso::VisoEngine::new(ctx, viso::options::VisoOptions::default()) {
                            Ok(e) => e,
                            Err(e) => {
                                log::error!("[App] engine init failed (fallback): {e:?}");
                                return;
                            }
                        };
                    engine.set_render_scale(render_scale);
                    self.attach_engine(engine);
                }
                Err(e) => log::error!("[App] fallback render context init failed: {e:?}"),
            }
            return;
        };

        // Shared-device path: build the renderer on this device, then hand the
        // same handles to cubecl. `device`/`queue` are ref-counted; cloning
        // gives the renderer its own copy while `WgpuSetup` takes the originals.
        let ctx = match viso::RenderContext::new_with_device(
            &instance,
            &adapter,
            device.clone(),
            queue.clone(),
            target,
            size,
        )
        .await
        {
            Ok(ctx) => ctx,
            Err(e) => {
                log::error!("[App] shared-device render context init failed: {e:?}");
                return;
            }
        };
        let mut engine = match viso::VisoEngine::new(ctx, viso::options::VisoOptions::default()) {
            Ok(e) => e,
            Err(e) => {
                log::error!("[App] engine init failed (shared device): {e:?}");
                return;
            }
        };
        engine.set_render_scale(render_scale);
        self.attach_engine(engine);

        let backend = adapter.get_info().backend;
        let setup = molex::xtal::WgpuSetup {
            instance,
            adapter,
            device,
            queue,
            backend,
        };
        self.shared_device = Some(molex::xtal::shared_gpu_device(setup));
        log::info!("[App] shared wgpu device created; molex xtal compute will run on GPU");
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
    /// Never blocks: the worker round-trips (warm connect, `Init`, first
    /// score) are all kicked here / by `advance_startup` and polled on later
    /// frames, so the host renders the loading screen throughout.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn begin_startup(&mut self) {
        if self.harness.engine.is_none() {
            log::error!("begin_startup called before create_render_context");
            return;
        }

        // A fresh orchestrator restarts request ids at 1, so drop any
        // stale composition targets before a new edit can reuse an old id.
        self.runner_client.init_orchestrator();
        self.scores.clear_targets();

        // Load the default score-term weights once, before the first score.
        // `reset` leaves `term_weights` untouched, so a single load here
        // carries across reloads; the empty-check makes a re-arm a no-op.
        // On failure, log and proceed degraded (every weight then resolves
        // to 0.0, so scores read 0 until a valid map lands -- the app stays
        // up rather than crashing on a missing asset).
        if self.scores.term_weights().is_empty() {
            match crate::scores::load_default_term_weights() {
                Ok(weights) => {
                    log::info!("[App] loaded {} default score-term weights", weights.len());
                    self.scores.set_term_weights(weights);
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
            log::info!(
                "[App] discovering + warming plugins under {}",
                root.display()
            );
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
                self.bringup.phase = StartupPhase::WarmingForLanding {
                    expected,
                    connected: std::collections::BTreeSet::new(),
                };
            }
            Some(path) => {
                self.set_app_phase(AppPhase::LoadingSession);
                self.bringup.phase = StartupPhase::Warming {
                    expected,
                    connected: std::collections::BTreeSet::new(),
                    path,
                };
            }
        }
    }

    /// True once the startup state-machine has reached its terminal state.
    /// The machine drives the first score itself (the post-Init kick); the
    /// tick's at-rest auto-rescore must hold off until then so it does not
    /// fire a `score` query before the head breakdown has stamped.
    #[cfg(not(target_arch = "wasm32"))]
    pub(in crate::app) const fn startup_settled(&self) -> bool {
        matches!(self.bringup.phase, StartupPhase::Done)
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(in crate::app) fn advance_startup(&mut self) {
        match std::mem::replace(&mut self.bringup.phase, StartupPhase::Idle) {
            StartupPhase::Idle => {}
            StartupPhase::Done => self.bringup.phase = StartupPhase::Done,
            StartupPhase::Warming {
                expected,
                mut connected,
                path,
            } => {
                for (plugin_id, _ok) in self.runner_client.poll_warms() {
                    connected.insert(plugin_id);
                }
                if connected.is_superset(&expected) {
                    self.bringup.phase = self.warms_done_load_and_kick_inits(&path);
                } else {
                    self.bringup.phase = StartupPhase::Warming {
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
                    // Every Init has replied: the bridge has built its pose
                    // and decoded the puzzle config (weight patch + filters)
                    // carried on the Init payload, so the first score can
                    // query it. Tick's at-rest gate may also fire, but
                    // `request_scores` coalesces, so this overlap is harmless.
                    self.bringup.phase = self.kick_first_score_then_phase();
                } else {
                    self.bringup.phase = StartupPhase::Initializing { expected, adopted };
                }
            }
            StartupPhase::Scoring => {
                // Flip into the session once the first score's breakdown
                // stamps (the tick's score poll stamps it; no edit is open
                // during startup, so it reads the committed head). If
                // every kicked query has instead returned without stamping,
                // enter the session unscored rather than wait on a breakdown
                // that will never come: a polymer-less (e.g. ligand-only) load
                // gives the scorer nothing to score, so its report is
                // content-empty and dropped, and the query's pending slot
                // clears on that empty reply. The at-rest scorer re-fires once
                // a later edit introduces scorable geometry.
                if self.store.current_composition_breakdown().is_some()
                    || !self.runner_client.has_pending_score_queries()
                {
                    self.enter_session_from_startup();
                    self.bringup.phase = StartupPhase::Done;
                } else {
                    self.bringup.phase = StartupPhase::Scoring;
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
                    self.bringup.phase = StartupPhase::Done;
                } else {
                    self.bringup.phase = StartupPhase::WarmingForLanding {
                        expected,
                        connected,
                    };
                }
            }
        }
    }

    /// Kick the first score and pick the next phase. If the kick put a query
    /// in flight (a scorer exists), wait in `Scoring` for the head breakdown
    /// to stamp. If nothing went in flight (no scorer queued anything), the
    /// breakdown predicate would never flip, so enter the session immediately
    /// rather than hang the loading screen. A scorer that replies with an
    /// empty / degraded report and never stamps does not hang either: the
    /// `Scoring` arm falls through to the session unscored once that query's
    /// pending slot clears, so a polymer-less load with nothing to score still
    /// reaches `Done`.
    ///
    /// The immediate-enter branch mirrors the `Scoring` arm's stamp terminal
    /// (`enter_session` + `InSession` + `Done`) so both paths clear the
    /// loading screen identically.
    #[cfg(not(target_arch = "wasm32"))]
    fn kick_first_score_then_phase(&mut self) -> StartupPhase {
        // Every plugin session is up; ask each whether its model weights are
        // present so the tick's drain can swap a not-ready plugin's buttons
        // for a download button. Fired once here; replies land per-tick.
        self.runner_client.request_weights_status();
        self.runner_client.request_scores();
        if self.runner_client.has_pending_score_queries() {
            StartupPhase::Scoring
        } else {
            self.enter_session_from_startup();
            StartupPhase::Done
        }
    }

    /// Enter the session from the startup state-machine: flip into the
    /// session and frame the camera on the settled geometry.
    ///
    /// The camera fit lives here rather than in `enter_session` because that
    /// method is shared with the in-session reload handlers, which fit the
    /// camera themselves; folding the fit in there would double-fit on
    /// reload. By the time any with-structure startup path reaches this seam,
    /// the molex-canonical head has been published and the tick has synced
    /// it, so `fit_camera_to_focus` frames what is actually displayed. The
    /// Landing (no-structure) path never reaches here.
    #[cfg(not(target_arch = "wasm32"))]
    fn enter_session_from_startup(&mut self) {
        self.enter_session();
        self.set_app_phase(AppPhase::InSession);
        // Consume the stashed camera intent + puzzle SS before the engine
        // borrow (these are separate `App` fields from `engine`).
        let camera = std::mem::take(&mut self.bringup.camera);
        let ss_override = self.bringup.ss_override.take();
        // Fire every viz channel once for the freshly-settled session.
        if self.harness.engine.is_some() {
            let opts = self.view_options();
            self.viz.replay(
                &mut self.runner_client,
                &self.store,
                &mut self.scores,
                &opts,
            );
        }
        if let Some(engine) = self.harness.engine.as_mut() {
            // Camera: a puzzle load supplies its saved eye/up (anchored on the
            // settled centroid); every other path frames on the focused
            // geometry.
            match camera {
                StartupCamera::Fit => engine.fit_camera_to_focus(),
                StartupCamera::PuzzlePose { eye, up } => {
                    engine.snap_camera_to_focus();
                    if let Some(centroid) = engine.focus_centroid() {
                        engine.set_camera_pose(centroid, eye, up);
                    } else {
                        engine.fit_camera_to_focus();
                    }
                }
            }
            // Startup replay: scores, then a head-move rebuild whose same-batch
            // score re-push restores the colors the head-move clear drops.
            self.projectors.render.clear_last_published_ids();
            self.projectors.render.consume(
                &[SessionUpdate::ScoresChanged],
                RenderSources {
                    session: &mut self.store,
                    reapply_options: None,
                    scores: &self.scores,
                    held_connections: self.viz.held_connections(),
                },
                engine,
            );
            self.projectors.render.consume(
                &[
                    SessionUpdate::HeadMoved {
                        cause: HeadMoveCause::Navigate,
                    },
                    SessionUpdate::ScoresChanged,
                ],
                RenderSources {
                    session: &mut self.store,
                    reapply_options: None,
                    scores: &self.scores,
                    held_connections: self.viz.held_connections(),
                },
                engine,
            );
            // Apply any puzzle-pinned SS override AFTER the rebake: it calls
            // `replace_assembly`, which rebuilds the cartoon from the
            // assembly's own (loop) SS, so an override set earlier would be
            // clobbered. Setting it last invalidates RE_MESH, so the next
            // engine.update remeshes the ribbon with the override shape.
            if let Some((entity_raw, ss)) = ss_override {
                engine.set_ss_override(entity_raw, ss);
            }
        }
        // The design-gating overlay is static per puzzle (the mask is set at
        // load), so a single load-time push suffices: viso re-derives the GPU
        // bitset from the per-entity set on every mesh rebuild, keeping the
        // overlay pinned across geometry changes without a per-tick re-push.
        if let Some(engine) = self.harness.engine.as_mut() {
            crate::viz::refresh_design_gating(&self.store, engine);
        }
    }

    /// Warms complete: parse + publish the bootstrap structure
    /// synchronously, snapshot it, and KICK each warm plugin's `Init`
    /// against it. Returns the next phase (`Initializing` over the kicked
    /// init set). The surrounding tick publishes the freshly parsed assembly
    /// this frame, so no inline `tick(0.0)` is taken here.
    #[cfg(not(target_arch = "wasm32"))]
    fn warms_done_load_and_kick_inits(&mut self, path: &str) -> StartupPhase {
        match crate::structure_io::load_file_as_entities(path) {
            Ok((entities, name)) => {
                // `--with-density`: fetch structure factors and compute an
                // experimental-weighted map before `entities` is moved into
                // history; warn-and-continue on any failure so the load never
                // hard-fails on a missing sf.cif or unsupported space group.
                if self.host.with_density() {
                    self.load_with_density(&entities, &name);
                }
                let _ = self.store.seed_history_with_entities(
                    entities,
                    std::path::PathBuf::new(),
                    &name,
                );
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
                if self.gui.view_touched() {
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

        // Snapshot the just-loaded assembly and KICK each warm plugin's
        // `Init` against it. Every plugin inits against this one molex-canonical
        // snapshot.
        self.arm_plugin_bringup()
    }

    /// Set [`Self::bringup`] to drive plugin bring-up for an in-session load
    /// (file / puzzle). When `loaded`, arms the same `Init` -> score ->
    /// `InSession` sequence the launch path runs (the plugins are
    /// already warm, so it enters at `Initializing`); when the load failed,
    /// flips straight into the session degraded so the loading screen still
    /// clears. The reload path's own reset already dropped the prior plugin
    /// sessions, so the kicked `Init`s re-bind every plugin to this structure.
    #[cfg(not(target_arch = "wasm32"))]
    pub(in crate::app) fn arm_session_bringup(&mut self, loaded: bool) {
        self.bringup.phase = if loaded {
            self.arm_plugin_bringup()
        } else {
            // Nothing loaded; clear the loading screen degraded (viewer-only).
            self.enter_session();
            StartupPhase::Idle
        };
    }

    /// Serialize the current head assembly, KICK each warm plugin's `Init`
    /// against it, and return the `Initializing` phase to install. Shared by
    /// the launch path (after the bootstrap parse) and the in-session reload
    /// paths (after reset + ingest); the plugins are already warm in both. On
    /// a serialize failure, or when no plugin session is brought up, kicks the
    /// first score and returns its phase so the load still completes
    /// (viewer-only on the plugin side).
    #[cfg(not(target_arch = "wasm32"))]
    fn arm_plugin_bringup(&mut self) -> StartupPhase {
        let initial_assembly = {
            let head_before = self.store.head_assembly();
            match head_before.to_bytes() {
                Ok(b) => b,
                Err(e) => {
                    log::warn!(
                        "[App] failed to serialize initial assembly for plugin \
                         session init: {e:?}; plugins disabled"
                    );
                    return self.kick_first_score_then_phase();
                }
            }
        };

        // Source the puzzle-specific session payload (ligand asset bytes +
        // catalytic constraints + electron-density map) from the loaded
        // puzzle. A free-form structure load has no puzzle, so these default
        // empty / `None`. Cloned out of the puzzle to release the
        // `self.store` borrow before the `&mut self.runner_client` kick below.
        let (ligands, constraints, mut density) = self.store.puzzle().map_or_else(
            || (Vec::new(), Vec::new(), None),
            |p| (p.ligands.clone(), p.constraints.clone(), p.density.clone()),
        );
        // Fall back to the free-form session density (a `--with-density` load)
        // when the puzzle path supplied none.
        if density.is_none() {
            density = self.store.session_density().cloned();
        }

        // Flatten the loaded puzzle's scorefunction weight patch and its
        // rosetta-targeted objective filters into the generic config-param
        // channel carried on the Init payload. The bridge stashes these on
        // the session at Init and applies them at every scorefunction build,
        // so weight-zero terms (e.g. `envsmooth`) ship and are optimized
        // against, and each forwarded filter emits its per-filter bonus on
        // the score report. Empty for a free-form load (no puzzle).
        let config_params = self.build_init_config_params();

        let expected: std::collections::BTreeSet<String> = self
            .runner_client
            .kick_inits(
                &initial_assembly,
                &ligands,
                &constraints,
                density.as_ref(),
                &config_params,
            )
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
}
