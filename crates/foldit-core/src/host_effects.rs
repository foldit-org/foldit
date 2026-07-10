use crate::TailUpdate;

/// Sink the host passes into [`crate::App::tick`]; core pushes per-frame
/// effects to it instead of the host pulling them after the tick.
pub trait HostEffects {
    /// Whether the host will accept a frontend push on this tick.
    ///
    /// Checked before `serialize_dirty` drains the dirty flags, so a
    /// `false` leaves those flags and any pending tail update intact for
    /// the next admitted tick — the update coalesces rather than being
    /// dropped. Hosts whose push is cheap leave this at `true`.
    fn may_push_frontend(&mut self) -> bool {
        true
    }

    /// Answer a request the host deferred, keyed by its `wish_id`. Fired when
    /// an async plugin query lands, so the reply is not tied to the tick that
    /// began it. Hosts with no deferred requests leave this as a no-op.
    fn push_response(
        &mut self,
        _wish_id: &str,
        _result: &foldit_gui::RequestResult,
    ) {
    }

    /// Serialized dirty `GuiState` sections for the host to push to its frontend.
    fn push_state(&mut self, json: &[u8]);

    /// Segment-panel tail-tip change (web: no-op).
    fn push_tail(&mut self, update: TailUpdate);

    /// Fullscreen flip for the host to apply to its native window (web: no-op).
    fn set_fullscreen(&mut self, value: bool);

    /// Serialized high-score progress map for the host to persist asynchronously.
    fn persist_progress(&mut self, bytes: Vec<u8>);
}
