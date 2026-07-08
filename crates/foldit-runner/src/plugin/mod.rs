//! Plugin abstraction and the manifest-driven native plugin loader.
//!
//! The `Plugin` trait, `AssemblyPayload`, and the C-ABI (`abi`) are owned
//! by `foldit-plugin-sdk` and re-exported here so internal
//! `crate::plugin::{Plugin, AssemblyPayload}` / `crate::plugin::abi::*`
//! paths resolve against the one source of truth.

#[cfg(not(target_arch = "wasm32"))]
pub use foldit_plugin_sdk::abi;
#[cfg(not(target_arch = "wasm32"))]
pub mod native;
#[cfg(not(target_arch = "wasm32"))]
pub mod python_host;

#[cfg(not(target_arch = "wasm32"))]
use std::collections::HashMap;
#[cfg(not(target_arch = "wasm32"))]
use std::path::Path;

#[cfg(not(target_arch = "wasm32"))]
use anyhow::Context;
pub use foldit_plugin_sdk::{AssemblyPayload, Plugin};

#[cfg(not(target_arch = "wasm32"))]
use crate::orchestrator::manifest::{PluginKind, PluginManifest};

// Manifest-driven plugin loader

/// Read `<plugin_dir>/plugin.toml`, parse the manifest, and load the
/// plugin via the kind-appropriate path:
///
/// - `kind = "python"` → dlopen `libfoldit_python_host.{dylib,so,dll}` sitting
///   next to the worker binary and call its Rust-ABI create entry with
///   `plugin_dir` (see [`python_host::load`]). The dylib re-reads the manifest,
///   initializes Python, loads the plugin module, and hands back a `Box<dyn
///   Plugin>` the worker calls directly.
/// - `kind = "native"` → [`native::NativePlugin::load`] against
///   `plugin_dir.join(manifest.native_binary_name())`.
/// - `kind = "wasm"` → not supported by the native worker host.
///
/// This is the entry point the worker binary calls after parsing its
/// 2-arg CLI (`<plugin_dir> <ipc_endpoint>`).
///
/// # Errors
///
/// Returns an error if the manifest can't be read or parsed, or if the
/// kind-specific load path fails.
#[cfg(not(target_arch = "wasm32"))]
pub fn load_plugin(plugin_dir: &Path) -> anyhow::Result<Box<dyn Plugin>> {
    let manifest_path = plugin_dir.join("plugin.toml");
    let toml_src =
        std::fs::read_to_string(&manifest_path).with_context(|| {
            format!("read manifest {}", manifest_path.display())
        })?;
    let manifest = PluginManifest::parse(&toml_src).map_err(|e| {
        anyhow::anyhow!("parse manifest {}: {}", manifest_path.display(), e)
    })?;
    load_plugin_from_manifest(plugin_dir, &manifest)
}

/// Variant of [`load_plugin`] that takes a pre-parsed manifest. Useful
/// when the caller has already parsed the manifest.
///
/// # Errors
///
/// Returns an error if the python-host dylib can't be located (Python
/// kind), the native dylib can't be loaded (Native kind), or the kind is
/// `Wasm` (unsupported by the native worker host).
#[cfg(not(target_arch = "wasm32"))]
pub fn load_plugin_from_manifest(
    plugin_dir: &Path,
    manifest: &PluginManifest,
) -> anyhow::Result<Box<dyn Plugin>> {
    match manifest.kind {
        PluginKind::Python => {
            let host_dylib = locate_python_host_dylib().ok_or_else(|| {
                anyhow::anyhow!(
                    "foldit-python-host dylib not found next to foldit-worker \
                     — build with `cargo build -p foldit-python-host` first"
                )
            })?;
            python_host::load(&host_dylib, plugin_dir).map_err(|e| {
                anyhow::anyhow!("foldit-python-host load failed: {e}")
            })
        }
        PluginKind::Native => {
            let dylib = plugin_dir.join(manifest.native_binary_name());
            // Pass plugin_dir through config_json so native plugins can
            // resolve sibling assets (rosetta walks up from here to find
            // assets/database/). Same key the python-host expects, so
            // there's a single config-json contract across kinds.
            let mut cfg = HashMap::new();
            let _ = cfg.insert(
                String::from("plugin_dir"),
                plugin_dir.to_string_lossy().into_owned(),
            );
            let config_json = native::NativePlugin::config_to_json(&cfg);
            let plugin =
                native::NativePlugin::load(&manifest.id, &dylib, &config_json)
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
            Ok(Box::new(plugin))
        }
        PluginKind::Wasm => Err(anyhow::anyhow!(
            "plugin {}: wasm kind is not supported by foldit-worker",
            manifest.id
        )),
    }
}

/// Locate `libfoldit_python_host.{dylib,so,dll}`.
///
/// Resolution order:
/// 1. `FOLDIT_PYTHON_HOST_DYLIB` if set and the path exists. The dylib is a
///    parent-workspace member and builds to the parent target, which is not
///    next to the crate-target `foldit-worker` the integration tests run; this
///    env override points straight at it (set by `just test`). Mirrors the
///    env-first pattern used elsewhere in the runner.
/// 2. Next to the running executable (the production bundle layout), plus one
///    parent dir up (cargo runs test binaries from `deps/`).
///
/// Returns `None` if not found — the caller turns that into a clear error
/// explaining how to build it.
#[cfg(not(target_arch = "wasm32"))]
fn locate_python_host_dylib() -> Option<std::path::PathBuf> {
    let filename = if cfg!(target_os = "macos") {
        "libfoldit_python_host.dylib"
    } else if cfg!(target_os = "windows") {
        "foldit_python_host.dll"
    } else {
        "libfoldit_python_host.so"
    };
    if let Some(p) = std::env::var_os("FOLDIT_PYTHON_HOST_DYLIB") {
        let path = std::path::PathBuf::from(p);
        if path.exists() {
            return Some(path);
        }
        log::warn!(
            "FOLDIT_PYTHON_HOST_DYLIB set to {} but it does not exist; \
             falling back to next-to-exe lookup",
            path.display()
        );
    }
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let direct = dir.join(filename);
    if direct.exists() {
        return Some(direct);
    }
    // cargo test runs binaries from target/<profile>/deps/; the cdylib
    // is built to target/<profile>/. Check the parent dir too.
    let parent = dir.parent()?.join(filename);
    if parent.exists() {
        return Some(parent);
    }
    None
}
