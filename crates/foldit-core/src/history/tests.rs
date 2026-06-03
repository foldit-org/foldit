use super::*;
use molex::entity::molecule::bulk::BulkEntity;
use molex::entity::molecule::EntityIdAllocator;
use molex::Element;
use molex::MoleculeType;
use molex::entity::molecule::atom::Atom;
use proptest::prelude::*;
use slotmap::{Key, KeyData};

/// One-atom Bulk entity — cheap to construct, exercises every path
/// through `History` without realistic protein data.
fn mk_entity(id: EntityId) -> MoleculeEntity {
    let atom = Atom {
        position: glam::Vec3::ZERO,
        occupancy: 1.0,
        b_factor: 0.0,
        element: Element::O,
        name: *b"O   ",
        formal_charge: 0,
    };
    MoleculeEntity::Bulk(BulkEntity::new(
        id,
        MoleculeType::Water,
        vec![atom],
        *b"HOH",
        1,
    ))
}

fn mk_history(n_entities: usize) -> (History, Vec<EntityId>) {
    let mut alloc = EntityIdAllocator::new();
    let ids: Vec<EntityId> = (0..n_entities).map(|_| alloc.allocate()).collect();
    let seed: Vec<(EntityId, MoleculeEntity)> =
        ids.iter().map(|id| (*id, mk_entity(*id))).collect();
    let h = History::new(seed, PathBuf::from("test"));
    (h, ids)
}

fn arc_entity(id: EntityId) -> Arc<MoleculeEntity> {
    Arc::new(mk_entity(id))
}

/// A plugin-op checkpoint kind for the record/begin tests. The op no
/// longer names an entity (the touched set rides on `entity_heads` /
/// the pending edit's lanes), so callers pass the entity separately.
fn plugin_op(op_id: &str) -> CheckpointKind {
    CheckpointKind::PluginOp {
        plugin_id: "rosetta".to_string(),
        op_id: op_id.to_string(),
        display: op_id.to_string(),
    }
}

// ── Linear push / undo / redo ─────────────────────────────────────

#[test]
fn linear_push_undo_redo_single_lane() {
    let (mut h, ids) = mk_history(1);
    let e = ids[0];
    let root = h.checkpoints().root();

    let c1 = h
        .record_entity_update(
            e,
            plugin_op("wiggle"),
            arc_entity(e),
            Cow::Borrowed("w1"),
            None,
            None,
        )
        .unwrap();
    let c2 = h
        .record_entity_update(
            e,
            plugin_op("wiggle"),
            arc_entity(e),
            Cow::Borrowed("w2"),
            None,
            None,
        )
        .unwrap();
    assert_eq!(h.checkpoints().head(), c2);

    // undo → c1
    let to = h.undo().unwrap().unwrap();
    assert_eq!(to, c1);
    // undo → root
    let to = h.undo().unwrap().unwrap();
    assert_eq!(to, root);
    // undo at root → None
    assert_eq!(h.undo().unwrap(), None);

    // redo (single child) → c1
    let to = h.redo(None).unwrap().unwrap();
    assert_eq!(to, c1);
    // redo → c2
    let to = h.redo(None).unwrap().unwrap();
    assert_eq!(to, c2);
    // redo at leaf → None
    assert_eq!(h.redo(None).unwrap(), None);

    // lane head must mirror checkpoint head's entity_heads
    assert_eq!(
        h.lane(e).unwrap().head(),
        h.checkpoint(c2).unwrap().entity_heads[&e]
    );
}

// ── Linear-undo prune ────────────────────────────────────────────

#[test]
fn push_after_undo_drops_redo_path() {
    let (mut h, ids) = mk_history(1);
    let e = ids[0];

    let c1 = h
        .record_entity_update(
            e,
            plugin_op("shake"),
            arc_entity(e),
            Cow::Borrowed("s1"),
            None,
            None,
        )
        .unwrap();
    let c2 = h
        .record_entity_update(
            e,
            plugin_op("shake"),
            arc_entity(e),
            Cow::Borrowed("s2"),
            None,
            None,
        )
        .unwrap();

    // Undo to c1; redo path still alive (c2 reachable).
    let to = h.undo().unwrap().unwrap();
    assert_eq!(to, c1);
    assert!(h.checkpoint(c2).is_some());
    let to = h.redo(None).unwrap().unwrap();
    assert_eq!(to, c2);

    // Undo again, then PUSH a new edit. Linear semantics: c2 must
    // be evicted (it was on the redo path that the new push
    // replaces), and c1 must have exactly one child — the new
    // checkpoint.
    h.undo().unwrap();
    let c2b = h
        .record_entity_update(
            e,
            plugin_op("shake"),
            arc_entity(e),
            Cow::Borrowed("s2b"),
            None,
            None,
        )
        .unwrap();
    assert!(
        h.checkpoint(c2).is_none(),
        "c2 must be evicted by the linear-undo prune",
    );
    assert_eq!(h.checkpoint(c1).unwrap().children.as_slice(), [c2b]);

    // Redo from c2b is a no-op (no children).
    assert_eq!(h.redo(None).unwrap(), None);
}

// ── Eviction ──────────────────────────────────────────────────────

#[test]
fn eviction_respects_refs_pinned_best_and_head_path() {
    let (mut h, ids) = mk_history(1);
    let e = ids[0];
    // Tighten the budget so we can observe eviction without
    // pushing 200 checkpoints.
    h.set_budget(HistoryBudget {
        max_checkpoints: 4,
        max_snapshots_per_lane: 4,
    });

    let c1 = h
        .record_entity_update(
            e,
            plugin_op("shake"),
            arc_entity(e),
            Cow::Borrowed("c1"),
            None,
            None,
        )
        .unwrap();
    let c2 = h
        .record_entity_update(
            e,
            plugin_op("shake"),
            arc_entity(e),
            Cow::Borrowed("c2"),
            None,
            None,
        )
        .unwrap();

    // Pin c1 so it survives eviction even when off the head path.
    h.pin_checkpoint(c1).unwrap();

    // Branch off c1 to create something *off* the head path that
    // can be evicted later.
    h.undo().unwrap(); // back to c1
    let c1b = h
        .record_entity_update(
            e,
            plugin_op("shake"),
            arc_entity(e),
            Cow::Borrowed("c1b"),
            None,
            None,
        )
        .unwrap();
    // Mark c1b's checkpoint with a raw_score so best cursor latches.
    h.set_exclude_from_best(c1b, false).unwrap();

    // Push enough to force eviction.
    for i in 0..6 {
        h.record_entity_update(
            e,
            plugin_op("shake"),
            arc_entity(e),
            Cow::from(format!("extra{i}")),
            None,
            None,
        )
        .unwrap();
    }

    // Pinned c1 must still be alive.
    assert!(h.checkpoint(c1).is_some(), "pinned checkpoint c1 evicted");
    // Root must still be alive.
    let root = h.checkpoints().root();
    assert!(h.checkpoint(root).is_some(), "root evicted");
    // Head's ancestor path must still be alive.
    let head = h.checkpoints().head();
    let mut cur = Some(head);
    while let Some(id) = cur {
        assert!(h.checkpoint(id).is_some(), "head-path checkpoint evicted");
        cur = h.checkpoint(id).and_then(|c| c.parent);
    }
    // c2 was off-head-path and unpinned; eviction's oldest-first
    // rule should have dropped it before any pinned / head-path /
    // root entry.
    assert!(
        h.checkpoint(c2).is_none(),
        "off-head-path unpinned checkpoint c2 should have been evicted before pinned / head-path / root"
    );

    // Refcount safety is the load-bearing post-condition: every
    // snapshot any live checkpoint references is itself alive. The
    // cross-DAG invariant (G8) has already enforced this on every
    // push — we just assert it once more from outside for
    // belt-and-braces.
    for (_, ckpt) in h.checkpoints().iter() {
        for (eid, sid) in &ckpt.entity_heads {
            assert!(
                h.snapshot(*eid, *sid).is_some(),
                "live checkpoint references dead snapshot"
            );
        }
    }
}

#[test]
fn snapshot_eviction_refuses_when_referenced() {
    let (mut h, ids) = mk_history(1);
    let e = ids[0];
    h.set_budget(HistoryBudget {
        max_checkpoints: 1024,
        max_snapshots_per_lane: 3,
    });

    // Push 5 record_entity_updates → 5 snapshots + root = 6 on the
    // lane. Budget is 3, so we need to evict 3. But: every snapshot
    // is a head of some live checkpoint (each push has its own
    // checkpoint). Therefore checkpoint_refs > 0 and refuses
    // eviction. The cross-DAG invariant must hold throughout.
    for i in 0..5 {
        h.record_entity_update(
            e,
            plugin_op("shake"),
            arc_entity(e),
            Cow::from(format!("s{i}")),
            None,
            None,
        )
        .unwrap();
    }
    // Lane should have at minimum: root + current head; checkpoint
    // refs prevent dropping intermediate snapshots until their
    // checkpoints are evicted. Since checkpoint budget is huge,
    // none are. So the lane is over budget — that's fine; eviction
    // simply *refuses* when the only candidates are referenced.
    assert!(
        h.lane(e).unwrap().len() >= 4,
        "lane shrank below ref-protected size"
    );
}

// ── Ongoing-action lock + state-machine refusal ──────────────────

#[test]
fn nav_during_active_is_refused_with_entity_locked() {
    let (mut h, ids) = mk_history(2);
    let e = ids[0];
    let other = ids[1];
    let root = h.checkpoints().root();

    // Push one regular checkpoint so undo has somewhere to go.
    h.record_entity_update(
        e,
        plugin_op("shake"),
        arc_entity(e),
        Cow::Borrowed("pre"),
        None,
        None,
    )
    .unwrap();

    // Begin an action on `e`.
    let rid = 1u64;
    h.begin_action(
        [e],
        plugin_op("wiggle"),
        Cow::Borrowed("w-active"),
        rid,
    )
    .unwrap();

    // Begin again → ActiveActionInProgress.
    assert!(matches!(
        h.begin_action([e], plugin_op("shake"), Cow::Borrowed("nope"), 2),
        Err(HistoryError::ActiveActionInProgress)
    ));

    // Undo → EntityLocked { entity: e }.
    assert!(matches!(
        h.undo(),
        Err(HistoryError::EntityLocked { entity }) if entity == e
    ));

    // jump to root → also refused.
    assert!(matches!(
        h.jump_checkpoint(root),
        Err(HistoryError::EntityLocked { entity }) if entity == e
    ));

    // lane_undo on a different entity → also refused (all lanes
    // frozen during Active per strategy doc § Lock semantics).
    let other_lane_root = h.lane(other).unwrap().root();
    assert!(matches!(
        h.lane_undo(other, other_lane_root),
        Err(HistoryError::EntityLocked { entity }) if entity == e
    ));

    // record_entity_update → ActiveActionInProgress.
    assert!(matches!(
        h.record_entity_update(
            e,
            plugin_op("shake"),
            arc_entity(e),
            Cow::Borrowed("nope"),
            None,
            None,
        ),
        Err(HistoryError::ActiveActionInProgress)
    ));

    // Commit → OK.
    h.commit_action(rid).unwrap();
    // Now nav unblocks.
    assert!(h.undo().is_ok());
}

#[test]
fn abort_action_drops_tentative() {
    let (mut h, ids) = mk_history(1);
    let e = ids[0];
    let lane_len_before = h.lane(e).unwrap().len();
    let ckpt_len_before = h.checkpoints().len();

    let rid = 1u64;
    h.begin_action(
        [e],
        plugin_op("wiggle"),
        Cow::Borrowed("about-to-abort"),
        rid,
    )
    .unwrap();

    // While in flight: the lane grew by one tentative snapshot, but a
    // begin mints no checkpoint, so the checkpoint graph is unchanged.
    assert_eq!(h.lane(e).unwrap().len(), lane_len_before + 1);
    assert_eq!(h.checkpoints().len(), ckpt_len_before);
    assert!(h.has_pending());

    h.abort_action(rid).unwrap();
    // Restored: the tentative snapshot is gone and nothing is pending.
    assert_eq!(h.lane(e).unwrap().len(), lane_len_before);
    assert_eq!(h.checkpoints().len(), ckpt_len_before);
    assert!(!h.has_pending());
}

#[test]
fn in_flight_score_spares_committed_parent_then_lands_on_commit() {
    let (mut h, ids) = mk_history(1);
    let e = ids[0];
    // Give the committed parent (root) a known score.
    h.set_head_scores(Some(10.0), Some(100.0));
    let parent = h.checkpoints().head();
    assert_eq!(h.checkpoint(parent).unwrap().raw_score, Some(10.0));

    let rid = 1u64;
    h.begin_action([e], plugin_op("wiggle"), Cow::Borrowed("w"), rid)
        .unwrap();
    // Stream a score into the open action.
    h.action_update(rid, Some(42.0), Some(420.0), None, |_| {})
        .unwrap();

    // Mid-action: the committed parent is untouched; the live composition
    // score reflects the streamed value.
    assert_eq!(h.checkpoint(parent).unwrap().raw_score, Some(10.0));
    assert_eq!(h.checkpoint(parent).unwrap().game_score, Some(100.0));
    assert_eq!(h.current_composition_scores(), (Some(42.0), Some(420.0)));

    // Commit mints a checkpoint carrying the streamed score; the parent
    // still holds its own.
    let committed = h.commit_action(rid).unwrap();
    assert_eq!(h.checkpoint(committed).unwrap().raw_score, Some(42.0));
    assert_eq!(h.checkpoint(committed).unwrap().game_score, Some(420.0));
    assert_eq!(h.checkpoint(parent).unwrap().raw_score, Some(10.0));
}

#[test]
fn committed_node_references_peer_committed_head_not_its_tentative() {
    let (mut h, ids) = mk_history(2);
    let e1 = ids[0];
    let e2 = ids[1];
    let e2_committed = h.lane(e2).unwrap().head();

    // Two concurrent open actions, one per lane (begin on a free lane
    // while another is open is allowed). Distinct ids: both edits are
    // pending at once.
    let rid1 = 1u64;
    h.begin_action([e1], plugin_op("wiggle"), Cow::Borrowed("w1"), rid1)
        .unwrap();
    let rid2 = 2u64;
    h.begin_action([e2], plugin_op("wiggle"), Cow::Borrowed("w2"), rid2)
        .unwrap();
    // e2's lane head is now its open tentative, distinct from its
    // committed head.
    assert_ne!(h.lane(e2).unwrap().head(), e2_committed);

    // Commit e1 while e2 is still open: the new checkpoint's entity_heads
    // for e2 must point at e2's COMMITTED head, never its open tentative.
    let c1 = h.commit_action(rid1).unwrap();
    assert_eq!(h.checkpoint(c1).unwrap().entity_heads[&e2], e2_committed);

    // e2's own commit then advances e2 off the now-committed head.
    let c2 = h.commit_action(rid2).unwrap();
    assert_ne!(h.checkpoint(c2).unwrap().entity_heads[&e2], e2_committed);
}

#[test]
fn multi_lane_begin_opens_a_tentative_per_entity_and_commits_one_checkpoint() {
    // A single edit spanning two entities: one begin opens a tentative on
    // both lanes, action_update fans across both, and commit mints ONE
    // checkpoint whose entity_heads advanced both lanes off their
    // committed heads. This is the multi-entity capability the post-Init
    // path relies on.
    let (mut h, ids) = mk_history(2);
    let e1 = ids[0];
    let e2 = ids[1];
    let e1_committed = h.lane(e1).unwrap().head();
    let e2_committed = h.lane(e2).unwrap().head();
    let ckpt_len_before = h.checkpoints().len();

    let rid = 7u64;
    h.begin_action([e1, e2], plugin_op("init"), Cow::Borrowed("init"), rid)
        .unwrap();

    // Both lanes now hold an open tentative; no checkpoint minted yet.
    assert_ne!(h.lane(e1).unwrap().head(), e1_committed);
    assert_ne!(h.lane(e2).unwrap().head(), e2_committed);
    assert_eq!(h.checkpoints().len(), ckpt_len_before);

    // One update mutates both held lanes.
    h.action_update(rid, None, None, None, |entity| {
        for atom in entity.atom_set_mut() {
            atom.position = glam::Vec3::new(5.0, 5.0, 5.0);
        }
    })
    .unwrap();

    // Commit: exactly one new checkpoint, both lanes advanced.
    let c = h.commit_action(rid).unwrap();
    assert_eq!(h.checkpoints().len(), ckpt_len_before + 1);
    assert_ne!(h.checkpoint(c).unwrap().entity_heads[&e1], e1_committed);
    assert_ne!(h.checkpoint(c).unwrap().entity_heads[&e2], e2_committed);
    assert!(!h.has_pending());
}

// ── Lane undo ────────────────────────────────────────────────────

#[test]
fn lane_undo_pushes_lane_undo_checkpoint() {
    let (mut h, ids) = mk_history(2);
    let e1 = ids[0];
    let e2 = ids[1];

    // Two pushes on e1, one on e2 — three checkpoints + root.
    let _ = h
        .record_entity_update(
            e1,
            plugin_op("shake"),
            arc_entity(e1),
            Cow::Borrowed("e1-1"),
            None,
            None,
        )
        .unwrap();
    let target_e1 = h.lane(e1).unwrap().head();
    let _ = h
        .record_entity_update(
            e1,
            plugin_op("shake"),
            arc_entity(e1),
            Cow::Borrowed("e1-2"),
            None,
            None,
        )
        .unwrap();
    let _ = h
        .record_entity_update(
            e2,
            plugin_op("shake"),
            arc_entity(e2),
            Cow::Borrowed("e2-1"),
            None,
            None,
        )
        .unwrap();
    let e2_head_before = h.lane(e2).unwrap().head();

    // Lane-undo e1 to target_e1.
    let lu = h.lane_undo(e1, target_e1).unwrap();
    // Lane head moved.
    assert_eq!(h.lane(e1).unwrap().head(), target_e1);
    // e2 lane untouched.
    assert_eq!(h.lane(e2).unwrap().head(), e2_head_before);
    // Checkpoint kind is LaneUndo.
    assert!(matches!(
        h.checkpoint(lu).unwrap().kind,
        CheckpointKind::LaneUndo { .. }
    ));
    // checkpoint head's entity_heads[e1] == target_e1.
    assert_eq!(
        h.checkpoint(lu).unwrap().entity_heads[&e1],
        target_e1
    );
    // entity_heads[e2] preserved.
    assert_eq!(
        h.checkpoint(lu).unwrap().entity_heads[&e2],
        e2_head_before
    );
}

// ── add_entity ───────────────────────────────────────────────────

#[test]
fn add_entity_introduces_lane_and_pushes_checkpoint() {
    let (mut h, ids) = mk_history(1);
    let mut alloc = EntityIdAllocator::new();
    // Walk the allocator past existing ids to mint a fresh one
    // that's distinct from `ids[0]`.
    let mut new_id = alloc.allocate();
    while ids.iter().any(|i| *i == new_id) {
        new_id = alloc.allocate();
    }

    let n_ckpts_before = h.checkpoints().len();
    let n_lanes_before = h.lanes.len();

    let ckpt = h
        .add_entity(
            new_id,
            arc_entity(new_id),
            CheckpointKind::AddEntity {
                entity: new_id,
                kind: MoleculeType::Water,
            },
            Cow::Borrowed("added"),
        )
        .unwrap();

    // Lane and checkpoint each grew by 1.
    assert_eq!(h.lanes.len(), n_lanes_before + 1);
    assert_eq!(h.checkpoints().len(), n_ckpts_before + 1);
    // New lane is keyed by the new id.
    assert!(h.lane(new_id).is_some());
    // Checkpoint's entity_heads contains both the original entity
    // and the new one.
    let heads = &h.checkpoint(ckpt).unwrap().entity_heads;
    assert!(heads.contains_key(&ids[0]));
    assert!(heads.contains_key(&new_id));

    // Adding the same id again is rejected.
    assert!(matches!(
        h.add_entity(
            new_id,
            arc_entity(new_id),
            CheckpointKind::AddEntity {
                entity: new_id,
                kind: MoleculeType::Water
            },
            Cow::Borrowed("dup")
        ),
        Err(HistoryError::EntityAlreadyExists { .. })
    ));
}

// ── WireId round-trip ────────────────────────────────────────────

#[test]
fn wire_id_round_trip_via_serde_string() {
    let (h, _) = mk_history(1);
    let head = h.checkpoints().head();
    let wire = WireId::new(head);
    let json = serde_json::to_string(&wire).unwrap();
    // Encoded as a JSON string, not a JSON number — that's the
    // whole point of WireId.
    assert!(json.starts_with('"') && json.ends_with('"'), "expected string, got {json}");
    let back: WireId<CheckpointId> = serde_json::from_str(&json).unwrap();
    assert_eq!(back.into_inner(), head);

    // Same for snapshot ids.
    let snap = h.lane(*h.lanes.keys().next().unwrap()).unwrap().head();
    let wire = WireId::new(snap);
    let json = serde_json::to_string(&wire).unwrap();
    let back: WireId<EntitySnapshotId> = serde_json::from_str(&json).unwrap();
    assert_eq!(back.into_inner(), snap);
}

#[test]
fn wire_id_round_trip_preserves_version_for_reused_slots() {
    // Forge a WireId with a high "version" component (upper 32 bits
    // of as_ffi). Confirm the string round-trip preserves it.
    let raw_high: u64 = (12345u64 << 32) | 7u64;
    let kd = KeyData::from_ffi(raw_high);
    let key: CheckpointId = kd.into();
    let wire = WireId::new(key);
    let s = wire.to_string();
    let back: WireId<CheckpointId> = s.parse().unwrap();
    assert_eq!(back.into_inner().data().as_ffi(), raw_high);
}

// ── Cross-DAG invariant proptest (G8) ─────────────────────────────
//
// Note: the History::record root *already* asserts the
// invariant on every public event when debug_assertions are on
// (which is the case under `cargo test`). So the proptest's job is
// mostly to feed the record root pseudo-random sequences and
// confirm no invariant assertion ever fires. We additionally verify
// the invariant from outside.

#[derive(Debug, Clone)]
enum Op {
    Record,
    BeginAction,
    UpdateAction,
    Commit,
    Abort,
    Undo,
    Redo,
    LaneUndoToRoot,
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        Just(Op::Record),
        Just(Op::BeginAction),
        Just(Op::UpdateAction),
        Just(Op::Commit),
        Just(Op::Abort),
        Just(Op::Undo),
        Just(Op::Redo),
        Just(Op::LaneUndoToRoot),
    ]
}

fn invariant_holds(h: &History) -> Result<(), String> {
    let head_ckpt = h
        .checkpoints
        .checkpoints
        .get(h.checkpoints.head)
        .ok_or_else(|| "head checkpoint is dead".to_string())?;
    // Point 1, relaxed for the open-action model: the lane head is either
    // the committed snapshot, or a tentative snapshot whose parent is it.
    for (eid, committed_snap) in &head_ckpt.entity_heads {
        let lane = h
            .lanes
            .get(eid)
            .ok_or_else(|| format!("ref to unknown entity {}", eid.raw()))?;
        if lane.head == *committed_snap {
            continue;
        }
        let head_snap = lane
            .snapshots
            .get(lane.head)
            .ok_or_else(|| format!("lane head missing for entity {}", eid.raw()))?;
        if !(head_snap.tentative && head_snap.parent == Some(*committed_snap)) {
            return Err(format!(
                "lane head for entity {} is neither committed snap {:?} nor a \
                 tentative child of it (got {:?}, tentative={})",
                eid.raw(),
                committed_snap,
                lane.head,
                head_snap.tentative
            ));
        }
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 64,
        .. ProptestConfig::default()
    })]

    #[test]
    fn cross_dag_invariant_holds_under_random_ops(
        ops in proptest::collection::vec(op_strategy(), 0..40)
    ) {
        let (mut h, ids) = mk_history(2);
        let e = ids[0];
        // Track the sole open action's request id across ops (begin sets
        // it, commit/abort clear it). A None/stale id makes update/commit/
        // abort return NoOngoingAction, which the invariant tolerates.
        // `next_rid` stands in for the orchestrator's monotonic allocator.
        let mut rid: Option<u64> = None;
        let mut next_rid: u64 = 1;

        for op in ops {
            let _ = match op {
                Op::Record => h
                    .record_entity_update(
                        e,
                        plugin_op("shake"),
                        arc_entity(e),
                        Cow::Borrowed("r"),
                        None,
                        None,
                    )
                    .map(|_| ()),
                Op::BeginAction => {
                    let id = next_rid;
                    next_rid += 1;
                    match h.begin_action(
                        [e],
                        plugin_op("wiggle"),
                        Cow::Borrowed("b"),
                        id,
                    ) {
                        Ok(()) => {
                            rid = Some(id);
                            Ok(())
                        }
                        Err(e) => Err(e),
                    }
                }
                Op::UpdateAction => {
                    h.action_update(rid.unwrap_or(u64::MAX), Some(0.0), Some(0.0), None, |_| {})
                }
                Op::Commit => {
                    let r = h.commit_action(rid.unwrap_or(u64::MAX)).map(|_| ());
                    if r.is_ok() {
                        rid = None;
                    }
                    r
                }
                Op::Abort => {
                    let r = h.abort_action(rid.unwrap_or(u64::MAX)).map(|_| ());
                    if r.is_ok() {
                        rid = None;
                    }
                    r
                }
                Op::Undo => h.undo().map(|_| ()),
                Op::Redo => h.redo(None).map(|_| ()),
                Op::LaneUndoToRoot => {
                    let r = h.lane(e).unwrap().root();
                    h.lane_undo(e, r).map(|_| ())
                }
            };

            prop_assert!(invariant_holds(&h).is_ok(), "{:?}", invariant_holds(&h));
        }
    }
}
