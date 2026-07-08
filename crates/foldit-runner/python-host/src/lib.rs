//! foldit-python-host — cdylib that hosts Python plugins.
//!
//! Loaded by `foldit-worker` via `foldit_runner::plugin::python_host::load`
//! (libloading) only when the plugin manifest declares `kind = "python"`.
//! Exports a Rust-ABI create entry, `foldit_python_host_create`, that
//! returns a `Box<dyn Plugin>` the worker calls directly — no C ABI
//! vtable, no per-call marshaling. A C-ABI version probe,
//! `foldit_python_host_abi_version`, lets the worker reject a stale or
//! mismatched dylib before trusting the create entry.
//!
//! `create(plugin_dir)` re-reads `<plugin_dir>/plugin.toml`, initializes
//! Python (via [`initialization`]), loads the Python plugin module per the
//! manifest (via [`pyplugin`]), and returns the boxed `Plugin`.
//! `plugin_dir` lets the Python plugin resolve its own assets (e.g.
//! weights under `<plugin_dir>/assets/weights/`).
//!
//! This Rust-ABI handoff is sound only because the worker and this dylib
//! are co-built from one workspace (same rustc, same dep versions, same
//! flags) and share the default System allocator; the worker holds the
//! dylib for the plugin's lifetime and drops the box before unloading.
//!
//! Pyo3 lives here, NOT in foldit-runner. The `foldit-worker` binary
//! has no pyo3 / libpython at link time; libpython only joins the
//! process when this dylib is dlopened.

// Several transitive deps (bitflags, core-foundation, heck, socket2,
// thiserror, wit-bindgen) appear at two major versions in the tree; the
// duplication originates in deps we do not control and is not resolvable
// from this crate.
#![allow(
    clippy::multiple_crate_versions,
    reason = "duplicate dep versions come from transitive deps, not \
              controllable here"
)]

pub mod initialization;
pub mod library;
pub mod pyplugin;
pub mod python_config;

use std::path::Path;

use foldit_runner::orchestrator::manifest::PluginManifest;
use foldit_runner::plugin::Plugin;

/// Manual ABI guard the worker checks before trusting the Rust-ABI create
/// entry. C-ABI so it is always callable even if the Rust ABI ever drifts.
#[no_mangle]
pub const extern "C" fn foldit_python_host_abi_version() -> u32 {
    foldit_runner::plugin::python_host::PYTHON_HOST_ABI_VERSION
}

/// Rust-ABI entry: build the Python plugin and hand back the boxed trait
/// object.
///
/// Sound only because the worker and this dylib are co-built (same
/// rustc/deps/flags) and share the default System allocator; the worker
/// holds the dylib for the plugin's lifetime and drops the box before
/// unloading.
#[no_mangle]
pub extern "Rust" fn foldit_python_host_create(
    plugin_dir: &Path,
) -> Option<Box<dyn Plugin>> {
    match create_impl(plugin_dir) {
        Ok(plugin) => Some(plugin),
        Err(e) => {
            log::error!("foldit-python-host: create failed: {e:#}");
            None
        }
    }
}

fn create_impl(plugin_dir: &Path) -> anyhow::Result<Box<dyn Plugin>> {
    // Re-read the manifest (the worker already parsed it, but passing
    // the full parsed manifest across the ABI would mean serializing it;
    // reading the file again is cheap).
    let manifest_path = plugin_dir.join("plugin.toml");
    let manifest_src =
        std::fs::read_to_string(&manifest_path).map_err(|e| {
            anyhow::anyhow!("read {} failed: {e}", manifest_path.display())
        })?;
    let manifest = PluginManifest::parse(&manifest_src).map_err(|e| {
        anyhow::anyhow!("parse {} failed: {e}", manifest_path.display())
    })?;

    let env_name = manifest.python_env();
    let python_config = python_config::PythonConfig::auto_detect(env_name)
        .map_err(|e| {
            anyhow::anyhow!("PythonConfig auto-detect for `{env_name}`: {e}")
        })?;
    initialization::initialize_python(&python_config)?;

    pyplugin::load_python_plugin(plugin_dir, &manifest)
        .map_err(|e| anyhow::anyhow!("load_python_plugin: {e}"))
}
