use serde::{Deserialize, Serialize};

use crate::wire::HistoryCommand;

/// Viewport input events forwarded from the GUI overlay
#[derive(Debug, Clone, Serialize, Deserialize, specta::Type)]
#[serde(tag = "kind")]
pub enum ViewportInput {
    PointerDown {
        x: f32,
        y: f32,
        button: u8,
        #[serde(default)]
        shift: bool,
        #[serde(default)]
        ctrl: bool,
        #[serde(default)]
        alt: bool,
    },
    PointerUp {
        x: f32,
        y: f32,
        button: u8,
        #[serde(default)]
        shift: bool,
        #[serde(default)]
        ctrl: bool,
        #[serde(default)]
        alt: bool,
    },
    PointerMove {
        x: f32,
        y: f32,
        dx: f32,
        dy: f32,
        #[serde(default)]
        shift: bool,
        #[serde(default)]
        ctrl: bool,
        #[serde(default)]
        alt: bool,
    },
    Scroll {
        delta: f32,
    },
    Key {
        code: String,
        pressed: bool,
    },
    Resize {
        width: u32,
        height: u32,
    },
}

/// GUI-side dispatch envelope for a plugin op-id.
///
/// The frontend builds
/// one of these per button click; the backend resolves `op_id` against
/// the orchestrator's [`foldit_runner::orchestrator::PluginRegistry`]
/// to pick `Invoke` vs `StartStream` and routes to the owning plugin.
///
/// Selection is not carried on the wire; the backend reads
/// `App.selection` at dispatch, so frontends only send the op id
/// (plus optional focus + params).
#[derive(Debug, Clone, Serialize, Deserialize, specta::Type)]
pub struct OpDispatch {
    /// Op-id from a `CatalogEntry`. Globally unique across plugins.
    pub op_id: String,
    /// Optional focused entity. Plain u64 over the wire so the
    /// frontend doesn't depend on Rust types.
    #[specta(type = Option<u32>)]
    pub focused_entity_id: Option<u64>,
    /// Typed parameter values keyed by `ParamSpec.name`. Populated by
    /// schema-driven panels; click-to-fire buttons omit it, so the
    /// `#[serde(default)]` empty-map path keeps `dispatchOp({ op_id })`
    /// callers deserializing unchanged.
    #[serde(default)]
    pub params: std::collections::HashMap<String, crate::state::ParamValue>,
}

/// Native GUI / chrome commands - the non-plugin action lane.
///
/// History navigation, tutorial-bubble stepping, view options, and
/// structure / puzzle loading all ride this typed envelope. Distinct
/// from the dynamic plugin-op catalog path ([`OpDispatch`]), which
/// resolves op-id strings against the orchestrator's registry; nothing
/// here touches a plugin.
#[derive(Debug, Clone, Serialize, Deserialize, specta::Type)]
#[serde(tag = "type")]
pub enum AppCommand {
    SetViewOptions {
        #[specta(type = specta_typescript::Unknown)]
        options: serde_json::Value,
    },
    LoadStructure {
        path: String,
    },
    LoadPuzzle {
        puzzle_id: u32,
    },
    /// Load a puzzle from an arbitrary directory containing `puzzle.toml`
    /// (user-chosen via Load Session). Distinct from `LoadPuzzle`, which is
    /// keyed by campaign id under the levels root.
    LoadPuzzleDir {
        path: String,
    },
    LoadViewPreset {
        name: String,
    },
    SaveViewPreset {
        name: String,
    },
    /// History navigation / curation. Wraps the typed [`HistoryCommand`]
    /// enum so the navigation surface rides the one `app_command` IPC
    /// envelope alongside the other native commands.
    History {
        cmd: HistoryCommand,
    },
    /// Step the active puzzle's tutorial-bubble cursor forward or back.
    /// `back == false` advances one; `back == true` retreats one
    /// (saturating at zero). The backend re-pushes the bubble at the
    /// new cursor, or `None` when the cursor walks past the end.
    AdvanceBubble {
        back: bool,
    },
    /// Set the active app focus. `None` is whole-session focus (all
    /// entities); `Some(raw)` focuses that entity by its raw id. Pure
    /// session state, so it needs no engine.
    SetFocus {
        entity_id: Option<u32>,
    },
    /// Edit one per-entity appearance override field. `entity_id` is the
    /// raw id; `field`/`value` are merged into that entity's overrides by
    /// the engine. Engine-dependent (the override map lives in viso).
    SetEntityAppearance {
        entity_id: u32,
        field: String,
        #[specta(type = specta_typescript::Unknown)]
        value: serde_json::Value,
    },
    /// Clear a single entity's whole appearance override entry, reverting
    /// it to inherited/global appearance (e.g. the panel Reset button).
    /// `entity_id` is the raw id; the session drops the entry and the
    /// render projector clears the engine working copy on the emitted
    /// `EntityAppearanceChanged`.
    ClearEntityAppearance {
        entity_id: u32,
    },
    /// Close the segment-info panel from the GUI (its X button). Clears the
    /// backend `App.open_segment` source of truth so the projected
    /// `segment_info` and the live tail pushes stop; a frontend-only hide
    /// would desync (the backend would keep producing both).
    CloseSegment,
    /// Show or hide a panel by id from the GUI. Pure UI state; the backend
    /// owns the open/closed set so visibility survives a reload.
    SetPanelVisible {
        panel: String,
        visible: bool,
    },
    /// Open the action picker for `op_id`, or close any open picker with
    /// `None`. Pure UI state; the backend owns the open picker so it survives
    /// a re-projection and can be toggled by a native hotkey too.
    SetActionPickerOpen {
        op_id: Option<String>,
    },
    /// Record a panel's dragged top-left position (pixels, origin
    /// top-left). Pure UI state.
    SetPanelPosition {
        panel: String,
        x: f32,
        y: f32,
    },
    /// Show or hide the tutorial-hint bubble. Pure UI state; the backend
    /// owns the flag so it survives a reload.
    SetHintsVisible {
        visible: bool,
    },
    /// Enter or leave OS fullscreen. Pure UI state on the backend mirror;
    /// the desktop host applies the change to the winit window, the web
    /// click handler drives the DOM fullscreen API in-gesture.
    SetFullscreen {
        value: bool,
    },
    /// Wipe all recorded puzzle high-score progress. Backend-authoritative;
    /// the menu reads the projected map, so a reset round-trips through a
    /// tick.
    ClearProgress,
    /// Cancel a running action from the GUI (a per-action toast X or a running
    /// button's X). `request_id = Some(rid)` cancels exactly that one stream;
    /// `refine = true` cancels the native B-factor refine (which is not a
    /// stream and has no request-id); both unset cancels everything cancellable
    /// (the ESC path). Drops any in-progress preview geometry either way.
    CancelAction {
        /// `u32` on the wire (specta forbids u64); matches
        /// [`crate::state::RunningAction::request_id`].
        #[specta(type = Option<u32>)]
        request_id: Option<u64>,
        refine: bool,
    },
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "unwrap in a test fails the test loudly, which is the intended behavior"
)]
mod tests {
    use super::*;
    use crate::state::ParamValue;

    /// Click-to-fire buttons (`ActionButtonWidget`) post `{ op_id }` with
    /// no `params` key. The `#[serde(default)]` attribute must fill an
    /// empty map so those dispatches keep deserializing; if this breaks,
    /// every zero-param button (wiggle, shake, ...) stops firing.
    #[test]
    fn opdispatch_without_params_defaults_empty() {
        let op: OpDispatch = serde_json::from_str(r#"{"op_id":"wiggle"}"#).unwrap();
        assert_eq!(op.op_id, "wiggle");
        assert!(op.params.is_empty());
        assert!(op.focused_entity_id.is_none());
    }

    /// Schema-driven panels post a populated `params` map using the
    /// externally-tagged `ParamValue` encoding (`{ "Float": 0.5 }`).
    #[test]
    fn opdispatch_with_params_deserializes() {
        let op: OpDispatch = serde_json::from_str(
            r#"{"op_id":"design","params":{"temperature":{"Float":0.5},"num_sequences":{"Int":8}}}"#,
        )
        .unwrap();
        assert_eq!(op.params.get("temperature"), Some(&ParamValue::Float(0.5)));
        assert_eq!(op.params.get("num_sequences"), Some(&ParamValue::Int(8)));
    }
}
