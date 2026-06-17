//! Non-blocking async-startup state machine for [`App`].
//!
//! These methods arm and advance the per-frame bring-up sequence (plugin
//! warm, `Init`, first score) that runs while the host keeps rendering the
//! loading screen; the phase enum itself lives in `super::load`.

use viso::VisoEngine;

use foldit_gui::AppPhase;
use molex::entity::molecule::id::EntityId;

use super::App;
use super::load::{StartupCamera, StartupPhase, locate_plugins_root};
use crate::history::CheckpointKind;
use crate::render_projector::RenderProjector;

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
    /// Never blocks: the worker round-trips (warm connect, `Init`, first
    /// score) are all kicked here / by `advance_startup` and polled on later
    /// frames, so the host renders the loading screen throughout.
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
    /// publish a step triggers (the structure parse) is drained + projected
    /// the same frame. Each step polls for whatever worker replies arrived
    /// since the last frame, folds them into the in-flight accumulator, and
    /// on completeness kicks the next phase. No step blocks. Inert in `Idle`
    /// / `Done`.
    /// True once the startup state-machine has reached its terminal state.
    /// The machine drives the first score itself (the post-Init kick); the
    /// tick's at-rest auto-rescore must hold off until then so it does not
    /// fire a `score` query before the head breakdown has stamped.
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
                    // Every Init has replied: the bridge has built its pose
                    // and decoded the puzzle config (weight patch + filters)
                    // carried on the Init payload, so the first score can
                    // query it. Tick's at-rest gate may also fire, but
                    // `request_scores` coalesces, so this overlap is harmless.
                    self.startup = self.kick_first_score_then_phase();
                } else {
                    self.startup = StartupPhase::Initializing { expected, adopted };
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
        let camera = std::mem::take(&mut self.startup_camera);
        let ss_override = self.startup_ss_override.take();
        // Choose the connection provider and (if a plugin provides them)
        // populate the held set BEFORE the rebake below stamps the assembly,
        // so the first display already carries the plugin's connections and
        // the rebake never runs molex's geometric fallback under a provider.
        // Gated on the engine, which is present at this seam.
        if self.engine.is_some() {
            crate::viz::refresh::refresh_connections(&mut self.runner_client, &mut self.store);
        }
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
            // the final geometry. The first score may have
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
        // void refresh in `tick` gates on `startup_settled()` and a
        // geometry change in the batch, but the geometry changes happen
        // DURING bring-up (before the
        // machine settles) and the settled session is at rest, so that gate
        // never fires for the first display. Kick them here so clashes,
        // voids, and exposed-hydrophobic beads show without waiting for the
        // first user edit. Each gates
        // on its display toggle and a plugin advertising the
        // query, so this is an inert no-op when any is absent. The caller
        // gates on the engine, which is present at this seam.
        if self.engine.is_some() {
            crate::viz::refresh::refresh_clashes(
                &mut self.runner_client,
                &mut self.store,
                &self.view_options,
            );
            crate::viz::refresh::refresh_external_cavities(
                &mut self.runner_client,
                &mut self.store,
                &self.view_options,
            );
            crate::viz::refresh::refresh_exposed_hydrophobics(
                &mut self.runner_client,
                &mut self.store,
                &self.view_options,
            );
        }
        // The design-gating overlay is static per puzzle (the mask is set at
        // load), so a single load-time push suffices: viso re-derives the GPU
        // bitset from the per-entity set on every mesh rebuild, keeping the
        // overlay pinned across geometry changes without a per-tick re-push.
        if let Some(engine) = self.engine.as_mut() {
            crate::viz::refresh::refresh_design_gating(&self.store, engine);
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

        // Snapshot the just-loaded assembly and KICK each warm plugin's
        // `Init` against it. Every plugin inits against this one molex-canonical
        // snapshot.
        self.arm_plugin_bringup()
    }

    /// Set [`Self::startup`] to drive plugin bring-up for an in-session load
    /// (file / puzzle). When `loaded`, arms the same `Init` -> score ->
    /// `InSession` sequence the launch path runs (the plugins are
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

        // Source the puzzle-specific session payload (ligand asset bytes +
        // catalytic constraints) from the loaded puzzle. A free-form
        // structure load has no puzzle, so both default empty. Cloned out of
        // the puzzle to release the `self.store` borrow before the
        // `&mut self.runner_client` kick below.
        let (ligands, constraints) = self.store.puzzle().map_or_else(
            || (Vec::new(), Vec::new()),
            |p| (p.ligands.clone(), p.constraints.clone()),
        );

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
            .kick_inits(&initial_assembly, &ligands, &constraints, &config_params)
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

    /// Build the loaded puzzle's generic config-param channel for the Init
    /// payload: the scorefunction weight patch as `weight.<scoretype>` ->
    /// `Float(weight)` entries, plus the rosetta-targeted objective filters
    /// as `filter.<i>.*` String entries. Empty when the session carries no
    /// puzzle (a free-form structure load) or the puzzle declares neither.
    ///
    /// Weight patch: one entry per patched term, so weight-zero terms (e.g.
    /// `envsmooth`) ship and are optimized against.
    ///
    /// Filters: only those naming `plugin = "rosetta"` are forwarded; each
    /// takes a contiguous index `i` over the forwarded filters, and the
    /// bridge decodes `filter.<i>.type` for the filter kind and
    /// `filter.<i>.<key>` for each flattened param, all String-typed. A
    /// non-rosetta plugin is unsupported: warn (naming it) and skip rather
    /// than forward to a bridge that cannot score it.
    #[cfg(not(target_arch = "wasm32"))]
    fn build_init_config_params(
        &self,
    ) -> std::collections::HashMap<String, foldit_gui::state::ParamValue> {
        let mut params: std::collections::HashMap<String, foldit_gui::state::ParamValue> = self
            .store
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

        if let Some(filters) = self.store.puzzle().map(|p| p.filters.clone()) {
            let mut i = 0;
            for spec in &filters {
                match spec.plugin.as_deref() {
                    None => {}
                    Some("rosetta") => {
                        params.insert(
                            format!("filter.{i}.type"),
                            foldit_gui::state::ParamValue::String(spec.kind.clone()),
                        );
                        for (key, value) in &spec.params {
                            params.insert(
                                format!("filter.{i}.{key}"),
                                foldit_gui::state::ParamValue::String(
                                    toml_value_to_plain_string(value),
                                ),
                            );
                        }
                        i += 1;
                    }
                    Some(other) => {
                        log::warn!(
                            "[App] puzzle filter '{}' names unknown plugin '{other}'; \
                             skipping (only 'rosetta' is forwarded)",
                            spec.kind,
                        );
                    }
                }
            }
        }

        params
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

/// Render a `toml::Value` as the bare string the forwarded-filter param
/// convention expects: an `Integer(-100)` becomes `"-100"`, a `Float` its
/// decimal string, and a `String` its bare contents (no surrounding quotes,
/// unlike `Value::to_string`). Other variants fall back to their `Display`
/// form. Used to flatten a `FilterSpec.params` entry into a `filter.<i>.<key>`
/// String param.
#[cfg(not(target_arch = "wasm32"))]
fn toml_value_to_plain_string(value: &toml::Value) -> String {
    match value {
        toml::Value::String(s) => s.clone(),
        toml::Value::Integer(n) => n.to_string(),
        toml::Value::Float(f) => f.to_string(),
        toml::Value::Boolean(b) => b.to_string(),
        other => other.to_string(),
    }
}
