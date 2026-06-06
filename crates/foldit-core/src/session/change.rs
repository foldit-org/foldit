//! The typed change-event stream.
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

use super::Session;

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
    /// The session focus changed (Tab-cycle / reset). Signal-only: the
    /// focus value lives on the `Document`, and consumers re-read it (the
    /// GUI focused-entity + focus-gated action catalog; viso's camera
    /// framing mirror). Focus is ambient, not a geometry mutation, so the
    /// render projector and the plugin broadcaster ignore it.
    FocusChanged,
    /// The active puzzle's tutorial-bubble cursor advanced or stepped back.
    /// Signal-only: the bubble sequence and cursor live inside the loaded
    /// puzzle on the `Document`, and the GUI re-reads them to push the
    /// current bubble. Bubbles are ambient tutorial flow, not a geometry
    /// mutation, so the render projector and the plugin broadcaster ignore
    /// it.
    BubbleChanged,
    /// The loaded puzzle changed (a puzzle loaded, or a free-form structure
    /// load dropped the objective). Signal-only: the puzzle add-on lives on
    /// the `Document`, and the GUI re-reads it to push the puzzle panel +
    /// score view. It is not a geometry mutation, so the render projector
    /// and the plugin broadcaster ignore it.
    PuzzleChanged,
    /// The active view options or the active preset changed (a render
    /// option toggled, or a preset applied). Signal-only: the options and
    /// the active-preset name live on the `Document`, and consumers re-read
    /// them (the GUI view panel; viso applies the options to the engine).
    /// View options are not a geometry mutation, so the plugin broadcaster
    /// ignores it.
    ViewOptionsChanged,
}

/// The contract every head-assembly-change consumer implements.
///
/// A consumer reacts to the drained [`SessionUpdate`] batch by
/// re-deriving whatever payload it needs from the authoritative
/// [`Session`] and writing it to its own `Sink`. Nothing flows through
/// the batch except the *signal* of what changed; the consumer reads the
/// current state back out of `session`.
///
/// Generic over `Sink` because the sinks are heterogeneous (viso's
/// engine, the plugin orchestrator, ...). This is a contract, not a
/// dispatch table: `App` calls [`Self::consume`] explicitly on each
/// consumer in tick order. There is no `Vec<dyn>` and no registry — the
/// trait exists so every consumer shares one shape (drained batch +
/// authoritative session + own sink), not so they can be erased behind a
/// trait object.
pub(crate) trait SessionUpdateConsumer<Sink> {
    /// React to the drained `updates` batch by re-deriving payload from
    /// `session` and writing it to `sink`.
    fn consume(&mut self, updates: &[SessionUpdate], session: &Session, sink: &mut Sink);
}
