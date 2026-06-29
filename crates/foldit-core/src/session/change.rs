//! The typed change-event stream.
//!
//! [`SessionUpdate`] is the single event type emitted by the `Session`
//! mutation funnel and consumed by the projectors. Every observable
//! mutation produces exactly one `SessionUpdate`; the projectors decide
//! independently how (or whether) to react to each variant.
//!
//! The enum is signal-only: it names *what changed*, not the payload.
//! Each projector re-derives whatever it needs from the [`Session`](super::Session).
//! Carrying payloads here would duplicate state that the projectors are
//! already reading from `Session` and invite drift.

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
    /// A preview (transient) entity's geometry was updated in place
    /// (e.g. a streaming diffusion frame). Same id set as before; the
    /// render projector republishes coords without a topology swap.
    PreviewUpdated,
    /// A preview (transient) entity was discarded from the overlay.
    PreviewDiscarded,
    /// A head / edit / checkpoint score *value* changed.
    ScoresChanged,
    /// The residue selection changed.
    SelectionChanged,
    /// The session focus changed (Tab-cycle / reset).
    FocusChanged,
    /// The active puzzle's tutorial-bubble cursor advanced or stepped back.
    BubbleChanged,
    /// The loaded puzzle changed (a puzzle loaded, or a free-form structure
    /// load dropped the objective).
    PuzzleChanged,
    /// The active view options or the active preset changed (a render
    /// option toggled, or a preset applied).
    ViewOptionsChanged,
    /// A per-entity ambient appearance override changed (a field merged into
    /// or removed from an entity's overrides).
    EntityAppearanceChanged,
    /// A history curation flag changed (pin / unpin / exclude-from-best).
    CurationChanged,
}

impl SessionUpdate {
    /// A geometry/coordinate change that makes the rendered scene, scores,
    /// and viz overlays stale; the signal both projectors and the at-rest
    /// refreshes key off.
    pub const fn is_geometry(&self) -> bool {
        matches!(
            self,
            Self::Edit { .. }
                | Self::HeadMoved
                | Self::PreviewAdded
                | Self::PreviewUpdated
                | Self::PreviewDiscarded
        )
    }
}

/// The contract every `SessionUpdate` consumer implements: react to the
/// drained batch by re-deriving payload from its `Sources` and writing it
/// to its own `Sink`.
pub trait SessionUpdateConsumer {
    /// The borrowed inputs the consumer reads from.
    type Sources<'a>;
    /// The single output the consumer writes to.
    type Sink;
    /// The value returned to the caller after a drain.
    type Out;
    fn consume(
        &mut self,
        updates: &[SessionUpdate],
        sources: Self::Sources<'_>,
        sink: &mut Self::Sink,
    ) -> Self::Out;
}
