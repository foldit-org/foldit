//! `HistoryError` - typed refusals from the mutation surface, plus
//! the `BestKind` enum that names which "best" cursor a recompute
//! affected.

use molex::entity::molecule::id::EntityId;

use super::{CheckpointId, EntitySnapshotId};

// ── Errors ─────────────────────────────────────────────────────────────

/// Error returned by every fallible [`History`] mutation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HistoryError {
    /// A streaming action is already in flight; the caller must
    /// `commit_action` or `abort_action` before starting a new one.
    #[error("an action is already in flight")]
    ActiveActionInProgress,
    /// No streaming action is in flight; `update` / `commit` / `abort`
    /// have nothing to operate on.
    #[error("no action is in flight")]
    NoOngoingAction,
    /// A navigation / immediate-commit mutation was refused because an
    /// action is in flight (the committed graph head is frozen while any
    /// pending edit is open). Multi-client locks live one layer up in the
    /// runner's orchestrator.
    #[error("entity {} is locked by the running action", entity.raw())]
    EntityLocked { entity: EntityId },
    /// Entity is not part of any current checkpoint or lane.
    #[error("unknown entity {}", entity.raw())]
    UnknownEntity { entity: EntityId },
    /// Snapshot id does not refer to a live snapshot on the named lane.
    #[error("unknown snapshot {id:?} on entity {}", entity.raw())]
    UnknownSnapshot {
        entity: EntityId,
        id: EntitySnapshotId,
    },
    /// Checkpoint id does not refer to a live checkpoint.
    #[error("unknown checkpoint {id:?}")]
    UnknownCheckpoint { id: CheckpointId },
    /// Branch hint did not match any child of the current head.
    #[error("no such branch")]
    NoSuchBranch,
    /// `undo` was called at the root (no parent).
    #[error("already at root")]
    AlreadyAtRoot,
    /// `redo` was called with no children.
    #[error("no children")]
    NoChildren,
    /// Branch was required because there is more than one child.
    #[error("branch hint required: head has multiple children")]
    AmbiguousBranch,
    /// `commit_action` / `update_action` mismatch - the active entity
    /// isn't the one the caller addressed. (Today only fires on
    /// internal misuse; reserved for the section-3 surface.)
    #[error("entity mismatch (expected {}, got {})", expected.raw(), got.raw())]
    EntityMismatch {
        expected: EntityId,
        got: EntityId,
    },
    /// Tentative target - head-pointer moves are not allowed onto a
    /// tentative checkpoint from outside its own action.
    #[error("cannot jump onto a tentative checkpoint")]
    TentativeNotJumpable,
    /// `add_entity` was called with an id that already has a lane.
    #[error("entity {} already has a lane", entity.raw())]
    EntityAlreadyExists { entity: EntityId },
}

/// Which best cursor was recomputed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BestKind {
    /// Highest raw Rosetta score.
    Best,
    /// Highest filter-passing raw score.
    BestThatCounts,
}
