//! Section serializer — pure function over `FrontendState`.
//!
//! Produces the partial-state JSON object that gets handed to
//! [`Transport::send_state`]. Sections are emitted only when their dirty
//! bit is set; calling `take_dirty()` clears the bits.

use serde_json::{Map, Value};

use crate::{DirtyFlags, FrontendState};

/// Drain dirty flags and serialize the corresponding sections. Returns
/// `None` when nothing was dirty (caller should skip the push).
#[allow(
    clippy::unwrap_used,
    clippy::missing_panics_doc,
    reason = "serde_json::to_value over these plain state structs is infallible: Value represents every field, NaN/Infinity floats map to Null rather than erroring, and the only error source (non-string map keys) does not occur here"
)]
pub fn serialize_dirty(state: &mut FrontendState) -> Option<Value> {
    let dirty = state.take_dirty();
    if dirty.is_empty() {
        return None;
    }

    let mut update = Map::new();

    if dirty.contains(DirtyFlags::SCORE) {
        update.insert("score".into(), serde_json::to_value(&state.score).unwrap());
    }
    if dirty.contains(DirtyFlags::PUZZLE) {
        update.insert("puzzle".into(), serde_json::to_value(&state.puzzle).unwrap());
    }
    if dirty.contains(DirtyFlags::SELECTION) {
        update.insert(
            "selection".into(),
            serde_json::to_value(&state.selection).unwrap(),
        );
    }
    if dirty.contains(DirtyFlags::VIEW) {
        update.insert("view".into(), serde_json::to_value(&state.view).unwrap());
    }
    if dirty.contains(DirtyFlags::UI) {
        update.insert("ui".into(), serde_json::to_value(&state.ui).unwrap());
    }
    if dirty.contains(DirtyFlags::ACTIONS) {
        update.insert(
            "actions".into(),
            serde_json::to_value(&state.actions).unwrap(),
        );
    }
    if dirty.contains(DirtyFlags::LOADING) {
        update.insert(
            "loading".into(),
            serde_json::to_value(&state.loading).unwrap(),
        );
    }
    if dirty.contains(DirtyFlags::SCENE) {
        update.insert("scene".into(), serde_json::to_value(&state.scene).unwrap());
    }
    if dirty.contains(DirtyFlags::HISTORY) {
        update.insert(
            "history".into(),
            serde_json::to_value(state.history()).unwrap(),
        );
    }
    if dirty.contains(DirtyFlags::APP_STATE) {
        update.insert(
            "app_state".into(),
            serde_json::to_value(state.app_state()).unwrap(),
        );
    }
    if dirty.contains(DirtyFlags::SEGMENT) {
        update.insert(
            "segment_info".into(),
            serde_json::to_value(&state.segment_info).unwrap(),
        );
    }
    if dirty.contains(DirtyFlags::PANELS) {
        update.insert("panels".into(), serde_json::to_value(&state.panels).unwrap());
    }
    if dirty.contains(DirtyFlags::PROGRESS) {
        update.insert(
            "progress".into(),
            serde_json::to_value(&state.progress).unwrap(),
        );
    }

    Some(Value::Object(update))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every section that `serialize_dirty` is responsible for emitting. A
    /// new `DirtyFlags` section bit that the GUI reads must be added here
    /// AND gain an arm in `serialize_dirty`; otherwise this test goes red,
    /// catching the silent drop of a projected-but-untransmitted section.
    ///
    /// `HISTORY_LIVE` and `TEXT_BUBBLE` are deliberately absent:
    /// `HISTORY_LIVE` carries no `serialize_dirty` arm (its `history_live`
    /// patch travels a separate channel), and `TEXT_BUBBLE` is a
    /// projector-internal trigger whose payload rides the `ui` section.
    const EXPECTED_SECTION_KEYS: &[&str] = &[
        "score",
        "puzzle",
        "selection",
        "view",
        "ui",
        "actions",
        "loading",
        "scene",
        "history",
        "app_state",
        "segment_info",
        "panels",
        "progress",
    ];

    #[test]
    fn full_dirty_emits_every_section() -> Result<(), Box<dyn std::error::Error>> {
        let mut state = FrontendState::new();
        state.mark_all_dirty();

        let value = serialize_dirty(&mut state).ok_or("all-dirty must serialize")?;
        let object = value.as_object().ok_or("payload is a JSON object")?;

        for key in EXPECTED_SECTION_KEYS {
            assert!(
                object.contains_key(*key),
                "section key `{key}` missing from full-dirty payload"
            );
        }

        Ok(())
    }
}
