//! Plugin driver: owns the orchestrator handle and the native stream
//! bookkeeping that drives plugin operations.
//!
//! `PluginDriver` holds the `Orchestrator` and (on native builds) the
//! in-flight `StreamHost` state, plus the orchestrator-lifecycle
//! handlers that touch only the orchestrator (`reset_for_new_structure`,
//! `shutdown`). The two big dispatch methods (`handle_dispatch_op` and
//! `apply_backend_updates`) interleave orchestrator I/O with store
//! mutations, so they stay on `App` until they are decomposed in RX8;
//! `App` reaches into `self.plugin_driver` for the orchestrator and
//! stream state they need.

/// Owns the orchestrator handle plus the native-only stream
/// bookkeeping. `App` holds one of these and reaches into its public
/// fields by direct path so the orchestrator and stream state can be
/// borrowed disjointly (the dispatch methods on `App` rely on this).
pub struct PluginDriver {
    pub orchestrator: Option<foldit_runner::Orchestrator>,
    #[cfg(not(target_arch = "wasm32"))]
    pub stream_host: StreamHost,
}

impl PluginDriver {
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

/// Owns the in-flight stream bookkeeping that only exists on native
/// builds: the plugin stream handle table plus the live pull-drag
/// state. Grouped so App's stream lifecycle touches one field.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) struct StreamHost {
    /// In-flight stream handles keyed by request_id. Populated by
    /// `handle_dispatch_op` on `StartStream`; the matching
    /// `release_dispatch_locks` runs in `apply_backend_updates` when
    /// the stream's terminal `PluginUpdate` arrives. The stored
    /// `plugin_id` is the dispatch target for `dispatch_cancel_stream`
    /// when the user hits ESC.
    pub(crate) active_streams: std::collections::HashMap<
        u64,
        ActiveStreamEntry,
    >,
    /// Live pull-drag state. `Some(...)` between pointer-down on an
    /// atom and pointer-up / stream-terminal / ESC cancel. The drag's
    /// stream id also lives in `active_streams` so Final/Error
    /// handling flows through the unified stream-cleanup path; this
    /// field carries the extra viso-side bookkeeping needed for
    /// pointer-move (PullInfo + op id).
    pub(crate) pull_drag: Option<crate::pull_drag::PullDrag>,
}

/// Bundle stored per running stream so `apply_backend_updates` /
/// `cancel_operations` can release locks and dispatch cancel against
/// the right plugin worker without re-querying the orchestrator.
/// `transition` is the viso animation preset to queue on each Pending
/// snapshot — resolved from the manifest catalog once at dispatch
/// time so per-poll handling stays orchestrator-free.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) struct ActiveStreamEntry {
    pub(crate) handle: foldit_runner::orchestrator::DispatchHandle,
    pub(crate) plugin_id: String,
    pub(crate) transition: foldit_runner::orchestrator::TransitionKind,
}
