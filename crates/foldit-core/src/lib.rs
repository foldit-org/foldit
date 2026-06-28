// Test code leans on unwrap/expect/panic as the idiomatic assertion
// shape; keep those lints to production paths only.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub(crate) mod app;
pub(crate) mod session;
pub(crate) mod gui_projector;
pub(crate) mod history;
mod host_effects;
mod host_resources;
pub(crate) mod runner_client;
pub(crate) mod runner_projector;
pub(crate) mod scores;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod viz;
mod puzzle_toml;
mod puzzle_load;
pub mod structure_io;
pub use crate::structure_io as puzzle;
pub mod puzzle_setup;
pub(crate) mod render_projector;
pub(crate) mod wire_params;

pub use app::App;
pub use app::TailUpdate;
#[cfg(not(target_arch = "wasm32"))]
pub use app::{locate_plugin_ui_entrypoints, locate_plugins_root};
pub use host_effects::HostEffects;
pub use host_resources::HostResources;
