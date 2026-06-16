// Test code leans on unwrap/expect/panic as the idiomatic assertion
// shape; keep those lints to production paths only.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod app;
pub(crate) mod session;
pub(crate) mod gui_projector;
pub(crate) mod history;
mod host_resources;
pub(crate) mod runner_client;
pub(crate) mod runner_projector;
pub(crate) mod scores;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod viz;
pub mod puzzle;
pub mod puzzle_setup;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod pull_drag;
pub(crate) mod render_projector;
pub(crate) mod wire_params;

pub use app::App;
#[cfg(not(target_arch = "wasm32"))]
pub use app::locate_plugins_root;
pub use host_resources::HostResources;
