//! Python-host plugin loader (libloading + Rust ABI).
//!
//! Unlike [`crate::plugin::native`] (which dlsyms a C ABI vtable and
//! marshals every call across it), this loader dlsyms a Rust-ABI entry
//! that returns a `Box<dyn Plugin>` and calls the [`Plugin`] trait
//! directly. The host (worker) holds the loaded library for the
//! plugin's lifetime; the boxed plugin is dropped before the library
//! is unloaded.
//!
//! This is sound only because the worker and the python-host dylib are
//! co-built from one workspace (same rustc, same dep versions, same
//! flags) and neither installs a custom global allocator (both use the
//! System allocator = libc malloc/free), so a `Box` allocated inside the
//! dylib frees correctly when dropped in the worker. The
//! [`PYTHON_HOST_ABI_VERSION`] probe guards against a stale or
//! mismatched dylib before the create entry is ever called.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use foldit_plugin_sdk::Result as PluginResult;
use libloading::{Library, Symbol};

use crate::error::{Result, RunnerError};
use crate::orchestrator::{DispatchContext, ParamValue, PollOutcome};
use crate::plugin::{AssemblyPayload, Plugin};
use crate::proto::plugin as proto;

/// Version of the Rust-ABI contract between the worker and the
/// python-host dylib.
///
/// Both bake in their own copy of this constant; the worker checks the
/// dylib's copy via the `foldit_python_host_abi_version` probe before
/// trusting its create entry, so a stale dylib mismatches loudly instead
/// of corrupting memory.
///
/// Bump this whenever the [`Plugin`] trait or the layout of any type that
/// crosses the `Box<dyn Plugin>` boundary changes: [`DispatchContext`],
/// [`ParamValue`], [`PollOutcome`], [`crate::orchestrator::ResidueRef`],
/// [`AssemblyPayload`], or any of the [`proto`] types those carry.
pub const PYTHON_HOST_ABI_VERSION: u32 = 1;

/// Signature of the dylib's `foldit_python_host_create` export: build the
/// hosted plugin from its directory, or `None` on failure.
type CreateFn = extern "Rust" fn(&Path) -> Option<Box<dyn Plugin>>;

/// Load the python-host dylib and build the Python plugin it hosts.
///
/// `dylib_path` is the co-built `libfoldit_python_host` artifact;
/// `plugin_dir` is the directory holding the Python plugin's
/// `plugin.toml` and assets, passed straight to the dylib's create entry.
///
/// # Errors
///
/// Returns an error if the dylib can't be opened, the version probe or
/// create symbol is missing, the dylib's ABI version doesn't match
/// [`PYTHON_HOST_ABI_VERSION`], or the create entry returns `None`.
pub fn load(dylib_path: &Path, plugin_dir: &Path) -> Result<Box<dyn Plugin>> {
    let library = unsafe {
        Library::new(dylib_path).map_err(|e| {
            RunnerError::Generic(format!(
                "failed to load python-host dylib from {}: {e}",
                dylib_path.display()
            ))
        })?
    };

    let dylib_version = unsafe {
        let probe: Symbol<unsafe extern "C" fn() -> u32> = library
            .get(b"foldit_python_host_abi_version\0")
            .map_err(|e| {
                RunnerError::Generic(format!(
                    "python-host dylib missing \
                     `foldit_python_host_abi_version` symbol: {e}"
                ))
            })?;
        probe()
    };
    if dylib_version != PYTHON_HOST_ABI_VERSION {
        return Err(RunnerError::Generic(format!(
            "python-host dylib ABI version mismatch: dylib reports \
             v{dylib_version}, worker expects v{PYTHON_HOST_ABI_VERSION}"
        )));
    }

    let plugin = unsafe {
        let create: Symbol<CreateFn> =
            library.get(b"foldit_python_host_create\0").map_err(|e| {
                RunnerError::Generic(format!(
                    "python-host dylib missing `foldit_python_host_create` \
                     symbol: {e}"
                ))
            })?;
        create(plugin_dir)
    }
    .ok_or_else(|| {
        RunnerError::Generic(
            "python-host create returned None (see worker log for the \
             detailed cause)"
                .into(),
        )
    })?;

    Ok(Box::new(WorkerPythonHost {
        plugin,
        _library: Arc::new(library),
    }))
}

/// In-process Python plugin: the boxed trait object built inside the
/// dylib, plus the loaded library held to keep the dylib's code mapped.
///
/// Field order is load-bearing: `plugin` is declared before `_library`
/// so Rust's struct drop order drops the plugin (whose vtable + code live
/// in the dylib) BEFORE the `Arc<Library>` unloads the dylib. Reversing
/// the fields would unmap the plugin's code before its destructor runs.
struct WorkerPythonHost {
    plugin: Box<dyn Plugin>,
    _library: Arc<Library>,
}

// SAFETY: WorkerPythonHost is Send because the inner `Box<dyn Plugin>` is
// Send (the trait requires it) and the `Arc<Library>` is held only to keep
// the dylib mapped; no plugin state is reached through it.
unsafe impl Send for WorkerPythonHost {}

impl Plugin for WorkerPythonHost {
    fn init(
        &self,
        assembly_bytes: &[u8],
        assets: &[proto::PuzzleAsset],
        params: &HashMap<String, ParamValue>,
    ) -> PluginResult<(u64, Vec<u8>)> {
        self.plugin.init(assembly_bytes, assets, params)
    }

    fn register(&self) -> PluginResult<proto::PluginRegistration> {
        self.plugin.register()
    }

    fn update_assembly(
        &self,
        session: u64,
        payload: AssemblyPayload<'_>,
        from_gen: u64,
        to_gen: u64,
    ) -> PluginResult<()> {
        self.plugin
            .update_assembly(session, payload, from_gen, to_gen)
    }

    fn drop_session(&self, session: u64) -> PluginResult<()> {
        self.plugin.drop_session(session)
    }

    fn invoke(
        &self,
        session: u64,
        op: &str,
        ctx: &DispatchContext,
        params: &HashMap<String, ParamValue>,
    ) -> PluginResult<Vec<u8>> {
        self.plugin.invoke(session, op, ctx, params)
    }

    fn start_stream(
        &self,
        session: u64,
        op: &str,
        ctx: &DispatchContext,
        params: &HashMap<String, ParamValue>,
        request_id: u64,
    ) -> PluginResult<()> {
        self.plugin
            .start_stream(session, op, ctx, params, request_id)
    }

    fn poll_stream(&self, request_id: u64) -> PluginResult<PollOutcome> {
        self.plugin.poll_stream(request_id)
    }

    fn update_stream(
        &self,
        request_id: u64,
        params: &HashMap<String, ParamValue>,
    ) -> PluginResult<()> {
        self.plugin.update_stream(request_id, params)
    }

    fn cancel_stream(&self, request_id: u64) -> PluginResult<()> {
        self.plugin.cancel_stream(request_id)
    }

    fn query(
        &self,
        session: u64,
        query: &str,
        ctx: &DispatchContext,
        params: &HashMap<String, ParamValue>,
        assembly: &[u8],
    ) -> PluginResult<Vec<u8>> {
        self.plugin.query(session, query, ctx, params, assembly)
    }
}
