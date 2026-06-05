//! The typed change-event spine.
//!
//! [`SessionUpdate`] is the single event type emitted by the `Document`
//! mutation funnel (RX6) and consumed by the three projectors: render
//! (RX13), plugin broadcast (RX6), and GUI (live cursor in RX10). Every
//! observable mutation produces exactly one `SessionUpdate`; the
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
pub enum SessionUpdate {
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
    /// A head / edit / checkpoint score *value* changed. Signal-only,
    /// like the rest of the enum: the score numbers live on the
    /// `Document`, and consumers re-read them (the GUI score widget from
    /// the head/composition score; the history panel from its live
    /// cursor). A score is not a scene mutation, so the render projector
    /// and the plugin broadcaster ignore it.
    ScoresChanged,
    /// The residue selection changed. Signal-only: the selected residues
    /// live on the `Document`, and consumers re-read them (the GUI
    /// selection mirror + selection-gated action catalog; viso's
    /// per-entity highlight). A selection is ambient, not a geometry
    /// mutation, so the render projector and the plugin broadcaster
    /// ignore it.
    SelectionChanged,
}
