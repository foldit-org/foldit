//! foldit-runner: unified plugin runner for Foldit.
//!
//! Hosts plugins behind the protocol defined in `proto::plugin`.
//! Plugins live out-of-process under `foldit-worker`; the orchestrator
//! owns the canonical Assembly and dispatches `Invoke` / `StartStream`
//! / `Query` to the owning worker.
//!
//! This crate has no pyo3 / libpython in its link graph. Python plugin
//! hosting lives in the sibling `foldit-python-host` cdylib, dlopened
//! by `foldit-worker` only when the plugin manifest declares
//! `kind = "python"`.
//!
//! On `wasm32` the crate compiles to a slice — IPC + worker
//! infrastructure is `cfg`-gated out.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_possible_wrap,
    )
)]

pub mod error;

// Plugin abstraction — the `Plugin` trait + native plugin loader.
pub mod plugin;

// Unified orchestrator (plugin workers + locks).
pub mod orchestrator;

// Cross-process IPC (iceoryx2 + interprocess sockets). Native-only.
#[cfg(not(target_arch = "wasm32"))]
pub mod ipc;

// Runtime utilities (worker binary search, IPC socket naming). Native-only.
#[cfg(not(target_arch = "wasm32"))]
pub mod runtime;

// Worker dispatch loop. Used by the foldit-worker binary.
#[cfg(not(target_arch = "wasm32"))]
pub mod worker;

/// Plugin protocol bindings. `plugin` is the unified plugin protocol
/// surface, owned and compiled by `foldit-plugin-sdk`. Re-exported here so
/// internal `crate::proto::plugin::*` paths resolve against the one source
/// of truth.
pub use foldit_plugin_sdk::proto;
pub use orchestrator::Orchestrator;
#[cfg(not(target_arch = "wasm32"))]
pub use orchestrator::{
    install_cleanup_signal_handlers, register_worker_pgid,
    unregister_worker_pgid,
};

#[cfg(test)]
mod tests {
    #[test]
    fn test_version() {
        assert_eq!(env!("CARGO_PKG_VERSION"), "0.1.0");
    }
}
