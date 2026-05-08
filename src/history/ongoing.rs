//! `OngoingState` — the streaming-action state machine. Only `Idle`
//! and `Active` exist by design (G2): no `None` flag that lets a
//! tentative live without an action.

use molex::entity::molecule::id::EntityId;

use super::{CheckpointId, CheckpointKind, EntitySnapshotId};

// ── Action lifecycle state machine (G2) ────────────────────────────────

/// State of the in-flight streaming action, if any.
///
/// `Idle` is the resting state; `Active` carries every id needed to
/// commit / abort / cycle-update without a fallible lookup. The tentative
/// snapshot and checkpoint exist iff `Active`; this is asserted by the
/// cross-DAG invariant (G8).
#[derive(Debug, Clone)]
pub enum OngoingState {
    /// No action in flight.
    Idle,
    /// An action is streaming Rosetta cycles.
    Active {
        /// Entity whose lane the action is mutating.
        entity: EntityId,
        /// Tentative snapshot at the lane head.
        tentative_snapshot: EntitySnapshotId,
        /// Tentative checkpoint at the graph head.
        tentative_checkpoint: CheckpointId,
        /// Typed action kind (mirrored on the checkpoint).
        kind: CheckpointKind,
    },
}

impl OngoingState {
    /// Whether an action is in flight.
    #[must_use]
    pub fn is_active(&self) -> bool {
        matches!(self, OngoingState::Active { .. })
    }

    /// Entity locked by the running action, if any.
    #[must_use]
    pub fn locked_entity(&self) -> Option<EntityId> {
        match self {
            OngoingState::Idle => None,
            OngoingState::Active { entity, .. } => Some(*entity),
        }
    }
}
