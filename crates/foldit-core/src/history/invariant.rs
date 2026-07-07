//! `assert_invariant` - debug-only cross-DAG sanity check. The
//! release build replaces this with a no-op (the caller wraps every
//! invocation in `if cfg!(debug_assertions)`).

use std::collections::HashSet;

use molex::entity::molecule::id::EntityId;

use super::{EntitySnapshotId, History};

impl History {
    /// Walk both DAGs and assert the cross-DAG invariant. Called at the
    /// tail of every public DAG-bearing mutation under
    /// `debug_assertions`. CI runs debug builds → bug becomes a test
    /// failure on the *next* mutation, not three weeks later.
    // Debug-only invariant checker; a violated invariant is a bug we
    // want to halt on immediately, alongside the assert! macros below.
    #[allow(clippy::panic, clippy::expect_used)]
    #[cfg(debug_assertions)]
    pub(super) fn assert_invariant(&self) {
        let head_ckpt = &self.checkpoints.checkpoints[self.checkpoints.head];

        // 1. For each (e, committed_snap) in the committed head's
        //    entity_heads, the lane head is either that committed snap, or
        //    a tentative snapshot whose parent is it (an open action's
        //    in-flight tentative sits one step past the committed head).
        for (eid, committed_snap) in &head_ckpt.entity_heads {
            let lane = self.lanes.get(eid).unwrap_or_else(|| {
                panic!(
                    "invariant: head ckpt references unknown entity {}",
                    eid.raw()
                )
            });
            if lane.head == *committed_snap {
                continue;
            }
            let head_snap = lane
                .snapshots
                .get(lane.head)
                .unwrap_or_else(|| panic!("invariant: lane head missing for entity {}", eid.raw()));
            assert!(
                head_snap.tentative && head_snap.parent == Some(*committed_snap),
                "invariant: lane head for entity {} is neither the committed snap {:?} \
                 nor a tentative child of it (got {:?}, tentative={})",
                eid.raw(),
                committed_snap,
                lane.head,
                head_snap.tentative,
            );
        }

        // 2. every snapshot referenced by a live checkpoint has
        //    checkpoint_refs > 0.
        let mut expected_refs: indexmap::IndexMap<(EntityId, EntitySnapshotId), u32> =
            indexmap::IndexMap::new();
        for (_, ckpt) in &self.checkpoints.checkpoints {
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

        // 3. Per-request tentative coherence: every tentative snapshot is
        //    the head of exactly one lane and is named by exactly one
        //    pending edit, and vice versa (bijection). (No checkpoint is
        //    ever tentative - a begin mints none - which the absence of
        //    the field now makes structural.)
        let mut tentative_lane_heads: HashSet<(EntityId, EntitySnapshotId)> = HashSet::new();
        for (eid, lane) in &self.lanes {
            for (sid, snap) in &lane.snapshots {
                if snap.tentative {
                    assert_eq!(
                        lane.head,
                        sid,
                        "invariant: tentative snapshot is not its lane head for entity {}",
                        eid.raw()
                    );
                    let fresh = tentative_lane_heads.insert((*eid, sid));
                    assert!(fresh, "invariant: duplicate tentative lane head");
                }
            }
        }
        let mut pending_lane_heads: HashSet<(EntityId, EntitySnapshotId)> = HashSet::new();
        for edit in self.pending.values() {
            for (eid, sid) in &edit.lanes {
                let lane = self
                    .lanes
                    .get(eid)
                    .expect("invariant: pending edit references unknown entity");
                let snap = lane
                    .snapshots
                    .get(*sid)
                    .expect("invariant: pending edit references unknown snapshot");
                assert!(
                    snap.tentative,
                    "invariant: pending edit lane head is not tentative"
                );
                assert_eq!(
                    lane.head, *sid,
                    "invariant: pending edit lane is not the lane head"
                );
                let fresh = pending_lane_heads.insert((*eid, *sid));
                assert!(
                    fresh,
                    "invariant: a lane appears in more than one pending edit"
                );
            }
        }
        assert_eq!(
            tentative_lane_heads, pending_lane_heads,
            "invariant: tentative snapshots and pending-edit lanes are not in bijection"
        );
    }

    #[cfg(not(debug_assertions))]
    #[allow(dead_code)]
    pub(super) fn assert_invariant(&self) {}
}
