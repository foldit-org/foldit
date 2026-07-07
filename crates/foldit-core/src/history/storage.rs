//! Per-entity timelines + the unified checkpoint graph storage.
//!
//! These are the data structures the rest of the module mutates.
//! Pure structs + small accessors; the actual mutation logic lives
//! in `super::record` and gets routed through `record`.

use std::borrow::Cow;
use std::collections::HashSet;
use std::sync::Arc;
use web_time::SystemTime;

use indexmap::IndexMap;
use molex::entity::molecule::id::EntityId;
use molex::MoleculeEntity;
use slotmap::SlotMap;
use smallvec::SmallVec;

use super::{CheckpointId, CheckpointKind, EntitySnapshotId, FilterStatus};

/// Memory cap policy. Two budgets, two policies.
#[derive(Debug, Clone, Copy)]
pub struct HistoryBudget {
    /// Maximum live checkpoints before eviction kicks in.
    pub max_checkpoints: usize,
    /// Maximum snapshots per entity lane before eviction kicks in.
    pub max_snapshots_per_lane: usize,
}

impl Default for HistoryBudget {
    fn default() -> Self {
        Self {
            max_checkpoints: 200,
            max_snapshots_per_lane: 100,
        }
    }
}

/// One node on an entity's swim lane.
#[derive(Debug, Clone)]
pub struct EntitySnapshot {
    /// Parent on this lane. `None` only for the lane root.
    pub parent: Option<EntitySnapshotId>,
    /// Children - branches diverging from this snapshot.
    pub children: SmallVec<[EntitySnapshotId; 2]>,
    /// Entity payload at this point.
    pub payload: Arc<MoleculeEntity>,
    /// Display label.
    pub label: Cow<'static, str>,
    /// Wall-clock timestamp.
    pub timestamp: SystemTime,
    /// True iff this snapshot is the open tentative of an in-flight
    /// action (named by exactly one entry in `History::pending`). Always
    /// the head of its lane while set; flipped to `false` at commit.
    pub tentative: bool,
    /// Number of live checkpoints whose `entity_heads` references this
    /// snapshot. Refuses eviction while > 0.
    pub checkpoint_refs: u32,
}

/// One entity's swim lane.
#[derive(Debug, Clone)]
pub struct EntityHistory {
    pub(super) snapshots: SlotMap<EntitySnapshotId, EntitySnapshot>,
    pub(super) head: EntitySnapshotId,
    pub(super) root: EntitySnapshotId,
}

impl EntityHistory {
    /// The current head snapshot id.
    #[must_use]
    pub const fn head(&self) -> EntitySnapshotId {
        self.head
    }

    /// Look up a snapshot by id.
    #[must_use]
    pub fn snapshot(&self, id: EntitySnapshotId) -> Option<&EntitySnapshot> {
        self.snapshots.get(id)
    }
}

/// One node in the unified checkpoint graph.
#[derive(Debug, Clone)]
pub struct Checkpoint {
    /// Parent in the checkpoint DAG. `None` only for the root.
    pub parent: Option<CheckpointId>,
    /// Branch children.
    pub children: SmallVec<[CheckpointId; 2]>,
    /// Tuple of pointers into the entity timelines. `IndexMap` order is the
    /// canonical entity order; preserved across pushes / lane undo /
    /// jump.
    pub entity_heads: IndexMap<EntityId, EntitySnapshotId>,
    /// Typed user-visible action.
    pub kind: CheckpointKind,
    /// Display label.
    pub label: Cow<'static, str>,
    /// Wall-clock timestamp.
    pub timestamp: SystemTime,
    /// Rosetta REU score.
    pub raw_score: Option<f64>,
    /// Game-points score.
    pub game_score: Option<f64>,
    /// RAW (unweighted) per-term breakdown of this checkpoint's
    /// composition, the source of truth for per-residue coloring. `None`
    /// until a score with a breakdown is stamped on this node; aligned to
    /// the session's `term_names`. Crate-private (the type is): the render
    /// projector re-derives the displayed colors from it on `ScoresChanged`.
    pub(crate) breakdown: Option<crate::scores::StoredBreakdown>,
    /// Filter evaluation status.
    pub filter_status: FilterStatus,
    /// User-set "skip me when computing best" flag.
    pub exclude_from_best: bool,
}

/// The unified checkpoint graph plus its cursors.
#[derive(Debug, Clone)]
pub struct CheckpointGraph {
    pub(super) checkpoints: SlotMap<CheckpointId, Checkpoint>,
    pub(super) head: CheckpointId,
    pub(super) root: CheckpointId,
    pub(super) best: Option<CheckpointId>,
    pub(super) best_that_counts: Option<CheckpointId>,
    pub(super) pinned: HashSet<CheckpointId>,
    pub(super) budget: HistoryBudget,
}

impl CheckpointGraph {
    /// Current head id.
    #[must_use]
    pub const fn head(&self) -> CheckpointId {
        self.head
    }

    /// Root id.
    #[must_use]
    pub const fn root(&self) -> CheckpointId {
        self.root
    }

    /// Look up a checkpoint by id.
    #[must_use]
    pub fn checkpoint(&self, id: CheckpointId) -> Option<&Checkpoint> {
        self.checkpoints.get(id)
    }

    /// Iterate all live checkpoints in slotmap order.
    pub fn iter(&self) -> impl Iterator<Item = (CheckpointId, &Checkpoint)> {
        self.checkpoints.iter()
    }

    /// Best-by-raw cursor (`None` until scoring populates it).
    #[must_use]
    pub const fn best(&self) -> Option<CheckpointId> {
        self.best
    }

    /// Best-that-counts cursor (`None` until scoring populates it).
    #[must_use]
    pub const fn best_that_counts(&self) -> Option<CheckpointId> {
        self.best_that_counts
    }

    /// Whether `id` is user-pinned.
    #[must_use]
    pub fn is_pinned(&self, id: CheckpointId) -> bool {
        self.pinned.contains(&id)
    }
}
