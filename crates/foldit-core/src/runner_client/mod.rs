//! Runner client: owns the orchestrator handle and the native stream
//! bookkeeping that drives plugin operations.
//!
//! `RunnerClient` holds the `Orchestrator` and (on native builds) the
//! in-flight `StreamHost` state, plus the orchestrator-lifecycle handlers
//! that touch only the orchestrator (`reset_for_new_structure`,
//! `shutdown`). Plugin bring-up is non-blocking: the kick/poll twins
//! ([`RunnerClient::kick_warms`]/[`RunnerClient::poll_warms`],
//! [`RunnerClient::kick_inits`]/[`RunnerClient::poll_inits`],
//! [`RunnerClient::kick_normalize`]/[`RunnerClient::poll_normalizes`])
//! let the startup state-machine on `App` drive bring-up one frame at a
//! time so the host renders throughout.
//! Inbound plugin traffic is drained here too: [`RunnerClient::drain_op_events`]
//! consumes the orchestrator's raw `PluginUpdate`s and the stream table,
//! resolving each into a core-side [`OpEvent`] keyed by the edit token so
//! `App` applies them without naming orchestrator types or touching the
//! stream bookkeeping.
//!
//! `App::handle_dispatch_op` still interleaves orchestrator I/O with store
//! mutations (it begins the edit only after the dispatch succeeds), so it
//! stays on `App` and reaches into `self.runner_client` for the orchestrator
//! and stream state it needs.

mod dispatch;
mod catalog;
mod scores;
mod types;

#[cfg(not(target_arch = "wasm32"))]
pub use types::{
    DispatchError, DispatchIntent, EditScope, OpEvent, OpOutcome, StreamHost, StreamStartIntent,
};

/// Owns the orchestrator handle and the native-only stream bookkeeping.
/// `App` holds one of these and reaches into its fields by direct path so
/// the orchestrator and stream state can be borrowed disjointly (the
/// dispatch methods on `App` rely on this). The `SessionUpdate` stream's
/// plugin projection lives separately in `RunnerProjector`, a peer `App`
/// field, so the two can be borrowed disjointly across the tick seam.
pub struct RunnerClient {
    orchestrator: Option<foldit_runner::Orchestrator>,
    #[cfg(not(target_arch = "wasm32"))]
    stream_host: StreamHost,
}

impl RunnerClient {
    pub fn new() -> Self {
        Self {
            orchestrator: None,
            #[cfg(not(target_arch = "wasm32"))]
            stream_host: StreamHost {
                active_streams: std::collections::HashMap::new(),
                pull_drag: None,
            },
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

    /// The op-id a plugin declares in its manifest as its load-time
    /// normalize op, if any. Read off the discovered spawn descriptor via
    /// the orchestrator. `None` when no orchestrator is installed, the
    /// plugin isn't discovered, or its manifest omits `normalize_op`.
    /// Bootstrap uses this to decide whether to invoke a canonicalizing op
    /// after Init.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn normalize_op_for(&self, plugin_id: &str) -> Option<String> {
        self.orchestrator
            .as_ref()?
            .plugin_descriptor(plugin_id)?
            .normalize_op()
            .map(String::from)
    }

    /// Resolve an op-id to its owning plugin id via the orchestrator's
    /// plugin registry. Returns `None` when no orchestrator is installed or
    /// the op-id is unknown to the registry. Encapsulates the registry lookup
    /// so the dispatch path names no orchestrator type.
    pub(crate) fn resolve_op_plugin_id(&self, op_id: &str) -> Option<String> {
        self.orchestrator
            .as_ref()?
            .plugin_registry()
            .get_op(op_id)
            .map(|op| op.plugin_id.clone())
    }

    /// Whether `op_id` declares `creates_entities` (its output is a NEW
    /// entity to adopt, not an edit of an existing lane). `false` for an
    /// unknown op-id. Lets the dispatch path skip `begin_action` for
    /// entity-creating ops without naming an orchestrator type.
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

// ── Generic raw-bytes plugin queries ──
//
// The at-rest viz coordinators (clashes, voids, exposed-hydrophobics) each
// forward a well-known query id to the orchestrator over the generic
// raw-bytes dispatch and decode the opaque reply themselves; the payload
// goes to the viso engine, not an orchestrator score merge, so it stays on
// this generic query path rather than the score-specialized one. The
// support gate keeps each path an inert no-op until a plugin advertises the
// query.

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
        match orch.dispatch_query(id, DispatchContext::default(), std::collections::HashMap::new()) {
            Ok(bytes) => bytes,
            Err(e) => {
                log::trace!("query '{id}' failed: {e}");
                Vec::new()
            }
        }
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
}

// ── Non-blocking two-phase bring-up: warm (startup) then session-init
//    (file-load), each a kick + poll pair ──

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
        ligands: &[crate::puzzle::LigandAsset],
        constraints: &[crate::puzzle_setup::Constraint],
    ) -> Vec<String> {
        let Some(orch) = self.orchestrator.as_mut() else {
            return Vec::new();
        };
        // Convert the core-side puzzle payload into the orchestrator's native
        // mirror types once; the per-plugin loop clones the payload per kick.
        let payload = init_payload_from_puzzle(ligands, constraints);
        let discovered = orch.discovered_plugin_ids();
        let mut kicked = Vec::with_capacity(discovered.len());
        for plugin_id in &discovered {
            match orch.kick_init_session(
                plugin_id,
                initial_assembly.to_owned(),
                payload.clone(),
            ) {
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

    /// Kick a load-time normalize op WITHOUT blocking on the worker reply.
    /// The non-blocking twin of the synchronous normalize dispatch: builds
    /// the orchestrator `DispatchContext` from the core-shaped
    /// [`DispatchIntent`] (same flatten as `dispatch_op`'s Invoke branch)
    /// and forwards to `kick_invoke`. The op selection (whole-structure,
    /// empty selection / no focus) and the intent construction stay in the
    /// caller, exactly as the synchronous path builds them. The reply is
    /// drained later by [`Self::poll_normalizes`]. Logs and degrades on a
    /// kick failure (matching the synchronous normalize loop's
    /// log-and-skip) without naming any `foldit_runner` error type.
    pub(crate) fn kick_normalize(
        &mut self,
        intent: DispatchIntent,
        plugin_id: &str,
        entity_type_of: impl Fn(molex::EntityId) -> Option<molex::EntityKind>,
    ) {
        use foldit_runner::orchestrator::{DispatchContext, ResidueRef};
        let Some(orch) = self.orchestrator.as_mut() else {
            return;
        };
        // Flatten the authoritative selection (molex ids) into the
        // wire-shape `ResidueRef` list the orchestrator's context expects.
        let selection: Vec<ResidueRef> = intent
            .selection
            .iter()
            .flat_map(|(entity, residues)| {
                let id = *entity;
                residues.iter().map(move |&residue_index| ResidueRef {
                    entity_id: id,
                    residue_index,
                })
            })
            .collect();
        let ctx = DispatchContext {
            focused_entity_id: intent.focused_entity_id,
            selection,
        };
        let params: std::collections::HashMap<
            String,
            foldit_runner::orchestrator::ParamValue,
        > = intent
            .params
            .into_iter()
            .map(|(k, v)| (k, crate::wire_params::param_value_from_wire(v)))
            .collect();
        if let Err(e) = orch.kick_invoke(&intent.op_id, ctx, params, entity_type_of) {
            log::warn!(
                "[RunnerClient] kick_invoke('{plugin_id}', {:?}) failed: {e}; \
                 skipping normalization apply",
                intent.op_id
            );
        }
    }

    /// Drain whatever async normalize replies have arrived since the last
    /// call. Non-blocking; a plugin whose normalize has not replied stays
    /// pending. Returns the `(plugin_id, normalized_bytes)` pair for each
    /// plugin whose normalize completed THIS poll, dropping the dispatch
    /// `request_id` and resolved targets (the caller's `apply_post_init`
    /// re-derives its target entities and mints its own checkpoint, so it
    /// only needs the bytes). Logs each failure without naming any
    /// `foldit_runner` error type. Empty when no orchestrator is wired up
    /// or nothing completed this poll.
    pub(crate) fn poll_normalizes(&mut self) -> Vec<(String, Vec<u8>)> {
        let Some(orch) = self.orchestrator.as_mut() else {
            return Vec::new();
        };
        let mut done = Vec::new();
        for (plugin_id, result) in orch.poll_invokes() {
            match result {
                Ok((_request_id, bytes, _targets)) => {
                    done.push((plugin_id, bytes));
                }
                Err(e) => log::warn!(
                    "[RunnerClient] normalize for '{plugin_id}' failed: {e}; \
                     skipping normalization apply"
                ),
            }
        }
        done
    }
}

// ── Core-side puzzle payload -> orchestrator native mirror ──
//
// The session-init kick delivers the loaded puzzle's ligand assets +
// catalytic constraints to the worker. These free functions convert core's
// puzzle types into the orchestrator's native mirror types (the same pattern
// `kick_normalize` uses to flatten selection / params); the orchestrator's
// IPC boundary then encodes them to proto.

#[cfg(not(target_arch = "wasm32"))]
fn init_payload_from_puzzle(
    ligands: &[crate::puzzle::LigandAsset],
    constraints: &[crate::puzzle_setup::Constraint],
) -> foldit_runner::orchestrator::InitPayload {
    use foldit_runner::orchestrator::{InitPayload, PuzzleAsset};

    // Each ligand contributes its `.params` asset and, when present, its
    // conformer PDB as a second asset.
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
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn constraint_to_runner(
    c: &crate::puzzle_setup::Constraint,
) -> foldit_runner::orchestrator::Constraint {
    use crate::puzzle_setup::{ConstraintFunc as CoreFunc, ConstraintKind as CoreKind};
    use foldit_runner::orchestrator::{
        Constraint, ConstraintAtom, ConstraintFunc, ConstraintKind,
    };

    let kind = match c.kind {
        CoreKind::AtomPair => ConstraintKind::AtomPair,
        CoreKind::Angle => ConstraintKind::Angle,
        CoreKind::Dihedral => ConstraintKind::Dihedral,
    };
    let func = match c.func {
        CoreFunc::FlatHarmonic { x0, sd, tol } => {
            ConstraintFunc::FlatHarmonic { x0, sd, tol }
        }
        CoreFunc::CircularHarmonic { x0, sd } => {
            ConstraintFunc::CircularHarmonic { x0, sd }
        }
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
