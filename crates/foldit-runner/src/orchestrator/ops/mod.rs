//! Orchestrator-side op dispatch surface.
//!
//! Public entry points for plugin lifecycle (`register_plugin`,
//! `ensure_plugin_registered`, `unregister_plugin`) and op dispatch
//! (`dispatch_invoke`, `dispatch_query`, `dispatch_start_stream`,
//! `dispatch_update_stream`, `dispatch_cancel_stream`). The per-plugin
//! pump threads that drive the IPC sockets live in [`super::pump`].
//!
//! Split across submodules: `registration` (lifecycle + catalog cache),
//! `dispatch` (op/query/stream dispatch + STALE_GEN recovery),
//! `broadcast` (update drain + peer fan-out).

use crate::error::RunnerError;
use crate::orchestrator::lock_check::DispatchError;

mod broadcast;
mod dispatch;
mod registration;

/// Errors from `Orchestrator::dispatch_*` calls. Wraps both the
/// lock-check rejection (`DispatchError`) and the wire-level error
/// (`RunnerError`) the worker may report.
#[derive(Debug)]
pub enum OpDispatchError {
    /// The op id isn't registered with any plugin.
    UnknownOp(String),
    /// The query id isn't registered with any plugin.
    UnknownQuery(String),
    /// The owning plugin has no active session (Init failed or hasn't run).
    NoSession(String),
    /// The owning plugin's worker has crashed or been terminated.
    WorkerGone(String),
    /// The lock check refused dispatch (entity busy, etc.).
    LockRefused(DispatchError),
    /// The plugin returned an op-level or transport-level error.
    Plugin(RunnerError),
}

impl std::fmt::Display for OpDispatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownOp(id) => write!(f, "unknown op id: {id}"),
            Self::UnknownQuery(id) => write!(f, "unknown query id: {id}"),
            Self::NoSession(plugin) => {
                write!(f, "plugin {plugin} has no active session")
            }
            Self::WorkerGone(plugin) => {
                write!(f, "plugin {plugin} worker is gone")
            }
            Self::LockRefused(e) => write!(f, "dispatch refused: {e:?}"),
            Self::Plugin(e) => write!(f, "plugin error: {e}"),
        }
    }
}

impl std::error::Error for OpDispatchError {}

impl From<RunnerError> for OpDispatchError {
    fn from(e: RunnerError) -> Self {
        Self::Plugin(e)
    }
}
