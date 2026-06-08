//! Runner client: owns the orchestrator handle and the native stream
//! bookkeeping that drives plugin operations.
//!
//! `RunnerClient` holds the `Orchestrator` and (on native builds) the
//! in-flight `StreamHost` state, plus the orchestrator-lifecycle handlers
//! that touch only the orchestrator (`reset_for_new_structure`,
//! `shutdown`).
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

// ── Two-phase bring-up: warm (startup) then session-init (file-load) ──

#[cfg(not(target_arch = "wasm32"))]
impl RunnerClient {
    /// Phase 1 (app startup): discover plugins under `root` and WARM each
    /// one — spawn its worker, which loads the backend / database /
    /// scoring with NO session and NO pose. This is the app-lifecycle
    /// warm-up, independent of any structure; the session is created
    /// later, at file-load, by [`Self::init_runner_sessions`]. Silently
    /// degrades to viewer-only when no orchestrator is wired up or
    /// discovery fails, rather than erroring.
    pub(crate) fn warm_runner(&mut self, root: &std::path::Path) {
        let Some(orch) = self.orchestrator.as_mut() else {
            return;
        };
        let discovered = match orch.discover_plugins(root) {
            Ok(ids) => ids,
            Err(e) => {
                log::warn!(
                    "[RunnerClient] discover_plugins({}) failed: {e}; plugins disabled",
                    root.display()
                );
                return;
            }
        };
        log::info!("[RunnerClient] discovered plugins: {discovered:?}");
        for plugin_id in &discovered {
            if let Some(descriptor) = orch.plugin_descriptor(plugin_id).cloned() {
                match orch.warm_plugin(&descriptor) {
                    Ok(()) => log::info!("[RunnerClient] {plugin_id} plugin warm"),
                    Err(e) => log::warn!(
                        "[RunnerClient] warm_plugin('{plugin_id}') failed: {e}; \
                         {plugin_id} plugin disabled"
                    ),
                }
            }
        }
    }

    /// Phase 2 (file-load): create each warm plugin's session by running
    /// `Init` against the given initial assembly (the structure is built
    /// here). Returns the `(plugin_id, post_init_bytes)` pair for every
    /// plugin whose session came up; the post-Init bytes carry each
    /// plugin's normalized assembly for the caller to apply. Empty `Vec`
    /// when no orchestrator is wired up — degrades to viewer-only rather
    /// than erroring. Iterates over the already-warmed (discovered)
    /// plugins; `init_plugin_session` is a no-op for any that already
    /// hold a session.
    pub(crate) fn init_runner_sessions(
        &mut self,
        initial_assembly: &[u8],
    ) -> Vec<(String, Vec<u8>)> {
        let Some(orch) = self.orchestrator.as_mut() else {
            return Vec::new();
        };
        let discovered = orch.discovered_plugin_ids();
        let mut registered = Vec::with_capacity(discovered.len());
        for plugin_id in &discovered {
            match orch.init_plugin_session(plugin_id, initial_assembly.to_owned()) {
                Ok(bytes) => {
                    log::info!("[RunnerClient] {plugin_id} plugin session ready");
                    registered.push((plugin_id.clone(), bytes));
                }
                Err(e) => {
                    log::warn!(
                        "[RunnerClient] init_plugin_session('{plugin_id}') \
                         failed: {e}; {plugin_id} plugin disabled"
                    );
                }
            }
        }
        registered
    }
}
