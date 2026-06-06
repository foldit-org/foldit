#[cfg(test)]
mod selection_tests {
    use crate::app::App;
    use molex::entity::molecule::id::{EntityId, EntityIdAllocator};
    use std::io;
    use std::path::Path;
    #[cfg(not(target_arch = "wasm32"))]
    use crate::runner_client::EditScope;
    #[cfg(not(target_arch = "wasm32"))]
    use crate::session::Session;

    /// Minimal [`HostResources`] stub. `App` construction needs one;
    /// these tests never touch the filesystem.
    struct TestHost;

    impl crate::HostResources for TestHost {
        fn read_file(&self, _path: &str) -> io::Result<Vec<u8>> {
            Err(io::Error::new(io::ErrorKind::NotFound, "test stub"))
        }
        fn view_presets_dir(&self) -> Option<&Path> {
            None
        }
        fn initial_structure_path(&self) -> Option<String> {
            None
        }
    }

    fn fresh_app() -> App {
        App::new(Box::new(TestHost))
    }

    /// Mint a sequence of distinct entity ids in a test-local order.
    /// `EntityId` is opaque, so we allocate via `EntityIdAllocator` and
    /// hand back the n-th id from a freshly-seeded allocator. The map
    /// keys we care about are just "different ids on the same App",
    /// not specific raw values.
    fn mint_ids(n: usize) -> Vec<EntityId> {
        let mut alloc = EntityIdAllocator::new();
        (0..n).map(|_| alloc.allocate()).collect()
    }

    #[test]
    fn handle_set_selection_clears_on_empty_input() {
        let mut app = fresh_app();
        let ids = mint_ids(1);
        app.store.select_residue(ids[0], 7);
        assert!(!app.store.selection_is_empty());
        // Empty entries: clear (`clear_selection` always runs first; no
        // entry loop body) — independent of whether the empty store
        // could even resolve a raw id.
        app.handle_set_selection(Vec::new());
        assert!(app.store.selection_is_empty());
    }

    #[test]
    fn handle_set_selection_drops_unknown_entity_ids() {
        let mut app = fresh_app();
        let ids = mint_ids(1);
        // Seed a non-empty selection so we can prove the clear ran.
        app.store.select_residue(ids[0], 9);
        // The test stub has no loaded structure, so `self.store.ids()`
        // is empty and every raw id is unresolvable. The mutator clears
        // the existing selection and drops the unknown entries.
        app.handle_set_selection(vec![
            foldit_gui::EntitySelection {
                entity_id: 0,
                residues: vec![1, 2, 3],
            },
            foldit_gui::EntitySelection {
                entity_id: 999,
                residues: vec![5],
            },
        ]);
        assert!(app.store.selection_is_empty());
    }

    /// One committed Bulk entity, promoted into history so the store has a
    /// non-root committed head.
    fn mk_bulk() -> molex::MoleculeEntity {
        use molex::entity::molecule::atom::Atom;
        use molex::entity::molecule::bulk::BulkEntity;
        use molex::{Element, MoleculeType};
        let id = EntityIdAllocator::new().allocate();
        let atom = Atom {
            position: glam::Vec3::ZERO,
            occupancy: 1.0,
            b_factor: 0.0,
            element: Element::O,
            name: *b"O   ",
            formal_charge: 0,
        };
        molex::MoleculeEntity::Bulk(BulkEntity::new(id, MoleculeType::Water, vec![atom], *b"HOH", 1))
    }

    /// A composition score for an open edit must land on that edit and be
    /// minted onto its committed checkpoint only at commit; the committed
    /// parent is never overwritten mid-action. This is the write the
    /// composition-score poll performs (`set_edit_scores`), targeted by the
    /// edit's `request_id` rather than "the first open edit".
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn composition_score_routes_to_pending_edit_not_committed_parent() {
        use crate::history::CheckpointKind;
        use crate::session::EntityOrigin;

        let mut app = fresh_app();
        // Commit one entity so the head is a real checkpoint, and stamp a
        // known score on it (the committed parent).
        let id = app
            .store
            .insert_preview(mk_bulk(), "e".to_string(), EntityOrigin::Loaded);
        app.store
            .promote_preview(
                id,
                CheckpointKind::PromotedPreview { entity: id },
                None,
                None,
                "e",
            )
            .expect("promote");
        app.store.set_head_scores(Some(10.0), Some(100.0), None);
        let parent = app.store.history().checkpoints().head();
        assert_eq!(
            app.store.history().checkpoint(parent).unwrap().raw_score,
            Some(10.0)
        );

        // Open an action on that entity.
        let rid = 1u64;
        app.store
            .begin_action(
                [id],
                CheckpointKind::PluginOp {
                    plugin_id: "rosetta".to_string(),
                    op_id: "wiggle".to_string(),
                    display: "wiggle".to_string(),
                },
                "w",
                rid,
            )
            .expect("begin_action");

        // Drive the composition-score write the poll path performs: stamp
        // the open edit by its request_id.
        let game = ((-42.0_f64 + 800.0) * 10.0).max(0.0);
        app.store.set_edit_scores(rid, Some(42.0), Some(game), None);

        // Mid-action: the committed parent is untouched; the composition
        // node carries the streamed score.
        assert_eq!(
            app.store.history().checkpoint(parent).unwrap().raw_score,
            Some(10.0),
            "committed parent score must not change mid-action"
        );
        assert_eq!(app.store.current_composition_scores().0, Some(42.0));

        // After commit: the minted checkpoint carries the streamed score;
        // the parent still holds its own.
        let committed = app.store.commit_action(rid).expect("commit");
        assert_eq!(
            app.store.history().checkpoint(committed).unwrap().raw_score,
            Some(42.0)
        );
        assert_eq!(
            app.store.history().checkpoint(committed).unwrap().game_score,
            Some(game)
        );
        assert_eq!(
            app.store.history().checkpoint(parent).unwrap().raw_score,
            Some(10.0)
        );
    }

    /// Post-Init normalization must reach *every* matching entity, not
    /// just the first. Guards the multi-lane path `apply_post_init` opens:
    /// one begin over the whole touched set, `apply_streaming_assembly`
    /// fanning across both lanes, and a single commit. Before the fix the
    /// begin ran on `first_protein_entity` only, so every entity past the
    /// first kept its pre-Init coordinates.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn post_init_normalizes_every_matching_entity_not_just_the_first() {
        use crate::history::CheckpointKind;
        use crate::session::EntityOrigin;
        use std::sync::Arc;

        let mut store = Session::new();
        // Two committed entities.
        let e1 = store.insert_preview(mk_bulk(), "a".to_string(), EntityOrigin::Loaded);
        store
            .promote_preview(e1, CheckpointKind::PromotedPreview { entity: e1 }, None, None, "a")
            .expect("promote a");
        let e2 = store.insert_preview(mk_bulk(), "b".to_string(), EntityOrigin::Loaded);
        store
            .promote_preview(e2, CheckpointKind::PromotedPreview { entity: e2 }, None, None, "b")
            .expect("promote b");
        let ckpts_before = store.history().checkpoints().len();

        // A "normalized" assembly that displaces BOTH entities' atoms,
        // keeping their store ids so `apply_streaming_assembly` id-matches.
        let moved = glam::Vec3::new(7.0, 7.0, 7.0);
        let mut a1 = store.entity(e1).expect("e1").clone();
        for atom in a1.atom_set_mut() {
            atom.position = moved;
        }
        let mut a2 = store.entity(e2).expect("e2").clone();
        for atom in a2.atom_set_mut() {
            atom.position = moved;
        }
        let normalized = molex::Assembly::from_arcs(vec![Arc::new(a1), Arc::new(a2)]);

        // The multi-lane apply path `apply_post_init` runs (sans the
        // orchestrator-driven request_id allocation, which a unit test
        // can't stand up): collect every assembly entity with a committed
        // lane, open ONE edit over the whole set, fan the stream across it,
        // commit once.
        let target_entities: Vec<EntityId> = normalized
            .entities()
            .iter()
            .map(|e| e.id())
            .filter(|id| store.history().lane(*id).is_some())
            .collect();
        assert_eq!(
            target_entities.len(),
            2,
            "both entities must resolve to a committed lane"
        );
        let rid = 99u64;
        store
            .begin_action(
                target_entities,
                CheckpointKind::PluginOp {
                    plugin_id: "rosetta".to_string(),
                    op_id: "_init_normalize".to_string(),
                    display: "Init".to_string(),
                },
                "Init",
                rid,
            )
            .expect("begin multi-lane edit");
        assert!(
            store.apply_streaming_assembly(&normalized, None, rid),
            "apply_streaming_assembly must update at least one lane"
        );
        store.commit_action(rid).expect("commit");

        // Exactly one new checkpoint, and BOTH entities carry the moved
        // coordinates — not just the first.
        assert_eq!(store.history().checkpoints().len(), ckpts_before + 1);
        let head = store.head_assembly();
        for e in [e1, e2] {
            let ent = head.entity(e).expect("entity present in head assembly");
            assert!(
                ent.positions().iter().all(|p| *p == moved),
                "entity {} was not normalized",
                e.raw()
            );
        }
    }

    /// A whole-pose dispatch must open its edit over EVERY committed entity,
    /// not the host's single-entity fallback guess. `EditScope::AllEntities`
    /// resolves to all committed lanes (transient previews filtered out), and
    /// a multi-entity streamed frame then updates every lane on commit.
    /// Before the fix the runner's resolved target never reached core, so the
    /// edit opened on one entity and every other entity kept its pre-op
    /// coordinates (which also blew up the committed score).
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn global_scope_opens_edit_over_all_committed_entities() {
        use crate::history::CheckpointKind;
        use crate::session::EntityOrigin;
        use std::sync::Arc;

        let mut app = fresh_app();
        // Two committed entities.
        let e1 = app
            .store
            .insert_preview(mk_bulk(), "a".to_string(), EntityOrigin::Loaded);
        app.store
            .promote_preview(e1, CheckpointKind::PromotedPreview { entity: e1 }, None, None, "a")
            .expect("promote a");
        let e2 = app
            .store
            .insert_preview(mk_bulk(), "b".to_string(), EntityOrigin::Loaded);
        app.store
            .promote_preview(e2, CheckpointKind::PromotedPreview { entity: e2 }, None, None, "b")
            .expect("promote b");
        // A preview that is never promoted: it has no committed lane and so
        // must be filtered out of a whole-pose edit's lane set.
        let e_transient = app
            .store
            .insert_preview(mk_bulk(), "c".to_string(), EntityOrigin::Loaded);

        // AllEntities resolves to exactly the two committed lanes.
        let mut lanes = app.lanes_for_scope(&EditScope::AllEntities);
        lanes.sort_unstable();
        let mut expected = vec![e1, e2];
        expected.sort_unstable();
        assert_eq!(lanes, expected, "global scope spans committed lanes only");
        assert!(!lanes.contains(&e_transient), "transient preview has no lane");

        // Open ONE edit over the whole set, fan a multi-entity frame across
        // it, commit once. Every lane must carry the moved coordinates.
        let moved = glam::Vec3::new(3.0, 3.0, 3.0);
        let mut a1 = app.store.entity(e1).expect("e1").clone();
        for atom in a1.atom_set_mut() {
            atom.position = moved;
        }
        let mut a2 = app.store.entity(e2).expect("e2").clone();
        for atom in a2.atom_set_mut() {
            atom.position = moved;
        }
        let frame = molex::Assembly::from_arcs(vec![Arc::new(a1), Arc::new(a2)]);

        let rid = 7u64;
        app.store
            .begin_action(
                lanes,
                CheckpointKind::PluginOp {
                    plugin_id: "rosetta".to_string(),
                    op_id: "wiggle".to_string(),
                    display: "Wiggle".to_string(),
                },
                "Wiggle",
                rid,
            )
            .expect("begin multi-lane edit");
        assert!(
            app.store.apply_streaming_assembly(&frame, None, rid),
            "frame applies across the locked lanes"
        );
        app.store.commit_action(rid).expect("commit");

        let head = app.store.head_assembly();
        for e in [e1, e2] {
            let ent = head.entity(e).expect("entity in head assembly");
            assert!(
                ent.positions().iter().all(|p| *p == moved),
                "entity {} was not updated by the whole-pose edit",
                e.raw()
            );
        }
    }

    /// An entity-scoped dispatch resolves to its named set, filtered to
    /// committed lanes: a resolved id without a lane drops out rather than
    /// refusing the whole multi-lane edit (`begin_action` is all-or-nothing).
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn entity_scope_filters_to_committed_lanes() {
        use crate::history::CheckpointKind;
        use crate::session::EntityOrigin;

        let mut app = fresh_app();
        let e1 = app
            .store
            .insert_preview(mk_bulk(), "a".to_string(), EntityOrigin::Loaded);
        app.store
            .promote_preview(e1, CheckpointKind::PromotedPreview { entity: e1 }, None, None, "a")
            .expect("promote a");
        let e_transient = app
            .store
            .insert_preview(mk_bulk(), "t".to_string(), EntityOrigin::Loaded);

        // The resolved set names a committed entity and a transient one;
        // only the committed lane survives the filter.
        let scope = EditScope::Entities(vec![e1, e_transient]);
        assert_eq!(app.lanes_for_scope(&scope), vec![e1]);
    }
}
