//! `PendingEdit` — one in-flight action's open composition.
//!
//! Replaces the old ambient single-action flag. Each in-flight action is
//! keyed by a `request_id` in `History::pending`; the value is a
//! `PendingEdit` naming the lane(s) it holds open and carrying the live
//! composition score streamed into it before commit.
//!
//! No committed checkpoint is minted at begin: the committed graph head
//! stays put for the whole action so each commit composes from a stable
//! committed head. That leaves the streamed score with no checkpoint to
//! live on, so it accumulates here and is stamped onto the checkpoint
//! minted at commit (a score is a property of a whole-pose composition;
//! the pending edit *is* the transient composition node, symmetric with a
//! committed checkpoint). A lane appears in at most one pending edit at a
//! time.

use molex::entity::molecule::id::EntityId;
use smallvec::SmallVec;

use super::{CheckpointKind, EntitySnapshotId, FilterStatus};

/// One in-flight action's open composition.
#[derive(Debug, Clone)]
pub(crate) struct PendingEdit {
    /// The lane(s) this edit holds open, each pinned to its tentative
    /// snapshot. One entry in the single-entity case; the `SmallVec`
    /// capacity anticipates multi-lane fan-out without a heap alloc.
    pub(crate) lanes: SmallVec<[(EntityId, EntitySnapshotId); 1]>,
    /// The action kind, mirrored onto the checkpoint minted at commit.
    pub(crate) kind: CheckpointKind,
    /// Live raw (REU) score streamed into the open composition. `None`
    /// until the scorer reports; transferred to the committed checkpoint
    /// at commit so the committed parent is never touched mid-action.
    pub(crate) raw_score: Option<f64>,
    /// Live game-points score; same lifecycle as `raw_score`.
    pub(crate) game_score: Option<f64>,
    /// Live filter status of the open composition.
    pub(crate) filter_status: FilterStatus,
}
