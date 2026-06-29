use crate::TailUpdate;

/// Sink the host passes into [`crate::App::tick`]; core pushes per-frame
/// effects to it instead of the host pulling them after the tick.
pub trait HostEffects {
    /// Serialized dirty `GuiState` sections for the host to push to its frontend.
    fn push_state(&mut self, json: &[u8]);

    /// Segment-panel tail-tip change (web: no-op).
    fn push_tail(&mut self, update: TailUpdate);

    /// Fullscreen flip for the host to apply to its native window (web: no-op).
    fn set_fullscreen(&mut self, value: bool);

    /// Serialized high-score progress map for the host to persist asynchronously.
    fn persist_progress(&mut self, bytes: Vec<u8>);
}
