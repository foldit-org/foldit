//! Eviction + linear-undo prune + best-cursor recompute. All three
//! are History-internal policies that run from `record` (prune
//! after push, evict at the tail of every dispatch, recompute_best
//! on commit).

use std::collections::HashSet;

use molex::entity::molecule::id::EntityId;

use super::{CheckpointId, EntitySnapshotId, FilterStatus, History};

impl History {
    // ── Linear-undo prune ─────────────────────────────────────────────

    /// Drop every checkpoint that isn't an ancestor of the current
    /// `head`, and every snapshot that isn't an ancestor of its lane's
    /// `head`. Called after a push mutation so the resulting history
    /// is a single linear chain - the redo path that existed before
    /// the push is now gone (classic editor-undo semantics).
    pub(super) fn prune_to_head_path(&mut self) {
        // Checkpoints: build the ancestor set, evict the rest.
        let head = self.checkpoints.head;
        let keep_ckpts = self.checkpoint_head_path();
        let victims: Vec<CheckpointId> = self
            .checkpoints
            .checkpoints
            .iter()
            .map(|(id, _)| id)
            .filter(|id| !keep_ckpts.contains(id))
            .collect();
        for victim in victims {
            self.dec_refs_for_checkpoint(victim);
            self.detach_checkpoint(victim);
            let _ = self.checkpoints.checkpoints.remove(victim);
        }
        debug_assert!(
            self.checkpoints.checkpoints.contains_key(head),
            "head must survive prune",
        );

        // Snapshots: only evict the ones whose `checkpoint_refs` hit
        // zero during the sweep above (i.e., the only checkpoints
        // referencing them were the redo-branch ones we just pruned).
        // Snapshots still referenced by older ancestor checkpoints -
        // for example, a `LaneUndo` checkpoint legitimately pointing
        // back at an old snapshot - stay live; pruning them
        // unconditionally would dangle that ancestor's
        // `entity_heads` reference and break the cross-DAG invariant.
        let lane_ids: Vec<EntityId> = self.lanes.keys().copied().collect();
        for eid in lane_ids {
            let lane_head = match self.lanes.get(&eid) {
                Some(l) => l.head,
                None => continue,
            };
            let lane_root = self.lanes[&eid].root;
            let lane = match self.lanes.get_mut(&eid) {
                Some(l) => l,
                None => continue,
            };
            let snap_victims: Vec<EntitySnapshotId> = lane
                .snapshots
                .iter()
                .filter(|(id, snap)| {
                    *id != lane_head
                        && *id != lane_root
                        && !snap.tentative
                        && snap.checkpoint_refs == 0
                })
                .map(|(id, _)| id)
                .collect();
            for victim in snap_victims {
                let removed = lane
                    .snapshots
                    .remove(victim)
                    .expect("victim taken from iter above");
                if let Some(parent) = removed.parent {
                    if let Some(p) = lane.snapshots.get_mut(parent) {
                        p.children.retain(|c| *c != victim);
                    }
                }
            }
        }
    }

    // ── Eviction ──────────────────────────────────────────────────────

    /// Evict checkpoints and snapshots until both budgets are satisfied.
    /// Called from `record` exactly once after each event.
    pub(super) fn evict_to_budget(&mut self) {
        // Checkpoints: oldest-first; protected: root, head-path, pinned,
        // best, best_that_counts.
        while self.checkpoints.checkpoints.len() > self.checkpoints.budget.max_checkpoints {
            let Some(victim) = self.pick_checkpoint_eviction() else { break };
            self.dec_refs_for_checkpoint(victim);
            self.detach_checkpoint(victim);
            let _ = self.checkpoints.checkpoints.remove(victim);
        }

        // Snapshots per lane: oldest with refcount == 0, not lane head,
        // not lane root, not on the head-path.
        let lane_ids: Vec<EntityId> = self.lanes.keys().copied().collect();
        for eid in lane_ids {
            loop {
                let lane_len = self.lanes.get(&eid).map_or(0, |l| l.snapshots.len());
                if lane_len <= self.checkpoints.budget.max_snapshots_per_lane {
                    break;
                }
                let Some(victim) = self.pick_snapshot_eviction(eid) else { break };
                let lane = self.lanes.get_mut(&eid).expect("checked above");
                let removed = lane.snapshots.remove(victim).expect("picked above");
                if let Some(parent) = removed.parent {
                    if let Some(p) = lane.snapshots.get_mut(parent) {
                        p.children.retain(|c| *c != victim);
                    }
                }
            }
        }
    }

    /// Pick a checkpoint to evict per the policy in the strategy doc.
    pub(super) fn pick_checkpoint_eviction(&self) -> Option<CheckpointId> {
        let head_path = self.checkpoint_head_path();
        self.checkpoints
            .checkpoints
            .iter()
            .filter(|(id, _)| {
                *id != self.checkpoints.root
                    && !head_path.contains(id)
                    && !self.checkpoints.pinned.contains(id)
                    && self.checkpoints.best != Some(*id)
                    && self.checkpoints.best_that_counts != Some(*id)
            })
            .min_by_key(|(_, ckpt)| ckpt.timestamp)
            .map(|(id, _)| id)
    }

    /// Pick a snapshot on `entity`'s lane to evict per the policy.
    pub(super) fn pick_snapshot_eviction(&self, entity: EntityId) -> Option<EntitySnapshotId> {
        let lane = self.lanes.get(&entity)?;
        let head_path = self.lane_head_path(entity);
        lane.snapshots
            .iter()
            .filter(|(id, snap)| {
                *id != lane.root
                    && *id != lane.head
                    && !head_path.contains(id)
                    && snap.checkpoint_refs == 0
                    && !snap.tentative
            })
            .min_by_key(|(_, snap)| snap.timestamp)
            .map(|(id, _)| id)
    }

    /// All checkpoint ids on the path from head to root (inclusive of
    /// both).
    pub(super) fn checkpoint_head_path(&self) -> HashSet<CheckpointId> {
        let mut path = HashSet::new();
        let mut cur = Some(self.checkpoints.head);
        while let Some(id) = cur {
            let _ = path.insert(id);
            cur = self.checkpoints.checkpoints.get(id).and_then(|c| c.parent);
        }
        path
    }

    /// All snapshot ids on the path from `entity`'s lane head to its
    /// root (inclusive).
    pub(super) fn lane_head_path(&self, entity: EntityId) -> HashSet<EntitySnapshotId> {
        let mut path = HashSet::new();
        if let Some(lane) = self.lanes.get(&entity) {
            let mut cur = Some(lane.head);
            while let Some(id) = cur {
                let _ = path.insert(id);
                cur = lane.snapshots.get(id).and_then(|s| s.parent);
            }
        }
        path
    }

    // ── Best cursor recompute ─────────────────────────────────────────

    /// Recompute `best` and `best_that_counts` cursors.
    /// `best` = highest `raw_score` across non-tentative, non-excluded
    /// checkpoints. `best_that_counts` adds the constraint
    /// `filter_status == Pass`.
    pub(super) fn recompute_best(&mut self) {
        let mut best: Option<(CheckpointId, f64)> = None;
        let mut best_counts: Option<(CheckpointId, f64)> = None;
        for (id, ckpt) in self.checkpoints.checkpoints.iter() {
            if ckpt.exclude_from_best {
                continue;
            }
            if let Some(score) = ckpt.raw_score {
                if best.is_none_or(|(_, b)| score > b) {
                    best = Some((id, score));
                }
                if matches!(ckpt.filter_status, FilterStatus::Pass)
                    && best_counts.is_none_or(|(_, b)| score > b)
                {
                    best_counts = Some((id, score));
                }
            }
        }
        self.checkpoints.best = best.map(|(id, _)| id);
        self.checkpoints.best_that_counts = best_counts.map(|(id, _)| id);
    }
}
