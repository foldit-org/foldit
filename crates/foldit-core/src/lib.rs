//! Host-agnostic Foldit application logic.
//!
//! This crate owns all game state and behavior and nothing window-system or
//! transport-specific. It compiles for both the desktop target and `wasm32`;
//! the desktop binary (`foldit-desktop`) and the web entry (`foldit-web`) each
//! wrap one [`App`] in their own shell.
//!
//! [`App`] holds the session (the authoritative document: history, selection,
//! focus, view options, previews, the optional puzzle), the plugin client, the
//! score coordinator, the overlay cache, and the three projectors that route
//! `SessionUpdate` changes to the render engine, the plugin workers, and the
//! GUI state. The host supplies resource access through
//! [`HostResources`] and receives per-frame outputs through [`HostEffects`];
//! outside structure loading the core makes no filesystem calls of its own.
//!
//! See the workspace book under `docs/` for the architecture walkthrough.

// Test code leans on unwrap/expect/panic as the idiomatic assertion
// shape; keep those lints to production paths only.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub(crate) mod app;
pub(crate) mod gui_projector;
pub(crate) mod history;
mod host_effects;
mod host_resources;
mod puzzle_load;
mod puzzle_toml;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod runner_client;
pub(crate) mod runner_projector;
pub(crate) mod scores;
pub(crate) mod session;
pub mod structure_io;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod viz;
pub use crate::structure_io as puzzle;
pub mod puzzle_setup;
pub(crate) mod render_projector;
pub(crate) mod wire_params;

pub use app::App;
pub use app::TailUpdate;
#[cfg(not(target_arch = "wasm32"))]
pub use app::{locate_plugin_ui_entrypoints, locate_plugins_root, strip_win32_extended_prefix};
pub use host_effects::HostEffects;
pub use host_resources::HostResources;
