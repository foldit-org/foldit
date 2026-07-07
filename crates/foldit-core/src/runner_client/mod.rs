//! Runner client: owns the orchestrator handle and the native stream
//! bookkeeping that drives plugin operations. Plugin bring-up is
//! non-blocking, driven through the kick/poll pairs.

mod catalog;
mod dispatch;
#[cfg(not(target_arch = "wasm32"))]
mod pull;
mod scores;
mod types;
#[cfg(not(target_arch = "wasm32"))]
mod weights;

#[cfg(not(target_arch = "wasm32"))]
pub use catalog::HotkeyOwner;
#[cfg(not(target_arch = "wasm32"))]
pub use pull::{build_pull_info, route_atom_pick, route_residue_pick, PullDrag, PullRoute};
#[cfg(not(target_arch = "wasm32"))]
pub use types::{
    DispatchError, DispatchIntent, EditScope, OpEvent, OpOutcome, StreamHost, StreamStartIntent,
};

/// Per-plugin model-weights readiness, decoded from each plugin's
/// `weights_status` reply and advanced by an in-flight download's stream
/// events. Gates the plugin's action buttons: any state other than `Ready`
/// surfaces only a download button (a `Missing`/`Failed` retry, or the
/// in-progress `Downloading`) in place of the plugin's normal buttons.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, PartialEq)]
pub(crate) enum WeightsState {
    Missing,
    Ready,
    /// A download is in flight for this plugin. `rid` is the stream
    /// `request_id` the download dispatched under; it is retained so a stream
    /// terminal can be matched back to the plugin that started it even when
    /// the terminal arrives before any progress frame.
    Downloading {
        progress: f32,
        stage: String,
        rid: u64,
    },
    /// The last download attempt failed; the plugin keeps its download button
    /// as a retry. `message` is the terminal's error text.
    Failed {
        message: String,
    },
}

/// Owns the orchestrator handle and the native-only stream bookkeeping.
/// `App` holds one of these and reaches into its fields by direct path so
/// the orchestrator and stream state can be borrowed disjointly (the
/// dispatch methods on `App` rely on this).
pub struct RunnerClient {
    orchestrator: Option<foldit_runner::Orchestrator>,
    #[cfg(not(target_arch = "wasm32"))]
    stream_host: StreamHost,
    /// Weights readiness per plugin id. A plugin absent from the map has not
    /// yet reported (or does not advertise `weights_status`); its buttons are
    /// left untouched until a `Missing` reply swaps them.
    #[cfg(not(target_arch = "wasm32"))]
    weights: std::collections::HashMap<String, WeightsState>,
}

impl RunnerClient {
    pub fn new() -> Self {
        Self {
            orchestrator: None,
            #[cfg(not(target_arch = "wasm32"))]
            stream_host: StreamHost {
                active_streams: std::collections::HashMap::new(),
                pull_drag: None,
                pending_pull_origin: None,
            },
            #[cfg(not(target_arch = "wasm32"))]
            weights: std::collections::HashMap::new(),
        }
    }

    /// Construct and install a fresh orchestrator handle. Called on
    /// structure load (and again on the load-error path) to replace any
    /// prior handle before plugin discovery runs.
    pub(crate) fn init_orchestrator(&mut self) {
        self.orchestrator = Some(foldit_runner::Orchestrator::new());
    }

    /// Mutable access to the orchestrator handle, so the tick seam can
    /// borrow it disjointly from the peer `RunnerProjector` field on `App`.
    pub(crate) const fn orchestrator_mut(&mut self) -> Option<&mut foldit_runner::Orchestrator> {
        self.orchestrator.as_mut()
    }

    /// Resolve an op-id to its owning plugin id via the orchestrator's
    /// plugin registry. Returns `None` when no orchestrator is installed or
    /// the op-id is unknown to the registry.
    pub(crate) fn resolve_op_plugin_id(&self, op_id: &str) -> Option<String> {
        self.orchestrator
            .as_ref()?
            .plugin_registry()
            .get_op(op_id)
            .map(|op| op.plugin_id.clone())
    }

    /// Whether the orchestrator currently holds any entity lock. Read
    /// immutably so a host-native edit can refuse while a plugin operation
    /// is mid-flight (a concurrent native mutation would race it). `false`
    /// when no orchestrator is wired up.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn any_lock_held(&self) -> bool {
        self.orchestrator
            .as_ref()
            .is_some_and(|orch| !orch.locked_entities().is_empty())
    }

    /// Whether `op_id` declares `creates_entities` (its output is a NEW
    /// entity to adopt, not an edit of an existing lane). `false` for an
    /// unknown op-id.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn op_creates_entities(&self, op_id: &str) -> bool {
        self.orchestrator
            .as_ref()
            .and_then(|o| o.plugin_registry().get_op(op_id))
            .is_some_and(|op| op.lock_meta.creates_entities)
    }

    /// Release lock state and drop every plugin session when puzzle topology
    /// changes. Dropping the sessions (the warm workers stay up) is what lets
    /// the load path re-`Init` each plugin against the new structure:
    /// `init_plugin_session` is idempotent on a live session, so without this
    /// a switched-in puzzle would keep the outgoing structure's pose and the
    /// op registry would never refresh.
    pub fn reset_for_new_structure(&mut self) {
        if let Some(ref mut orch) = self.orchestrator {
            for eid in orch.locked_entities() {
                orch.unlock(eid);
            }
            #[cfg(not(target_arch = "wasm32"))]
            orch.drop_all_sessions();
        }
    }

    /// Shut down the orchestrator (and, through it, plugin workers).
    pub fn shutdown(&self) {
        if let Some(ref orch) = self.orchestrator {
            orch.shutdown();
        }
    }
}

// Each query stays an inert no-op until a plugin advertises it.

#[cfg(not(target_arch = "wasm32"))]
impl RunnerClient {
    /// Whether any plugin has registered the query `id`. The bridge
    /// advertises a query by registration (same index the `score` query
    /// lives in), so this is the host-side support gate: an at-rest viz
    /// trigger requests the query ONLY when this is `true`, keeping the path
    /// inert until a plugin implements it. `false` when no orchestrator is
    /// installed.
    pub(crate) fn supports_query(&self, id: &str) -> bool {
        self.orchestrator
            .as_ref()
            .is_some_and(|orch| orch.plugin_registry().get_query(id).is_some())
    }

    /// Request the query `id` synchronously and return its raw opaque reply
    /// bytes, the payload the caller decodes. Passes no bytes and the default
    /// dispatch context: the query covers the current session pose, like the
    /// whole-assembly `score` query.
    ///
    /// Returns an empty `Vec` (the "clear" signal) when no orchestrator is
    /// installed, no plugin advertises the query, or the query errors. The
    /// unsupported case is filtered by [`Self::supports_query`] before the
    /// call; the error case is swallowed at `trace` level so an at-rest miss
    /// never spams the log.
    pub(crate) fn request_query_bytes(&mut self, id: &str) -> Vec<u8> {
        use foldit_runner::orchestrator::DispatchContext;
        let Some(orch) = self.orchestrator.as_mut() else {
            return Vec::new();
        };
        match orch.dispatch_query(
            id,
            DispatchContext::default(),
            std::collections::HashMap::new(),
        ) {
            Ok(bytes) => bytes,
            Err(e) => {
                log::trace!("query '{id}' failed: {e}");
                Vec::new()
            }
        }
    }

    /// Forward a panel-initiated query `id` against the supplied focus /
    /// selection context and return the plugin's raw opaque reply bytes for
    /// the caller to relay undecoded. The orchestrator routes the query to
    /// its owning plugin by id; this layer never interprets the query or its
    /// bytes. The `DispatchContext` is built from the caller's real focus /
    /// selection the same way [`Self::dispatch_op`] builds it (flatten the
    /// per-entity residue maps to `ResidueRef`s), and the wire-form params are
    /// converted to the orchestrator's native `ParamValue` through the shared
    /// [`crate::wire_params::param_value_from_wire`].
    ///
    /// Unlike [`Self::request_query_bytes`], the error is surfaced rather than
    /// swallowed so the caller can reject the originating request.
    ///
    /// # Errors
    ///
    /// Returns `Err` when no plugin registers `id`, no orchestrator is
    /// installed, or the query dispatch fails.
    ///
    /// [`Self::dispatch_op`]: super::RunnerClient::dispatch_op
    pub(crate) fn dispatch_plugin_query(
        &mut self,
        id: &str,
        focus: Option<molex::EntityId>,
        selection: &std::collections::BTreeMap<molex::EntityId, std::collections::BTreeSet<u32>>,
        designable: &std::collections::BTreeMap<molex::EntityId, std::collections::BTreeSet<u32>>,
        params: std::collections::HashMap<String, foldit_gui::state::ParamValue>,
    ) -> Result<Vec<u8>, String> {
        if !self.supports_query(id) {
            return Err(format!("query '{id}' is not registered"));
        }
        let ctx = types::build_dispatch_context(focus, selection, designable);
        let orch = self
            .orchestrator
            .as_mut()
            .ok_or_else(|| String::from("orchestrator not initialized"))?;

        let params: std::collections::HashMap<String, foldit_runner::orchestrator::ParamValue> =
            params
                .into_iter()
                .map(|(k, v)| (k, crate::wire_params::param_value_from_wire(v)))
                .collect();

        orch.dispatch_query(id, ctx, params)
            .map_err(|e| format!("query '{id}' failed: {e}"))
    }

    /// Fire the query `id` non-blocking against the live session pose. The
    /// reply lands on a stored receiver drained by
    /// [`Self::poll_query_results`]; the caller decodes and applies it then.
    /// Passes the default dispatch context (whole-assembly, like the sync
    /// [`Self::request_query_bytes`]) and no params. No-op when no
    /// orchestrator is installed; the orchestrator no-ops further when no
    /// plugin advertises the query or one is already outstanding for `id`.
    pub(crate) fn request_query(&mut self, id: &str) {
        use foldit_runner::orchestrator::DispatchContext;
        if let Some(orch) = self.orchestrator.as_mut() {
            orch.request_query(id, &DispatchContext::default());
        }
    }

    /// Drain whatever async query replies have arrived, each as
    /// `(query_id, opaque_bytes)`. Non-blocking; empty when nothing is ready
    /// or no orchestrator is installed.
    pub(crate) fn poll_query_results(&mut self) -> Vec<(String, Vec<u8>)> {
        self.orchestrator
            .as_mut()
            .map(foldit_runner::Orchestrator::poll_query_results)
            .unwrap_or_default()
    }

    /// Stamp a plugin as downloading its weights under stream `rid`. Called
    /// at dispatch time (not on the first progress frame) so a download that
    /// terminates before any progress frame is still matched back by rid at
    /// its terminal.
    pub(crate) fn set_weights_downloading(&mut self, plugin_id: &str, rid: u64) {
        self.weights.insert(
            plugin_id.to_owned(),
            WeightsState::Downloading {
                progress: 0.0,
                stage: String::new(),
                rid,
            },
        );
    }

    /// Advance the in-flight download that is `Downloading` under stream
    /// `rid`, folding in whatever `progress` / `stage` the frame carried.
    /// Returns whether a plugin matched, so the caller re-projects the action
    /// catalog only when something changed. A frame whose rid matches no
    /// in-flight download is ignored (some other stream's assembly-less
    /// progress).
    pub(crate) fn update_weights_progress(
        &mut self,
        rid: u64,
        progress: Option<f32>,
        stage: Option<String>,
    ) -> bool {
        for state in self.weights.values_mut() {
            if let WeightsState::Downloading {
                rid: r,
                progress: p,
                stage: s,
            } = state
            {
                if *r == rid {
                    if let Some(progress) = progress {
                        *p = progress;
                    }
                    if let Some(stage) = stage {
                        *s = stage;
                    }
                    return true;
                }
            }
        }
        false
    }

    /// Per-plugin live download progress for the GUI, one entry per plugin
    /// whose weights are currently `Downloading`. Non-`Downloading` states
    /// contribute nothing, so the map is empty when no download is in flight.
    pub(crate) fn weights_download_progress(
        &self,
    ) -> std::collections::HashMap<String, foldit_gui::state::DownloadProgress> {
        self.weights
            .iter()
            .filter_map(|(plugin_id, state)| match state {
                WeightsState::Downloading {
                    progress, stage, ..
                } => Some((
                    plugin_id.clone(),
                    foldit_gui::state::DownloadProgress {
                        fraction: *progress,
                        stage: stage.clone(),
                    },
                )),
                _ => None,
            })
            .collect()
    }

    /// The plugin whose download is in flight under stream `rid`, or `None`
    /// when no plugin is `Downloading` under it. A stream terminal uses this
    /// to tell a weight-download terminal apart from an ordinary op stream
    /// (so it can drive the download state machine instead of the edit-commit
    /// path) and to name the plugin in the completion notification.
    pub(crate) fn is_downloading_rid(&self, rid: u64) -> Option<String> {
        self.weights.iter().find_map(|(plugin_id, state)| {
            matches!(state, WeightsState::Downloading { rid: r, .. } if *r == rid)
                .then(|| plugin_id.clone())
        })
    }

    /// Whether `plugin_id`'s weights are currently `Downloading`. The host
    /// download intercept reads this to refuse a second download for a plugin
    /// whose download is already in flight.
    pub(crate) fn is_plugin_downloading(&self, plugin_id: &str) -> bool {
        matches!(
            self.weights.get(plugin_id),
            Some(WeightsState::Downloading { .. })
        )
    }

    /// Flip the plugin downloading under stream `rid` to `Failed` (its
    /// download button stays as a retry). Returns whether a plugin matched.
    pub(crate) fn set_weights_failed(&mut self, rid: u64, message: String) -> bool {
        for state in self.weights.values_mut() {
            if matches!(state, WeightsState::Downloading { rid: r, .. } if *r == rid) {
                *state = WeightsState::Failed { message };
                return true;
            }
        }
        false
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl RunnerClient {
    /// Kick off phase 1 (app startup) WITHOUT blocking on each worker's
    /// connect: discover plugins under `root` and KICK a warm for each one
    /// (bind the listener, spawn the worker child). The connects are
    /// finished later by [`Self::poll_warms`]. Returns the plugin ids whose
    /// warm was successfully KICKED (those that entered the pending-warm
    /// table), so the caller can tally `poll_warms` completions against
    /// exactly this set without reaching back into the orchestrator. A
    /// plugin whose kick failed is dropped from the returned set: it never
    /// becomes pending, so `poll_warms` would never report it and waiting on
    /// it would hang bring-up. Silently degrades to viewer-only (returns an
    /// empty `Vec`) when no orchestrator is wired up or discovery fails.
    pub(crate) fn kick_warms(&mut self, root: &std::path::Path) -> Vec<String> {
        let Some(orch) = self.orchestrator.as_mut() else {
            return Vec::new();
        };
        let discovered = match orch.discover_plugins(root) {
            Ok(ids) => ids,
            Err(e) => {
                log::warn!(
                    "[RunnerClient] discover_plugins({}) failed: {e}; plugins disabled",
                    root.display()
                );
                return Vec::new();
            }
        };
        log::info!("[RunnerClient] discovered plugins: {discovered:?}");
        let mut kicked = Vec::with_capacity(discovered.len());
        for plugin_id in &discovered {
            if let Some(descriptor) = orch.plugin_descriptor(plugin_id).cloned() {
                match orch.kick_warm_plugin(&descriptor) {
                    Ok(()) => {
                        log::info!("[RunnerClient] {plugin_id} plugin warming");
                        kicked.push(plugin_id.clone());
                    }
                    Err(e) => log::warn!(
                        "[RunnerClient] kick_warm_plugin('{plugin_id}') failed: {e}; \
                         {plugin_id} plugin disabled"
                    ),
                }
            }
        }
        kicked
    }

    /// Finish any warms whose worker has connected since the last call.
    /// Non-blocking; a worker that has not connected yet stays warming and
    /// is not reported until a later poll. Returns the `(plugin_id, ok)`
    /// outcome for each plugin that COMPLETED its warm this poll: `true` on
    /// success, `false` on failure (also logged, log-and-degrade). The `ok`
    /// flag lets the caller answer "did any fail" without naming any
    /// `foldit_runner` type. The poll twin of [`Self::kick_warms`]. Empty
    /// when no orchestrator is wired up or nothing completed this poll; the
    /// caller tallies completions against the kicked-plugin set to know when
    /// all warms are done.
    pub(crate) fn poll_warms(&mut self) -> Vec<(String, bool)> {
        let Some(orch) = self.orchestrator.as_mut() else {
            return Vec::new();
        };
        orch.poll_warm_plugins()
            .into_iter()
            .map(|(plugin_id, result)| match result {
                Ok(()) => {
                    log::info!("[RunnerClient] {plugin_id} plugin warm");
                    (plugin_id, true)
                }
                Err(e) => {
                    log::warn!(
                        "[RunnerClient] warm_plugin('{plugin_id}') failed: {e}; \
                         {plugin_id} plugin disabled"
                    );
                    (plugin_id, false)
                }
            })
            .collect()
    }

    /// Kick off phase 2 (file-load) WITHOUT blocking on each `Init` reply:
    /// for every warmed (discovered) plugin, KICK an `Init` against
    /// `initial_assembly`. The replies are drained later by
    /// [`Self::poll_inits`]. Returns the plugin ids whose `Init` was
    /// successfully KICKED (those that entered the pending-init table), so
    /// the caller can tally `poll_inits` completions against exactly this
    /// set. A plugin whose kick failed is dropped from the returned set: it
    /// never becomes pending, so `poll_inits` would never report it and
    /// waiting on it would hang bring-up. Empty `Vec` when no orchestrator
    /// is wired up.
    pub(crate) fn kick_inits(
        &mut self,
        initial_assembly: &[u8],
        ligands: &[crate::puzzle_load::LigandAsset],
        constraints: &[crate::puzzle_setup::Constraint],
        density: Option<&crate::puzzle_load::DensityAsset>,
        config_params: &std::collections::HashMap<String, foldit_gui::state::ParamValue>,
    ) -> Vec<String> {
        let Some(orch) = self.orchestrator.as_mut() else {
            return Vec::new();
        };
        // Convert the core-side puzzle payload into the orchestrator's native
        // mirror types once; the per-plugin loop clones the payload per kick.
        // The density map is per-plugin (only `uses_density` plugins receive
        // it), so it is appended inside the loop rather than baked into the
        // shared base payload.
        let payload = init_payload_from_puzzle(ligands, constraints, config_params);
        let discovered = orch.discovered_plugin_ids();
        let mut kicked = Vec::with_capacity(discovered.len());
        for plugin_id in &discovered {
            // Copy the opt-in bool out of the immutable descriptor borrow
            // before the `&mut` kick call below.
            let wants_density = orch
                .plugin_descriptor(plugin_id)
                .is_some_and(|d| d.uses_density());
            let plugin_payload = match (wants_density, density) {
                (true, Some(map)) => with_density(payload.clone(), map),
                _ => payload.clone(),
            };
            match orch.kick_init_session(plugin_id, initial_assembly.to_owned(), plugin_payload) {
                Ok(()) => kicked.push(plugin_id.clone()),
                Err(e) => log::warn!(
                    "[RunnerClient] kick_init_session('{plugin_id}') failed: \
                     {e}; {plugin_id} plugin disabled"
                ),
            }
        }
        kicked
    }

    /// Drain whatever `Init` replies have arrived since the last call.
    /// Non-blocking; a plugin whose Init has not replied stays pending and
    /// is not reported until a later poll. Returns the `(plugin_id,
    /// post_init_bytes)` pair for each plugin whose session came up THIS
    /// poll; the caller feeds the post-Init bytes to `apply_post_init` to
    /// adopt each plugin's normalized assembly. Logs each failure
    /// (log-and-degrade) without naming any `foldit_runner` type. The
    /// poll twin of [`Self::kick_inits`]. Empty when no orchestrator is
    /// wired up or nothing completed this poll; the caller tallies
    /// completions against the kicked-plugin set to know when all inits are
    /// done.
    pub(crate) fn poll_inits(&mut self) -> Vec<(String, Vec<u8>)> {
        let Some(orch) = self.orchestrator.as_mut() else {
            return Vec::new();
        };
        let mut registered = Vec::new();
        for (plugin_id, result) in orch.poll_init_sessions() {
            match result {
                Ok(bytes) => {
                    log::info!("[RunnerClient] {plugin_id} plugin session ready");
                    registered.push((plugin_id, bytes));
                }
                Err(e) => log::warn!(
                    "[RunnerClient] init_plugin_session('{plugin_id}') failed: \
                     {e}; {plugin_id} plugin disabled"
                ),
            }
        }
        registered
    }
}

// The session-init kick delivers the loaded puzzle's ligand assets,
// catalytic constraints, and generic config params (weight patch + objective
// filters) to the worker. These free functions convert core's puzzle types
// into the orchestrator's native mirror types; the orchestrator's IPC
// boundary then encodes them to proto.

#[cfg(not(target_arch = "wasm32"))]
fn init_payload_from_puzzle(
    ligands: &[crate::puzzle_load::LigandAsset],
    constraints: &[crate::puzzle_setup::Constraint],
    config_params: &std::collections::HashMap<String, foldit_gui::state::ParamValue>,
) -> foldit_runner::orchestrator::InitPayload {
    use foldit_runner::orchestrator::{InitPayload, PuzzleAsset};

    let mut assets: Vec<PuzzleAsset> = Vec::new();
    for lig in ligands {
        assets.push(PuzzleAsset {
            name: lig.name.clone(),
            data: lig.params.clone(),
        });
        if let Some((conf_name, conf_bytes)) = &lig.conformers {
            assets.push(PuzzleAsset {
                name: conf_name.clone(),
                data: conf_bytes.clone(),
            });
        }
    }

    InitPayload {
        assets,
        constraints: constraints.iter().map(constraint_to_runner).collect(),
        params: config_params
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    crate::wire_params::param_value_from_wire(v.clone()),
                )
            })
            .collect(),
    }
}

/// Append a puzzle's electron-density map to `payload`: the raw map bytes
/// ride the same `PuzzleAsset` lane as ligand assets, and the map's
/// resolution / grid spacing ride the generic `params` map as float
/// scalars keyed `density.resolution` / `density.grid_spacing`.
#[cfg(not(target_arch = "wasm32"))]
fn with_density(
    mut payload: foldit_runner::orchestrator::InitPayload,
    density: &crate::puzzle_load::DensityAsset,
) -> foldit_runner::orchestrator::InitPayload {
    use foldit_runner::orchestrator::{ParamValue, PuzzleAsset};

    payload.assets.push(PuzzleAsset {
        name: density.name.clone(),
        data: density.bytes.clone(),
    });
    let _ = payload.params.insert(
        String::from("density.resolution"),
        ParamValue::Float(density.resolution),
    );
    if let Some(spacing) = density.grid_spacing {
        let _ = payload.params.insert(
            String::from("density.grid_spacing"),
            ParamValue::Float(spacing),
        );
    }
    payload
}

#[cfg(not(target_arch = "wasm32"))]
fn constraint_to_runner(
    c: &crate::puzzle_setup::Constraint,
) -> foldit_runner::orchestrator::Constraint {
    use crate::puzzle_setup::{ConstraintFunc as CoreFunc, ConstraintKind as CoreKind};
    use foldit_runner::orchestrator::{Constraint, ConstraintAtom, ConstraintFunc, ConstraintKind};

    let kind = match c.kind {
        CoreKind::AtomPair => ConstraintKind::AtomPair,
        CoreKind::Angle => ConstraintKind::Angle,
        CoreKind::Dihedral => ConstraintKind::Dihedral,
    };
    let func = match c.func {
        CoreFunc::FlatHarmonic { x0, sd, tol } => ConstraintFunc::FlatHarmonic { x0, sd, tol },
        CoreFunc::CircularHarmonic { x0, sd } => ConstraintFunc::CircularHarmonic { x0, sd },
    };
    Constraint {
        kind,
        atoms: c
            .atoms
            .iter()
            .map(|a| ConstraintAtom {
                atom_name: a.atom_name.clone(),
                res_num: a.res_num,
                // proto has no char; chain travels as a single-char string.
                chain: a.chain.to_string(),
            })
            .collect(),
        func,
    }
}
