//! Worker process library.
//!
//! Internal library used by the foldit-worker binary. Loads ONE plugin
//! per worker process (selected at spawn time via the manifest at
//! `<plugin_dir>/plugin.toml`) and dispatches `proto::plugin` requests
//! received over IPC.
//!
//! Native plugins are dlopened directly via
//! [`crate::plugin::native::NativePlugin`] (a C ABI vtable). Python
//! plugins are dlopened via the `foldit-python-host` cdylib, whose
//! Rust-ABI `foldit_python_host_create` entry the worker dlsyms to get a
//! `Box<dyn Plugin>` it calls directly (see
//! [`crate::plugin::python_host`]). The worker binary itself has no pyo3
//! / libpython in its link graph.

pub mod runner;

pub use runner::main;
