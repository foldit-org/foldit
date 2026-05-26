pub(crate) mod action_router;
pub mod app;
pub(crate) mod document;
pub(crate) mod history;
pub(crate) mod plugin_driver;
pub mod puzzle;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod pull_drag;
pub(crate) mod render_projector;

pub use app::App;
#[cfg(not(target_arch = "wasm32"))]
pub use app::locate_plugins_root;
