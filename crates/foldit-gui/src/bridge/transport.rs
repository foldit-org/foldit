//! Transport trait — the platform-specific seam.
//!
//! Two impls today:
//! - `WryTransport` (desktop): JS side via `webview.evaluate_script(...)`.
//! - `WasmTransport` (web): JS side via a stored `js_sys::Function` callback.
//!
//! Both consume the same payloads from [`crate::bridge::push::serialize_dirty`]
//! and [`Dispatcher::handle_request`], so the only thing that varies between
//! platforms is the byte-delivery mechanism.

use serde_json::Value;

/// Result returned by [`Dispatcher::handle_request`].
pub type RequestResult = Result<Value, String>;

/// Platform-specific delivery channel from Rust to JS.
pub trait Transport {
    /// Push a partial-`GuiState` JSON object to the frontend's
    /// `__onStateUpdate` channel.
    fn send_state(&self, payload: &Value);

    /// Resolve or reject a pending JS-side request by `wish_id`.
    /// Implementations call into JS as `__onResponse(wish_id, ok, payload)`.
    fn send_response(&self, wish_id: &str, result: &RequestResult);
}
