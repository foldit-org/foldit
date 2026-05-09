//! `assert_invariant` — debug-only cross-DAG sanity check (G8). The
//! release build replaces this with a no-op (the caller wraps every
//! invocation in `if cfg!(debug_assertions)`).

use molex::entity::molecule::id::EntityId;

use super::{EntitySnapshotId, History, OngoingState};

impl History {
    // ── Cross-DAG invariant (G8) ──────────────────────────────────────

    /// Walk both DAGs and assert the cross-DAG invariant. Called at the
    /// tail of every public DAG-bearing mutation under
    /// `debug_assertions`. CI runs debug builds → bug becomes a test
    /// failure on the *next* mutation, not three weeks later.
    #[cfg(debug_assertions)]
    pub(super) fn assert_invariant(&self) {
        let head_ckpt = &self.checkpoints.checkpoints[self.checkpoints.head];

        // 1. checkpoint_head.entity_heads[e] == lane_head(e).
        for (eid, snap_id) in &head_ckpt.entity_heads {
            let lane = self
                .lanes
                .get(eid)
                .unwrap_or_else(|| panic!("invariant: head ckpt references unknown entity {}", eid.raw()));
            assert_eq!(
                lane.head,
                *snap_id,
                "invariant: lane head mismatch for entity {} (expected {:?}, got {:?})",
                eid.raw(),
                snap_id,
                lane.head
            );
        }

        // 2. every snapshot referenced by a live checkpoint has
        //    checkpoint_refs > 0.
        let mut expected_refs: indexmap::IndexMap<(EntityId, EntitySnapshotId), u32> =
            indexmap::IndexMap::new();
        for (_, ckpt) in self.checkpoints.checkpoints.iter() {
            for (eid, sid) in &ckpt.entity_heads {
                *expected_refs.entry((*eid, *sid)).or_default() += 1;
            }
        }
        for ((eid, sid), expected) in &expected_refs {
            let lane = self
                .lanes
                .get(eid)
                .unwrap_or_else(|| panic!("invariant: ref to unknown entity {}", eid.raw()));
            let snap = lane
                .snapshots
                .get(*sid)
                .unwrap_or_else(|| panic!("invariant: ref to unknown snapshot on {}", eid.raw()));
            assert!(
                snap.checkpoint_refs >= *expected,
                "invariant: snapshot for entity {} has refs={} but expected ≥{}",
                eid.raw(),
                snap.checkpoint_refs,
                expected
            );
        }

        // 3. tentative coupling: exactly one tentative iff Active.
        match &self.ongoing {
            OngoingState::Idle => {
                for lane in self.lanes.values() {
                    for (_, snap) in lane.snapshots.iter() {
                        assert!(!snap.tentative, "invariant: tentative snapshot while Idle");
                    }
                }
                for (_, ckpt) in self.checkpoints.checkpoints.iter() {
                    assert!(!ckpt.tentative, "invariant: tentative checkpoint while Idle");
                }
            }
            OngoingState::Active {
                entity,
                tentative_snapshot,
                tentative_checkpoint,
                ..
            } => {
                let lane = self
                    .lanes
                    .get(entity)
                    .expect("invariant: Active references unknown entity");
                let snap = lane
                    .snapshots
                    .get(*tentative_snapshot)
                    .expect("invariant: Active references unknown snapshot");
                assert!(
                    snap.tentative,
                    "invariant: Active tentative snapshot is not flagged"
                );
                assert_eq!(
                    lane.head, *tentative_snapshot,
                    "invariant: Active's tentative snapshot is not the lane head"
                );
                let ckpt = self
                    .checkpoints
                    .checkpoints
                    .get(*tentative_checkpoint)
                    .expect("invariant: Active references unknown checkpoint");
                assert!(
                    ckpt.tentative,
                    "invariant: Active tentative checkpoint is not flagged"
                );
                assert_eq!(
                    self.checkpoints.head, *tentative_checkpoint,
                    "invariant: Active's tentative checkpoint is not the graph head"
                );
                // exactly one tentative snapshot, on the active lane.
                let mut tentative_count = 0;
                for (eid, lane) in &self.lanes {
                    for (sid, snap) in lane.snapshots.iter() {
                        if snap.tentative {
                            tentative_count += 1;
                            assert_eq!(
                                (*eid, sid),
                                (*entity, *tentative_snapshot),
                                "invariant: stray tentative snapshot"
                            );
                        }
                    }
                }
                assert_eq!(tentative_count, 1, "invariant: not exactly one tentative snapshot");
                let mut tentative_ckpts = 0;
                for (id, ckpt) in self.checkpoints.checkpoints.iter() {
                    if ckpt.tentative {
                        tentative_ckpts += 1;
                        assert_eq!(
                            id, *tentative_checkpoint,
                            "invariant: stray tentative checkpoint"
                        );
                    }
                }
                assert_eq!(tentative_ckpts, 1, "invariant: not exactly one tentative checkpoint");
            }
        }
    }

    #[cfg(not(debug_assertions))]
    #[allow(dead_code)]
    pub(super) fn assert_invariant(&self) {}
}
