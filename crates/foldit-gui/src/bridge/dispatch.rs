//! Dispatcher trait — the application-side seam.
//!
//! Implemented by the host app once. Both `WryTransport` (desktop) and
//! `WasmTransport` (web) call the same trait methods after decoding an
//! [`IpcMessage`], so the action surface is defined exactly once.

use serde_json::Value;

use crate::state::EntitySelection;
use crate::{AppCommand, OpDispatch, ViewportInput};

use super::message::RequestKind;
use super::transport::RequestResult;

/// Application-side handler for incoming IPC messages.
pub trait Dispatcher {
    /// Webview signaled it's ready to receive state pushes. Default:
    /// no-op; impls typically mark all dirty so the next push is a snapshot.
    fn on_ready(&mut self) {}

    fn on_viewport_input(&mut self, input: ViewportInput);
    fn on_dispatch_op(&mut self, op: OpDispatch);
    fn on_app_command(&mut self, command: AppCommand);
    /// Replace the App selection with `entries`. Panel-originated
    /// (rama, sequence panel, ...); pointer-pick selection still flows
    /// through `on_viewport_input` and viso's `ClickEvent` path.
    fn on_set_selection(&mut self, entries: Vec<EntitySelection>);

    /// Synchronously resolve an async JS-side request. Genuinely async work
    /// should spawn a task and call `Transport::send_response` from there;
    /// the cheap cases (filesystem read, hotkey lookup) return inline.
    ///
    /// # Errors
    ///
    /// Returns `Err(message)` when the request cannot be served (unknown
    /// kind, malformed payload, or an underlying operation fails); the
    /// string is surfaced to the JS caller as the rejection reason.
    fn handle_request(&mut self, kind: RequestKind, payload: Value) -> RequestResult;
}
