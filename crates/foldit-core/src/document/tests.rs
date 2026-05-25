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
/// this into [`Document::insert_preview`] don't observe the
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

/// A coord-only [`CheckpointKind::Wiggle`] for `entity`. Shared by the
/// record/commit/navigation tests.
fn wiggle(entity: EntityId) -> CheckpointKind {
    CheckpointKind::Wiggle {
        entity,
        mask: WiggleMask::default(),
        duration_ms: 1,
    }
}

// ── Preview lifecycle: insert → promote moves into history ────────

#[test]
fn insert_preview_then_promote_lands_in_history() {
    let mut store = Document::new();
    let alloc_id = {
        // Burn a few ids so we can verify preview keys are minted
        // by Document::insert_preview.
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
    let mut store = Document::new();
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
    assert!(matches!(err, DocumentError::NotAPreview { .. }));
}

// ── Live membership: derived from history + transient, not metadata ──

#[test]
fn live_membership_lists_committed_then_preview() {
    let mut store = Document::new();
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
    let mut store = Document::new();
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
    let mut store = Document::new();
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

// ── SceneChange spine emission ────────────────────────────────────
//
// These assert the *funnel*: each mutator emits exactly the expected
// `SceneChange` (or none). The Full/Delta projection of those changes is
// the `PluginBroadcaster`'s job and is tested in `plugin_driver`.

/// Drive an entity through promote_preview → drain so the change queue
/// is at a known-empty starting point.
fn store_with_protein(n_residues: usize) -> (Document, EntityId) {
    let mut store = Document::new();
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
    let _ = store.take_scene_changes();
    (store, id)
}

#[test]
fn pending_changes_empty_at_construction() {
    let mut store = Document::new();
    assert!(store.take_scene_changes().is_empty());
}

#[test]
fn insert_preview_emits_preview_added() {
    let mut store = Document::new();
    let id = store.insert_preview(
        mk_protein(mk_dummy_id(), 2),
        "p".to_string(),
        EntityOrigin::Loaded,
    );
    let changes = store.take_scene_changes();
    assert!(
        matches!(changes.as_slice(), [SceneChange::PreviewAdded { entity }] if *entity == id),
        "got {changes:?}",
    );
    // Drain is destructive — second take returns empty.
    assert!(store.take_scene_changes().is_empty());
}

#[test]
fn remove_preview_emits_preview_discarded() {
    let mut store = Document::new();
    let id = store.insert_preview(
        mk_protein(mk_dummy_id(), 1),
        "p".to_string(),
        EntityOrigin::Loaded,
    );
    let _ = store.take_scene_changes();
    assert!(store.remove_preview(id));
    let changes = store.take_scene_changes();
    assert!(
        matches!(changes.as_slice(), [SceneChange::PreviewDiscarded { entity }] if *entity == id),
        "got {changes:?}",
    );
}

#[test]
fn remove_preview_unknown_emits_nothing() {
    let mut store = Document::new();
    assert!(!store.remove_preview(mk_dummy_id()));
    assert!(store.take_scene_changes().is_empty());
}

#[test]
fn promote_preview_emits_head_moved() {
    let mut store = Document::new();
    let id = store.insert_preview(
        mk_protein(mk_dummy_id(), 1),
        "p".to_string(),
        EntityOrigin::Loaded,
    );
    let _ = store.take_scene_changes();
    store
        .promote_preview(
            id,
            CheckpointKind::PromotedPreview { entity: id },
            None,
            None,
            "promote",
        )
        .expect("promote_preview");
    let changes = store.take_scene_changes();
    assert!(matches!(changes.as_slice(), [SceneChange::HeadMoved { .. }]), "got {changes:?}");
}

#[test]
fn record_entity_update_emits_head_moved() {
    let (mut store, id) = store_with_protein(1);
    store
        .record_entity_update(wiggle(id), id, mk_protein(id, 1), "wiggle", None, None)
        .expect("record_entity_update");
    let changes = store.take_scene_changes();
    assert!(matches!(changes.as_slice(), [SceneChange::HeadMoved { .. }]), "got {changes:?}");
}

#[test]
fn begin_action_emits_nothing() {
    let (mut store, id) = store_with_protein(2);
    store.begin_action(wiggle(id), "wiggle").expect("begin_action");
    assert!(store.take_scene_changes().is_empty());
}

#[test]
fn action_update_emits_tentative_edit_with_post_mutation_coords() {
    let (mut store, id) = store_with_protein(2);
    store.begin_action(wiggle(id), "wiggle").expect("begin_action");
    let _ = store.take_scene_changes();

    store
        .action_update(None, None, None, |e| {
            for atom in e.atom_set_mut() {
                atom.position = glam::Vec3::new(9.0, 9.0, 9.0);
            }
        })
        .expect("action_update");

    let changes = store.take_scene_changes();
    let [SceneChange::Edit {
        edit: AssemblyEdit::SetEntityCoords { entity, coords },
        tentative,
    }] = changes.as_slice()
    else {
        panic!("expected one tentative SetEntityCoords edit, got {changes:?}");
    };
    assert!(*tentative, "action_update edits must be tentative");
    assert_eq!(*entity, id);
    assert!(coords.iter().all(|c| *c == glam::Vec3::new(9.0, 9.0, 9.0)));
}

#[test]
fn commit_action_emits_head_moved() {
    let (mut store, id) = store_with_protein(2);
    store.begin_action(wiggle(id), "wiggle").expect("begin_action");
    store
        .action_update(None, None, None, |e| {
            for atom in e.atom_set_mut() {
                atom.position = glam::Vec3::new(9.0, 9.0, 9.0);
            }
        })
        .expect("action_update");
    // Drain begin (none) + action_update (tentative Edit) so the next
    // take only sees the commit.
    let _ = store.take_scene_changes();

    store.commit_action().expect("commit_action");
    let changes = store.take_scene_changes();
    assert!(matches!(changes.as_slice(), [SceneChange::HeadMoved { .. }]), "got {changes:?}");
}

#[test]
fn abort_action_emits_head_moved() {
    let (mut store, id) = store_with_protein(2);
    store.begin_action(wiggle(id), "wiggle").expect("begin_action");
    let _ = store.take_scene_changes();
    store.abort_action().expect("abort_action");
    let changes = store.take_scene_changes();
    assert!(matches!(changes.as_slice(), [SceneChange::HeadMoved { .. }]), "got {changes:?}");
}

#[test]
fn undo_then_redo_each_emit_head_moved() {
    let (mut store, id) = store_with_protein(2);
    store
        .record_entity_update(wiggle(id), id, mk_protein(id, 2), "wiggle", None, None)
        .expect("record_entity_update");
    let _ = store.take_scene_changes();

    store.undo().expect("undo");
    assert!(
        matches!(store.take_scene_changes().as_slice(), [SceneChange::HeadMoved { .. }]),
        "undo emits HeadMoved",
    );
    store.redo(None).expect("redo");
    assert!(
        matches!(store.take_scene_changes().as_slice(), [SceneChange::HeadMoved { .. }]),
        "redo emits HeadMoved",
    );
}

#[test]
fn undo_at_root_emits_nothing() {
    let mut store = Document::new();
    assert_eq!(store.undo().expect("undo"), None);
    assert!(store.take_scene_changes().is_empty());
}

#[test]
fn redo_at_leaf_emits_nothing() {
    let (mut store, _id) = store_with_protein(2);
    assert_eq!(store.redo(None).expect("redo"), None);
    assert!(store.take_scene_changes().is_empty());
}

#[test]
fn lane_undo_emits_head_moved() {
    let (mut store, id) = store_with_protein(2);
    let original = store.history().lane(id).expect("lane").head();
    store
        .record_entity_update(wiggle(id), id, mk_protein(id, 2), "wiggle", None, None)
        .expect("record_entity_update");
    let _ = store.take_scene_changes();

    store.lane_undo(id, original).expect("lane_undo");
    assert!(
        matches!(store.take_scene_changes().as_slice(), [SceneChange::HeadMoved { .. }]),
        "lane_undo emits HeadMoved",
    );
}

#[test]
fn jump_checkpoint_emits_head_moved() {
    let (mut store, _id) = store_with_protein(2);
    let first = store.history().checkpoints().head();

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
    let _ = store.take_scene_changes();

    store.jump_checkpoint(first).expect("jump_checkpoint");
    assert!(
        matches!(store.take_scene_changes().as_slice(), [SceneChange::HeadMoved { .. }]),
        "jump_checkpoint emits HeadMoved",
    );
}

#[test]
fn set_head_scores_emits_scores_updated() {
    let (mut store, _id) = store_with_protein(2);
    store.set_head_scores(Some(1.0), Some(2.0));
    let changes = store.take_scene_changes();
    assert!(matches!(changes.as_slice(), [SceneChange::ScoresUpdated]), "got {changes:?}");
}

#[test]
fn reset_clears_pending_then_emits_one_head_moved() {
    let (mut store, _id) = store_with_protein(2);
    // Leave an undrained change in the queue — reset must drop it before
    // emitting its own HeadMoved.
    store.set_head_scores(Some(1.0), None);

    store.reset();

    let changes = store.take_scene_changes();
    assert!(
        matches!(changes.as_slice(), [SceneChange::HeadMoved { .. }]),
        "reset drops pending changes and emits exactly one HeadMoved, got {changes:?}",
    );
}
