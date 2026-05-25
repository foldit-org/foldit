//! The `SceneChange` emit funnel.
//!
//! [`Document::apply`] is the sole pusher onto the private
//! `pending_changes` queue: every public mutator, after performing its
//! state change, emits exactly one [`SceneChange`] (or none, where the
//! change is unobservable) through here. Because `pending_changes` is
//! private and `apply` is the only thing that pushes, "one emit per
//! mutator" is a structural invariant rather than a runtime check.
//!
//! `App` drains the queue once per tick via [`Document::take_scene_changes`]
//! and routes the batch to the projectors (the `PluginBroadcaster` today;
//! the render + GUI projectors in later sessions). `Document` itself holds
//! no projection logic.

use super::{Document, SceneChange};

impl Document {
    /// Emit one [`SceneChange`]. The single push point onto
    /// `pending_changes`; mutators call it after their state change.
    pub(super) fn apply(&mut self, change: SceneChange) {
        self.pending_changes.push(change);
    }

    /// Drain the emitted scene changes. `App` calls this once per tick
    /// and routes the batch to the projectors; always empty in steady
    /// state.
    pub fn take_scene_changes(&mut self) -> Vec<SceneChange> {
        std::mem::take(&mut self.pending_changes)
    }
}
