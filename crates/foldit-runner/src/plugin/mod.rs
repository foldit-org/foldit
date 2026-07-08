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
/// - `kind = "native"` → [`native::NativePlugin::load`] against the path
///   [`resolve_native_binary`] picks under the local/prebuilt convention.
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
            let dylib = resolve_native_binary(plugin_dir, manifest)?;
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

/// Resolve the shared-library path for a native plugin under the
/// two-location convention.
///
/// A `[native].binary` value carrying a path separator is a literal
/// location relative to `plugin_dir` and is honored verbatim. A bare
/// filename is resolved against, in order:
///
/// 1. `<plugin_dir>/local/<name>` — opt-in local build output (gitignored),
///    shadowing the vendored binary when present.
/// 2. `<plugin_dir>/prebuilt/<host-triple>/<name>` — the committed
///    per-platform fallback.
///
/// # Errors
///
/// Returns an error naming both candidate paths and the host triple when
/// neither exists.
#[cfg(not(target_arch = "wasm32"))]
fn resolve_native_binary(
    plugin_dir: &Path,
    manifest: &PluginManifest,
) -> anyhow::Result<std::path::PathBuf> {
    let name = manifest.native_binary_name();
    resolve_native_binary_inner(
        plugin_dir,
        &manifest.id,
        &name,
        host_target_triple(),
        |p| p.exists(),
    )
}

/// The host target triple, matching the `prebuilt/<triple>/` directory names
/// we vendor per-platform binaries under.
///
/// `unknown` on an unsupported platform: the resolver then reports no
/// `prebuilt/unknown/` directory, which is the correct clear failure.
#[cfg(not(target_arch = "wasm32"))]
pub fn host_target_triple() -> &'static str {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "aarch64-apple-darwin"
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        "x86_64-apple-darwin"
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "x86_64-unknown-linux-gnu"
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        "aarch64-unknown-linux-gnu"
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        // Placeholder: msvc-vs-gnu is revisited when Windows binaries
        // are actually vendored.
        "x86_64-pc-windows-msvc"
    } else {
        "unknown"
    }
}

/// Path of a native plugin's opt-in local build output under the
/// `<plugin_dir>/local/<name>` convention (shadows the vendored binary).
#[cfg(not(target_arch = "wasm32"))]
pub fn local_binary_path(plugin_dir: &Path, name: &str) -> std::path::PathBuf {
    plugin_dir.join("local").join(name)
}

/// Path of a native plugin's committed per-platform binary under the
/// `<plugin_dir>/prebuilt/<triple>/<name>` convention.
#[cfg(not(target_arch = "wasm32"))]
pub fn prebuilt_binary_path(
    plugin_dir: &Path,
    triple: &str,
    name: &str,
) -> std::path::PathBuf {
    plugin_dir.join("prebuilt").join(triple).join(name)
}

/// Existence-check-injected core of [`resolve_native_binary`], kept pure so
/// the precedence logic is testable without touching the filesystem.
#[cfg(not(target_arch = "wasm32"))]
pub fn resolve_native_binary_inner(
    plugin_dir: &Path,
    plugin_id: &str,
    name: &str,
    triple: &str,
    exists: impl Fn(&Path) -> bool,
) -> anyhow::Result<std::path::PathBuf> {
    if name.contains('/') || name.contains('\\') {
        return Ok(plugin_dir.join(name));
    }
    let local = local_binary_path(plugin_dir, name);
    if exists(&local) {
        return Ok(local);
    }
    let prebuilt = prebuilt_binary_path(plugin_dir, triple, name);
    if exists(&prebuilt) {
        return Ok(prebuilt);
    }
    Err(anyhow::anyhow!(
        "no native binary for plugin `{plugin_id}`: neither {} nor {} exists \
         (host triple {triple}). Build it from source with \
         `cargo xtask setup-plugins {plugin_id}`, or add a prebuilt binary \
         for this platform.",
        local.display(),
        prebuilt.display(),
    ))
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

#[cfg(all(test, not(target_arch = "wasm32")))]
mod resolve_native_tests {
    use super::*;
    use std::collections::HashSet;
    use std::path::PathBuf;

    fn resolve(
        name: &str,
        triple: &str,
        existing: &[&str],
    ) -> anyhow::Result<PathBuf> {
        let plugin_dir = Path::new("/plugins/rosetta");
        let set: HashSet<PathBuf> =
            existing.iter().map(|p| plugin_dir.join(p)).collect();
        resolve_native_binary_inner(
            plugin_dir,
            "rosetta",
            name,
            triple,
            |p| set.contains(p),
        )
    }

    #[test]
    fn local_preferred_over_prebuilt() {
        let got = resolve(
            "librosetta_interactive.dylib",
            "aarch64-apple-darwin",
            &[
                "local/librosetta_interactive.dylib",
                "prebuilt/aarch64-apple-darwin/librosetta_interactive.dylib",
            ],
        )
        .unwrap();
        assert_eq!(
            got,
            Path::new("/plugins/rosetta/local/librosetta_interactive.dylib")
        );
    }

    #[test]
    fn prebuilt_used_when_only_prebuilt_exists() {
        let got = resolve(
            "librosetta_interactive.dylib",
            "aarch64-apple-darwin",
            &["prebuilt/aarch64-apple-darwin/librosetta_interactive.dylib"],
        )
        .unwrap();
        assert_eq!(
            got,
            Path::new(
                "/plugins/rosetta/prebuilt/aarch64-apple-darwin/\
                 librosetta_interactive.dylib"
            )
        );
    }

    #[test]
    fn error_when_neither_exists() {
        let err = resolve(
            "librosetta_interactive.dylib",
            "aarch64-apple-darwin",
            &[],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("local/librosetta_interactive.dylib"));
        assert!(err.contains(
            "prebuilt/aarch64-apple-darwin/librosetta_interactive.dylib"
        ));
        assert!(err.contains("aarch64-apple-darwin"));
    }

    #[test]
    fn literal_path_bypasses_convention() {
        // A separator-bearing name is a literal location under plugin_dir;
        // the local/prebuilt convention is skipped even with nothing on disk.
        let got = resolve("bin/rosetta-plugin", "aarch64-apple-darwin", &[])
            .unwrap();
        assert_eq!(got, Path::new("/plugins/rosetta/bin/rosetta-plugin"));
    }
}
