//! Non-blocking async-startup state machine for [`App`].
//!
//! These methods arm and advance the per-frame bring-up sequence (plugin
//! warm, `Init`, normalize, first score) that runs while the host keeps
//! rendering the loading screen; the phase enum itself lives in `super::load`.

use viso::VisoEngine;

use foldit_gui::AppPhase;
use molex::entity::molecule::id::EntityId;

use super::App;
use super::load::{StartupCamera, StartupPhase, locate_plugins_root};
use crate::history::CheckpointKind;
use crate::render_projector::RenderProjector;
use crate::runner_client::DispatchIntent;

impl App {
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
        // Consume the stashed camera intent + puzzle SS before the engine
        // borrow (these are separate `App` fields from `engine`).
        let camera = std::mem::take(&mut self.startup_camera);
        let ss_override = self.startup_ss_override.take();
        // Choose the connection provider and (if a plugin provides them)
        // populate the held set BEFORE the rebake below stamps the assembly,
        // so the first display already carries the plugin's connections and
        // the rebake never runs molex's geometric fallback under a provider.
        // Self-gates on the engine, which is present at this seam.
        self.refresh_connections();
        if let Some(engine) = self.engine.as_mut() {
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
            // Apply any puzzle-pinned SS override AFTER the rebake: it calls
            // `replace_assembly`, which rebuilds the cartoon from the
            // assembly's own (loop) SS, so an override set earlier would be
            // clobbered. Setting it last invalidates RE_MESH, so the next
            // engine.update remeshes the ribbon with the override shape.
            if let Some((entity_raw, ss)) = ss_override {
                engine.set_ss_override(entity_raw, ss);
            }
        }
        // Fire the initial structural-viz queries once. The at-rest clash /
        // void refresh in `tick` gates on `startup_settled() && render_changes
        // > 0`, but the geometry changes happen DURING bring-up (before the
        // machine settles) and the settled session is at rest, so that gate
        // never fires for the first display. Kick them here so clashes,
        // voids, and exposed-hydrophobic beads show without waiting for the
        // first user edit. Each self-gates
        // on the engine, its display toggle, and a plugin advertising the
        // query, so this is an inert no-op when any is absent.
        self.refresh_clashes();
        self.refresh_external_cavities();
        self.refresh_exposed_hydrophobics();
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
        self.arm_plugin_bringup()
    }

    /// Set [`Self::startup`] to drive plugin bring-up for an in-session load
    /// (file / puzzle). When `loaded`, arms the same `Init` -> normalize ->
    /// score -> `InSession` sequence the launch path runs (the plugins are
    /// already warm, so it enters at `Initializing`); when the load failed,
    /// flips straight into the session degraded so the loading screen still
    /// clears. The reload path's own reset already dropped the prior plugin
    /// sessions, so the kicked `Init`s re-bind every plugin to this structure.
    #[cfg(not(target_arch = "wasm32"))]
    pub(in crate::app) fn arm_session_bringup(&mut self, loaded: bool) {
        self.startup = if loaded {
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
            match molex::ops::wire::serialize_assembly(&head_before) {
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
    /// Whole-structure normalize: empty selection / no focus (the bridge
    /// ignores selection for normalize). The params carry the loaded puzzle's
    /// scorefunction weight patch as `weight.<scoretype>` entries (empty when
    /// the puzzle declares no patch); see `weight_params` below. The
    /// dispatch's own `request_id` / scope are discarded; `apply_post_init`
    /// re-derives its target entities and mints its own checkpoint when the
    /// reply lands.
    #[cfg(not(target_arch = "wasm32"))]
    fn inits_done_kick_normalizes(
        &mut self,
        adopted: &std::collections::BTreeSet<String>,
    ) -> StartupPhase {
        // Thread the loaded puzzle's scorefunction weight patch to the bridge
        // through the normalize dispatch params, one `weight.<scoretype>` ->
        // Float(weight) entry per patch entry. The bridge stashes these on the
        // session at normalize and applies them at every scorefunction build,
        // so weight-zero terms (e.g. `envsmooth`) ship and are optimized
        // against. Empty when the session carries no puzzle or no patch.
        let weight_params: std::collections::HashMap<String, foldit_gui::state::ParamValue> =
            self.store
                .puzzle()
                .and_then(|p| p.weight_patch.as_ref())
                .map(|patch| {
                    patch
                        .iter()
                        .map(|(name, &w)| {
                            (
                                format!("weight.{name}"),
                                foldit_gui::state::ParamValue::Float(w),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();

        let mut expected = std::collections::BTreeSet::new();
        for plugin_id in adopted {
            let Some(op_id) = self.runner_client.normalize_op_for(plugin_id) else {
                continue;
            };
            let intent = DispatchIntent {
                selection: std::collections::BTreeMap::new(),
                focused_entity_id: None,
                op_id,
                params: weight_params.clone(),
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
}
