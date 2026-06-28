// foldit:allow-long-file: exhaustive session unit-test module; length is intrinsic.
use super::*;
use crate::history::CheckpointKind;
use molex::entity::molecule::atom::Atom;
use molex::entity::molecule::bulk::BulkEntity;
use molex::entity::molecule::protein::ProteinEntity;
use molex::entity::molecule::polymer::Residue;
use molex::Element;
use molex::MoleculeType;

fn mk_atom() -> Atom {
    Atom {
        position: glam::Vec3::ZERO,
        occupancy: 1.0,
        b_factor: 0.0,
        element: Element::O,
        name: *b"O   ",
        formal_charge: 0,
        observed: true,
    }
}

/// Some valid `EntityId`. `EntityId` has no public constructor so
/// every call mints id 0 from a fresh allocator. Callers that pass
/// this into [`Session::insert_preview`] don't observe the
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
        String::from("W"),
    ))
}

/// Construct a protein with `n_residues` residues. Each residue
/// has the four backbone atoms (N, CA, C, O) - required by the
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
                observed: true,
            });
        }
        let end = atoms.len();
        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
        let label_seq_id = i as i32 + 1;
        residues.push(Residue {
            name: *b"ALA",
            label_seq_id,
            auth_seq_id: None,
            auth_comp_id: None,
            ins_code: None,
            atom_range: start..end,
            variants: Vec::new(),
        });
    }
    MoleculeEntity::Protein(ProteinEntity::new_continuous(id, atoms, residues, "A".to_owned()))
}

/// A plugin-op checkpoint kind standing in for a streaming action.
/// Shared by the record/commit/navigation tests; the entity it runs on
/// is passed to `begin_action` separately.
fn wiggle() -> CheckpointKind {
    CheckpointKind::PluginOp {
        plugin_id: "rosetta".to_owned(),
        op_id: "wiggle".to_owned(),
        display: "wiggle".to_owned(),
    }
}

// ── Preview lifecycle: insert → promote moves into history ────────

#[test]
fn insert_preview_then_promote_lands_in_history() {
    let mut store = Session::new();
    let alloc_id = {
        // Burn a few ids so we can verify preview keys are minted
        // by Session::insert_preview.
        store.allocator.allocate()
    };
    let _ = alloc_id;

    let id = store.insert_preview(
        mk_bulk(mk_dummy_id()),
        "preview".to_owned(),
    );
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
            "promoted",
        )
        .unwrap();
    // Now in history; new checkpoint references the entity.
    let new_head = store.history().checkpoint(ckpt).unwrap();
    assert!(new_head.entity_heads.contains_key(&id));
}

#[test]
fn promote_preview_unknown_id_returns_not_a_preview() {
    let mut store = Session::new();
    let mut alloc = EntityIdAllocator::new();
    let stranger = alloc.allocate();
    let err = store
        .promote_preview(
            stranger,
            CheckpointKind::PromotedPreview { entity: stranger },
            None,
            "no",
        )
        .unwrap_err();
    assert!(matches!(err, SessionError::NotAPreview { .. }));
}

// ── Live membership: derived from history + transient, not metadata ──

#[test]
fn live_membership_lists_committed_then_preview() {
    let mut store = Session::new();
    // Insert + promote A: a committed entity.
    let a = store.insert_preview(
        mk_protein(mk_dummy_id(), 2),
        "a".to_owned(),
    );
    store
        .promote_preview(
            a,
            CheckpointKind::PromotedPreview { entity: a },
            None,
            "a",
        )
        .expect("promote a");
    // Insert B and leave it as a preview.
    let b = store.insert_preview(
        mk_bulk(mk_dummy_id()),
        "b".to_owned(),
    );

    assert_eq!(store.count(), 2);
    // Committed first, then preview.
    assert_eq!(store.ids().collect::<Vec<_>>(), vec![a, b]);
}

#[test]
fn undone_entity_drops_from_membership_though_metadata_lingers() {
    // Membership is derived from the live head
    // checkpoint, so navigating back past an entity's checkpoint drops
    // it from ids/count/iter - even though its side-table metadata is
    // never GC'd. The old metadata-keyed implementation got this wrong.
    let mut store = Session::new();
    let x = store.insert_preview(
        mk_protein(mk_dummy_id(), 2),
        "x".to_owned(),
    );
    store
        .promote_preview(
            x,
            CheckpointKind::PromotedPreview { entity: x },
            None,
            "x",
        )
        .expect("promote x");
    assert_eq!(store.count(), 1);
    assert!(store.ids().any(|id| id == x));

    // Navigate back past X's checkpoint to the empty root.
    store.undo().expect("undo");

    // Name lingers: the side table still holds X.
    assert!(store.name(x).is_some());
    // Derived membership must NOT surface the undone entity.
    assert_eq!(store.count(), 0);
    assert!(store.ids().next().is_none());
    assert!(store.iter().next().is_none());
}

// ── Reset clears everything ───────────────────────────────────────

#[test]
fn reset_clears_history_metadata_and_transient() {
    let mut store = Session::new();
    let _id = store.insert_preview(
        mk_bulk(mk_dummy_id()),
        "x".to_owned(),
    );
    assert_eq!(store.count(), 1);

    store.reset();

    assert_eq!(store.count(), 0);
    assert_eq!(store.history().checkpoints().iter().count(), 1); // root only
    assert!(store
        .history()
        .checkpoint(store.history().checkpoints().head())
        .unwrap()
        .entity_heads
        .is_empty());
}

// ── SessionUpdate stream emission ───────────────────────────────────
//
// These assert the *funnel*: each mutator emits exactly the expected
// `SessionUpdate` (or none). The Full/Delta projection of those changes is
// the `RunnerProjector`'s job and is tested in `runner_projector`.

/// Drive an entity through `promote_preview` → drain so the change queue
/// is at a known-empty starting point.
fn store_with_protein(n_residues: usize) -> (Session, EntityId) {
    let mut store = Session::new();
    let id = store.insert_preview(
        mk_protein(mk_dummy_id(), n_residues),
        "p".to_owned(),
    );
    store
        .promote_preview(
            id,
            CheckpointKind::PromotedPreview { entity: id },
            None,
            "promote",
        )
        .expect("promote_preview");
    let _ = store.take_updates();
    (store, id)
}

#[test]
fn pending_updates_empty_at_construction() {
    let mut store = Session::new();
    assert!(store.take_updates().is_empty());
}

#[test]
fn insert_preview_emits_preview_added() {
    let mut store = Session::new();
    let _ = store.insert_preview(
        mk_protein(mk_dummy_id(), 2),
        "p".to_owned(),
    );
    let changes = store.take_updates();
    assert!(
        matches!(changes.as_slice(), [SessionUpdate::PreviewAdded]),
        "got {changes:?}",
    );
    // Drain is destructive - second take returns empty.
    assert!(store.take_updates().is_empty());
}

#[test]
fn remove_preview_emits_preview_discarded() {
    let mut store = Session::new();
    let id = store.insert_preview(
        mk_protein(mk_dummy_id(), 1),
        "p".to_owned(),
    );
    let _ = store.take_updates();
    assert!(store.remove_preview(id));
    let changes = store.take_updates();
    assert!(
        matches!(changes.as_slice(), [SessionUpdate::PreviewDiscarded]),
        "got {changes:?}",
    );
}

#[test]
fn remove_preview_unknown_emits_nothing() {
    let mut store = Session::new();
    assert!(!store.remove_preview(mk_dummy_id()));
    assert!(store.take_updates().is_empty());
}

#[test]
fn promote_preview_emits_head_moved() {
    let mut store = Session::new();
    let id = store.insert_preview(
        mk_protein(mk_dummy_id(), 1),
        "p".to_owned(),
    );
    let _ = store.take_updates();
    store
        .promote_preview(
            id,
            CheckpointKind::PromotedPreview { entity: id },
            None,
            "promote",
        )
        .expect("promote_preview");
    let changes = store.take_updates();
    assert!(matches!(changes.as_slice(), [SessionUpdate::HeadMoved]), "got {changes:?}");
}

#[test]
fn begin_action_emits_nothing() {
    let (mut store, id) = store_with_protein(2);
    store.begin_action([id], wiggle(), "wiggle", 1).expect("begin_action");
    assert!(store.take_updates().is_empty());
}

#[test]
fn action_update_emits_tentative_edit() {
    // SessionUpdate is signal-only: payload coords are gone - the
    // RenderProjector rebuilds from `Session::head_assembly`. The test
    // asserts the funnel shape (one tentative Edit) and that the
    // post-mutation coords are reachable through the document; the
    // payload itself is no longer on the `SessionUpdate` stream.
    let (mut store, id) = store_with_protein(2);
    let rid = 1u64;
    store.begin_action([id], wiggle(), "wiggle", rid).expect("begin_action");
    let _ = store.take_updates();

    store
        .action_update(rid, None, None, None, |e| {
            for pos in &mut e.columns_mut().position {
                *pos = glam::Vec3::new(9.0, 9.0, 9.0);
            }
        })
        .expect("action_update");

    let changes = store.take_updates();
    assert!(
        matches!(changes.as_slice(), [SessionUpdate::Edit { tentative: true }]),
        "expected one tentative Edit, got {changes:?}",
    );
    let head = store.head_assembly();
    let entity = head
        .entity(id)
        .expect("action's locked entity is in the head assembly");
    assert!(entity
        .positions()
        .iter()
        .all(|c| *c == glam::Vec3::new(9.0, 9.0, 9.0)));
}

#[test]
fn commit_action_emits_head_moved() {
    let (mut store, id) = store_with_protein(2);
    let rid = 1u64;
    store.begin_action([id], wiggle(), "wiggle", rid).expect("begin_action");
    store
        .action_update(rid, None, None, None, |e| {
            for pos in &mut e.columns_mut().position {
                *pos = glam::Vec3::new(9.0, 9.0, 9.0);
            }
        })
        .expect("action_update");
    // Drain begin (none) + action_update (tentative Edit) so the next
    // take only sees the commit.
    let _ = store.take_updates();

    store.commit_action(rid).expect("commit_action");
    let changes = store.take_updates();
    assert!(matches!(changes.as_slice(), [SessionUpdate::HeadMoved]), "got {changes:?}");
}

#[test]
fn abort_action_emits_head_moved() {
    let (mut store, id) = store_with_protein(2);
    let rid = 1u64;
    store.begin_action([id], wiggle(), "wiggle", rid).expect("begin_action");
    let _ = store.take_updates();
    store.abort_action(rid).expect("abort_action");
    let changes = store.take_updates();
    assert!(matches!(changes.as_slice(), [SessionUpdate::HeadMoved]), "got {changes:?}");
}

#[test]
fn undo_then_redo_each_emit_head_moved() {
    let (mut store, id) = store_with_protein(2);
    let rid = 1u64;
    store.begin_action([id], wiggle(), "wiggle", rid).expect("begin_action");
    store
        .action_update(rid, None, None, None, |_| {})
        .expect("action_update");
    store.commit_action(rid).expect("commit_action");
    let _ = store.take_updates();

    store.undo().expect("undo");
    assert!(
        matches!(store.take_updates().as_slice(), [SessionUpdate::HeadMoved]),
        "undo emits HeadMoved",
    );
    store.redo(None).expect("redo");
    assert!(
        matches!(store.take_updates().as_slice(), [SessionUpdate::HeadMoved]),
        "redo emits HeadMoved",
    );
}

#[test]
fn undo_at_root_emits_nothing() {
    let mut store = Session::new();
    assert_eq!(store.undo().expect("undo"), None);
    assert!(store.take_updates().is_empty());
}

#[test]
fn redo_at_leaf_emits_nothing() {
    let (mut store, _id) = store_with_protein(2);
    assert_eq!(store.redo(None).expect("redo"), None);
    assert!(store.take_updates().is_empty());
}

#[test]
fn lane_undo_emits_head_moved() {
    let (mut store, id) = store_with_protein(2);
    let original = store.history().lane(id).expect("lane").head();
    let rid = 1u64;
    store.begin_action([id], wiggle(), "wiggle", rid).expect("begin_action");
    store
        .action_update(rid, None, None, None, |_| {})
        .expect("action_update");
    store.commit_action(rid).expect("commit_action");
    let _ = store.take_updates();

    store.lane_undo(id, original).expect("lane_undo");
    assert!(
        matches!(store.take_updates().as_slice(), [SessionUpdate::HeadMoved]),
        "lane_undo emits HeadMoved",
    );
}

#[test]
fn jump_checkpoint_emits_head_moved() {
    let (mut store, _id) = store_with_protein(2);
    let first = store.history().checkpoints().head();

    let id_b = store.insert_preview(
        mk_protein(mk_dummy_id(), 3),
        "b".to_owned(),
    );
    store
        .promote_preview(
            id_b,
            CheckpointKind::PromotedPreview { entity: id_b },
            None,
            "promote b",
        )
        .expect("promote b");
    let _ = store.take_updates();

    store.jump_checkpoint(first).expect("jump_checkpoint");
    assert!(
        matches!(store.take_updates().as_slice(), [SessionUpdate::HeadMoved]),
        "jump_checkpoint emits HeadMoved",
    );
}

#[test]
fn set_head_scores_emits_scores_changed() {
    // A score value write emits `ScoresChanged` (the GUI score widget
    // consumes it) and also bumps the history's `live_version` (the history
    // panel's live cursor). Plugins never see this signal.
    let (mut store, _id) = store_with_protein(2);
    let before = store.history().live_version();
    store.set_head_scores(Some(1.0), Some(2.0), None);
    let changes = store.take_updates();
    assert!(
        matches!(changes.as_slice(), [SessionUpdate::ScoresChanged]),
        "set_head_scores emits exactly ScoresChanged, got {changes:?}",
    );
    assert_ne!(
        before,
        store.history().live_version(),
        "set_head_scores bumps live_version so the history panel picks it up",
    );
}

#[test]
fn set_head_scores_noop_emits_nothing() {
    // `(None, None)` writes nothing, so no signal is emitted.
    let (mut store, _id) = store_with_protein(2);
    store.set_head_scores(None, None, None);
    assert!(
        store.take_updates().is_empty(),
        "a no-op score write emits no SessionUpdate",
    );
}

#[test]
fn set_edit_scores_emits_scores_changed() {
    // Stamping the open edit's composition score rides the `SessionUpdate` stream; an
    // unknown request id writes nothing and emits nothing.
    let (mut store, id) = store_with_protein(2);
    let _ = store.take_updates();
    let rid = 1u64;
    store.begin_action([id], wiggle(), "wiggle", rid).expect("begin_action");
    let _ = store.take_updates();

    store.set_edit_scores(rid, Some(3.0), Some(4.0), None);
    assert!(
        matches!(store.take_updates().as_slice(), [SessionUpdate::ScoresChanged]),
        "set_edit_scores on an open edit emits ScoresChanged",
    );

    store.set_edit_scores(999, Some(3.0), Some(4.0), None);
    assert!(
        store.take_updates().is_empty(),
        "set_edit_scores on an unknown request id emits nothing",
    );
}

#[test]
fn reset_clears_pending_then_emits_one_head_moved() {
    let (mut store, _id) = store_with_protein(2);
    // Leave an undrained change in the queue. Use insert_preview (it
    // emits `PreviewAdded`); reset must drop that pending change before
    // emitting its own HeadMoved.
    let _ = store.insert_preview(
        mk_protein(mk_dummy_id(), 1),
        "leftover".to_owned(),
    );

    store.reset();

    let changes = store.take_updates();
    assert!(
        matches!(changes.as_slice(), [SessionUpdate::HeadMoved]),
        "reset drops pending changes and emits exactly one HeadMoved, got {changes:?}",
    );
}

// ── Ambient selection ─────────────────────────────────────────────────

/// Mint `n` distinct entity ids in sequence. The selection map is keyed
/// by `EntityId` and does not validate keys against membership, so these
/// stand in for live entities without loading any.
fn mint_ids(n: usize) -> Vec<EntityId> {
    let mut alloc = EntityIdAllocator::new();
    (0..n).map(|_| alloc.allocate()).collect()
}

/// Remove a single residue via the live selection surface: re-set the
/// entity to its current set minus `residue`. Empty result drops the
/// entry (`set_residues_on` invariant), so absent residues are a no-op.
fn deselect(store: &mut Session, entity: EntityId, residue: u32) {
    let keep: Vec<u32> = store
        .selection()
        .get(&entity)
        .into_iter()
        .flatten()
        .copied()
        .filter(|&r| r != residue)
        .collect();
    store.set_residues_on(entity, keep);
}

#[test]
fn new_session_has_empty_selection() {
    let store = Session::new();
    let ids = mint_ids(1);
    assert!(store.selection().is_empty());
    assert_eq!(store.selection_total_count(), 0);
    assert!(store.selection().get(&ids[0]).is_none());
}

#[test]
fn select_residue_is_idempotent() {
    let mut store = Session::new();
    let e = mint_ids(1)[0];
    store.select_residue(e, 7);
    store.select_residue(e, 7);
    store.select_residue(e, 7);
    assert_eq!(store.selection_total_count(), 1);
    assert!(store.selection().get(&e).is_some_and(|s| s.contains(&7)));
    let set = store.selection().get(&e).expect("present");
    assert_eq!(set.len(), 1);
}

#[test]
fn clear_selection_empties_the_map() {
    let mut store = Session::new();
    let ids = mint_ids(2);
    store.select_residue(ids[0], 0);
    store.select_residue(ids[1], 5);
    assert_eq!(store.selection_total_count(), 2);
    store.clear_selection();
    assert!(store.selection().is_empty());
    assert!(store.selection().get(&ids[0]).is_none());
    assert!(store.selection().get(&ids[1]).is_none());
    assert!(!store.selection().get(&ids[0]).is_some_and(|s| s.contains(&0)));
}

#[test]
fn set_residues_on_replaces_not_merges() {
    let mut store = Session::new();
    let e = mint_ids(1)[0];
    store.select_residue(e, 1);
    store.select_residue(e, 2);
    store.select_residue(e, 3);
    store.set_residues_on(e, [10, 11]);
    let set = store.selection().get(&e).expect("present");
    assert_eq!(set.len(), 2);
    assert!(set.contains(&10));
    assert!(set.contains(&11));
    assert!(!set.contains(&1));
    assert!(!set.contains(&2));
    assert!(!set.contains(&3));
}

#[test]
fn set_residues_on_empty_removes_entity_entry() {
    let mut store = Session::new();
    let e = mint_ids(1)[0];
    store.select_residue(e, 9);
    store.set_residues_on(e, std::iter::empty());
    assert!(store.selection().get(&e).is_none());
    assert!(store.selection().is_empty());
}

#[test]
fn multi_entity_isolation() {
    let mut store = Session::new();
    let ids = mint_ids(2);
    let a = ids[0];
    let b = ids[1];
    store.select_residue(a, 1);
    store.select_residue(a, 2);
    store.select_residue(b, 100);

    assert!(store.selection().get(&a).is_some_and(|s| s.contains(&1)));
    assert!(store.selection().get(&a).is_some_and(|s| s.contains(&2)));
    assert!(!store.selection().get(&a).is_some_and(|s| s.contains(&100)));
    assert!(store.selection().get(&b).is_some_and(|s| s.contains(&100)));
    assert!(!store.selection().get(&b).is_some_and(|s| s.contains(&1)));

    store.clear_selection();
    store.select_residue(a, 1);
    store.set_residues_on(b, [42, 43]);
    // Mutating B must not have touched A.
    assert_eq!(store.selection().get(&a).expect("present").len(), 1);
}

#[test]
fn deselect_last_residue_removes_entity_entry() {
    let mut store = Session::new();
    let e = mint_ids(1)[0];
    store.select_residue(e, 0);
    store.select_residue(e, 1);
    deselect(&mut store, e, 0);
    // Set is still non-empty: entry must remain.
    assert!(store.selection().get(&e).is_some());
    deselect(&mut store, e, 1);
    // Last residue gone: entity entry must be removed.
    assert!(store.selection().get(&e).is_none());
    assert!(store.selection().is_empty());
}

#[test]
fn deselect_idempotent_on_missing() {
    let mut store = Session::new();
    let e = mint_ids(1)[0];
    // Deselect a residue that was never selected: no panic, no phantom
    // entity entry left behind.
    deselect(&mut store, e, 99);
    assert!(store.selection().is_empty());
    store.select_residue(e, 1);
    deselect(&mut store, e, 99);
    assert!(store.selection().get(&e).is_some_and(|s| s.contains(&1)));
    assert_eq!(store.selection_total_count(), 1);
}

#[test]
fn toggle_residue_round_trips() {
    let mut store = Session::new();
    let e = mint_ids(1)[0];
    // First toggle selects.
    assert!(store.toggle_residue(e, 3));
    assert!(store.selection().get(&e).is_some_and(|s| s.contains(&3)));
    // Second toggle deselects and removes the empty entity entry.
    assert!(!store.toggle_residue(e, 3));
    assert!(!store.selection().get(&e).is_some_and(|s| s.contains(&3)));
    assert!(store.selection().get(&e).is_none());
    // Toggle on a sibling residue while none are selected: same entity,
    // but the entry was removed in step 2, so this is a fresh insert.
    assert!(store.toggle_residue(e, 4));
    assert!(store.selection().get(&e).is_some_and(|s| s.contains(&4)));
    assert!(!store.selection().get(&e).is_some_and(|s| s.contains(&3)));
}

#[test]
fn selected_entities_enumerates_only_nonempty() {
    let mut store = Session::new();
    let ids = mint_ids(3);
    store.select_residue(ids[0], 0);
    store.select_residue(ids[1], 0);
    store.select_residue(ids[2], 0);
    deselect(&mut store, ids[1], 0);
    let ents: Vec<EntityId> = store.selection().keys().copied().collect();
    // BTreeMap key order is by `EntityId`'s `Ord`, which for the molex
    // newtype is the underlying u32 order. The allocator hands out ids in
    // sequence so ids[0] < ids[1] < ids[2]; after removing ids[1], the
    // selection keys enumerate ids[0], ids[2] in that order.
    assert_eq!(ents, vec![ids[0], ids[2]]);
}

#[test]
fn selection_mutation_emits_one_selection_changed() {
    // Each selection mutator funnels through `apply`, emitting exactly one
    // `SelectionChanged` (the App tick turns this into the viso highlight
    // push + SELECTION/ACTIONS dirty). The signal is unconditional, even
    // for an idempotent re-select, mirroring the prior inline dirty-raise.
    let mut store = Session::new();
    let e = mint_ids(1)[0];

    store.select_residue(e, 1);
    assert!(
        matches!(store.take_updates().as_slice(), [SessionUpdate::SelectionChanged]),
        "select_residue emits exactly SelectionChanged",
    );

    store.select_residue(e, 1);
    assert!(
        matches!(store.take_updates().as_slice(), [SessionUpdate::SelectionChanged]),
        "an idempotent re-select still emits SelectionChanged",
    );

    store.clear_selection();
    assert!(
        matches!(store.take_updates().as_slice(), [SessionUpdate::SelectionChanged]),
        "clear_selection emits exactly SelectionChanged",
    );
}

#[test]
fn set_entity_appearance_field_inserts_and_emits_one_change() {
    // A valid appearance-field merge on a fresh session inserts an entry and
    // emits exactly one `EntityAppearanceChanged` (the App tick pushes it
    // into the engine working copy via the render projector). A second field
    // on the same id merges into the same entry rather than replacing it.
    let mut store = Session::new();
    let e = mint_ids(1)[0];

    store.set_entity_appearance_field(e, "show_sidechains", &serde_json::json!(true));
    assert!(
        matches!(
            store.take_updates().as_slice(),
            [SessionUpdate::EntityAppearanceChanged]
        ),
        "a valid appearance merge emits exactly EntityAppearanceChanged",
    );
    assert!(
        store.appearance().contains_key(&e),
        "the merge inserted an override entry for the entity",
    );

    store.set_entity_appearance_field(e, "color_scheme", &serde_json::json!("score"));
    assert!(
        matches!(
            store.take_updates().as_slice(),
            [SessionUpdate::EntityAppearanceChanged]
        ),
        "a second field on the same id emits one more EntityAppearanceChanged",
    );
    assert_eq!(
        store.appearance().len(),
        1,
        "the second field merges into the same entry, not a new one",
    );
}

#[test]
fn reset_clears_selection() {
    // Selection is ambient, not history-versioned, but a topology swap
    // (`reset`) must drop it: the incoming assembly can reuse the outgoing
    // entity ids without referring to the same entities.
    let mut store = Session::new();
    let e = mint_ids(1)[0];
    store.select_residue(e, 1);
    store.select_residue(e, 2);
    assert!(!store.selection().is_empty());

    store.reset();
    assert!(
        store.selection().is_empty(),
        "reset drops the stale selection on a topology swap",
    );
}

#[test]
fn focus_mutation_emits_one_focus_changed_and_guards_idempotent() {
    // A focus change funnels through `apply`, emitting exactly one
    // `FocusChanged` (the App tick turns this into viso's camera mirror
    // push + SCENE/UI/ACTIONS dirty). Unlike selection, the emit is
    // guarded: an idempotent re-focus to the current value is silent.
    let mut store = Session::new();
    let e = mint_ids(1)[0];

    store.set_focus(viso::Focus::Entity(e));
    assert!(
        matches!(store.take_updates().as_slice(), [SessionUpdate::FocusChanged]),
        "set_focus to a new value emits exactly FocusChanged",
    );

    store.set_focus(viso::Focus::Entity(e));
    assert!(
        store.take_updates().is_empty(),
        "an idempotent re-focus emits nothing (change-guard)",
    );

    store.set_focus(viso::Focus::All);
    assert!(
        matches!(store.take_updates().as_slice(), [SessionUpdate::FocusChanged]),
        "set_focus back to All emits FocusChanged",
    );
}

#[test]
fn reset_clears_focus_to_all() {
    // Focus is ambient, not history-versioned, but a topology swap
    // (`reset`) returns it to the all-entities view. The reset sets it
    // silently (no `FocusChanged`): viso resets its own mirror on the
    // assembly replace, and `reset` already emits `HeadMoved`.
    let mut store = Session::new();
    let e = mint_ids(1)[0];
    store.set_focus(viso::Focus::Entity(e));
    assert_eq!(store.focus(), viso::Focus::Entity(e));

    store.reset();
    assert_eq!(
        store.focus(),
        viso::Focus::All,
        "reset returns focus to the all-entities view",
    );
}

/// A bare tutorial bubble (all optional flow fields empty). Only the
/// vector length matters to the cursor mutators under test.
fn mk_bubble() -> crate::puzzle_toml::Bubble {
    crate::puzzle_toml::Bubble {
        text: String::new(),
        color: None,
        point_to: None,
        point_to_index: None,
        image: None,
        button: None,
        alt_button: None,
        alt_skip: None,
        alt_next: None,
        no_repeat: false,
        link_name: None,
        link_url: None,
        trigger: None,
    }
}

/// A puzzle add-on carrying `bubble_count` tutorial bubbles (cursor at 0
/// when non-empty, both `None` when empty). Only the sequence length and
/// the cursor matter to the mutators under test.
fn mk_puzzle(bubble_count: usize) -> Puzzle {
    let bubbles = if bubble_count == 0 {
        None
    } else {
        Some((0..bubble_count).map(|_| mk_bubble()).collect())
    };
    let current_bubble = bubbles.as_ref().map(|_| 0);
    Puzzle {
        id: 1,
        start_energy: 0.0,
        completion_energy: 100.0,
        weight_patch: None,
        filters: Vec::new(),
        bubbles,
        current_bubble,
        constraints: Vec::new(),
        ligands: Vec::new(),
        design_gating: None,
    }
}

#[test]
fn bubble_cursor_advance_emits_one_bubble_changed() {
    // The tutorial-bubble cursor lives inside the loaded puzzle. Stepping
    // it funnels through `apply`, emitting exactly one `BubbleChanged` (the
    // App tick turns this into TEXT_BUBBLE dirty). With no puzzle loaded the
    // step is a silent no-op.
    let mut store = Session::new();

    // No puzzle: advancing is a silent no-op.
    store.advance_bubble(false);
    assert!(
        store.take_updates().is_empty(),
        "advancing with no puzzle loaded emits nothing",
    );

    // Install a 2-bubble puzzle (emits PuzzleChanged, drained here).
    store.set_puzzle(mk_puzzle(2));
    let _ = store.take_updates();
    assert_eq!(store.puzzle().and_then(|p| p.current_bubble), Some(0));

    store.advance_bubble(false);
    assert!(
        matches!(store.take_updates().as_slice(), [SessionUpdate::BubbleChanged]),
        "advance forward emits exactly BubbleChanged",
    );
    assert_eq!(store.puzzle().and_then(|p| p.current_bubble), Some(1));
}

#[test]
fn advance_bubble_clamps_at_both_ends_silently() {
    // Forward saturates one past the last bubble; back saturates at 0. A
    // step that hits either clamp does not move the cursor, so it emits
    // nothing.
    let mut store = Session::new();
    store.set_puzzle(mk_puzzle(2));
    let _ = store.take_updates();

    // Back at the start: already 0, clamp, silent.
    store.advance_bubble(true);
    assert_eq!(store.puzzle().and_then(|p| p.current_bubble), Some(0));
    assert!(
        store.take_updates().is_empty(),
        "stepping back at the start is silent",
    );

    // Walk forward to one-past-the-end (len = 2): 0 -> 1 -> 2.
    store.advance_bubble(false);
    store.advance_bubble(false);
    let _ = store.take_updates();
    assert_eq!(store.puzzle().and_then(|p| p.current_bubble), Some(2));

    // Forward at the end: clamp at len, silent.
    store.advance_bubble(false);
    assert_eq!(store.puzzle().and_then(|p| p.current_bubble), Some(2));
    assert!(
        store.take_updates().is_empty(),
        "stepping forward at the end is silent",
    );
}

#[test]
fn puzzle_mutation_emits_one_puzzle_changed_and_guards_idempotent() {
    // The puzzle add-on is ambient session state. `set_puzzle` always emits
    // (a load is a change); `clear_puzzle` emits only when there was a
    // puzzle to clear.
    let mut store = Session::new();
    assert!(store.puzzle().is_none());

    store.set_puzzle(Puzzle {
        id: 7,
        start_energy: 0.0,
        completion_energy: 100.0,
        weight_patch: None,
        filters: Vec::new(),
        bubbles: None,
        current_bubble: None,
        constraints: Vec::new(),
        ligands: Vec::new(),
        design_gating: None,
    });
    assert!(
        matches!(store.take_updates().as_slice(), [SessionUpdate::PuzzleChanged]),
        "set_puzzle emits exactly PuzzleChanged",
    );
    assert_eq!(store.puzzle().map(|p| p.id), Some(7));

    store.clear_puzzle();
    assert!(
        matches!(store.take_updates().as_slice(), [SessionUpdate::PuzzleChanged]),
        "clear_puzzle drops the puzzle add-on and emits PuzzleChanged",
    );
    assert!(store.puzzle().is_none());

    // Idempotent: clearing when already cleared is silent.
    store.clear_puzzle();
    assert!(
        store.take_updates().is_empty(),
        "clear_puzzle on an already-cleared puzzle emits nothing",
    );
}

#[test]
fn start_sets_title_and_installs_puzzle() {
    // The create seam funnels title + `Option<Puzzle>` setup. A free-form
    // start over an empty session sets the title and leaves no puzzle (the
    // inner `clear_puzzle` is a silent no-op); a puzzle start sets the title
    // to the puzzle name and emits exactly PuzzleChanged.
    let mut store = Session::new();

    store.start("apo".to_owned(), None);
    assert_eq!(store.title(), "apo");
    assert!(store.puzzle().is_none());
    assert!(
        store.take_updates().is_empty(),
        "free-form start over an empty session emits nothing",
    );

    store.start("Intro".to_owned(), Some(mk_puzzle(0)));
    assert_eq!(store.title(), "Intro");
    assert!(store.puzzle().is_some());
    assert!(
        matches!(store.take_updates().as_slice(), [SessionUpdate::PuzzleChanged]),
        "puzzle start emits exactly PuzzleChanged",
    );
}

#[test]
fn reset_clears_puzzle_and_leaves_title() {
    // A topology swap (`reset`) drops the ambient puzzle add-on (filters +
    // bubble flow) tied to the outgoing structure. The clear is silent (the
    // load path that follows re-installs via `start`); `reset` already emits
    // `HeadMoved`. `title` is left untouched for the following `start` to
    // overwrite.
    let mut store = Session::new();
    store.start("P".to_owned(), Some(mk_puzzle(1)));
    store.advance_bubble(false);
    let _ = store.take_updates();

    store.reset();

    assert!(store.puzzle().is_none(), "reset drops the puzzle add-on");
    assert_eq!(store.title(), "P", "reset leaves the title untouched");
}

#[test]
fn bglb_design_gating_locks_catalytic_residues_and_ligand() {
    // The chain->EntityId resolution that `App::load_puzzle_from_data` runs,
    // exercised at the `Session` boundary (no App/host/runner needed): load
    // the real BglB entities into history, resolve each polymer entity's PDB
    // chain against the puzzle's per-chain masks, install the gating, then
    // query. A protein entity on chain "A" is masked; the LG1 ligand has no
    // chain byte, so it never matches and stays locked (secure-by-default).
    let bglb_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../assets/levels/bglb");
    let data = crate::puzzle_load::load_puzzle_data_from_dir(&bglb_dir)
        .expect("BglB puzzle should load");

    let mut store = Session::new();
    store.start(
        "BglB".to_owned(),
        Some(Puzzle {
            id: 0,
            start_energy: 0.0,
            completion_energy: 0.0,
            weight_patch: None,
            filters: Vec::new(),
            bubbles: None,
            current_bubble: None,
            constraints: Vec::new(),
            ligands: Vec::new(),
            design_gating: None,
        }),
    );

    // Mirror the load path: capture each entity's chain before history
    // consumes it, resolve against the per-chain masks onto its EntityId.
    let mut protein_entity: Option<EntityId> = None;
    let mut ligand_entity: Option<EntityId> = None;
    let mut gating: BTreeMap<EntityId, crate::puzzle_setup::DesignMask> =
        BTreeMap::new();
    for entity in data.entities {
        let is_ligand = entity
            .as_small_molecule()
            .is_some_and(|sm| &sm.residue_name == b"LG1");
        let chain_key = entity.pdb_chain_id().map(str::to_owned);
        if let Some(id) = store.load_entity_into_history(entity, "BglB") {
            if let Some(key) = chain_key {
                if let Some(mask) = data.design_masks.get(&key) {
                    gating.insert(id, mask.clone());
                    protein_entity = Some(id);
                }
            }
            if is_ligand {
                ligand_entity = Some(id);
            }
        }
    }
    store.set_puzzle_design_gating(Some(gating));

    let protein = protein_entity.expect("BglB protein chain A should load");
    let ligand = ligand_entity.expect("BglB LG1 ligand should load");

    // The session reports design gating is active.
    assert!(store.design_gating_active(), "BglB declares design gating");

    // Protein residue 100 is designable; the catalytic gap (164/295/353) is
    // locked.
    assert!(store.is_designable(protein, 100), "residue 100 is designable");
    assert!(!store.is_designable(protein, 164), "residue 164 is locked");
    assert!(!store.is_designable(protein, 295), "residue 295 is locked");
    assert!(!store.is_designable(protein, 353), "residue 353 is locked");

    // The ligand entity carries no mask (no chain match), so every residue
    // on it is locked - the secure-by-default answer.
    assert!(
        !store.is_designable(ligand, 1),
        "the ligand entity is never designable",
    );
    assert!(
        !store.is_designable(ligand, 100),
        "the ligand entity is never designable",
    );

    // ── selection_is_designable: the DG-B design gate predicate ──────────
    //
    // Empty selection is vacuously designable (the min-residues selection
    // spec gates the empty case separately).
    assert!(
        store.selection_is_designable(),
        "an empty selection is vacuously designable",
    );

    // Focus::All, all selected residues designable → true.
    store.set_focus(viso::Focus::All);
    store.select_residue(protein, 100);
    store.select_residue(protein, 101);
    assert!(
        store.selection_is_designable(),
        "a selection of only designable residues passes the gate",
    );

    // Focus::All, add a locked catalytic residue → false (gate disables
    // Shake Mutate). This is the BglB 164 case the design gate must catch.
    store.select_residue(protein, 164);
    assert!(
        !store.selection_is_designable(),
        "selecting locked catalytic residue 164 fails the design gate",
    );

    // Focus scoping: focus the ligand (no selection on it) → the protein's
    // out-of-scope locked residue is ignored, so the gate is vacuously
    // true for the focused entity's empty selection.
    store.set_focus(viso::Focus::Entity(ligand));
    assert!(
        store.selection_is_designable(),
        "focusing the ligand scopes out the protein's locked selection",
    );

    // Focus the protein: now its locked 164 is back in scope → false.
    store.set_focus(viso::Focus::Entity(protein));
    assert!(
        !store.selection_is_designable(),
        "focusing the protein brings locked residue 164 back into scope",
    );
}

#[test]
fn adopted_design_entity_registers_as_fully_designable() {
    // A created (rfd3) design entity adopted into a design-gated session must
    // become the designable target: every residue on it answers `true` to
    // `is_designable`. Exercises the Session mutator the adopt path calls
    // (`register_full_designable_entity`) without App/host/runner.
    const N: usize = 12;
    let n_u32 = u32::try_from(N).expect("N fits u32");

    let mut store = Session::new();
    store.start(
        "Design".to_owned(),
        Some(Puzzle {
            id: 0,
            start_energy: 0.0,
            completion_energy: 0.0,
            weight_patch: None,
            filters: Vec::new(),
            bubbles: None,
            current_bubble: None,
            constraints: Vec::new(),
            ligands: Vec::new(),
            // Gating active (a design puzzle) but the new entity is absent.
            design_gating: Some(BTreeMap::new()),
        }),
    );
    assert!(store.design_gating_active(), "the puzzle declares design gating");

    let design = store
        .load_entity_into_history(mk_protein(mk_dummy_id(), N), "rfd3")
        .expect("the design entity should commit");

    // Before registration: absent from the gating map → secure-by-default
    // locks the whole entity (the bug).
    for r in 0..n_u32 {
        assert!(
            !store.is_designable(design, r),
            "residue {r} is locked before registration",
        );
    }

    store.register_full_designable_entity(design, N);

    // After registration: every residue is designable.
    for r in 0..n_u32 {
        assert!(
            store.is_designable(design, r),
            "residue {r} is designable after registration",
        );
    }

    // The selection gate (Shake Mutate enablement) passes over the design.
    store.set_focus(viso::Focus::All);
    store.select_residue(design, 0);
    store.select_residue(design, n_u32 - 1);
    assert!(
        store.selection_is_designable(),
        "a selection on the fully-designable design passes the gate",
    );
}

#[test]
fn register_full_designable_entity_is_noop_when_gating_inactive() {
    // A non-design context (no gating) must stay ungated: registration is a
    // no-op, so designability remains governed elsewhere (secure-by-default
    // `false` here).
    const N: usize = 8;

    let mut store = Session::new();
    store.start(
        "Free".to_owned(),
        Some(Puzzle {
            id: 0,
            start_energy: 0.0,
            completion_energy: 0.0,
            weight_patch: None,
            filters: Vec::new(),
            bubbles: None,
            current_bubble: None,
            constraints: Vec::new(),
            ligands: Vec::new(),
            design_gating: None,
        }),
    );
    assert!(!store.design_gating_active(), "no gating on this puzzle");

    let design = store
        .load_entity_into_history(mk_protein(mk_dummy_id(), N), "rfd3")
        .expect("the entity should commit");

    store.register_full_designable_entity(design, N);

    assert!(!store.design_gating_active(), "registration fabricated no gating");
    assert!(
        !store.is_designable(design, 0),
        "an ungated session stays secure-by-default",
    );
}
