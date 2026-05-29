//! Undo / redo history: per-entity timelines + a unified checkpoint graph.
//!
//! Two cooperating DAGs.
//!
//! 1. **Entity timelines** ("swim lanes") — each [`EntityId`] has its own
//!    [`EntityHistory`], a slotmap of [`EntitySnapshot`]s linked by
//!    parent/children. Snapshots own the entity payload (an
//!    `Arc<MoleculeEntity>`).
//!
//! 2. **Checkpoint graph** (the unified "river") — a slotmap of
//!    [`Checkpoint`]s linked by parent/children. Each checkpoint carries an
//!    `IndexMap<EntityId, EntitySnapshotId>` plus per-checkpoint state
//!    (score, filter status). It does **not** own snapshot payloads.
//!
//! **Cross-DAG invariant.** For every `e ∈ keys(checkpoint_head.entity_heads)`,
//! `checkpoint_head.entity_heads[e] == lane_head(e)`. Asserted at the tail of
//! every DAG-bearing event under `debug_assertions`.
//!
//! **Single record root.** Every checkpoint-bearing event funnels through
//! the private [`History::record`]; the public methods are thin shims
//! that build a [`HistoryEvent`] variant and delegate. Per-cycle byte
//! mutation (`action_update`) and curation (pin / unpin / exclude / budget)
//! do not change DAG topology and do not go through the root.
//!
//! **Lock layering.** `History` enforces only the *action lock* — refusing
//! navigation/mutation that would touch the entity held by an in-flight
//! [`OngoingState::Active`]. Multi-client locking (the case where the runner
//! is server-side and clients are remote) is owned by the runner's
//! orchestrator (its `EntityLockTable`), not by foldit-core; do not push it
//! into this module.

use std::borrow::Cow;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use web_time::SystemTime;

use indexmap::IndexMap;
use molex::entity::molecule::id::EntityId;
use molex::MoleculeEntity;
use slotmap::SlotMap;
use smallvec::SmallVec;

// Slotmap key types and the wire-side `WireId<K>` newtype both live in
// `foldit_gui::wire` so the GUI crate can build wire payloads without
// inverting the dependency direction (foldit → foldit_gui, never
// the reverse). Re-exported through this module for ergonomic
// `foldit::history::CheckpointId` use sites.
pub use foldit_gui::wire::{CheckpointId, EntitySnapshotId};

mod types;
pub use types::{CheckpointKind, EntityActionKind, FilterStatus};

mod storage;
pub use storage::{Checkpoint, CheckpointGraph, EntityHistory, EntitySnapshot, HistoryBudget};

mod ongoing;
pub use ongoing::OngoingState;

mod error;
pub use error::HistoryError;

// ── History (the public type) ──────────────────────────────────────────

/// Two-layer history: per-entity timelines plus a unified checkpoint
/// graph. Owns both DAGs; enforces the cross-DAG invariant; funnels every
/// checkpoint-bearing event through a single private root.
///
/// See module docs for the design contract; see `record` for the
/// single event funnel.
#[derive(Debug, Clone)]
pub struct History {
    /// Per-entity swim lanes. IndexMap so insertion order is the
    /// canonical entity order across the assembly.
    pub(super) lanes: IndexMap<EntityId, EntityHistory>,
    /// Unified checkpoint graph + cursors.
    pub(super) checkpoints: CheckpointGraph,
    /// Streaming-action state machine (G2).
    pub(super) ongoing: OngoingState,
    /// Bumped on push / move / evict — full reproject on the wire.
    pub(super) topology_version: u64,
    /// Bumped on tentative in-place byte mutation — small live-update
    /// payload on the wire.
    pub(super) live_version: u64,
}

/// One operation passed into the private [`History::record`] root.
///
/// Every checkpoint-bearing event is one of these variants. Public
/// methods are thin shims that build a variant and delegate. New
/// operations land here, not on a sibling root (G3).
#[derive(Debug, Clone)]
enum HistoryEvent {
    Begin {
        entity: EntityId,
        kind: CheckpointKind,
        payload: Arc<MoleculeEntity>,
        label: Cow<'static, str>,
    },
    Commit,
    Abort,
    RecordEntityUpdate {
        entity: EntityId,
        kind: CheckpointKind,
        payload: Arc<MoleculeEntity>,
        label: Cow<'static, str>,
        raw_score: Option<f64>,
        game_score: Option<f64>,
    },
    LaneUndo {
        entity: EntityId,
        target: EntitySnapshotId,
    },
    LaneRedo {
        entity: EntityId,
        branch: Option<EntitySnapshotId>,
    },
    Undo,
    Redo {
        branch: Option<CheckpointId>,
    },
    JumpCheckpoint {
        id: CheckpointId,
    },
    AddEntity {
        entity_id: EntityId,
        payload: Arc<MoleculeEntity>,
        kind: CheckpointKind,
        label: Cow<'static, str>,
    },
}

/// Outcome of [`History::record`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HistoryEventOutcome {
    Pushed(CheckpointId),
    HeadMoved {
        from: CheckpointId,
        to: CheckpointId,
    },
    Aborted,
}

impl History {
    /// Build a new history seeded with one root checkpoint and one root
    /// snapshot per entity in `seed`. The root checkpoint's
    /// `entity_heads` indexes those root snapshots in the supplied
    /// order; head and root cursors point at the new root.
    #[must_use]
    pub fn new(
        seed: impl IntoIterator<Item = (EntityId, MoleculeEntity)>,
        source: PathBuf,
    ) -> Self {
        let now = SystemTime::now();
        let mut lanes: IndexMap<EntityId, EntityHistory> = IndexMap::new();
        let mut entity_heads: IndexMap<EntityId, EntitySnapshotId> = IndexMap::new();

        for (eid, entity) in seed {
            let mut snapshots: SlotMap<EntitySnapshotId, EntitySnapshot> = SlotMap::with_key();
            let snap_id = snapshots.insert(EntitySnapshot {
                parent: None,
                children: SmallVec::new(),
                payload: Arc::new(entity),
                kind: EntityActionKind::Loaded,
                label: Cow::Borrowed("Loaded"),
                timestamp: now,
                tentative: false,
                checkpoint_refs: 1,
            });
            lanes.insert(
                eid,
                EntityHistory {
                    snapshots,
                    head: snap_id,
                    root: snap_id,
                },
            );
            entity_heads.insert(eid, snap_id);
        }

        let mut checkpoints: SlotMap<CheckpointId, Checkpoint> = SlotMap::with_key();
        let root_ckpt = checkpoints.insert(Checkpoint {
            parent: None,
            children: SmallVec::new(),
            entity_heads,
            kind: CheckpointKind::Loaded { source },
            label: Cow::Borrowed("Initial state"),
            timestamp: now,
            raw_score: None,
            game_score: None,
            filter_status: FilterStatus::NotEvaluated,
            exclude_from_best: false,
            tentative: false,
        });

        let history = Self {
            lanes,
            checkpoints: CheckpointGraph {
                checkpoints,
                head: root_ckpt,
                root: root_ckpt,
                best: None,
                best_that_counts: None,
                pinned: HashSet::new(),
                budget: HistoryBudget::default(),
            },
            ongoing: OngoingState::Idle,
            topology_version: 0,
            live_version: 0,
        };

        if cfg!(debug_assertions) {
            history.assert_invariant();
        }
        history
    }

    // ── Read accessors ─────────────────────────────────────────────────

    /// Read access to the checkpoint graph.
    #[must_use]
    pub fn checkpoints(&self) -> &CheckpointGraph {
        &self.checkpoints
    }

    /// Read access to one entity's lane.
    #[must_use]
    pub fn lane(&self, entity: EntityId) -> Option<&EntityHistory> {
        self.lanes.get(&entity)
    }

    /// Iterate (entity_id, lane) pairs in canonical order.
    pub fn lanes(&self) -> impl Iterator<Item = (EntityId, &EntityHistory)> {
        self.lanes.iter().map(|(eid, lane)| (*eid, lane))
    }

    /// Current ongoing-action state.
    #[must_use]
    pub fn ongoing(&self) -> &OngoingState {
        &self.ongoing
    }

    /// Topology version — bumped on push / move / evict. Triggers full
    /// reproject on the wire.
    #[must_use]
    pub fn topology_version(&self) -> u64 {
        self.topology_version
    }

    /// Live version — bumped on per-cycle in-place mutation. Triggers
    /// the small live-update payload on the wire.
    #[must_use]
    pub fn live_version(&self) -> u64 {
        self.live_version
    }

    /// Lookup helper.
    #[must_use]
    pub fn checkpoint(&self, id: CheckpointId) -> Option<&Checkpoint> {
        self.checkpoints.checkpoint(id)
    }

    /// Lookup helper.
    #[must_use]
    pub fn snapshot(&self, entity: EntityId, id: EntitySnapshotId) -> Option<&EntitySnapshot> {
        self.lanes.get(&entity)?.snapshot(id)
    }

    // ── Public surface — thin shims over record ───────────────────────

    /// Begin a streaming action on `entity`. Pushes a tentative snapshot
    /// (parent = current lane head) and a tentative checkpoint
    /// (parent = current graph head). Refused while another action is
    /// in flight.
    pub fn begin_action(
        &mut self,
        entity: EntityId,
        kind: CheckpointKind,
        payload: Arc<MoleculeEntity>,
        label: Cow<'static, str>,
    ) -> Result<CheckpointId, HistoryError> {
        let result = self.record(HistoryEvent::Begin {
            entity,
            kind,
            payload,
            label,
        })?;
        match result {
            HistoryEventOutcome::Pushed(id) => Ok(id),
            _ => unreachable!("Begin always pushes"),
        }
    }

    /// Per-cycle update. Mutates the tentative snapshot's payload via
    /// `Arc::make_mut`, updates the tentative checkpoint's score, bumps
    /// `live_version` only. Does NOT change DAG topology and is not
    /// routed through `record`.
    ///
    /// Returns `NoOngoingAction` if no action is in flight.
    pub fn action_update(
        &mut self,
        raw_score: Option<f64>,
        game_score: Option<f64>,
        filter_status: Option<FilterStatus>,
        mutate: impl FnOnce(&mut MoleculeEntity),
    ) -> Result<(), HistoryError> {
        let (entity, snap_id, ckpt_id) = match &self.ongoing {
            OngoingState::Idle => return Err(HistoryError::NoOngoingAction),
            OngoingState::Active {
                entity,
                tentative_snapshot,
                tentative_checkpoint,
                ..
            } => (*entity, *tentative_snapshot, *tentative_checkpoint),
        };

        let lane = self
            .lanes
            .get_mut(&entity)
            .expect("active lane must exist (G8)");
        let snap = lane
            .snapshots
            .get_mut(snap_id)
            .expect("tentative snapshot must exist (G8)");
        let payload = Arc::make_mut(&mut snap.payload);
        mutate(payload);

        let ckpt = self
            .checkpoints
            .checkpoints
            .get_mut(ckpt_id)
            .expect("tentative checkpoint must exist (G8)");
        if let Some(s) = raw_score {
            ckpt.raw_score = Some(s);
        }
        if let Some(s) = game_score {
            ckpt.game_score = Some(s);
        }
        if let Some(fs) = filter_status {
            ckpt.filter_status = fs;
        }

        self.live_version = self.live_version.saturating_add(1);

        if cfg!(debug_assertions) {
            self.assert_invariant();
        }
        Ok(())
    }

    /// Commit the in-flight action. Flips tentative flags to `false`;
    /// recomputes best cursors; transitions back to `Idle`.
    pub fn commit_action(&mut self) -> Result<CheckpointId, HistoryError> {
        match self.record(HistoryEvent::Commit)? {
            HistoryEventOutcome::Pushed(id) => Ok(id),
            _ => unreachable!("Commit returns the now-real checkpoint"),
        }
    }

    /// Abort the in-flight action. Removes the tentative snapshot from
    /// its lane and the tentative checkpoint from the graph; head
    /// pointers fall back to their parents; transitions to `Idle`.
    pub fn abort_action(&mut self) -> Result<(), HistoryError> {
        match self.record(HistoryEvent::Abort)? {
            HistoryEventOutcome::Aborted => Ok(()),
            _ => unreachable!("Abort returns Aborted"),
        }
    }

    /// Atomic non-streaming entity update — used by RFD3-final / MPNN
    /// results, manual moves, etc. Pushes one snapshot + one checkpoint
    /// with `tentative = false` immediately. Refused while `Active`.
    /// Optional `raw_score` / `game_score` are stamped on the new
    /// checkpoint (G7: caller carries both; projection picks at read).
    pub fn record_entity_update(
        &mut self,
        entity: EntityId,
        kind: CheckpointKind,
        payload: Arc<MoleculeEntity>,
        label: Cow<'static, str>,
        raw_score: Option<f64>,
        game_score: Option<f64>,
    ) -> Result<CheckpointId, HistoryError> {
        match self.record(HistoryEvent::RecordEntityUpdate {
            entity,
            kind,
            payload,
            label,
            raw_score,
            game_score,
        })? {
            HistoryEventOutcome::Pushed(id) => Ok(id),
            _ => unreachable!("RecordEntityUpdate always pushes"),
        }
    }

    /// Move `entity`'s lane head to `target`. Pushes a `LaneUndo`
    /// checkpoint with `entity_heads` mirroring the new lane head;
    /// no new snapshot.
    pub fn lane_undo(
        &mut self,
        entity: EntityId,
        target: EntitySnapshotId,
    ) -> Result<CheckpointId, HistoryError> {
        match self.record(HistoryEvent::LaneUndo { entity, target })? {
            HistoryEventOutcome::Pushed(id) => Ok(id),
            _ => unreachable!("LaneUndo always pushes a LaneUndo checkpoint"),
        }
    }

    /// Move `entity`'s lane head to a child of the current lane head;
    /// `branch` picks among multiple children. Pushes a `LaneUndo`
    /// checkpoint (the kind covers redo too — both directions move the
    /// lane head along the lane DAG).
    pub fn lane_redo(
        &mut self,
        entity: EntityId,
        branch: Option<EntitySnapshotId>,
    ) -> Result<CheckpointId, HistoryError> {
        match self.record(HistoryEvent::LaneRedo { entity, branch })? {
            HistoryEventOutcome::Pushed(id) => Ok(id),
            _ => unreachable!("LaneRedo always pushes a LaneUndo checkpoint"),
        }
    }

    /// Move checkpoint head to its parent. Mirror lane heads to match
    /// the new head's `entity_heads`. Returns `None` at the root.
    pub fn undo(&mut self) -> Result<Option<CheckpointId>, HistoryError> {
        match self.record(HistoryEvent::Undo) {
            Ok(HistoryEventOutcome::HeadMoved { to, .. }) => Ok(Some(to)),
            Err(HistoryError::AlreadyAtRoot) => Ok(None),
            Err(e) => Err(e),
            Ok(_) => unreachable!("Undo returns HeadMoved or AlreadyAtRoot"),
        }
    }

    /// Move checkpoint head to a child. `branch` picks among multiple
    /// children. Returns `None` at a leaf.
    pub fn redo(
        &mut self,
        branch: Option<CheckpointId>,
    ) -> Result<Option<CheckpointId>, HistoryError> {
        match self.record(HistoryEvent::Redo { branch }) {
            Ok(HistoryEventOutcome::HeadMoved { to, .. }) => Ok(Some(to)),
            Err(HistoryError::NoChildren) => Ok(None),
            Err(e) => Err(e),
            Ok(_) => unreachable!("Redo returns HeadMoved or NoChildren"),
        }
    }

    /// Introduce a new entity (and its lane) into history. Pushes a
    /// fresh checkpoint whose `entity_heads` is the parent's plus
    /// `entity_id → root_snapshot`. Refused while `Active` (the running
    /// action freezes the assembly per § Lock semantics).
    ///
    /// Used by [`crate::session::Session::promote_preview`] to
    /// move a previously-transient entity into history.
    pub fn add_entity(
        &mut self,
        entity_id: EntityId,
        payload: Arc<MoleculeEntity>,
        kind: CheckpointKind,
        label: Cow<'static, str>,
    ) -> Result<CheckpointId, HistoryError> {
        match self.record(HistoryEvent::AddEntity {
            entity_id,
            payload,
            kind,
            label,
        })? {
            HistoryEventOutcome::Pushed(id) => Ok(id),
            _ => unreachable!("AddEntity always pushes"),
        }
    }

    /// Move checkpoint head to `id`. Mirror lane heads to match.
    pub fn jump_checkpoint(&mut self, id: CheckpointId) -> Result<CheckpointId, HistoryError> {
        match self.record(HistoryEvent::JumpCheckpoint { id })? {
            HistoryEventOutcome::HeadMoved { to, .. } => Ok(to),
            _ => unreachable!("JumpCheckpoint returns HeadMoved"),
        }
    }

    // ── Curation (no DAG topology change — direct field writes) ───────

    /// Pin a checkpoint as user-marked best.
    pub fn pin_checkpoint(&mut self, id: CheckpointId) -> Result<(), HistoryError> {
        if !self.checkpoints.checkpoints.contains_key(id) {
            return Err(HistoryError::UnknownCheckpoint { id });
        }
        let _ = self.checkpoints.pinned.insert(id);
        Ok(())
    }

    /// Unpin a checkpoint.
    pub fn unpin_checkpoint(&mut self, id: CheckpointId) -> Result<(), HistoryError> {
        if !self.checkpoints.checkpoints.contains_key(id) {
            return Err(HistoryError::UnknownCheckpoint { id });
        }
        let _ = self.checkpoints.pinned.remove(&id);
        Ok(())
    }

    /// Set the "exclude from best" flag.
    pub fn set_exclude_from_best(
        &mut self,
        id: CheckpointId,
        exclude: bool,
    ) -> Result<(), HistoryError> {
        let ckpt = self
            .checkpoints
            .checkpoints
            .get_mut(id)
            .ok_or(HistoryError::UnknownCheckpoint { id })?;
        ckpt.exclude_from_best = exclude;
        Ok(())
    }

    /// Replace the eviction budget.
    pub fn set_budget(&mut self, budget: HistoryBudget) {
        self.checkpoints.budget = budget;
    }

    /// Stamp `raw_score` / `game_score` on the current head checkpoint
    /// in place. Bumps `live_version` only — DAG topology unchanged, no
    /// new checkpoint, no new snapshot. Idempotent on `(None, None)`.
    ///
    /// This is the right call for cycle-zero scoring during session init
    /// (Rosetta streams a score before the user takes any action). It
    /// avoids the pre-fix behavior where every init cycle pushed a fresh
    /// checkpoint on top of root + AddEntity.
    pub fn set_head_scores(&mut self, raw_score: Option<f64>, game_score: Option<f64>) {
        if raw_score.is_none() && game_score.is_none() {
            return;
        }
        let head_id = self.checkpoints.head;
        let ckpt = self
            .checkpoints
            .checkpoints
            .get_mut(head_id)
            .expect("head checkpoint must exist (G8)");
        if let Some(s) = raw_score {
            ckpt.raw_score = Some(s);
        }
        if let Some(s) = game_score {
            ckpt.game_score = Some(s);
        }
        self.live_version = self.live_version.saturating_add(1);

        if cfg!(debug_assertions) {
            self.assert_invariant();
        }
    }

    // ── Private root: every DAG-bearing event funnels here (G3) ──────

    /// The single root through which every checkpoint- or lane-DAG-
    /// bearing event passes. Validates the [`OngoingState`]
    /// preconditions, performs the mutation, updates `checkpoint_refs`,
    /// runs eviction, bumps `topology_version`, and asserts the cross-
    /// DAG invariant (G8).
    ///
    /// New events land here as a new [`HistoryEvent`] variant. A
    /// sibling root would carry state this function doesn't know about
    /// and is therefore illegal (G3).
    fn record(&mut self, event: HistoryEvent) -> Result<HistoryEventOutcome, HistoryError> {
        // ── Action-lock pre-check ─────────────────────────────────────
        // While Active, the only legal events are Commit / Abort.
        // Per strategy doc § Lock semantics, the running action
        // freezes every lane (not just its own); navigation and record
        // updates are refused uniformly.
        if let OngoingState::Active { entity: locked, .. } = &self.ongoing {
            let locked = *locked;
            match &event {
                HistoryEvent::Commit | HistoryEvent::Abort => {}
                HistoryEvent::Begin { .. } | HistoryEvent::RecordEntityUpdate { .. } => {
                    return Err(HistoryError::ActiveActionInProgress)
                }
                HistoryEvent::LaneUndo { .. }
                | HistoryEvent::LaneRedo { .. }
                | HistoryEvent::Undo
                | HistoryEvent::Redo { .. }
                | HistoryEvent::JumpCheckpoint { .. } => {
                    return Err(HistoryError::EntityLocked { entity: locked })
                }
                HistoryEvent::AddEntity { .. } => return Err(HistoryError::ActiveActionInProgress),
            }
        }

        // Linear-undo invariant: after a push, every checkpoint must
        // lie on the root → head path, and every snapshot must lie on
        // its lane's root → head path. Navigation-only events
        // (Undo / Redo / JumpCheckpoint) skip the prune so the user
        // can move the cursor without losing the redo path; the next
        // *push* drops it (classic editor undo).
        let is_push = matches!(
            event,
            HistoryEvent::Begin { .. }
                | HistoryEvent::RecordEntityUpdate { .. }
                | HistoryEvent::LaneUndo { .. }
                | HistoryEvent::LaneRedo { .. }
                | HistoryEvent::AddEntity { .. }
        );

        let result = match event {
            HistoryEvent::Begin {
                entity,
                kind,
                payload,
                label,
            } => self.do_begin(entity, kind, payload, label)?,
            HistoryEvent::Commit => self.do_commit()?,
            HistoryEvent::Abort => self.do_abort()?,
            HistoryEvent::RecordEntityUpdate {
                entity,
                kind,
                payload,
                label,
                raw_score,
                game_score,
            } => {
                self.do_record_entity_update(entity, kind, payload, label, raw_score, game_score)?
            }
            HistoryEvent::LaneUndo { entity, target } => self.do_lane_undo(entity, target)?,
            HistoryEvent::LaneRedo { entity, branch } => self.do_lane_redo(entity, branch)?,
            HistoryEvent::Undo => self.do_undo()?,
            HistoryEvent::Redo { branch } => self.do_redo(branch)?,
            HistoryEvent::JumpCheckpoint { id } => self.do_jump(id)?,
            HistoryEvent::AddEntity {
                entity_id,
                payload,
                kind,
                label,
            } => self.do_add_entity(entity_id, payload, kind, label)?,
        };

        if is_push {
            self.prune_to_head_path();
        }

        self.evict_to_budget();
        self.topology_version = self.topology_version.saturating_add(1);

        if cfg!(debug_assertions) {
            self.assert_invariant();
        }

        Ok(result)
    }
}

mod eviction;
mod invariant;
mod record;

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests;
