//! `record` dispatch arms (the `do_*` methods) plus the small
//! mutation helpers they share. Lives behind the `record` root
//! (G3): callers go through the public methods on `History` (in
//! `mod.rs`), which build a `HistoryEvent` variant and route it
//! into `record`, which then delegates to one of these arms.

use std::borrow::Cow;
use std::sync::Arc;
use web_time::SystemTime;

use molex::entity::molecule::id::EntityId;
use molex::MoleculeEntity;
use slotmap::SlotMap;
use smallvec::SmallVec;

use super::{
    Checkpoint, CheckpointId, CheckpointKind, EntityActionKind, EntityHistory, EntitySnapshot,
    EntitySnapshotId, FilterStatus, History, HistoryError, HistoryEventOutcome, OngoingState,
};

impl History {
    // ── record dispatch arms ──────────────────────────────────────────

    pub(super) fn do_begin(
        &mut self,
        entity: EntityId,
        kind: CheckpointKind,
        payload: Arc<MoleculeEntity>,
        label: Cow<'static, str>,
    ) -> Result<HistoryEventOutcome, HistoryError> {
        // ongoing == Idle is guaranteed by the caller-side pre-check.
        if !self.lanes.contains_key(&entity) {
            return Err(HistoryError::UnknownEntity { entity });
        }

        let now = SystemTime::now();
        let action_kind = kind
            .entity_action_kind()
            .unwrap_or(EntityActionKind::Loaded);

        // Push tentative snapshot on the entity's lane.
        let lane = self.lanes.get_mut(&entity).expect("checked above");
        let parent = lane.head;
        let new_snap = lane.snapshots.insert(EntitySnapshot {
            parent: Some(parent),
            children: SmallVec::new(),
            payload,
            kind: action_kind,
            label: label.clone(),
            timestamp: now,
            tentative: true,
            checkpoint_refs: 0,
        });
        lane.snapshots[parent].children.push(new_snap);
        lane.head = new_snap;

        // Build the tentative checkpoint's entity_heads — preserve
        // canonical order; replace `entity`'s entry.
        let parent_ckpt_id = self.checkpoints.head;
        let parent_ckpt = &self.checkpoints.checkpoints[parent_ckpt_id];
        let mut entity_heads = parent_ckpt.entity_heads.clone();
        entity_heads.insert(entity, new_snap);

        let new_ckpt = self.checkpoints.checkpoints.insert(Checkpoint {
            parent: Some(parent_ckpt_id),
            children: SmallVec::new(),
            entity_heads,
            kind: kind.clone(),
            label,
            timestamp: now,
            raw_score: None,
            game_score: None,
            filter_status: FilterStatus::NotEvaluated,
            exclude_from_best: false,
            tentative: true,
        });
        self.checkpoints.checkpoints[parent_ckpt_id]
            .children
            .push(new_ckpt);
        self.checkpoints.head = new_ckpt;

        self.inc_refs_for_checkpoint(new_ckpt);

        self.ongoing = OngoingState::Active {
            entity,
            tentative_snapshot: new_snap,
            tentative_checkpoint: new_ckpt,
            kind,
        };

        Ok(HistoryEventOutcome::Pushed(new_ckpt))
    }

    pub(super) fn do_commit(&mut self) -> Result<HistoryEventOutcome, HistoryError> {
        let (entity, snap_id, ckpt_id) = match &self.ongoing {
            OngoingState::Idle => return Err(HistoryError::NoOngoingAction),
            OngoingState::Active {
                entity,
                tentative_snapshot,
                tentative_checkpoint,
                ..
            } => (*entity, *tentative_snapshot, *tentative_checkpoint),
        };

        let lane = self.lanes.get_mut(&entity).expect("active lane (G8)");
        lane.snapshots[snap_id].tentative = false;

        let ckpt = self
            .checkpoints
            .checkpoints
            .get_mut(ckpt_id)
            .expect("active checkpoint (G8)");
        ckpt.tentative = false;

        self.recompute_best();
        self.ongoing = OngoingState::Idle;

        Ok(HistoryEventOutcome::Pushed(ckpt_id))
    }

    pub(super) fn do_abort(&mut self) -> Result<HistoryEventOutcome, HistoryError> {
        let (entity, snap_id, ckpt_id) = match &self.ongoing {
            OngoingState::Idle => return Err(HistoryError::NoOngoingAction),
            OngoingState::Active {
                entity,
                tentative_snapshot,
                tentative_checkpoint,
                ..
            } => (*entity, *tentative_snapshot, *tentative_checkpoint),
        };

        // Remove tentative checkpoint first (drops its refs).
        self.dec_refs_for_checkpoint(ckpt_id);
        self.detach_checkpoint(ckpt_id);
        let removed_ckpt = self
            .checkpoints
            .checkpoints
            .remove(ckpt_id)
            .expect("active checkpoint (G8)");
        let parent_ckpt = removed_ckpt
            .parent
            .expect("tentative is never the root checkpoint");
        self.checkpoints.head = parent_ckpt;

        // Remove tentative snapshot from its lane.
        let lane = self.lanes.get_mut(&entity).expect("active lane (G8)");
        let removed_snap = lane.snapshots.remove(snap_id).expect("active snap (G8)");
        let parent_snap = removed_snap.parent.expect("tentative is never lane root");
        if let Some(parent) = lane.snapshots.get_mut(parent_snap) {
            parent.children.retain(|c| *c != snap_id);
        }
        lane.head = parent_snap;

        self.ongoing = OngoingState::Idle;

        Ok(HistoryEventOutcome::Aborted)
    }

    pub(super) fn do_record_entity_update(
        &mut self,
        entity: EntityId,
        kind: CheckpointKind,
        payload: Arc<MoleculeEntity>,
        label: Cow<'static, str>,
        raw_score: Option<f64>,
        game_score: Option<f64>,
    ) -> Result<HistoryEventOutcome, HistoryError> {
        if !self.lanes.contains_key(&entity) {
            return Err(HistoryError::UnknownEntity { entity });
        }
        let now = SystemTime::now();
        let action_kind = kind
            .entity_action_kind()
            .unwrap_or(EntityActionKind::Loaded);

        let lane = self.lanes.get_mut(&entity).expect("checked above");
        let parent = lane.head;
        let new_snap = lane.snapshots.insert(EntitySnapshot {
            parent: Some(parent),
            children: SmallVec::new(),
            payload,
            kind: action_kind,
            label: label.clone(),
            timestamp: now,
            tentative: false,
            checkpoint_refs: 0,
        });
        lane.snapshots[parent].children.push(new_snap);
        lane.head = new_snap;

        let parent_ckpt_id = self.checkpoints.head;
        let parent_ckpt = &self.checkpoints.checkpoints[parent_ckpt_id];
        let mut entity_heads = parent_ckpt.entity_heads.clone();
        entity_heads.insert(entity, new_snap);

        let new_ckpt = self.checkpoints.checkpoints.insert(Checkpoint {
            parent: Some(parent_ckpt_id),
            children: SmallVec::new(),
            entity_heads,
            kind,
            label,
            timestamp: now,
            raw_score,
            game_score,
            filter_status: FilterStatus::NotEvaluated,
            exclude_from_best: false,
            tentative: false,
        });
        self.checkpoints.checkpoints[parent_ckpt_id]
            .children
            .push(new_ckpt);
        self.checkpoints.head = new_ckpt;

        self.inc_refs_for_checkpoint(new_ckpt);
        self.recompute_best();

        Ok(HistoryEventOutcome::Pushed(new_ckpt))
    }

    pub(super) fn do_lane_undo(
        &mut self,
        entity: EntityId,
        target: EntitySnapshotId,
    ) -> Result<HistoryEventOutcome, HistoryError> {
        let lane = self
            .lanes
            .get_mut(&entity)
            .ok_or(HistoryError::UnknownEntity { entity })?;
        if !lane.snapshots.contains_key(target) {
            return Err(HistoryError::UnknownSnapshot { entity, id: target });
        }
        if lane.snapshots[target].tentative {
            return Err(HistoryError::TentativeNotJumpable);
        }
        lane.head = target;

        self.push_lane_undo_checkpoint(entity, target)
    }

    pub(super) fn do_lane_redo(
        &mut self,
        entity: EntityId,
        branch: Option<EntitySnapshotId>,
    ) -> Result<HistoryEventOutcome, HistoryError> {
        let lane = self
            .lanes
            .get_mut(&entity)
            .ok_or(HistoryError::UnknownEntity { entity })?;
        let head = lane.head;
        let kids: SmallVec<[EntitySnapshotId; 2]> =
            lane.snapshots[head].children.iter().copied().collect();
        let target = match (branch, kids.as_slice()) {
            (_, []) => return Err(HistoryError::NoChildren),
            (Some(b), kids) if kids.contains(&b) => b,
            (Some(_), _) => return Err(HistoryError::NoSuchBranch),
            (None, [only]) => *only,
            (None, _) => return Err(HistoryError::AmbiguousBranch),
        };
        if lane.snapshots[target].tentative {
            return Err(HistoryError::TentativeNotJumpable);
        }
        lane.head = target;

        self.push_lane_undo_checkpoint(entity, target)
    }

    pub(super) fn do_undo(&mut self) -> Result<HistoryEventOutcome, HistoryError> {
        let head = self.checkpoints.head;
        let parent = self.checkpoints.checkpoints[head]
            .parent
            .ok_or(HistoryError::AlreadyAtRoot)?;
        self.move_checkpoint_head_to(parent)
    }

    pub(super) fn do_redo(
        &mut self,
        branch: Option<CheckpointId>,
    ) -> Result<HistoryEventOutcome, HistoryError> {
        let head = self.checkpoints.head;
        let kids: SmallVec<[CheckpointId; 2]> =
            self.checkpoints.checkpoints[head].children.iter().copied().collect();
        let target = match (branch, kids.as_slice()) {
            (_, []) => return Err(HistoryError::NoChildren),
            (Some(b), kids) if kids.contains(&b) => b,
            (Some(_), _) => return Err(HistoryError::NoSuchBranch),
            (None, [only]) => *only,
            (None, _) => return Err(HistoryError::AmbiguousBranch),
        };
        self.move_checkpoint_head_to(target)
    }

    pub(super) fn do_add_entity(
        &mut self,
        entity_id: EntityId,
        payload: Arc<MoleculeEntity>,
        kind: CheckpointKind,
        label: Cow<'static, str>,
    ) -> Result<HistoryEventOutcome, HistoryError> {
        if self.lanes.contains_key(&entity_id) {
            return Err(HistoryError::EntityAlreadyExists { entity: entity_id });
        }
        let now = SystemTime::now();
        let action_kind = kind
            .entity_action_kind()
            .unwrap_or(EntityActionKind::Loaded);

        let mut snapshots: SlotMap<EntitySnapshotId, EntitySnapshot> = SlotMap::with_key();
        let snap_id = snapshots.insert(EntitySnapshot {
            parent: None,
            children: SmallVec::new(),
            payload,
            kind: action_kind,
            label: label.clone(),
            timestamp: now,
            tentative: false,
            checkpoint_refs: 0,
        });
        self.lanes.insert(
            entity_id,
            EntityHistory {
                snapshots,
                head: snap_id,
                root: snap_id,
            },
        );

        let parent_ckpt_id = self.checkpoints.head;
        let parent_ckpt = &self.checkpoints.checkpoints[parent_ckpt_id];
        let mut entity_heads = parent_ckpt.entity_heads.clone();
        entity_heads.insert(entity_id, snap_id);

        let new_ckpt = self.checkpoints.checkpoints.insert(Checkpoint {
            parent: Some(parent_ckpt_id),
            children: SmallVec::new(),
            entity_heads,
            kind,
            label,
            timestamp: now,
            raw_score: None,
            game_score: None,
            filter_status: FilterStatus::NotEvaluated,
            exclude_from_best: false,
            tentative: false,
        });
        self.checkpoints.checkpoints[parent_ckpt_id]
            .children
            .push(new_ckpt);
        self.checkpoints.head = new_ckpt;

        self.inc_refs_for_checkpoint(new_ckpt);
        self.recompute_best();

        Ok(HistoryEventOutcome::Pushed(new_ckpt))
    }

    pub(super) fn do_jump(&mut self, id: CheckpointId) -> Result<HistoryEventOutcome, HistoryError> {
        if !self.checkpoints.checkpoints.contains_key(id) {
            return Err(HistoryError::UnknownCheckpoint { id });
        }
        if self.checkpoints.checkpoints[id].tentative {
            return Err(HistoryError::TentativeNotJumpable);
        }
        self.move_checkpoint_head_to(id)
    }

    // ── Helpers used by the dispatch arms ─────────────────────────────

    /// Push a `LaneUndo` checkpoint mirroring the new lane head, with
    /// `entity_heads` cloned from the current graph head and `entity`'s
    /// entry replaced. Returns the `Pushed` result.
    pub(super) fn push_lane_undo_checkpoint(
        &mut self,
        entity: EntityId,
        target: EntitySnapshotId,
    ) -> Result<HistoryEventOutcome, HistoryError> {
        let now = SystemTime::now();
        let parent_ckpt_id = self.checkpoints.head;
        let parent_ckpt = &self.checkpoints.checkpoints[parent_ckpt_id];
        let mut entity_heads = parent_ckpt.entity_heads.clone();
        entity_heads.insert(entity, target);

        let new_ckpt = self.checkpoints.checkpoints.insert(Checkpoint {
            parent: Some(parent_ckpt_id),
            children: SmallVec::new(),
            entity_heads,
            kind: CheckpointKind::LaneUndo { entity, target },
            label: Cow::Borrowed("Lane undo"),
            timestamp: now,
            raw_score: None,
            game_score: None,
            filter_status: FilterStatus::NotEvaluated,
            exclude_from_best: false,
            tentative: false,
        });
        self.checkpoints.checkpoints[parent_ckpt_id]
            .children
            .push(new_ckpt);
        self.checkpoints.head = new_ckpt;

        self.inc_refs_for_checkpoint(new_ckpt);

        Ok(HistoryEventOutcome::Pushed(new_ckpt))
    }

    /// Move the checkpoint head to `target` and mirror lane heads to
    /// match `target.entity_heads`. Returns `HeadMoved`.
    pub(super) fn move_checkpoint_head_to(
        &mut self,
        target: CheckpointId,
    ) -> Result<HistoryEventOutcome, HistoryError> {
        let from = self.checkpoints.head;
        if from == target {
            return Ok(HistoryEventOutcome::HeadMoved { from, to: target });
        }
        // Mirror lane heads.
        let entity_heads = self.checkpoints.checkpoints[target].entity_heads.clone();
        for (eid, snap_id) in &entity_heads {
            if let Some(lane) = self.lanes.get_mut(eid) {
                if !lane.snapshots.contains_key(*snap_id) {
                    return Err(HistoryError::UnknownSnapshot {
                        entity: *eid,
                        id: *snap_id,
                    });
                }
                lane.head = *snap_id;
            } else {
                return Err(HistoryError::UnknownEntity { entity: *eid });
            }
        }
        self.checkpoints.head = target;
        Ok(HistoryEventOutcome::HeadMoved { from, to: target })
    }

    /// Increment `checkpoint_refs` for every snapshot referenced by
    /// `id`'s `entity_heads`.
    pub(super) fn inc_refs_for_checkpoint(&mut self, id: CheckpointId) {
        let heads: Vec<(EntityId, EntitySnapshotId)> = self.checkpoints.checkpoints[id]
            .entity_heads
            .iter()
            .map(|(e, s)| (*e, *s))
            .collect();
        for (eid, sid) in heads {
            if let Some(lane) = self.lanes.get_mut(&eid) {
                if let Some(snap) = lane.snapshots.get_mut(sid) {
                    snap.checkpoint_refs = snap.checkpoint_refs.saturating_add(1);
                }
            }
        }
    }

    /// Decrement `checkpoint_refs` for every snapshot referenced by
    /// `id`'s `entity_heads`. Caller is responsible for then removing
    /// the checkpoint.
    pub(super) fn dec_refs_for_checkpoint(&mut self, id: CheckpointId) {
        let heads: Vec<(EntityId, EntitySnapshotId)> = self.checkpoints.checkpoints[id]
            .entity_heads
            .iter()
            .map(|(e, s)| (*e, *s))
            .collect();
        for (eid, sid) in heads {
            if let Some(lane) = self.lanes.get_mut(&eid) {
                if let Some(snap) = lane.snapshots.get_mut(sid) {
                    snap.checkpoint_refs = snap.checkpoint_refs.saturating_sub(1);
                }
            }
        }
    }

    /// Detach `id` from its parent's `children` and from cursors. Caller
    /// then `remove`s it from the slotmap. The parent may already have
    /// been evicted in the same sweep (e.g., `prune_to_head_path`
    /// iterating an unsorted victim list); a missing parent is silently
    /// ignored — there's no live `children` list to update.
    pub(super) fn detach_checkpoint(&mut self, id: CheckpointId) {
        let parent_id = self
            .checkpoints
            .checkpoints
            .get(id)
            .and_then(|c| c.parent);
        if let Some(parent) = parent_id {
            if let Some(p) = self.checkpoints.checkpoints.get_mut(parent) {
                p.children.retain(|c| *c != id);
            }
        }
        let _ = self.checkpoints.pinned.remove(&id);
        if self.checkpoints.best == Some(id) {
            self.checkpoints.best = None;
        }
        if self.checkpoints.best_that_counts == Some(id) {
            self.checkpoints.best_that_counts = None;
        }
    }
}
