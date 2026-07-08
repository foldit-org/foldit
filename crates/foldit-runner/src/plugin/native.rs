//! Native plugin loader (libloading + C ABI vtable).
//!
//! Loads a shared library (`lib{id}.{dylib,so,dll}`) that exports the
//! `foldit_plugin_vtable` C ABI symbol, validates the ABI version, and
//! wraps the resulting vtable in a [`NativePlugin`] that implements the
//! [`Plugin`] trait. The host (orchestrator) holds the loaded library
//! for the plugin's lifetime; dropping `NativePlugin` calls
//! `vtable.destroy(handle)` and unloads the library.
//!
//! See [`crate::plugin::abi`] for the C ABI surface this loader speaks.

use std::collections::HashMap;
use std::ffi::CString;
use std::path::Path;
use std::sync::Arc;

use foldit_plugin_sdk::{PluginError, Result as PluginResult};
use libloading::{Library, Symbol};

use crate::error::{Result, RunnerError};
use crate::orchestrator::{
    DispatchContext, ParamValue, PollOutcome, ResidueRef,
};
use crate::plugin::abi::{
    FolditPluginAssemblyPayloadKind, FolditPluginAsset, FolditPluginBuffer,
    FolditPluginDispatchContext, FolditPluginError, FolditPluginHandle,
    FolditPluginParamEntry, FolditPluginParamTag, FolditPluginParamValue,
    FolditPluginResidueRef, FolditPluginStatus, FolditPluginVec3,
    FolditPluginVtable, FolditPluginVtableFn, FOLDIT_PLUGIN_ABI_VERSION,
    VTABLE_SYMBOL,
};
use crate::plugin::{AssemblyPayload, Plugin};
use crate::proto::plugin as proto;

/// In-process native plugin. Wraps a loaded shared library + the C ABI
/// vtable retrieved from it.
pub struct NativePlugin {
    /// Held to keep the library loaded for the plugin's lifetime.
    /// Dropping this unloads the dylib (after `destroy(handle)` runs).
    _library: Arc<Library>,
    /// Pointer to the plugin's vtable; valid as long as `_library` is
    /// loaded. Stored as raw pointer because the vtable lives in the
    /// dylib's memory.
    vtable: *const FolditPluginVtable,
    handle: FolditPluginHandle,
    plugin_id: String,
}

// SAFETY: NativePlugin is Send because the underlying handle is opaque
// to Rust and the vtable contract guarantees calls from any thread are
// safe (per-instance state is not assumed to be thread-pinned). The
// orchestrator serializes calls into a single instance, so no Sync
// required.
unsafe impl Send for NativePlugin {}

impl NativePlugin {
    /// Load a native plugin dylib and instantiate it with the given
    /// JSON-encoded config. The plugin id is recorded for diagnostics.
    ///
    /// # Errors
    ///
    /// Returns an error if the dylib can't be opened, the vtable symbol
    /// is missing, the ABI version doesn't match
    /// [`crate::plugin::abi::FOLDIT_PLUGIN_ABI_VERSION`], or `create`
    /// returns null.
    pub fn load(
        plugin_id: impl Into<String>,
        dylib_path: &Path,
        config_json: &str,
    ) -> Result<Self> {
        let plugin_id = plugin_id.into();
        let library = unsafe {
            Library::new(dylib_path).map_err(|e| {
                RunnerError::Generic(format!(
                    "failed to load native plugin {} from {}: {e}",
                    plugin_id,
                    dylib_path.display()
                ))
            })?
        };

        let vtable_ptr: *const FolditPluginVtable = unsafe {
            let sym: Symbol<FolditPluginVtableFn> =
                library.get(VTABLE_SYMBOL).map_err(|e| {
                    RunnerError::Generic(format!(
                        "native plugin {plugin_id} missing \
                         `foldit_plugin_vtable` symbol: {e}"
                    ))
                })?;
            (*sym)()
        };
        if vtable_ptr.is_null() {
            return Err(RunnerError::Generic(format!(
                "native plugin {plugin_id} returned null vtable"
            )));
        }
        let abi_version = unsafe { (*vtable_ptr).abi_version };
        if abi_version != FOLDIT_PLUGIN_ABI_VERSION {
            return Err(RunnerError::Generic(format!(
                "native plugin {plugin_id} ABI version mismatch: plugin \
                 reports v{abi_version}, host expects \
                 v{FOLDIT_PLUGIN_ABI_VERSION}"
            )));
        }

        let config_bytes = config_json.as_bytes();
        let handle = unsafe {
            ((*vtable_ptr).create)(
                config_bytes.as_ptr().cast(),
                config_bytes.len(),
            )
        };
        if handle.is_null() {
            return Err(RunnerError::Generic(format!(
                "native plugin {plugin_id} create() returned null"
            )));
        }

        Ok(Self {
            _library: Arc::new(library),
            vtable: vtable_ptr,
            handle,
            plugin_id,
        })
    }

    /// Stable plugin id this instance was created with.
    #[must_use]
    pub fn plugin_id(&self) -> &str {
        &self.plugin_id
    }

    /// Convenience: serialize a config map to JSON for the plugin's
    /// `create` entry. Used by the loader; plugin authors don't see this.
    #[must_use]
    pub fn config_to_json(config: &HashMap<String, String>) -> String {
        // Hand-roll a tiny JSON encoder to avoid pulling serde_json into
        // the universal dep set just for this. Keys + values are strings.
        let mut s = String::from("{");
        let mut first = true;
        for (k, v) in config {
            if !first {
                s.push(',');
            }
            first = false;
            s.push('"');
            push_json_escaped(&mut s, k);
            s.push_str("\":\"");
            push_json_escaped(&mut s, v);
            s.push('"');
        }
        s.push('}');
        s
    }

    fn vtable(&self) -> &FolditPluginVtable {
        // SAFETY: vtable lives as long as _library; we hold the Arc.
        unsafe { &*self.vtable }
    }
}

impl Drop for NativePlugin {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe {
                (self.vtable().destroy)(self.handle);
            }
            self.handle = std::ptr::null_mut();
        }
        // _library Arc drops here, unloading the dylib.
    }
}

fn push_json_escaped(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                use std::fmt::Write;
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
}

// Plugin impl — converts Rust args ↔ C ABI structs around vtable calls.
impl Plugin for NativePlugin {
    fn init(
        &self,
        assembly_bytes: &[u8],
        assets: &[proto::PuzzleAsset],
        params: &HashMap<String, ParamValue>,
    ) -> PluginResult<(u64, Vec<u8>)> {
        let c_assets = assets_to_c(assets);
        let (c_params, _string_storage) = params_to_c(params);
        let mut session: u64 = 0;
        let mut initial_buf = FolditPluginBuffer::empty();
        let mut err = FolditPluginError::empty();
        let status = unsafe {
            (self.vtable().init)(
                self.handle,
                assembly_bytes.as_ptr(),
                assembly_bytes.len(),
                c_assets.as_ptr(),
                c_assets.len(),
                c_params.as_ptr(),
                c_params.len(),
                &raw mut session,
                &raw mut initial_buf,
                &raw mut err,
            )
        };
        check_status(self.vtable(), status, &mut err, "init")?;
        let initial_assembly = take_buffer(self.vtable(), &mut initial_buf);
        Ok((session, initial_assembly))
    }

    fn register(&self) -> PluginResult<proto::PluginRegistration> {
        let mut buf = FolditPluginBuffer::empty();
        let mut err = FolditPluginError::empty();
        let status = unsafe {
            (self.vtable().register)(self.handle, &raw mut buf, &raw mut err)
        };
        check_status(self.vtable(), status, &mut err, "register")?;
        let bytes = take_buffer(self.vtable(), &mut buf);
        <proto::PluginRegistration as prost::Message>::decode(&bytes[..])
            .map_err(PluginError::Decode)
    }

    fn update_assembly(
        &self,
        session: u64,
        payload: AssemblyPayload<'_>,
        from_gen: u64,
        to_gen: u64,
    ) -> PluginResult<()> {
        let (kind, bytes) = match payload {
            AssemblyPayload::Full(b) => {
                (FolditPluginAssemblyPayloadKind::Full, b)
            }
            AssemblyPayload::Delta(b) => {
                (FolditPluginAssemblyPayloadKind::Delta, b)
            }
        };
        let mut err = FolditPluginError::empty();
        let status = unsafe {
            (self.vtable().update_assembly)(
                self.handle,
                session,
                kind,
                bytes.as_ptr(),
                bytes.len(),
                from_gen,
                to_gen,
                &raw mut err,
            )
        };
        check_status(self.vtable(), status, &mut err, "update_assembly")
    }

    fn drop_session(&self, session: u64) -> PluginResult<()> {
        let mut err = FolditPluginError::empty();
        let status = unsafe {
            (self.vtable().drop_session)(self.handle, session, &raw mut err)
        };
        check_status(self.vtable(), status, &mut err, "drop_session")
    }

    fn invoke(
        &self,
        session: u64,
        op: &str,
        ctx: &DispatchContext,
        params: &HashMap<String, ParamValue>,
    ) -> PluginResult<Vec<u8>> {
        let (c_ctx, _selection_storage, _designable_storage) =
            dispatch_context_to_c(ctx);
        let (c_params, _string_storage) = params_to_c(params);
        let mut buf = FolditPluginBuffer::empty();
        let mut err = FolditPluginError::empty();
        let status = unsafe {
            (self.vtable().invoke)(
                self.handle,
                session,
                op.as_ptr(),
                op.len(),
                &raw const c_ctx,
                c_params.as_ptr(),
                c_params.len(),
                &raw mut buf,
                &raw mut err,
            )
        };
        check_status(self.vtable(), status, &mut err, "invoke")?;
        Ok(take_buffer(self.vtable(), &mut buf))
    }

    fn start_stream(
        &self,
        session: u64,
        op: &str,
        ctx: &DispatchContext,
        params: &HashMap<String, ParamValue>,
        request_id: u64,
    ) -> PluginResult<()> {
        let (c_ctx, _selection_storage, _designable_storage) =
            dispatch_context_to_c(ctx);
        let (c_params, _string_storage) = params_to_c(params);
        let mut err = FolditPluginError::empty();
        let status = unsafe {
            (self.vtable().start_stream)(
                self.handle,
                session,
                op.as_ptr(),
                op.len(),
                &raw const c_ctx,
                c_params.as_ptr(),
                c_params.len(),
                request_id,
                &raw mut err,
            )
        };
        check_status(self.vtable(), status, &mut err, "start_stream")
    }

    // One arm per `PollStreamResponse` oneof variant decoded into
    // `PollOutcome`; the body is per-variant deserialization, not
    // splittable duplication.
    #[allow(clippy::too_many_lines)]
    fn poll_stream(&self, request_id: u64) -> PluginResult<PollOutcome> {
        let mut buf = FolditPluginBuffer::empty();
        let mut err = FolditPluginError::empty();
        let status = unsafe {
            (self.vtable().poll_stream)(
                self.handle,
                request_id,
                &raw mut buf,
                &raw mut err,
            )
        };
        check_status(self.vtable(), status, &mut err, "poll_stream")?;
        let bytes = take_buffer(self.vtable(), &mut buf);
        let resp =
            <proto::PollStreamResponse as prost::Message>::decode(&bytes[..])
                .map_err(PluginError::Decode)?;
        match resp.result {
            Some(proto::poll_stream_response::Result::Pending(p)) => {
                Ok(PollOutcome::Pending {
                    latest_assembly: if p.latest_assembly.is_empty() {
                        None
                    } else {
                        Some(p.latest_assembly)
                    },
                    progress: p.progress,
                    stage: p.stage,
                    score: p.score,
                })
            }
            Some(proto::poll_stream_response::Result::Checkpoint(c)) => {
                Ok(PollOutcome::Checkpoint {
                    latest_assembly: if c.latest_assembly.is_empty() {
                        None
                    } else {
                        Some(c.latest_assembly)
                    },
                    progress: c.progress,
                    stage: c.stage,
                    score: c.score,
                })
            }
            Some(proto::poll_stream_response::Result::Cancelled(c)) => {
                Ok(PollOutcome::Cancelled {
                    assembly: c.assembly,
                    score: c.score,
                })
            }
            Some(proto::poll_stream_response::Result::Final(f)) => {
                Ok(PollOutcome::Final {
                    assembly: f.assembly,
                    score: f.score,
                })
            }
            Some(proto::poll_stream_response::Result::Error(e)) => {
                Ok(PollOutcome::Error {
                    code: e.code,
                    message: e.message,
                    details: e.details,
                })
            }
            None => Err(PluginError::Other(
                "poll_stream returned PollStreamResponse with no result".into(),
            )),
        }
    }

    fn update_stream(
        &self,
        request_id: u64,
        params: &HashMap<String, ParamValue>,
    ) -> PluginResult<()> {
        let (c_params, _string_storage) = params_to_c(params);
        let mut err = FolditPluginError::empty();
        let status = unsafe {
            (self.vtable().update_stream)(
                self.handle,
                request_id,
                c_params.as_ptr(),
                c_params.len(),
                &raw mut err,
            )
        };
        check_status(self.vtable(), status, &mut err, "update_stream")
    }

    fn cancel_stream(&self, request_id: u64) -> PluginResult<()> {
        let mut err = FolditPluginError::empty();
        let status = unsafe {
            (self.vtable().cancel_stream)(self.handle, request_id, &raw mut err)
        };
        check_status(self.vtable(), status, &mut err, "cancel_stream")
    }

    fn query(
        &self,
        session: u64,
        query: &str,
        ctx: &DispatchContext,
        params: &HashMap<String, ParamValue>,
        assembly: &[u8],
    ) -> PluginResult<Vec<u8>> {
        let (c_ctx, _selection_storage, _designable_storage) =
            dispatch_context_to_c(ctx);
        let (c_params, _string_storage) = params_to_c(params);
        let mut buf = FolditPluginBuffer::empty();
        let mut err = FolditPluginError::empty();
        let status = unsafe {
            (self.vtable().query)(
                self.handle,
                session,
                query.as_ptr(),
                query.len(),
                &raw const c_ctx,
                c_params.as_ptr(),
                c_params.len(),
                assembly.as_ptr(),
                assembly.len(),
                &raw mut buf,
                &raw mut err,
            )
        };
        check_status(self.vtable(), status, &mut err, "query")?;
        Ok(take_buffer(self.vtable(), &mut buf))
    }
}

// Conversion helpers (Rust ↔ C ABI).

/// Build a [`FolditPluginDispatchContext`] borrowing from `ctx`. The two
/// returned `Vec<FolditPluginResidueRef>` MUST stay alive for the duration
/// of the C call (they back the `selection` and `designable` pointers).
fn dispatch_context_to_c(
    ctx: &DispatchContext,
) -> (
    FolditPluginDispatchContext,
    Vec<FolditPluginResidueRef>,
    Vec<FolditPluginResidueRef>,
) {
    let to_c = |refs: &[ResidueRef]| -> Vec<FolditPluginResidueRef> {
        refs.iter()
            .map(|r| FolditPluginResidueRef {
                entity_id: u64::from(r.entity_id.raw()),
                residue_index: r.residue_index,
                padding: 0,
            })
            .collect()
    };
    let ptr_of =
        |v: &[FolditPluginResidueRef]| -> *const FolditPluginResidueRef {
            if v.is_empty() {
                std::ptr::null()
            } else {
                v.as_ptr()
            }
        };
    let selection = to_c(&ctx.selection);
    let designable = to_c(&ctx.designable);
    let (has, eid) = ctx
        .focused_entity_id
        .map_or((0u8, 0), |e| (1u8, u64::from(e.raw())));
    let c_ctx = FolditPluginDispatchContext {
        has_focused_entity: has,
        padding: [0; 7],
        focused_entity_id: eid,
        selection: ptr_of(&selection),
        selection_len: selection.len(),
        designable: ptr_of(&designable),
        designable_len: designable.len(),
    };
    (c_ctx, selection, designable)
}

fn param_value_to_c(v: &ParamValue) -> FolditPluginParamValue {
    let base = FolditPluginParamValue {
        tag: FolditPluginParamTag::Int,
        padding: 0,
        int_value: 0,
        float_value: 0.0,
        bool_value: 0,
        padding2: [0; 7],
        string_data: std::ptr::null(),
        string_len: 0,
        vec3_value: FolditPluginVec3 {
            x: 0.0,
            y: 0.0,
            z: 0.0,
        },
    };
    match v {
        ParamValue::Int(i) => FolditPluginParamValue {
            tag: FolditPluginParamTag::Int,
            int_value: *i,
            ..base
        },
        ParamValue::Float(f) => FolditPluginParamValue {
            tag: FolditPluginParamTag::Float,
            float_value: *f,
            ..base
        },
        ParamValue::Bool(b) => FolditPluginParamValue {
            tag: FolditPluginParamTag::Bool,
            bool_value: u8::from(*b),
            ..base
        },
        ParamValue::String(s) => FolditPluginParamValue {
            tag: FolditPluginParamTag::String,
            string_data: s.as_ptr(),
            string_len: s.len(),
            ..base
        },
        ParamValue::Vec3([x, y, z]) => FolditPluginParamValue {
            tag: FolditPluginParamTag::Vec3,
            vec3_value: FolditPluginVec3 {
                x: *x,
                y: *y,
                z: *z,
            },
            ..base
        },
    }
}

/// Build a borrowed [`FolditPluginAsset`] vec from `assets`. Each entry
/// points into the `PuzzleAsset`'s owned `name`/`data`, so `assets` must
/// stay alive for the C call duration.
fn assets_to_c(assets: &[proto::PuzzleAsset]) -> Vec<FolditPluginAsset> {
    assets
        .iter()
        .map(|a| FolditPluginAsset {
            name_data: a.name.as_ptr(),
            name_len: a.name.len(),
            data: a.data.as_ptr(),
            data_len: a.data.len(),
        })
        .collect()
}

/// Build a borrowed [`FolditPluginParamEntry`] vec from `params`. The
/// returned `Vec<CString>` (and the original `params` map) must stay
/// alive for the C call duration — string entries reference into them.
fn params_to_c(
    params: &HashMap<String, ParamValue>,
) -> (Vec<FolditPluginParamEntry>, Vec<CString>) {
    let storage: Vec<CString> = Vec::new();
    let entries: Vec<FolditPluginParamEntry> = params
        .iter()
        .map(|(k, v)| FolditPluginParamEntry {
            key_data: k.as_ptr(),
            key_len: k.len(),
            value: param_value_to_c(v),
        })
        .collect();
    (entries, storage)
}

/// Convert a [`FolditPluginStatus`] + populated error into a Rust
/// `Result`. On `Ok`, returns `Ok(())`. On `Err`/`Unsupported`, drains
/// and frees the inner error buffers and returns the appropriate
/// `PluginError`.
fn check_status(
    vtable: &FolditPluginVtable,
    status: FolditPluginStatus,
    err: &mut FolditPluginError,
    method: &'static str,
) -> PluginResult<()> {
    match status {
        FolditPluginStatus::Ok => Ok(()),
        FolditPluginStatus::Unsupported => {
            // Plugin didn't populate err for Unsupported, but free
            // anyway in case it did, then return.
            unsafe { (vtable.free_error)(err) };
            Err(PluginError::Unsupported)
        }
        FolditPluginStatus::Err => {
            let code = drain_buffer(vtable, &mut err.code);
            let message = drain_buffer(vtable, &mut err.message);
            unsafe { (vtable.free_error)(err) };
            let code_str = String::from_utf8_lossy(&code);
            let message_str = String::from_utf8_lossy(&message);
            Err(PluginError::Other(format!(
                "native plugin {method} error [{code_str}] {message_str}"
            )))
        }
    }
}

/// Drain a plugin-allocated buffer into a Rust `Vec<u8>`, then free
/// the original. Reads bytes via copy because we don't own the
/// allocator.
fn take_buffer(
    vtable: &FolditPluginVtable,
    buf: &mut FolditPluginBuffer,
) -> Vec<u8> {
    let bytes = drain_buffer(vtable, buf);
    unsafe { (vtable.free_buffer)(buf) };
    bytes
}

fn drain_buffer(
    _vtable: &FolditPluginVtable,
    buf: &mut FolditPluginBuffer,
) -> Vec<u8> {
    if buf.data.is_null() || buf.len == 0 {
        return Vec::new();
    }
    let slice = unsafe { std::slice::from_raw_parts(buf.data, buf.len) };
    slice.to_vec()
}
