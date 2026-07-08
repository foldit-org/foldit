//! Per-worker plugin protocol client — the orchestrator's view of a plugin.
//!
//! [`PluginClient`] is a trait so future transports (in-process for
//! testing, wasm postMessage for the web build, etc.) can slot in
//! without changing dispatch code. Today there's one impl,
//! [`SocketPluginClient`], used for every plugin — both Python and
//! native plugins are hosted in a `foldit-worker` subprocess and
//! talk to the orchestrator via the local socket. (The worker process
//! decides internally whether to load a Python plugin via PyO3 or a
//! native dylib via `dlopen`; that distinction doesn't reach the
//! orchestrator side.)
//!
//! Each method is synchronous. The orchestrator's per-plugin worker
//! thread (in [`super::ops`]) holds the `Box<dyn PluginClient>` and
//! serializes calls into it.

use std::collections::HashMap;

use prost::Message;

use crate::error::{Result, RunnerError};
use crate::ipc::{receive_message, send_message, LocalSocketStream};
use crate::orchestrator::{
    Constraint, ConstraintAtom, ConstraintFunc, ConstraintKind,
    DispatchContext, InitPayload, ParamValue, PollOutcome, PuzzleAsset,
    ResidueRef,
};
use crate::proto::plugin as proto;

// BroadcastPayload — owned Assembly-update payload

/// Owned counterpart to [`crate::plugin::AssemblyPayload`].
///
/// The orchestrator constructs `BroadcastPayload`s when emitting
/// `UpdateAssembly` broadcasts (post-op fan-out, plus host-originated
/// mutations once `EntityStore` wires the drain hook). `Full` carries
/// fresh assembly bytes; `Delta` carries delta bytes.
#[derive(Debug, Clone)]
pub enum BroadcastPayload {
    /// Complete assembly bytes; replaces the plugin's view of the
    /// assembly.
    Full(Vec<u8>),
    /// Delta patch bytes applied on top of the plugin's current
    /// view.
    Delta(Vec<u8>),
}

impl BroadcastPayload {
    /// Convert to the proto oneof variant for transmission.
    #[must_use]
    pub fn into_proto(self) -> proto::update_assembly_request::Payload {
        match self {
            BroadcastPayload::Full(b) => {
                proto::update_assembly_request::Payload::Full(b)
            }
            BroadcastPayload::Delta(b) => {
                proto::update_assembly_request::Payload::Delta(b)
            }
        }
    }
}

// PluginClient trait

/// Per-worker plugin protocol client. Synchronous; the caller (typically
/// the per-plugin worker thread) serializes calls.
///
/// Every fallible method returns `Err` for one of two reasons:
/// transport failure (socket I/O, prost decode) or a plugin-level error
/// surfaced as [`RunnerError::PluginError`]. Per-method `# Errors`
/// sections don't repeat that.
pub trait PluginClient: Send {
    /// Stable plugin identifier (matches `PluginRegistration.id`).
    fn plugin_id(&self) -> &str;

    /// Open a session, handing the plugin the initial assembly bytes plus
    /// any puzzle-specific payload (`payload`: ligand asset bytes + typed
    /// catalytic constraints; empty for protein-only / free-form loads).
    /// Returns the assigned session id, the plugin's registration, and
    /// the plugin's post-Init normalized assembly bytes. The
    /// normalized assembly is non-empty when the plugin's Init mutates
    /// the input (Rosetta: builds a full-atom pose, may add atoms);
    /// empty when the plugin's settled assembly matches the input
    /// byte-for-byte (Python plugins).
    ///
    /// # Errors
    ///
    /// Transport failure or plugin-side init failure.
    fn init(
        &mut self,
        assembly_bytes: Vec<u8>,
        payload: InitPayload,
    ) -> Result<(u64, proto::PluginRegistration, Vec<u8>)>;

    /// Push an assembly update broadcast to the plugin. `from_gen` and
    /// `to_gen` bracket the generation range covered by the payload.
    ///
    /// # Errors
    ///
    /// Transport failure or plugin-reported error (e.g. `STALE_GEN`).
    fn update_assembly(
        &mut self,
        session: u64,
        payload: BroadcastPayload,
        from_gen: u64,
        to_gen: u64,
    ) -> Result<()>;

    /// Close a session and release its per-session state on the plugin
    /// side.
    ///
    /// # Errors
    ///
    /// Transport failure or plugin-reported error.
    fn drop_session(&mut self, session: u64) -> Result<()>;

    /// Run a one-shot operation, returning the resulting assembly bytes
    /// (typically a delta or full snapshot, as declared by the op).
    ///
    /// # Errors
    ///
    /// Transport failure or plugin-reported op error.
    fn invoke(
        &mut self,
        session: u64,
        op: &str,
        ctx: &DispatchContext,
        params: HashMap<String, ParamValue>,
    ) -> Result<Vec<u8>>;

    /// Start a streaming operation under the host-assigned `request_id`.
    /// The caller threads the same id through subsequent poll / update /
    /// cancel calls.
    ///
    /// # Errors
    ///
    /// Transport failure or plugin-reported start error.
    // Arg list is the streaming-dispatch ABI contract (session/op/ctx/params/
    // request_id); it mirrors the C-ABI and must not be refactored away.
    #[allow(clippy::too_many_arguments)]
    fn start_stream(
        &mut self,
        session: u64,
        op: &str,
        ctx: &DispatchContext,
        params: HashMap<String, ParamValue>,
        request_id: u64,
    ) -> Result<()>;

    /// Advance a stream by one step and return its [`PollOutcome`].
    ///
    /// # Errors
    ///
    /// Transport failure. Op-level failure surfaces as
    /// `PollOutcome::Error`, not as `Err`.
    fn poll_stream(&mut self, request_id: u64) -> Result<PollOutcome>;

    /// Apply a live parameter update to an active stream. Plugins are
    /// allowed to coalesce or ignore updates between poll boundaries.
    ///
    /// # Errors
    ///
    /// Transport failure or plugin-reported update error.
    fn update_stream(
        &mut self,
        request_id: u64,
        params: HashMap<String, ParamValue>,
    ) -> Result<()>;

    /// Request that the plugin tear down an active stream. The plugin
    /// is expected to release stream state before the next poll returns
    /// `Final` or `Error`.
    ///
    /// # Errors
    ///
    /// Transport failure or plugin-reported cancel error.
    fn cancel_stream(&mut self, request_id: u64) -> Result<()>;

    /// Run a read-only query (no assembly mutation). The byte payload
    /// is op-defined; callers parse it against the query's contract.
    ///
    /// # Errors
    ///
    /// Transport failure or plugin-reported query error.
    // Arg list mirrors the C-ABI `query` contract (session/query/ctx/params/
    // assembly); it must stay in lockstep with the vtable signature.
    #[allow(clippy::too_many_arguments)]
    fn query(
        &mut self,
        session: u64,
        query: &str,
        ctx: &DispatchContext,
        params: HashMap<String, ParamValue>,
        assembly: Vec<u8>,
    ) -> Result<Vec<u8>>;
}

// SocketPluginClient — out-of-process (Python today; future native subprocess)

/// Speaks `proto::plugin` over a local socket connection to a worker
/// subprocess.
///
/// Wire format: every call wraps its endpoint message in a
/// [`proto::PluginRequest`] and sends it length-prefixed; the worker
/// replies with a [`proto::PluginResponse`] of the corresponding variant.
pub struct SocketPluginClient {
    plugin_id: String,
    stream: LocalSocketStream,
}

impl SocketPluginClient {
    /// Wrap an open socket stream. Caller is responsible for spawning
    /// the worker process and connecting the socket; the spawn primitive
    /// in [`super::spawn`] handles that.
    pub fn new(
        plugin_id: impl Into<String>,
        stream: LocalSocketStream,
    ) -> Self {
        Self {
            plugin_id: plugin_id.into(),
            stream,
        }
    }

    fn round_trip(
        &mut self,
        req: &proto::PluginRequest,
    ) -> Result<proto::PluginResponse> {
        let bytes = req.encode_to_vec();
        send_message(&mut self.stream, &bytes).map_err(|e| {
            RunnerError::Generic(format!("PluginClient send: {e}"))
        })?;
        let resp_bytes = receive_message(&mut self.stream).map_err(|e| {
            RunnerError::Generic(format!("PluginClient receive: {e}"))
        })?;
        proto::PluginResponse::decode(&resp_bytes[..])
            .map_err(RunnerError::Protobuf)
    }
}

impl PluginClient for SocketPluginClient {
    fn plugin_id(&self) -> &str {
        &self.plugin_id
    }

    fn init(
        &mut self,
        assembly_bytes: Vec<u8>,
        payload: InitPayload,
    ) -> Result<(u64, proto::PluginRegistration, Vec<u8>)> {
        let req = wrap_request(proto::plugin_request::Request::Init(
            proto::InitRequest {
                assembly: assembly_bytes,
                assets: payload
                    .assets
                    .into_iter()
                    .map(puzzle_asset_to_proto)
                    .collect(),
                constraints: payload
                    .constraints
                    .into_iter()
                    .map(constraint_to_proto)
                    .collect(),
                params: params_to_proto(payload.params),
            },
        ));
        let resp = self.round_trip(&req)?;
        match unwrap_response(resp)? {
            proto::plugin_response::Response::Init(init_resp) => {
                match init_resp.response {
                    Some(proto::init_response::Response::Success(s)) => {
                        let registration = s.registration.ok_or_else(|| {
                            RunnerError::Generic(
                                "InitSuccess missing registration".into(),
                            )
                        })?;
                        Ok((s.session, registration, s.initial_assembly))
                    }
                    Some(proto::init_response::Response::Error(e)) => {
                        Err(plugin_error(e))
                    }
                    None => Err(RunnerError::Generic(
                        "InitResponse missing payload".into(),
                    )),
                }
            }
            other => Err(unexpected_variant("Init", &other)),
        }
    }

    fn update_assembly(
        &mut self,
        session: u64,
        payload: BroadcastPayload,
        from_gen: u64,
        to_gen: u64,
    ) -> Result<()> {
        let req = wrap_request(proto::plugin_request::Request::UpdateAssembly(
            proto::UpdateAssemblyRequest {
                session,
                payload: Some(payload.into_proto()),
                from_gen,
                to_gen,
            },
        ));
        let resp = self.round_trip(&req)?;
        match unwrap_response(resp)? {
            proto::plugin_response::Response::UpdateAssembly(r) => {
                r.error.map_or(Ok(()), |e| Err(plugin_error(e)))
            }
            other => Err(unexpected_variant("UpdateAssembly", &other)),
        }
    }

    fn drop_session(&mut self, session: u64) -> Result<()> {
        let req = wrap_request(proto::plugin_request::Request::Drop(
            proto::DropRequest { session },
        ));
        let resp = self.round_trip(&req)?;
        match unwrap_response(resp)? {
            proto::plugin_response::Response::Drop(r) => {
                r.error.map_or(Ok(()), |e| Err(plugin_error(e)))
            }
            other => Err(unexpected_variant("Drop", &other)),
        }
    }

    fn invoke(
        &mut self,
        session: u64,
        op: &str,
        ctx: &DispatchContext,
        params: HashMap<String, ParamValue>,
    ) -> Result<Vec<u8>> {
        let req = wrap_request(proto::plugin_request::Request::Invoke(
            proto::InvokeRequest {
                session,
                op: String::from(op),
                context: Some(dispatch_context_to_proto(ctx)),
                params: params_to_proto(params),
            },
        ));
        let resp = self.round_trip(&req)?;
        match unwrap_response(resp)? {
            proto::plugin_response::Response::Invoke(invoke_resp) => {
                match invoke_resp.response {
                    Some(proto::invoke_response::Response::Assembly(bytes)) => {
                        Ok(bytes)
                    }
                    Some(proto::invoke_response::Response::Error(e)) => {
                        Err(plugin_error(e))
                    }
                    None => Err(RunnerError::Generic(
                        "InvokeResponse missing payload".into(),
                    )),
                }
            }
            other => Err(unexpected_variant("Invoke", &other)),
        }
    }

    fn start_stream(
        &mut self,
        session: u64,
        op: &str,
        ctx: &DispatchContext,
        params: HashMap<String, ParamValue>,
        request_id: u64,
    ) -> Result<()> {
        let req = wrap_request(proto::plugin_request::Request::StartStream(
            proto::StartStreamRequest {
                session,
                op: String::from(op),
                context: Some(dispatch_context_to_proto(ctx)),
                params: params_to_proto(params),
                request_id,
            },
        ));
        let resp = self.round_trip(&req)?;
        match unwrap_response(resp)? {
            proto::plugin_response::Response::StartStream(start_resp) => {
                start_resp.error.map_or(Ok(()), |e| Err(plugin_error(e)))
            }
            other => Err(unexpected_variant("StartStream", &other)),
        }
    }

    fn poll_stream(&mut self, request_id: u64) -> Result<PollOutcome> {
        let req = wrap_request(proto::plugin_request::Request::PollStream(
            proto::PollStreamRequest { request_id },
        ));
        let resp = self.round_trip(&req)?;
        match unwrap_response(resp)? {
            proto::plugin_response::Response::PollStream(poll_resp) => {
                match poll_resp.result {
                    Some(proto::poll_stream_response::Result::Pending(p)) => {
                        Ok(PollOutcome::Pending {
                            latest_assembly: opt_bytes(p.latest_assembly),
                            progress: p.progress,
                            stage: p.stage,
                            score: p.score,
                        })
                    }
                    Some(proto::poll_stream_response::Result::Checkpoint(
                        c,
                    )) => Ok(PollOutcome::Checkpoint {
                        latest_assembly: opt_bytes(c.latest_assembly),
                        progress: c.progress,
                        stage: c.stage,
                        score: c.score,
                    }),
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
                    None => Err(RunnerError::Generic(
                        "PollStreamResponse missing result".into(),
                    )),
                }
            }
            other => Err(unexpected_variant("PollStream", &other)),
        }
    }

    fn update_stream(
        &mut self,
        request_id: u64,
        params: HashMap<String, ParamValue>,
    ) -> Result<()> {
        let req = wrap_request(proto::plugin_request::Request::UpdateStream(
            proto::UpdateStreamRequest {
                request_id,
                params: params_to_proto(params),
            },
        ));
        let resp = self.round_trip(&req)?;
        match unwrap_response(resp)? {
            proto::plugin_response::Response::UpdateStream(r) => {
                r.error.map_or(Ok(()), |e| Err(plugin_error(e)))
            }
            other => Err(unexpected_variant("UpdateStream", &other)),
        }
    }

    fn cancel_stream(&mut self, request_id: u64) -> Result<()> {
        let req = wrap_request(proto::plugin_request::Request::CancelStream(
            proto::CancelStreamRequest { request_id },
        ));
        let resp = self.round_trip(&req)?;
        match unwrap_response(resp)? {
            proto::plugin_response::Response::CancelStream(r) => {
                r.error.map_or(Ok(()), |e| Err(plugin_error(e)))
            }
            other => Err(unexpected_variant("CancelStream", &other)),
        }
    }

    fn query(
        &mut self,
        session: u64,
        query: &str,
        ctx: &DispatchContext,
        params: HashMap<String, ParamValue>,
        assembly: Vec<u8>,
    ) -> Result<Vec<u8>> {
        let req = wrap_request(proto::plugin_request::Request::Query(
            proto::QueryRequest {
                session,
                query: String::from(query),
                context: Some(dispatch_context_to_proto(ctx)),
                params: params_to_proto(params),
                // Empty bytes mean "no specific composition" — query the
                // live session pose; non-empty names an assembly to score.
                assembly: (!assembly.is_empty()).then_some(assembly),
            },
        ));
        let resp = self.round_trip(&req)?;
        match unwrap_response(resp)? {
            proto::plugin_response::Response::Query(query_resp) => {
                match query_resp.response {
                    Some(proto::query_response::Response::Data(bytes)) => {
                        Ok(bytes)
                    }
                    Some(proto::query_response::Response::Error(e)) => {
                        Err(plugin_error(e))
                    }
                    None => Err(RunnerError::Generic(
                        "QueryResponse missing payload".into(),
                    )),
                }
            }
            other => Err(unexpected_variant("Query", &other)),
        }
    }
}

// Conversion helpers (Rust ↔ proto::plugin)

fn wrap_request(r: proto::plugin_request::Request) -> proto::PluginRequest {
    proto::PluginRequest { request: Some(r) }
}

fn unwrap_response(
    resp: proto::PluginResponse,
) -> Result<proto::plugin_response::Response> {
    resp.response.ok_or_else(|| {
        RunnerError::Generic("PluginResponse missing oneof".into())
    })
}

fn unexpected_variant(
    expected: &str,
    got: &proto::plugin_response::Response,
) -> RunnerError {
    RunnerError::Generic(format!(
        "PluginClient expected {expected} response, got {:?}",
        std::mem::discriminant(got)
    ))
}

fn plugin_error(e: proto::Error) -> RunnerError {
    RunnerError::PluginError {
        code: e.code,
        message: e.message,
    }
}

fn opt_bytes(v: Vec<u8>) -> Option<Vec<u8>> {
    if v.is_empty() {
        None
    } else {
        Some(v)
    }
}

fn dispatch_context_to_proto(ctx: &DispatchContext) -> proto::DispatchContext {
    let to_proto = |refs: &[ResidueRef]| -> Vec<proto::ResidueRef> {
        refs.iter()
            .map(|r| proto::ResidueRef {
                entity_id: u64::from(r.entity_id.raw()),
                residue_index: r.residue_index,
            })
            .collect()
    };
    proto::DispatchContext {
        focused_entity_id: ctx.focused_entity_id.map(|e| u64::from(e.raw())),
        selection: to_proto(&ctx.selection),
        designable: to_proto(&ctx.designable),
    }
}

fn params_to_proto(
    params: HashMap<String, ParamValue>,
) -> HashMap<String, proto::ParamValue> {
    params
        .into_iter()
        .map(|(k, v)| (k, param_value_to_proto(v)))
        .collect()
}

fn param_value_to_proto(v: ParamValue) -> proto::ParamValue {
    use proto::param_value::Value;
    let value = match v {
        ParamValue::Int(i) => Value::IntValue(i),
        ParamValue::Float(f) => Value::FloatValue(f),
        ParamValue::Bool(b) => Value::BoolValue(b),
        ParamValue::String(s) => Value::StringValue(s),
        ParamValue::Vec3([x, y, z]) => {
            Value::Vec3Value(proto::Vec3 { x, y, z })
        }
    };
    proto::ParamValue { value: Some(value) }
}

fn puzzle_asset_to_proto(a: PuzzleAsset) -> proto::PuzzleAsset {
    proto::PuzzleAsset {
        name: a.name,
        data: a.data,
    }
}

fn constraint_to_proto(c: Constraint) -> proto::Constraint {
    proto::Constraint {
        kind: constraint_kind_to_proto(c.kind) as i32,
        atoms: c.atoms.into_iter().map(constraint_atom_to_proto).collect(),
        func: Some(constraint_func_to_proto(c.func)),
    }
}

fn constraint_kind_to_proto(k: ConstraintKind) -> proto::ConstraintKind {
    match k {
        ConstraintKind::AtomPair => proto::ConstraintKind::AtomPair,
        ConstraintKind::Angle => proto::ConstraintKind::Angle,
        ConstraintKind::Dihedral => proto::ConstraintKind::Dihedral,
    }
}

fn constraint_atom_to_proto(a: ConstraintAtom) -> proto::ConstraintAtom {
    proto::ConstraintAtom {
        atom_name: a.atom_name,
        res_num: a.res_num,
        chain: a.chain,
    }
}

fn constraint_func_to_proto(f: ConstraintFunc) -> proto::ConstraintFunc {
    use proto::constraint_func::Func;
    let func = match f {
        ConstraintFunc::FlatHarmonic { x0, sd, tol } => {
            Func::FlatHarmonic(proto::FlatHarmonic { x0, sd, tol })
        }
        ConstraintFunc::CircularHarmonic { x0, sd } => {
            Func::CircularHarmonic(proto::CircularHarmonic { x0, sd })
        }
    };
    proto::ConstraintFunc { func: Some(func) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broadcast_payload_full_into_proto() {
        let payload = BroadcastPayload::Full(vec![1, 2, 3]);
        match payload.into_proto() {
            proto::update_assembly_request::Payload::Full(b) => {
                assert_eq!(b, vec![1, 2, 3]);
            }
            other @ proto::update_assembly_request::Payload::Delta(_) => {
                panic!("expected Full arm, got {other:?}")
            }
        }
    }

    #[test]
    fn broadcast_payload_delta_into_proto() {
        let payload = BroadcastPayload::Delta(vec![9, 8, 7]);
        match payload.into_proto() {
            proto::update_assembly_request::Payload::Delta(b) => {
                assert_eq!(b, vec![9, 8, 7]);
            }
            other @ proto::update_assembly_request::Payload::Full(_) => {
                panic!("expected Delta arm, got {other:?}")
            }
        }
    }

    #[test]
    fn update_assembly_request_round_trip_full() {
        let req = proto::UpdateAssemblyRequest {
            session: 42,
            payload: Some(
                BroadcastPayload::Full(vec![10, 20, 30]).into_proto(),
            ),
            from_gen: 5,
            to_gen: 6,
        };
        let bytes = Message::encode_to_vec(&req);
        let decoded =
            <proto::UpdateAssemblyRequest as Message>::decode(&bytes[..])
                .expect("round-trip decode succeeds");
        assert_eq!(decoded.session, 42);
        assert_eq!(decoded.from_gen, 5);
        assert_eq!(decoded.to_gen, 6);
        match decoded.payload {
            Some(proto::update_assembly_request::Payload::Full(b)) => {
                assert_eq!(b, vec![10, 20, 30]);
            }
            other => panic!("expected Full payload, got {other:?}"),
        }
    }

    #[test]
    fn update_assembly_request_round_trip_delta() {
        let req = proto::UpdateAssemblyRequest {
            session: 7,
            payload: Some(
                BroadcastPayload::Delta(vec![1, 2, 3, 4]).into_proto(),
            ),
            from_gen: 100,
            to_gen: 101,
        };
        let bytes = Message::encode_to_vec(&req);
        let decoded =
            <proto::UpdateAssemblyRequest as Message>::decode(&bytes[..])
                .expect("round-trip decode succeeds");
        assert_eq!(decoded.session, 7);
        assert_eq!(decoded.from_gen, 100);
        assert_eq!(decoded.to_gen, 101);
        match decoded.payload {
            Some(proto::update_assembly_request::Payload::Delta(b)) => {
                assert_eq!(b, vec![1, 2, 3, 4]);
            }
            other => panic!("expected Delta payload, got {other:?}"),
        }
    }

    #[test]
    fn legacy_bytes_assembly_field_parses_as_full_arm() {
        // Wire compatibility: a sender that emits the old
        // `bytes assembly = 2` field must parse on the new side as
        // the `full` oneof arm. proto3 oneof preserves field
        // numbers, so encoding tag-2 bytes manually and re-decoding
        // through the new message must land in `Payload::Full`.
        //
        // Encode tag-2 bytes the same way prost would for a bare
        // `bytes assembly = 2` field at session id 1.
        let mut buf: Vec<u8> = Vec::new();
        prost::encoding::uint64::encode(1, &1u64, &mut buf); // session = 1
        prost::encoding::bytes::encode(2, &vec![0xAA, 0xBB, 0xCC], &mut buf);
        // Decode through the new message shape.
        let decoded =
            <proto::UpdateAssemblyRequest as Message>::decode(&buf[..])
                .expect("legacy-shaped wire bytes parse on the new schema");
        assert_eq!(decoded.session, 1);
        assert_eq!(decoded.from_gen, 0);
        assert_eq!(decoded.to_gen, 0);
        match decoded.payload {
            Some(proto::update_assembly_request::Payload::Full(b)) => {
                assert_eq!(b, vec![0xAA, 0xBB, 0xCC]);
            }
            other => panic!(
                "legacy `bytes assembly = 2` must parse as Full arm, got \
                 {other:?}"
            ),
        }
    }

    #[test]
    fn plugin_error_carries_structured_code() {
        let proto_err = proto::Error {
            code: String::from("STALE_GEN"),
            message: String::from("drift"),
            details: Default::default(),
        };
        let model_err = plugin_error(proto_err);
        match model_err {
            RunnerError::PluginError { code, message } => {
                assert_eq!(code, "STALE_GEN");
                assert_eq!(message, "drift");
            }
            other => panic!("expected PluginError variant, got {other:?}"),
        }
    }
}
