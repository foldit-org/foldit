//! `HistoryError` — typed refusals from the mutation surface, plus
//! the `BestKind` enum that names which "best" cursor a recompute
//! affected.

use molex::entity::molecule::id::EntityId;

use super::{CheckpointId, EntitySnapshotId};

// ── Errors ─────────────────────────────────────────────────────────────

/// Error returned by every fallible [`History`] mutation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryError {
    /// A streaming action is already in flight; the caller must
    /// `commit_action` or `abort_action` before starting a new one.
    ActiveActionInProgress,
    /// No streaming action is in flight; `update` / `commit` / `abort`
    /// have nothing to operate on.
    NoOngoingAction,
    /// A mutation tried to touch an entity locked by the running
    /// action. (Multi-client locks live one layer up — see
    /// [`OngoingState`] doc.)
    EntityLocked { entity: EntityId },
    /// Entity is not part of any current checkpoint or lane.
    UnknownEntity { entity: EntityId },
    /// Snapshot id does not refer to a live snapshot on the named lane.
    UnknownSnapshot {
        entity: EntityId,
        id: EntitySnapshotId,
    },
    /// Checkpoint id does not refer to a live checkpoint.
    UnknownCheckpoint { id: CheckpointId },
    /// Branch hint did not match any child of the current head.
    NoSuchBranch,
    /// `undo` was called at the root (no parent).
    AlreadyAtRoot,
    /// `redo` was called with no children.
    NoChildren,
    /// Branch was required because there is more than one child.
    AmbiguousBranch,
    /// `commit_action` / `update_action` mismatch — the active entity
    /// isn't the one the caller addressed. (Today only fires on
    /// internal misuse; reserved for the section-3 surface.)
    EntityMismatch {
        expected: EntityId,
        got: EntityId,
    },
    /// Tentative target — head-pointer moves are not allowed onto a
    /// tentative checkpoint from outside its own action.
    TentativeNotJumpable,
    /// `add_entity` was called with an id that already has a lane.
    EntityAlreadyExists { entity: EntityId },
}

impl std::fmt::Display for HistoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HistoryError::ActiveActionInProgress => f.write_str("an action is already in flight"),
            HistoryError::NoOngoingAction => f.write_str("no action is in flight"),
            HistoryError::EntityLocked { entity } => {
                write!(f, "entity {} is locked by the running action", entity.raw())
            }
            HistoryError::UnknownEntity { entity } => {
                write!(f, "unknown entity {}", entity.raw())
            }
            HistoryError::UnknownSnapshot { entity, id } => write!(
                f,
                "unknown snapshot {:?} on entity {}",
                id,
                entity.raw()
            ),
            HistoryError::UnknownCheckpoint { id } => {
                write!(f, "unknown checkpoint {id:?}")
            }
            HistoryError::NoSuchBranch => f.write_str("no such branch"),
            HistoryError::AlreadyAtRoot => f.write_str("already at root"),
            HistoryError::NoChildren => f.write_str("no children"),
            HistoryError::AmbiguousBranch => {
                f.write_str("branch hint required: head has multiple children")
            }
            HistoryError::EntityMismatch { expected, got } => write!(
                f,
                "entity mismatch (expected {}, got {})",
                expected.raw(),
                got.raw()
            ),
            HistoryError::TentativeNotJumpable => {
                f.write_str("cannot jump onto a tentative checkpoint")
            }
            HistoryError::EntityAlreadyExists { entity } => {
                write!(f, "entity {} already has a lane", entity.raw())
            }
        }
    }
}

impl std::error::Error for HistoryError {}

/// Which best cursor was recomputed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BestKind {
    /// Highest raw Rosetta score.
    Best,
    /// Highest filter-passing raw score.
    BestThatCounts,
}
