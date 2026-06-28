//! Transport-agnostic IPC message + request shapes shared by all platforms.
//!
//! Both `WryTransport` (desktop) and `WasmTransport` (web) decode incoming
//! traffic into [`IpcMessage`] and dispatch through the same `App`.
//! Adding a new IPC command is one variant + one decode arm.

use serde::{Deserialize, Serialize};

use crate::state::EntitySelection;
use crate::{AppCommand, OpDispatch, ViewportInput};

/// Request types that JS sends through the async request/response channel.
/// Wire encoding: `snake_case` string discriminant on the transport boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestKind {
    /// Read a file from the resource bundle; payload `{ filepath: String }`.
    /// Response: base64-encoded bytes plus encoding tag.
    ReadResourceFile,
    /// One-shot catalog of plugin-contributed custom panels. No payload.
    /// Response: an array of [`crate::state::PanelInfo`].
    PanelsCatalog,
    /// One-shot catalog of plugin-contributed settings tabs. No payload.
    /// Response: an array of [`crate::state::SettingsTabInfo`].
    SettingsCatalog,
}

/// Decoded IPC message from JS to Rust. Transport-neutral.
#[derive(Debug)]
pub enum IpcMessage {
    /// Webview is ready to receive state pushes.
    Ready,
    /// Forwarded pointer/keyboard/scroll/resize from the viewport overlay.
    ViewportInput(ViewportInput),
    /// Plugin op dispatch keyed on op-id. Catalog entries
    /// (wiggle, shake, ...) flow through here; the App routes the op
    /// to the orchestrator's Invoke / `StartStream` path based on the
    /// registered op kind.
    DispatchOp(OpDispatch),
    /// Native GUI / chrome command (history nav, bubble advance, view
    /// options, load structure / puzzle). Non-plugin lane.
    AppCommand(AppCommand),
    /// Panel-originated selection mutation. Replaces the current
    /// [`crate::GuiState::selection`] (and the backend `App.selection`
    /// source of truth) with the supplied per-entity residue lists.
    SetSelection { entries: Vec<EntitySelection> },
    /// Desktop-only: request the native "Load Session" file picker. Handled
    /// entirely in the desktop binary (it owns the event loop + window); never
    /// reaches foldit-core. No web equivalent (browser picking is separate).
    OpenSessionDialog,
    /// Async request awaiting a response. The transport correlates `wish_id`
    /// to the JS-side Promise.
    Request {
        wish_id: String,
        kind: RequestKind,
        payload: serde_json::Value,
    },
}
