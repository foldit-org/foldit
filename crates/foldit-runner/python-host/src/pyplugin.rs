//! Python plugin host (`PyO3` wrapper + module loader).
//!
//! The [`Plugin`] trait it implements lives at
//! [`foldit_runner::plugin::Plugin`]. `PyPlugin` converts Rust-native
//! trait args to/from the Python `PluginInterface` ABC at the pyo3
//! boundary. The worker reaches it by dlsyming this dylib's Rust-ABI
//! `foldit_python_host_create` entry, which hands back a boxed `PyPlugin`
//! the worker calls directly (no C vtable).
//!
//! This module is python-feature-gated and only compiles inside the
//! `foldit-worker` subprocess where Python is initialized.

use std::collections::HashMap;
use std::path::Path;

use foldit_plugin_sdk::{PluginError, Result as PluginResult};
use foldit_runner::error::{Result, RunnerError};
use foldit_runner::orchestrator::manifest::PluginManifest;
use foldit_runner::orchestrator::{DispatchContext, ParamValue, PollOutcome};
use foldit_runner::plugin::{AssemblyPayload, Plugin};
use foldit_runner::proto::plugin as proto;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyTuple};

// =============================================================================
// PyPlugin — pyo3 wrapper around a Python `PluginInterface` instance
// =============================================================================

/// Wrapper around a Python `PluginInterface` instance that implements the
/// Rust [`Plugin`] trait. All Rust ↔ Python conversions happen here.
pub struct PyPlugin {
    instance: Py<PyAny>,
    /// Cached reference to the `foldit_plugin_sdk` package — used for
    /// constructing the bound `DispatchContext` / `ResidueRef` types on each
    /// dispatch without re-importing.
    sdk_module: Py<PyAny>,
}

impl PyPlugin {
    /// Wrap a Python plugin instance. Caller is responsible for verifying
    /// the instance subclasses `PluginInterface` (see [`load_python_plugin`]).
    ///
    /// # Errors
    ///
    /// Returns `Err` if the `foldit_plugin_sdk` module cannot be imported.
    pub fn new(instance: Py<PyAny>) -> Result<Self> {
        let sdk_module = Python::attach(|py| {
            py.import("foldit_plugin_sdk")
                .map(|m| m.unbind().into_any())
                .map_err(|e: pyo3::PyErr| RunnerError::Generic(e.to_string()))
        })?;
        Ok(Self {
            instance,
            sdk_module,
        })
    }
}

impl Plugin for PyPlugin {
    fn init(
        &self,
        assembly_bytes: &[u8],
        _assets: &[proto::PuzzleAsset],
        _params: &HashMap<String, ParamValue>,
    ) -> PluginResult<(u64, Vec<u8>)> {
        // Python plugins do not perform post-Init normalization (none of
        // them rebuild atoms the way Rosetta does), so the second tuple
        // element is always empty. The Python ABC's `init` keeps its
        // existing `-> int` contract; only the Rust wrapper widens. The
        // generic `params` channel and puzzle `assets` are unused by
        // Python plugins.
        Python::attach(|py| {
            let inst = self.instance.bind(py);
            let py_bytes = PyBytes::new(py, assembly_bytes);
            let session = inst
                .call_method1("init", (py_bytes,))
                .map_err(|e: pyo3::PyErr| PluginError::Other(e.to_string()))?
                .extract::<u64>()
                .map_err(|e: pyo3::PyErr| PluginError::Other(e.to_string()))?;
            Ok((session, Vec::new()))
        })
    }

    fn register(&self) -> PluginResult<proto::PluginRegistration> {
        Python::attach(|py| {
            let inst = self.instance.bind(py);
            let py_reg = inst
                .call_method0("register")
                .map_err(|e: pyo3::PyErr| PluginError::Other(e.to_string()))?;
            // `register()` returns a plugin_pb2.PluginRegistration. Round-trip
            // via SerializeToString → prost decode.
            let bytes: Vec<u8> = py_reg
                .call_method0("SerializeToString")
                .map_err(|e: pyo3::PyErr| PluginError::Other(e.to_string()))?
                .extract()
                .map_err(|e: pyo3::PyErr| PluginError::Other(e.to_string()))?;
            <proto::PluginRegistration as prost::Message>::decode(&bytes[..])
                .map_err(PluginError::Decode)
        })
    }

    fn update_assembly(
        &self,
        session: u64,
        payload: AssemblyPayload<'_>,
        from_gen: u64,
        to_gen: u64,
    ) -> PluginResult<()> {
        // Payload kind crosses to Python as a small integer: 0=Full,
        // 1=Delta — matches `FolditPluginAssemblyPayloadKind`. Python
        // ABC handlers branch on this tag.
        let kind: u8 = match payload {
            AssemblyPayload::Full(_) => 0,
            AssemblyPayload::Delta(_) => 1,
        };
        let bytes = payload.bytes();
        Python::attach(|py| {
            let inst = self.instance.bind(py);
            let py_bytes = PyBytes::new(py, bytes);
            inst.call_method1(
                "update_assembly",
                (session, kind, py_bytes, from_gen, to_gen),
            )
            .map_err(|e: pyo3::PyErr| PluginError::Other(e.to_string()))?;
            Ok(())
        })
    }

    fn drop_session(&self, session: u64) -> PluginResult<()> {
        Python::attach(|py| {
            let inst = self.instance.bind(py);
            inst.call_method1("drop", (session,))
                .map_err(|e: pyo3::PyErr| PluginError::Other(e.to_string()))?;
            Ok(())
        })
    }

    fn invoke(
        &self,
        session: u64,
        op: &str,
        ctx: &DispatchContext,
        params: &HashMap<String, ParamValue>,
    ) -> PluginResult<Vec<u8>> {
        Python::attach(|py| {
            let inst = self.instance.bind(py);
            let py_ctx = dispatch_context_to_py(py, &self.sdk_module, ctx)?;
            let py_params = params_to_py(py, params)?;
            inst.call_method1("invoke", (session, op, py_ctx, py_params))
                .map_err(|e: pyo3::PyErr| PluginError::Other(e.to_string()))?
                .extract::<Vec<u8>>()
                .map_err(|e: pyo3::PyErr| PluginError::Other(e.to_string()))
        })
    }

    fn start_stream(
        &self,
        session: u64,
        op: &str,
        ctx: &DispatchContext,
        params: &HashMap<String, ParamValue>,
        request_id: u64,
    ) -> PluginResult<()> {
        Python::attach(|py| {
            let inst = self.instance.bind(py);
            let py_ctx = dispatch_context_to_py(py, &self.sdk_module, ctx)?;
            let py_params = params_to_py(py, params)?;
            inst.call_method1(
                "start_stream",
                (session, op, py_ctx, py_params, request_id),
            )
            .map_err(|e: pyo3::PyErr| PluginError::Other(e.to_string()))?;
            Ok(())
        })
    }

    fn poll_stream(&self, request_id: u64) -> PluginResult<PollOutcome> {
        Python::attach(|py| {
            let inst = self.instance.bind(py);
            let py_result = inst
                .call_method1("poll_stream", (request_id,))
                .map_err(|e: pyo3::PyErr| PluginError::Other(e.to_string()))?;
            poll_outcome_from_py(&py_result)
        })
    }

    fn update_stream(
        &self,
        request_id: u64,
        params: &HashMap<String, ParamValue>,
    ) -> PluginResult<()> {
        Python::attach(|py| {
            let inst = self.instance.bind(py);
            let py_params = params_to_py(py, params)?;
            inst.call_method1("update_stream", (request_id, py_params))
                .map_err(|e: pyo3::PyErr| PluginError::Other(e.to_string()))?;
            Ok(())
        })
    }

    fn cancel_stream(&self, request_id: u64) -> PluginResult<()> {
        Python::attach(|py| {
            let inst = self.instance.bind(py);
            inst.call_method1("cancel_stream", (request_id,))
                .map_err(|e: pyo3::PyErr| PluginError::Other(e.to_string()))?;
            Ok(())
        })
    }

    // The host-supplied `assembly` (composition bytes for an assembly-arg
    // query) is not forwarded to Python: the Python `query` interface does
    // not yet carry it, and the only assembly-arg scorer is native Rosetta.
    // Python score-query providers score their live session pose.
    fn query(
        &self,
        session: u64,
        query: &str,
        ctx: &DispatchContext,
        params: &HashMap<String, ParamValue>,
        _assembly: &[u8],
    ) -> PluginResult<Vec<u8>> {
        Python::attach(|py| {
            let inst = self.instance.bind(py);
            let py_ctx = dispatch_context_to_py(py, &self.sdk_module, ctx)?;
            let py_params = params_to_py(py, params)?;
            inst.call_method1("query", (session, query, py_ctx, py_params))
                .map_err(|e: pyo3::PyErr| PluginError::Other(e.to_string()))?
                .extract::<Vec<u8>>()
                .map_err(|e: pyo3::PyErr| PluginError::Other(e.to_string()))
        })
    }
}

// =============================================================================
// Plugin loader
// =============================================================================

/// Import a Python plugin module and wrap its `PluginInterface` subclass.
///
/// Imports the entry module per `manifest`, finds its `PluginInterface`
/// subclass via `foldit_plugin_sdk.find_plugin_class`, instantiates it
/// with a config dict carrying `plugin_dir` (per protocol "Plugin
/// self-configuration": the plugin resolves its own assets, e.g. weights
/// at `<plugin_dir>/assets/weights/`, from this), and wraps the result in
/// a [`PyPlugin`].
///
/// `plugin_dir` is the plugin's directory. It is forwarded to the plugin
/// in `config["plugin_dir"]` (the same key native plugins receive via
/// `config_json`, so the orchestrator is the single source of truth for
/// where a plugin lives across kinds). `manifest` supplies the importable
/// Python entry module via `manifest.python_entry()`.
///
/// # Errors
///
/// Returns `Err` if the entry module or `foldit_plugin_sdk` cannot be
/// imported, no `PluginInterface` subclass is found, or the plugin class
/// cannot be instantiated.
pub fn load_python_plugin(
    plugin_dir: &Path,
    manifest: &PluginManifest,
) -> Result<Box<dyn Plugin>> {
    let module_name = manifest.python_entry().to_owned();
    log::info!(
        "loading Python plugin {} (entry module `{}`, dir {})",
        manifest.id,
        module_name,
        plugin_dir.display()
    );
    let py_plugin = Python::attach(|py| -> Result<PyPlugin> {
        let module = py
            .import(module_name.as_str())
            .map_err(|e: pyo3::PyErr| RunnerError::Generic(e.to_string()))?;
        let sdk_module = py
            .import("foldit_plugin_sdk")
            .map_err(|e: pyo3::PyErr| RunnerError::Generic(e.to_string()))?;
        let plugin_cls = sdk_module
            .call_method1("find_plugin_class", (module,))
            .map_err(|e: pyo3::PyErr| RunnerError::Generic(e.to_string()))?;
        let py_config = PyDict::new(py);
        let plugin_dir_str = plugin_dir.to_string_lossy();
        py_config
            .set_item("plugin_dir", plugin_dir_str.as_ref())
            .map_err(|e: pyo3::PyErr| RunnerError::Generic(e.to_string()))?;
        let instance = plugin_cls
            .call1((py_config,))
            .map_err(|e: pyo3::PyErr| RunnerError::Generic(e.to_string()))?
            .unbind();
        PyPlugin::new(instance)
    })?;
    Ok(Box::new(py_plugin))
}

// =============================================================================
// Conversion helpers
// =============================================================================

fn dispatch_context_to_py<'py>(
    py: Python<'py>,
    sdk_module: &Py<PyAny>,
    ctx: &DispatchContext,
) -> PluginResult<Bound<'py, PyAny>> {
    let sdk = sdk_module.bind(py);
    let dispatch_context_cls = sdk
        .getattr("DispatchContext")
        .map_err(|e: pyo3::PyErr| PluginError::Other(e.to_string()))?;
    let residue_ref_cls = sdk
        .getattr("ResidueRef")
        .map_err(|e: pyo3::PyErr| PluginError::Other(e.to_string()))?;

    let build_refs = |refs: &[foldit_runner::orchestrator::ResidueRef]| -> PluginResult<Bound<'py, PyTuple>> {
        let mut items: Vec<Bound<'py, PyAny>> = Vec::with_capacity(refs.len());
        for r in refs {
            let item = residue_ref_cls
                .call1((r.entity_id.raw(), r.residue_index))
                .map_err(|e: pyo3::PyErr| PluginError::Other(e.to_string()))?;
            items.push(item);
        }
        PyTuple::new(py, items).map_err(|e: pyo3::PyErr| PluginError::Other(e.to_string()))
    };

    let selection_tuple = build_refs(&ctx.selection)?;
    let designable_tuple = build_refs(&ctx.designable)?;

    let kwargs = PyDict::new(py);
    if let Some(eid) = ctx.focused_entity_id {
        kwargs
            .set_item("focused_entity_id", eid.raw())
            .map_err(|e: pyo3::PyErr| PluginError::Other(e.to_string()))?;
    }
    kwargs
        .set_item("selection", selection_tuple)
        .map_err(|e: pyo3::PyErr| PluginError::Other(e.to_string()))?;
    kwargs
        .set_item("designable", designable_tuple)
        .map_err(|e: pyo3::PyErr| PluginError::Other(e.to_string()))?;

    dispatch_context_cls
        .call((), Some(&kwargs))
        .map_err(|e: pyo3::PyErr| PluginError::Other(e.to_string()))
}

fn params_to_py<'py>(
    py: Python<'py>,
    params: &HashMap<String, ParamValue>,
) -> PluginResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    for (k, v) in params {
        match v {
            ParamValue::Int(i) => dict
                .set_item(k, *i)
                .map_err(|e: pyo3::PyErr| PluginError::Other(e.to_string()))?,
            ParamValue::Float(f) => dict
                .set_item(k, *f)
                .map_err(|e: pyo3::PyErr| PluginError::Other(e.to_string()))?,
            ParamValue::Bool(b) => dict
                .set_item(k, *b)
                .map_err(|e: pyo3::PyErr| PluginError::Other(e.to_string()))?,
            ParamValue::String(s) => dict
                .set_item(k, s)
                .map_err(|e: pyo3::PyErr| PluginError::Other(e.to_string()))?,
            ParamValue::Vec3([x, y, z]) => {
                let tup = PyTuple::new(py, [*x, *y, *z]).map_err(
                    |e: pyo3::PyErr| PluginError::Other(e.to_string()),
                )?;
                dict.set_item(k, tup).map_err(|e: pyo3::PyErr| {
                    PluginError::Other(e.to_string())
                })?;
            }
        }
    }
    Ok(dict)
}

/// Decode the bound `foldit_plugin_sdk.PollOutcome` a plugin returns from
/// `poll_stream` into the native [`PollOutcome`]. The variant is read from
/// the `kind` discriminant string and the payload from the per-field getters
/// (`assembly` / `progress` / `stage` / `error_*`); the host never extracts
/// the Rust type out of the handle.
//
// Score stays `None` for every variant: the Python path has never carried a
// score channel, and wiring it through is deferred to a later step.
fn poll_outcome_from_py(
    py_result: &Bound<'_, PyAny>,
) -> PluginResult<PollOutcome> {
    // Read a getattr field and extract it into the call-site type. A macro
    // (not a generic fn) so each `.extract()` resolves against a concrete
    // target, keeping pyo3's `FromPyObject::Error` pinned to `PyErr`.
    macro_rules! field {
        ($attr:literal) => {
            py_result
                .getattr($attr)
                .map_err(|e: pyo3::PyErr| PluginError::Other(e.to_string()))?
                .extract()
                .map_err(|e: pyo3::PyErr| PluginError::Other(e.to_string()))?
        };
    }

    let kind: String = field!("kind");
    match kind.as_str() {
        "pending" => Ok(PollOutcome::Pending {
            latest_assembly: field!("assembly"),
            progress: field!("progress"),
            stage: field!("stage"),
            score: None,
        }),
        "checkpoint" => Ok(PollOutcome::Checkpoint {
            latest_assembly: field!("assembly"),
            progress: field!("progress"),
            stage: field!("stage"),
            score: None,
        }),
        "cancelled" => Ok(PollOutcome::Cancelled {
            assembly: field!("assembly"),
            score: None,
        }),
        "final" => Ok(PollOutcome::Final {
            assembly: field!("assembly"),
            score: None,
        }),
        "error" => Ok(PollOutcome::Error {
            code: field!("error_code"),
            message: field!("error_message"),
            details: field!("error_details"),
        }),
        other => Err(PluginError::Other(format!(
            "poll_stream returned PollOutcome with unknown kind {other:?}"
        ))),
    }
}
