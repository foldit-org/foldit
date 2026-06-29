//! JSON → [`IpcMessage`] decoding. Transport-neutral.
//!
//! The transport hands a `&str` body in; the decoder returns `Some(msg)` for
//! known commands or `None` for anything unrecognized (transport logs and
//! drops). Decoding never panics — every shape is best-effort.

use serde_json::Value;

use super::message::{IpcMessage, RequestKind};

/// Parse an IPC envelope of the form `{ "cmd": "...", "data": ... }`.
///
/// The async-request case is `{ "cmd": "request", "wish_id": ..., "kind": ...,
/// "payload": ... }`. Returns `None` on parse failure or unknown cmd.
pub fn from_json(body: &str) -> Option<IpcMessage> {
    let val: Value = serde_json::from_str(body).ok()?;
    let cmd = val.get("cmd").and_then(Value::as_str)?;
    match cmd {
        "ready" => Some(IpcMessage::Ready),
        "viewport_input" => val
            .get("data")
            .and_then(|d| serde_json::from_value(d.clone()).ok())
            .map(IpcMessage::ViewportInput),
        "dispatch_op" => val
            .get("data")
            .and_then(|d| serde_json::from_value(d.clone()).ok())
            .map(IpcMessage::DispatchOp),
        "app_command" => val
            .get("data")
            .and_then(|d| serde_json::from_value(d.clone()).ok())
            .map(IpcMessage::AppCommand),
        "set_selection" => {
            let entries = val
                .get("data")
                .and_then(|d| d.get("entries"))
                .and_then(|e| serde_json::from_value(e.clone()).ok())?;
            Some(IpcMessage::SetSelection { entries })
        }
        "update_stream" => {
            let data = val.get("data")?;
            let request_id = data.get("request_id").and_then(Value::as_u64)?;
            let params = data
                .get("params")
                .and_then(|p| serde_json::from_value(p.clone()).ok())
                .unwrap_or_default();
            Some(IpcMessage::UpdateStream { request_id, params })
        }
        "open_session_dialog" => Some(IpcMessage::OpenSessionDialog),
        "request" => {
            let wish_id = val.get("wish_id").and_then(Value::as_str)?.to_owned();
            let kind: RequestKind =
                serde_json::from_value(val.get("kind")?.clone()).ok()?;
            let payload = val.get("payload").cloned().unwrap_or(Value::Null);
            Some(IpcMessage::Request { wish_id, kind, payload })
        }
        _ => None,
    }
}
