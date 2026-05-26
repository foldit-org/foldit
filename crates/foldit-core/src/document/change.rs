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
/// Emitted by the [`Document::apply`] funnel and consumed by the
/// projectors. As of RX6 the only consumer is the `PluginBroadcaster`,
/// which reads the discriminant and `Edit::tentative` (it re-derives the
/// assembly from the `Document` rather than from these payloads). The
/// per-variant payload fields (`edit`, `from`/`to`, `entity`) are
/// populated now but only *read* once the render projector (RX7) and GUI
/// projector (RX9) land; the `dead_code` allow covers that gap — drop it
/// when those projectors consume the fields. (Derived `Debug`/`Clone` are
/// ignored by dead-code analysis, so they don't keep the fields live.)
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
}
