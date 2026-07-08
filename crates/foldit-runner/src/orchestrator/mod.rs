//! Unified backend orchestrator for Foldit.
//!
//! Single entry point for plugin protocol dispatch. Owns the entity-lock
//! table, the plugin worker pool, and per-frame update pumping. Rosetta
//! itself is a plugin under `plugins/rosetta/` — no in-process executor
//! lives here; dispatch flows through the unified plugin path like
//! every other plugin.
//!
//! Worker-process management (`cleanup`, `spawn`, `client`) lives here
//! too — the orchestrator owns its workers directly.

#[cfg(not(target_arch = "wasm32"))]
pub mod assets;
#[cfg(not(target_arch = "wasm32"))]
pub mod client;
mod core;
mod lock_check;
#[cfg(not(target_arch = "wasm32"))]
pub mod manifest;
#[cfg(not(target_arch = "wasm32"))]
pub mod ops;
#[cfg(not(target_arch = "wasm32"))]
pub mod pump;
#[cfg(not(target_arch = "wasm32"))]
pub mod queries;
#[cfg(not(target_arch = "wasm32"))]
pub mod scores;
mod stream_update;
mod types;
#[cfg(not(target_arch = "wasm32"))]
pub mod weights;

#[cfg(not(target_arch = "wasm32"))]
pub mod cleanup;
#[cfg(not(target_arch = "wasm32"))]
pub mod spawn;

pub use core::Orchestrator;
#[cfg(not(target_arch = "wasm32"))]
pub use core::{PanelInfo, PluginGroupEntry, SettingsTabInfo};

#[cfg(not(target_arch = "wasm32"))]
pub use cleanup::{
    install_cleanup_signal_handlers, kill_all_worker_groups,
    register_worker_pgid, unregister_worker_pgid,
};
#[cfg(not(target_arch = "wasm32"))]
pub use client::{BroadcastPayload, PluginClient, SocketPluginClient};
pub use lock_check::{DispatchError, DispatchHandle};
#[cfg(not(target_arch = "wasm32"))]
pub use ops::OpDispatchError;
#[cfg(not(target_arch = "wasm32"))]
pub use spawn::{spawn_plugin_worker, PluginId, PluginSpawnDescriptor};
#[cfg(not(target_arch = "wasm32"))]
pub use types::CatalogEntry;
pub use types::{
    CachedPluginOp, CachedPluginQuery, Constraint, ConstraintAtom,
    ConstraintFunc, ConstraintKind, DispatchContext, EntityLock,
    EntityLockTable, InitPayload, LockTargets, OpKind, OpLockMeta,
    ParamConstraint, ParamSpec, ParamType, ParamValue, PluginRegistry,
    PluginUpdate, PollOutcome, PuzzleAsset, ResidueRef,
};
