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
#[cfg(not(target_arch = "wasm32"))]
mod clashes;
#[cfg(not(target_arch = "wasm32"))]
mod voids;
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

    /// Release any lock state when puzzle topology changes.
    pub fn reset_for_new_structure(&mut self) {
        if let Some(ref mut orch) = self.orchestrator {
            for eid in orch.locked_entities() {
                orch.unlock(eid);
            }
        }
    }

    /// Shut down the orchestrator (and, through it, plugin workers).
    pub fn shutdown(&self) {
        if let Some(ref orch) = self.orchestrator {
            orch.shutdown();
        }
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
    pub(crate) fn kick_inits(&mut self, initial_assembly: &[u8]) -> Vec<String> {
        let Some(orch) = self.orchestrator.as_mut() else {
            return Vec::new();
        };
        let discovered = orch.discovered_plugin_ids();
        let mut kicked = Vec::with_capacity(discovered.len());
        for plugin_id in &discovered {
            match orch.kick_init_session(plugin_id, initial_assembly.to_owned()) {
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
