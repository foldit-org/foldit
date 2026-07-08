//! `record` dispatch arms (the `do_*` methods) plus the small
//! mutation helpers they share. Lives behind the `record` root:
//! callers go through the public methods on `History` (in
//! `mod.rs`), which build a `HistoryEvent` variant and route it
//! into `record`, which then delegates to one of these arms.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use web_time::SystemTime;

use molex::entity::molecule::id::EntityId;
use molex::MoleculeEntity;
use slotmap::SlotMap;
use smallvec::SmallVec;

use super::{
    Checkpoint, CheckpointId, CheckpointKind, EntityHistory, EntitySnapshot, EntitySnapshotId,
    FilterStatus, History, HistoryError, HistoryEventOutcome, PendingEdit,
};

impl History {
    // `expect` resolves a lane whose presence the caller established above.
    #[allow(clippy::expect_used)]
    pub(super) fn do_begin(
        &mut self,
        entities: &SmallVec<[EntityId; 1]>,
        kind: CheckpointKind,
        label: &str,
        request_id: u64,
        selection: BTreeMap<EntityId, BTreeSet<u32>>,
    ) -> Result<HistoryEventOutcome, HistoryError> {
        // The lane-not-busy precondition is enforced by the caller-side
        // pre-check; validate lane existence for every named entity up
        // front so a missing lane fails before any tentative is pushed
        // (the begin is all-or-nothing across its lanes).
        for entity in entities {
            if !self.lanes.contains_key(entity) {
                return Err(HistoryError::UnknownEntity { entity: *entity });
            }
        }

        let now = SystemTime::now();

        // Open one tentative lane per entity, each forked from its own
        // committed lane head, and advance that lane head. No checkpoint is
        // minted and the committed graph head does not move: the checkpoint
        // is composed at commit from the committed head, so a committed node
        // never references another action's open tentative.
        let mut lanes: SmallVec<[(EntityId, EntitySnapshotId); 1]> = SmallVec::new();
        for entity in entities {
            let lane = self.lanes.get_mut(entity).expect("checked above");
            let parent = lane.head;
            let payload = Arc::clone(&lane.snapshots[parent].payload);
            let new_snap = lane.snapshots.insert(EntitySnapshot {
                parent: Some(parent),
                children: SmallVec::new(),
                payload,
                label: Cow::Owned(label.to_owned()),
                timestamp: now,
                tentative: true,
                checkpoint_refs: 0,
            });
            lane.snapshots[parent].children.push(new_snap);
            lane.head = new_snap;
            lanes.push((*entity, new_snap));
        }

        // Register the open composition under the caller-supplied request
        // id (allocated by the orchestrator).
        let _ = self.pending.insert(
            request_id,
            PendingEdit {
                lanes,
                selection,
                kind,
                raw_score: None,
                game_score: None,
                breakdown: None,
                filter_status: FilterStatus::NotEvaluated,
            },
        );

        Ok(HistoryEventOutcome::Began)
    }

    // `expect` resolves each pending edit's lane, live by construction.
    #[allow(clippy::expect_used)]
    pub(super) fn do_commit(
        &mut self,
        request_id: u64,
    ) -> Result<HistoryEventOutcome, HistoryError> {
        let edit = self
            .pending
            .swap_remove(&request_id)
            .ok_or(HistoryError::NoOngoingAction)?;
        let now = SystemTime::now();

        // Recover the action label from the (first) held lane's tentative
        // snapshot - `do_begin` stamped it there. The committed checkpoint
        // carries it so the history panel shows the action's name.
        let label = edit
            .lanes
            .first()
            .and_then(|(entity, snap_id)| self.lanes.get(entity).and_then(|l| l.snapshot(*snap_id)))
            .map_or(Cow::Borrowed("Action"), |s| s.label.clone());

        // Flip each held lane's tentative snapshot to committed.
        for (entity, snap_id) in &edit.lanes {
            let lane = self.lanes.get_mut(entity).expect("pending lane");
            lane.snapshots[*snap_id].tentative = false;
        }

        // Compose the new checkpoint's entity_heads from the CURRENT
        // committed graph head (never lane heads): start from its map and
        // overlay this edit's lanes. Reading the committed head for the
        // peer entities is what keeps a committed node from referencing
        // another open edit's tentative.
        let parent_ckpt_id = self.checkpoints.head;
        let mut entity_heads = self.checkpoints.checkpoints[parent_ckpt_id]
            .entity_heads
            .clone();
        for (entity, snap_id) in &edit.lanes {
            entity_heads.insert(*entity, *snap_id);
        }

        let new_ckpt = self.mint_checkpoint(Checkpoint {
            parent: Some(parent_ckpt_id),
            children: SmallVec::new(),
            entity_heads,
            kind: edit.kind,
            label,
            timestamp: now,
            raw_score: edit.raw_score,
            game_score: edit.game_score,
            breakdown: edit.breakdown,
            filter_status: edit.filter_status,
            exclude_from_best: false,
        });

        Ok(HistoryEventOutcome::Pushed(new_ckpt))
    }

    // `expect`s resolve the pending edit's lane/snapshot, and a tentative
    // snapshot always has a parent (it is never a lane root).
    #[allow(clippy::expect_used)]
    pub(super) fn do_abort(
        &mut self,
        request_id: u64,
    ) -> Result<HistoryEventOutcome, HistoryError> {
        let edit = self
            .pending
            .swap_remove(&request_id)
            .ok_or(HistoryError::NoOngoingAction)?;

        // Tear down each held lane's tentative snapshot; the lane head
        // falls back to the tentative's parent. There is no checkpoint to
        // remove (a begin mints none) and the committed graph head never
        // moved, so nothing else unwinds.
        for (entity, snap_id) in &edit.lanes {
            let lane = self.lanes.get_mut(entity).expect("pending lane");
            let removed = lane.snapshots.remove(*snap_id).expect("pending snap");
            let parent_snap = removed.parent.expect("tentative is never lane root");
            if let Some(parent) = lane.snapshots.get_mut(parent_snap) {
                parent.children.retain(|c| c != snap_id);
            }
            lane.head = parent_snap;
        }

        Ok(HistoryEventOutcome::Aborted)
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

        Ok(self.push_lane_undo_checkpoint(entity, target))
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

        Ok(self.push_lane_undo_checkpoint(entity, target))
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
        let kids: SmallVec<[CheckpointId; 2]> = self.checkpoints.checkpoints[head]
            .children
            .iter()
            .copied()
            .collect();
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

        let mut snapshots: SlotMap<EntitySnapshotId, EntitySnapshot> = SlotMap::with_key();
        let snap_id = snapshots.insert(EntitySnapshot {
            parent: None,
            children: SmallVec::new(),
            payload,
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

        let new_ckpt = self.mint_checkpoint(Checkpoint {
            parent: Some(parent_ckpt_id),
            children: SmallVec::new(),
            entity_heads,
            kind,
            label,
            timestamp: now,
            raw_score: None,
            game_score: None,
            breakdown: None,
            filter_status: FilterStatus::NotEvaluated,
            exclude_from_best: false,
        });

        Ok(HistoryEventOutcome::Pushed(new_ckpt))
    }

    pub(super) fn do_jump(
        &mut self,
        id: CheckpointId,
    ) -> Result<HistoryEventOutcome, HistoryError> {
        if !self.checkpoints.checkpoints.contains_key(id) {
            return Err(HistoryError::UnknownCheckpoint { id });
        }
        self.move_checkpoint_head_to(id)
    }

    /// Push a `LaneUndo` checkpoint mirroring the new lane head, with
    /// `entity_heads` cloned from the current graph head and `entity`'s
    /// entry replaced. Returns the `Pushed` result.
    pub(super) fn push_lane_undo_checkpoint(
        &mut self,
        entity: EntityId,
        target: EntitySnapshotId,
    ) -> HistoryEventOutcome {
        let now = SystemTime::now();
        let parent_ckpt_id = self.checkpoints.head;
        let parent_ckpt = &self.checkpoints.checkpoints[parent_ckpt_id];
        let mut entity_heads = parent_ckpt.entity_heads.clone();
        entity_heads.insert(entity, target);

        let new_ckpt = self.mint_checkpoint(Checkpoint {
            parent: Some(parent_ckpt_id),
            children: SmallVec::new(),
            entity_heads,
            kind: CheckpointKind::LaneUndo { entity, target },
            label: Cow::Borrowed("Lane undo"),
            timestamp: now,
            raw_score: None,
            game_score: None,
            breakdown: None,
            filter_status: FilterStatus::NotEvaluated,
            exclude_from_best: false,
        });

        HistoryEventOutcome::Pushed(new_ckpt)
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

    /// Insert `ckpt`, link it under its parent, advance the head, and
    /// bump snapshot refs. Best cursors are recomputed by the record tail.
    fn mint_checkpoint(&mut self, ckpt: Checkpoint) -> CheckpointId {
        let parent = ckpt.parent;
        let new_ckpt = self.checkpoints.checkpoints.insert(ckpt);
        if let Some(parent) = parent {
            self.checkpoints.checkpoints[parent].children.push(new_ckpt);
        }
        self.checkpoints.head = new_ckpt;
        self.inc_refs_for_checkpoint(new_ckpt);
        new_ckpt
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
    /// ignored - there's no live `children` list to update.
    pub(super) fn detach_checkpoint(&mut self, id: CheckpointId) {
        let parent_id = self.checkpoints.checkpoints.get(id).and_then(|c| c.parent);
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
