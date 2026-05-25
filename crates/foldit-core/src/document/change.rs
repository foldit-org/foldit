//! The typed change-event spine.
//!
//! [`SceneChange`] is the single event type emitted by the `Document`
//! mutation funnel (RX6) and consumed by the three projectors: render
//! (RX7), plugin broadcast (RX8), and GUI (RX9). Every observable
//! mutation produces exactly one `SceneChange`; the projectors decide
//! independently how (or whether) to react to each variant.
//!
//! Rather than flatten molex's structural-edit taxonomy, the [`Edit`]
//! variant wraps the whole [`AssemblyEdit`] enum verbatim. There is
//! therefore zero duplication of the molex edit vocabulary here, and a
//! render projector can hand the inner edit straight to
//! `Assembly::apply_edits`.
//!
//! [`Edit`]: SceneChange::Edit

use molex::entity::molecule::id::EntityId;
use molex::ops::AssemblyEdit;

use crate::history::CheckpointId;

/// A single observable change to the scene.
///
/// Consumed by the `Document::apply` funnel + projectors starting in
/// RX6; remove the `allow` then.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum SceneChange {
    /// A structural/coordinate edit, wrapping molex's whole
    /// [`AssemblyEdit`] enum. `tentative` marks a per-cycle
    /// live/streaming edit (e.g. a pull-drag or mid-action plugin frame)
    /// vs a committed one.
    Edit { edit: AssemblyEdit, tentative: bool },
    /// The history head moved (undo / redo / jump). Projectors rebuild;
    /// the plugin broadcaster sends a Full snapshot.
    HeadMoved { from: CheckpointId, to: CheckpointId },
    /// A preview (transient) entity was added to the overlay.
    PreviewAdded { entity: EntityId },
    /// A preview (transient) entity was discarded from the overlay.
    PreviewDiscarded { entity: EntityId },
    /// The head checkpoint's scores changed. Routed to the GUI projector
    /// only (plugins compute their own scores).
    ScoresUpdated,
}
