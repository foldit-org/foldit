use super::*;
use crate::history::WiggleMask;
use molex::ops::edit::AssemblyEdit;
use molex::entity::molecule::atom::Atom;
use molex::entity::molecule::bulk::BulkEntity;
use molex::entity::molecule::protein::ProteinEntity;
use molex::entity::molecule::polymer::Residue;
use molex::Element;

fn mk_atom() -> Atom {
    Atom {
        position: glam::Vec3::ZERO,
        occupancy: 1.0,
        b_factor: 0.0,
        element: Element::O,
        name: *b"O   ",
        formal_charge: 0,
    }
}

/// Some valid EntityId. `EntityId` has no public constructor so
/// every call mints id 0 from a fresh allocator. Callers that pass
/// this into [`EntityStore::insert_preview`] don't observe the
/// value because the store overwrites the entity's id immediately.
fn mk_dummy_id() -> EntityId {
    EntityIdAllocator::new().allocate()
}

fn mk_bulk(id: EntityId) -> MoleculeEntity {
    MoleculeEntity::Bulk(BulkEntity::new(
        id,
        MoleculeType::Water,
        vec![mk_atom()],
        *b"HOH",
        1,
    ))
}

/// Construct a protein with `n_residues` residues. Each residue
/// has the four backbone atoms (N, CA, C, O) — required by the
/// `ProteinEntity` constructor's canonicalization, which silently
/// drops residues that lack a complete backbone.
fn mk_protein(id: EntityId, n_residues: usize) -> MoleculeEntity {
    let backbone_names = [b"N   ", b"CA  ", b"C   ", b"O   "];
    let backbone_elements = [Element::N, Element::C, Element::C, Element::O];
    let mut atoms = Vec::with_capacity(n_residues * 4);
    let mut residues = Vec::with_capacity(n_residues);
    for i in 0..n_residues {
        let start = atoms.len();
        for (name, element) in backbone_names.iter().zip(backbone_elements.iter()) {
            atoms.push(Atom {
                position: glam::Vec3::ZERO,
                occupancy: 1.0,
                b_factor: 0.0,
                element: *element,
                name: **name,
                formal_charge: 0,
            });
        }
        let end = atoms.len();
        residues.push(Residue {
            name: *b"ALA",
            label_seq_id: i as i32 + 1,
            auth_seq_id: None,
            auth_comp_id: None,
            ins_code: None,
            atom_range: start..end,
            variants: Vec::new(),
        });
    }
    MoleculeEntity::Protein(ProteinEntity::new_continuous(id, atoms, residues, b'A', None))
}

// ── Preview lifecycle: insert → promote moves into history ────────

#[test]
fn insert_preview_then_promote_lands_in_history() {
    let mut store = EntityStore::new();
    let alloc_id = {
        // Burn a few ids so we can verify preview keys are minted
        // by EntityStore::insert_preview.
        store.allocator.allocate()
    };
    let _ = alloc_id;

    let id = store.insert_preview(
        mk_bulk(mk_dummy_id()),
        "preview".to_string(),
        EntityOrigin::Loaded,
    );
    assert!(store.is_preview(id));
    // Preview is visible in head_assembly.
    let asm = store.head_assembly();
    assert_eq!(asm.entities().len(), 1);
    // Preview is NOT in the checkpoint head (not in history).
    let head = store.history().checkpoint(store.history().checkpoints().head()).unwrap();
    assert!(!head.entity_heads.contains_key(&id));

    // Promote.
    let ckpt = store
        .promote_preview(
            id,
            CheckpointKind::PromotedPreview { entity: id },
            None,
            None,
            "promoted",
        )
        .unwrap();
    // No longer a preview.
    assert!(!store.is_preview(id));
    // Now in history; new checkpoint references the entity.
    let new_head = store.history().checkpoint(ckpt).unwrap();
    assert!(new_head.entity_heads.contains_key(&id));
}

#[test]
fn promote_preview_unknown_id_returns_not_a_preview() {
    let mut store = EntityStore::new();
    let mut alloc = EntityIdAllocator::new();
    let stranger = alloc.allocate();
    let err = store
        .promote_preview(
            stranger,
            CheckpointKind::PromotedPreview { entity: stranger },
            None,
            None,
            "no",
        )
        .unwrap_err();
    assert!(matches!(err, EntityStoreError::NotAPreview { .. }));
}

// ── Live membership: derived from history + transient, not metadata ──

#[test]
fn live_membership_lists_committed_then_preview() {
    let mut store = EntityStore::new();
    // Insert + promote A: a committed entity.
    let a = store.insert_preview(
        mk_protein(mk_dummy_id(), 2),
        "a".to_string(),
        EntityOrigin::Loaded,
    );
    store
        .promote_preview(
            a,
            CheckpointKind::PromotedPreview { entity: a },
            None,
            None,
            "a",
        )
        .expect("promote a");
    // Insert B and leave it as a preview.
    let b = store.insert_preview(
        mk_bulk(mk_dummy_id()),
        "b".to_string(),
        EntityOrigin::Loaded,
    );

    assert_eq!(store.count(), 2);
    // Committed first, then preview.
    assert_eq!(store.ids().collect::<Vec<_>>(), vec![a, b]);
    assert!(!store.is_preview(a));
    assert!(store.is_preview(b));
    // loaded_entity is the first committed entity.
    assert_eq!(store.loaded_entity(), Some(a));
}

#[test]
fn undone_entity_drops_from_membership_though_metadata_lingers() {
    // The point of P2: membership is derived from the live head
    // checkpoint, so navigating back past an entity's checkpoint drops
    // it from ids/count/iter — even though its side-table metadata is
    // never GC'd. The old metadata-keyed implementation got this wrong.
    let mut store = EntityStore::new();
    let x = store.insert_preview(
        mk_protein(mk_dummy_id(), 2),
        "x".to_string(),
        EntityOrigin::Loaded,
    );
    store
        .promote_preview(
            x,
            CheckpointKind::PromotedPreview { entity: x },
            None,
            None,
            "x",
        )
        .expect("promote x");
    assert_eq!(store.count(), 1);
    assert!(store.ids().any(|id| id == x));

    // Navigate back past X's checkpoint to the empty root.
    store.undo().expect("undo");

    // Metadata lingers: the side table still holds X.
    assert!(store.metadata(x).is_some());
    // Derived membership must NOT surface the undone entity.
    assert_eq!(store.count(), 0);
    assert!(store.ids().next().is_none());
    assert!(store.iter().next().is_none());
}

// ── Reset clears everything ───────────────────────────────────────

#[test]
fn reset_clears_history_metadata_and_transient() {
    let mut store = EntityStore::new();
    let id = store.insert_preview(
        mk_bulk(mk_dummy_id()),
        "x".to_string(),
        EntityOrigin::Loaded,
    );
    assert_eq!(store.count(), 1);
    assert!(store.is_preview(id));

    store.reset();

    assert_eq!(store.count(), 0);
    assert!(!store.is_preview(id));
    assert_eq!(store.history().checkpoints().len(), 1); // root only
    assert!(store
        .history()
        .checkpoint(store.history().checkpoints().head())
        .unwrap()
        .entity_heads
        .is_empty());
}

// ── plugin-broadcast queue ────────────────────────────────────────

#[test]
fn pending_broadcasts_empty_at_construction() {
    let mut store = EntityStore::new();
    assert!(store.take_pending_broadcasts().is_empty());
}

#[test]
fn insert_preview_queues_one_full_broadcast() {
    let mut store = EntityStore::new();
    let _id = store.insert_preview(
        mk_protein(mk_dummy_id(), 2),
        "p".to_string(),
        EntityOrigin::Loaded,
    );
    let payloads = store.take_pending_broadcasts();
    assert_eq!(payloads.len(), 1);
    match &payloads[0] {
        foldit_runner::orchestrator::BroadcastPayload::Full(bytes) => {
            assert!(!bytes.is_empty(), "Full payload bytes should be non-empty");
        }
        other => panic!("expected Full payload, got {other:?}"),
    }
    // Drain is destructive — second take returns empty.
    assert!(store.take_pending_broadcasts().is_empty());
}

#[test]
fn remove_preview_also_queues_a_broadcast() {
    let mut store = EntityStore::new();
    let id = store.insert_preview(
        mk_protein(mk_dummy_id(), 1),
        "p".to_string(),
        EntityOrigin::Loaded,
    );
    // Drain the insert broadcast.
    let _ = store.take_pending_broadcasts();
    assert!(store.remove_preview(id));
    let payloads = store.take_pending_broadcasts();
    assert_eq!(payloads.len(), 1, "remove_preview must queue a broadcast");
}

#[test]
fn record_entity_update_queues_one_broadcast() {
    let mut store = EntityStore::new();
    let id = store.insert_preview(
        mk_protein(mk_dummy_id(), 1),
        "p".to_string(),
        EntityOrigin::Loaded,
    );
    store
        .promote_preview(
            id,
            CheckpointKind::PromotedPreview { entity: id },
            None,
            None,
            "promote",
        )
        .expect("promote_preview");
    // Drain the insert + promote broadcasts.
    let _ = store.take_pending_broadcasts();

    let updated = mk_protein(id, 1);
    store
        .record_entity_update(
            CheckpointKind::Wiggle {
                entity: id,
                mask: WiggleMask::default(),
                duration_ms: 1,
            },
            id,
            updated,
            "wiggle",
            None,
            None,
        )
        .expect("record_entity_update");

    let payloads = store.take_pending_broadcasts();
    assert_eq!(payloads.len(), 1);
}

// ── Single-entity mutation Delta-payload emission ────────────────

/// Helper: drive an entity through promote_preview → drain so the
/// broadcast queue is at a known-empty starting point.
fn store_with_protein(n_residues: usize) -> (EntityStore, EntityId) {
    let mut store = EntityStore::new();
    let id = store.insert_preview(
        mk_protein(mk_dummy_id(), n_residues),
        "p".to_string(),
        EntityOrigin::Loaded,
    );
    store
        .promote_preview(
            id,
            CheckpointKind::PromotedPreview { entity: id },
            None,
            None,
            "promote",
        )
        .expect("promote_preview");
    let _ = store.take_pending_broadcasts();
    (store, id)
}

#[test]
fn record_entity_update_with_matching_topology_emits_delta() {
    let (mut store, id) = store_with_protein(2);

    // Re-issue the same shape but with shifted positions.
    let mut updated = mk_protein(id, 2);
    for atom in updated.atom_set_mut() {
        atom.position = glam::Vec3::new(1.5, 2.5, 3.5);
    }
    store
        .record_entity_update(
            CheckpointKind::Wiggle {
                entity: id,
                mask: WiggleMask::default(),
                duration_ms: 1,
            },
            id,
            updated,
            "wiggle",
            None,
            None,
        )
        .expect("record_entity_update");

    let payloads = store.take_pending_broadcasts();
    assert_eq!(payloads.len(), 1);
    match &payloads[0] {
        foldit_runner::orchestrator::BroadcastPayload::Delta(bytes) => {
            let edits = molex::ops::wire::delta::deserialize_edits(bytes)
                .expect("delta decodes");
            assert_eq!(edits.len(), 1);
            assert!(matches!(
                edits[0],
                AssemblyEdit::SetEntityCoords { .. }
            ));
        }
        other => panic!("expected Delta, got {other:?}"),
    }
}

#[test]
fn record_entity_update_with_topology_change_falls_back_to_full() {
    let (mut store, id) = store_with_protein(2);

    // Different residue count → topology change → Full.
    let updated = mk_protein(id, 5);
    store
        .record_entity_update(
            CheckpointKind::Mutate {
                entity: id,
                residue: 0,
                from: b'A',
                to: b'W',
            },
            id,
            updated,
            "mutate",
            None,
            None,
        )
        .expect("record_entity_update");

    let payloads = store.take_pending_broadcasts();
    assert_eq!(payloads.len(), 1);
    assert!(matches!(
        payloads[0],
        foldit_runner::orchestrator::BroadcastPayload::Full(_)
    ));
}

#[test]
fn record_entity_update_delta_round_trips_through_apply_edit() {
    // Round-trip invariant: applying the emitted delta to the
    // *prior* assembly must produce the same post-mutation
    // assembly the host now holds.
    let (mut store, id) = store_with_protein(2);

    // Prior assembly is the current head_assembly() before the
    // update — capture it for the receiver-side replay.
    let prior_asm = store.head_assembly();

    let mut updated = mk_protein(id, 2);
    for (i, atom) in updated.atom_set_mut().iter_mut().enumerate() {
        atom.position = glam::Vec3::new(i as f32, i as f32 * 2.0, -1.0);
    }
    store
        .record_entity_update(
            CheckpointKind::Wiggle {
                entity: id,
                mask: WiggleMask::default(),
                duration_ms: 1,
            },
            id,
            updated,
            "wiggle",
            None,
            None,
        )
        .expect("record_entity_update");

    let payloads = store.take_pending_broadcasts();
    let bytes = match &payloads[0] {
        foldit_runner::orchestrator::BroadcastPayload::Delta(b) => b.clone(),
        other => panic!("expected Delta, got {other:?}"),
    };
    let edits = molex::ops::wire::delta::deserialize_edits(&bytes)
        .expect("delta decodes");

    // Replay: clone prior, apply edits, compare positions to host head.
    let mut replay = prior_asm;
    replay.apply_edits(&edits).expect("apply_edits");

    let host_head = store.head_assembly();
    let replay_positions = replay
        .entities()
        .iter()
        .find(|e| e.id() == id)
        .expect("entity in replay")
        .positions();
    let host_positions = host_head
        .entities()
        .iter()
        .find(|e| e.id() == id)
        .expect("entity in host head")
        .positions();
    assert_eq!(replay_positions, host_positions);
}

#[test]
fn commit_action_with_coord_only_mutation_emits_delta() {
    let (mut store, id) = store_with_protein(2);

    store
        .begin_action(
            CheckpointKind::Wiggle {
                entity: id,
                mask: WiggleMask::default(),
                duration_ms: 1,
            },
            "wiggle",
        )
        .expect("begin_action");
    // Drain the broadcast emitted during begin (currently none —
    // begin_action does not queue; sanity-check by clearing).
    let _ = store.take_pending_broadcasts();

    store
        .action_update(None, None, None, |e| {
            for atom in e.atom_set_mut() {
                atom.position = glam::Vec3::new(9.0, 9.0, 9.0);
            }
        })
        .expect("action_update");

    store.commit_action().expect("commit_action");

    let payloads = store.take_pending_broadcasts();
    assert_eq!(payloads.len(), 1);
    match &payloads[0] {
        foldit_runner::orchestrator::BroadcastPayload::Delta(bytes) => {
            let edits = molex::ops::wire::delta::deserialize_edits(bytes)
                .expect("delta decodes");
            assert_eq!(edits.len(), 1);
            let AssemblyEdit::SetEntityCoords { entity, coords } = &edits[0]
            else {
                panic!("expected SetEntityCoords, got {:?}", edits[0]);
            };
            assert_eq!(*entity, id);
            assert!(coords.iter().all(|c| *c == glam::Vec3::new(9.0, 9.0, 9.0)));
        }
        other => panic!("expected Delta, got {other:?}"),
    }
}

// ── History navigation Delta emission ─────────────────────────────

#[test]
fn lane_undo_with_coord_only_history_emits_delta() {
    let (mut store, id) = store_with_protein(2);
    let original_snap = store.history().lane(id).expect("lane").head();

    // Move the lane head forward with a coord-only update.
    let mut updated = mk_protein(id, 2);
    for atom in updated.atom_set_mut() {
        atom.position = glam::Vec3::new(4.0, 5.0, 6.0);
    }
    store
        .record_entity_update(
            CheckpointKind::Wiggle {
                entity: id,
                mask: WiggleMask::default(),
                duration_ms: 1,
            },
            id,
            updated,
            "wiggle",
            None,
            None,
        )
        .expect("record_entity_update");

    // Drain the wiggle broadcast; the next take only sees lane_undo.
    let _ = store.take_pending_broadcasts();
    let prior_asm = store.head_assembly();

    store.lane_undo(id, original_snap).expect("lane_undo");

    let payloads = store.take_pending_broadcasts();
    assert_eq!(payloads.len(), 1);
    let bytes = match &payloads[0] {
        foldit_runner::orchestrator::BroadcastPayload::Delta(b) => b.clone(),
        other => panic!("expected Delta, got {other:?}"),
    };
    let edits = molex::ops::wire::delta::deserialize_edits(&bytes)
        .expect("delta decodes");

    let mut replay = prior_asm;
    replay.apply_edits(&edits).expect("apply_edits");
    let host_head = store.head_assembly();
    assert_eq!(
        replay
            .entities()
            .iter()
            .find(|e| e.id() == id)
            .expect("entity")
            .positions(),
        host_head
            .entities()
            .iter()
            .find(|e| e.id() == id)
            .expect("entity")
            .positions(),
    );
}

#[test]
fn undo_within_topology_emits_delta() {
    let (mut store, id) = store_with_protein(2);

    let mut updated = mk_protein(id, 2);
    for atom in updated.atom_set_mut() {
        atom.position = glam::Vec3::new(7.0, 8.0, 9.0);
    }
    store
        .record_entity_update(
            CheckpointKind::Wiggle {
                entity: id,
                mask: WiggleMask::default(),
                duration_ms: 1,
            },
            id,
            updated,
            "wiggle",
            None,
            None,
        )
        .expect("record_entity_update");
    let _ = store.take_pending_broadcasts();
    let prior_asm = store.head_assembly();

    store.undo().expect("undo");

    let payloads = store.take_pending_broadcasts();
    assert_eq!(payloads.len(), 1);
    let bytes = match &payloads[0] {
        foldit_runner::orchestrator::BroadcastPayload::Delta(b) => b.clone(),
        other => panic!("expected Delta, got {other:?}"),
    };
    let edits = molex::ops::wire::delta::deserialize_edits(&bytes)
        .expect("delta decodes");
    let mut replay = prior_asm;
    replay.apply_edits(&edits).expect("apply_edits");
    let host_head = store.head_assembly();
    assert_eq!(
        replay
            .entities()
            .iter()
            .find(|e| e.id() == id)
            .expect("entity")
            .positions(),
        host_head
            .entities()
            .iter()
            .find(|e| e.id() == id)
            .expect("entity")
            .positions(),
    );
}

#[test]
fn jump_checkpoint_topology_change_falls_back_to_full() {
    // Start with one entity, capture its checkpoint. Add a second
    // entity via promote_preview (new checkpoint with both
    // entities). Jumping back to the single-entity checkpoint is a
    // topology change → Full broadcast.
    let (mut store, _id_a) = store_with_protein(2);
    let single_entity_ckpt = store.history().checkpoints().head();

    let id_b = store.insert_preview(
        mk_protein(mk_dummy_id(), 3),
        "b".to_string(),
        EntityOrigin::Loaded,
    );
    store
        .promote_preview(
            id_b,
            CheckpointKind::PromotedPreview { entity: id_b },
            None,
            None,
            "promote b",
        )
        .expect("promote b");
    let _ = store.take_pending_broadcasts();

    store
        .jump_checkpoint(single_entity_ckpt)
        .expect("jump_checkpoint");

    let payloads = store.take_pending_broadcasts();
    assert_eq!(payloads.len(), 1);
    assert!(matches!(
        payloads[0],
        foldit_runner::orchestrator::BroadcastPayload::Full(_)
    ));
}

// ── Preview lifecycle Delta + Full emission ──────────────────────

#[test]
fn remove_preview_emits_full() {
    let mut store = EntityStore::new();
    let id = store.insert_preview(
        mk_protein(mk_dummy_id(), 1),
        "p".to_string(),
        EntityOrigin::Loaded,
    );
    let _ = store.take_pending_broadcasts();
    assert!(store.remove_preview(id));
    let payloads = store.take_pending_broadcasts();
    assert_eq!(payloads.len(), 1);
    assert!(matches!(
        payloads[0],
        foldit_runner::orchestrator::BroadcastPayload::Full(_)
    ));
}

#[test]
fn promote_preview_emits_full() {
    let mut store = EntityStore::new();
    let id = store.insert_preview(
        mk_protein(mk_dummy_id(), 1),
        "p".to_string(),
        EntityOrigin::Loaded,
    );
    let _ = store.take_pending_broadcasts();
    store
        .promote_preview(
            id,
            CheckpointKind::PromotedPreview { entity: id },
            None,
            None,
            "promote",
        )
        .expect("promote_preview");
    let payloads = store.take_pending_broadcasts();
    assert_eq!(payloads.len(), 1);
    assert!(matches!(
        payloads[0],
        foldit_runner::orchestrator::BroadcastPayload::Full(_)
    ));
}

#[test]
fn reset_emits_full() {
    let (mut store, _id) = store_with_protein(2);
    // Queue a couple of stale payloads to confirm reset clears them
    // before emitting its own Full.
    let _ = store.queue_full_broadcast();
    let _ = store.queue_full_broadcast();

    store.reset();

    let payloads = store.take_pending_broadcasts();
    assert_eq!(
        payloads.len(),
        1,
        "reset drops queued payloads and emits exactly one Full",
    );
    assert!(matches!(
        payloads[0],
        foldit_runner::orchestrator::BroadcastPayload::Full(_)
    ));
}
