//! The typed change-event spine.
//!
//! [`SceneChange`] is the single event type emitted by the `Document`
//! mutation funnel (RX6) and consumed by the three projectors: render
//! (RX13), plugin broadcast (RX6), and GUI (live cursor in RX10). Every
//! observable mutation produces exactly one `SceneChange`; the
//! projectors decide independently how (or whether) to react to each
//! variant.
//!
//! The enum is signal-only: it names *what changed*, not the payload.
//! Each projector re-derives whatever it needs from the [`Document`]
//! (render rebuilds the whole `Assembly` from arcs, broadcaster diffs
//! its last published snapshot, GUI polls `live_version`). Carrying
//! payloads here would duplicate state that the projectors are already
//! reading from `Document` and invite drift.

/// A single observable change to the scene.
#[derive(Debug, Clone)]
pub enum SceneChange {
    /// A structural/coordinate edit. `tentative` marks per-cycle
    /// live/streaming edits (e.g. a pull-drag or mid-action plugin
    /// frame); the plugin broadcaster and persistent projectors skip
    /// these, the render projector consumes them.
    Edit { tentative: bool },
    /// The history head moved (undo / redo / jump / commit / reset).
    /// Projectors rebuild; the plugin broadcaster sends a Full/Delta
    /// snapshot diff.
    HeadMoved,
    /// A preview (transient) entity was added to the overlay.
    PreviewAdded,
    /// A preview (transient) entity was discarded from the overlay.
    PreviewDiscarded,
}
