//! The `SessionUpdate` emit funnel.
//!
//! [`Session::apply`] is the sole pusher onto the private
//! `pending_updates` queue: every public mutator, after performing its
//! state change, emits exactly one [`SessionUpdate`] (or none, where the
//! change is unobservable) through here. Because `pending_updates` is
//! private and `apply` is the only thing that pushes, "one emit per
//! mutator" is a structural invariant rather than a runtime check.
//!
//! `App` drains the queue once per tick via [`Session::take_updates`]
//! and routes the batch to the projectors (the `RunnerProjector` today;
//! the render + GUI projectors in later sessions). `Session` itself holds
//! no projection logic.

use super::{Session, SessionUpdate};

impl Session {
    /// Emit one [`SessionUpdate`]. The single push point onto
    /// `pending_updates`; mutators call it after their state change.
    pub(super) fn apply(&mut self, change: SessionUpdate) {
        self.pending_updates.push(change);
    }

    /// Drain the emitted scene changes. `App` calls this once per tick
    /// and routes the batch to the projectors; always empty in steady
    /// state.
    pub fn take_updates(&mut self) -> Vec<SessionUpdate> {
        std::mem::take(&mut self.pending_updates)
    }

    /// Push a [`SessionUpdate::ViewOptionsChanged`] onto the drain queue.
    /// The view options live on `App` (so they survive a topology swap),
    /// but the change still flows through the one `SessionUpdate` stream the
    /// projectors drain. This thin emitter lets `App` signal a view-options
    /// change without exposing the private [`Self::apply`] funnel or the
    /// `pending_updates` queue.
    pub fn note_view_options_changed(&mut self) {
        self.apply(SessionUpdate::ViewOptionsChanged);
    }
}
