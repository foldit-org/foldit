//! The typed change-event stream.
//!
//! [`SessionUpdate`] is the single event type emitted by the `Session`
//! mutation funnel and consumed by the projectors. Every observable
//! mutation produces exactly one `SessionUpdate`; the projectors decide
//! independently how (or whether) to react to each variant.
//!
//! The enum is signal-only: it names *what changed*, not the payload.
//! Each projector re-derives whatever it needs from the [`Session`].
//! Carrying payloads here would duplicate state that the projectors are
//! already reading from `Session` and invite drift.

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
/// consumer in tick order. There is no `Vec<dyn>` and no registry - the
/// trait exists so every consumer shares one shape (drained batch +
/// authoritative session + own sink), not so they can be erased behind a
/// trait object.
pub trait SessionUpdateConsumer<Sink> {
    /// React to the drained `updates` batch by re-deriving payload from
    /// `session` and writing it to `sink`. Takes `&mut session` so a
    /// consumer can update its own per-session diff baselines on the
    /// session's `VizState` (the render projector does); most consumers
    /// only read.
    fn consume(&mut self, updates: &[SessionUpdate], session: &mut Session, sink: &mut Sink);
}
