//! Plugin projection of the `SessionUpdate` stream.
//!
//! `RunnerProjector` holds its own last-published `Assembly` snapshot and
//! diffs it against the authoritative `Session` to fan Full/Delta
//! `UpdateAssembly` broadcasts out to peer plugins. It is the third
//! consumer of the `SessionUpdate` batch, alongside the render and GUI
//! projectors, and is cross-platform: the broadcast decision is the same
//! on native and wasm.

use molex::ops::edit::AssemblyEdit;
use molex::Assembly;

use crate::history::CheckpointKind;
use crate::session::{Session, SessionUpdate, SessionUpdateConsumer};

/// Plugin projection of the [`SessionUpdate`] stream.
///
/// Holds its own last-published `Assembly` snapshot and diffs it against
/// the authoritative [`Session`] to fan a Full/Delta `UpdateAssembly`
/// broadcast out to peer plugins (whose Assembly mirrors must stay in
/// sync with the host). Cross-platform: the broadcast decision is the
/// same on native and wasm.
///
/// Per drain it coalesces the batch into one snapshot-diff broadcast
/// (vs the old one-broadcast-per-mutation queue), so the orchestrator's
/// generation advances once per drain. It ignores tentative `Edit`s:
/// plugins never see live frames. (Score updates are off-stream
/// entirely; canonical writes happen via `Session::set_head_scores`
/// and never reach the projector.)
pub struct RunnerProjector {
    /// The `Assembly` last serialized and broadcast. `None` before the
    /// first broadcast (and after construction), which forces a Full.
    /// Deliberately **not** cleared on `Session::reset`: the post-reset
    /// empty-assembly diff still produces a Full that advances the
    /// orchestrator's gen counter, so plugins never see `from_gen` go
    /// backwards.
    last_published: Option<Assembly>,
}

impl RunnerProjector {
    pub(crate) const fn new() -> Self {
        Self {
            last_published: None,
        }
    }

    /// Whether a change is a non-tentative observable mutation.
    /// Tentative edits (live per-cycle frames) are filtered out: plugins
    /// never see live frames. Signal-only updates that aren't scene
    /// mutations (`ScoresChanged`, `SelectionChanged`, `FocusChanged`,
    /// `BubbleChanged`, `PuzzleChanged`) have no arm here, so they're
    /// non-observable: plugins compute their own scores and never see the
    /// residue selection, session focus, tutorial bubbles, or puzzle
    /// objective.
    const fn is_observable(change: &SessionUpdate) -> bool {
        matches!(
            change,
            SessionUpdate::HeadMoved
                | SessionUpdate::PreviewAdded
                | SessionUpdate::PreviewDiscarded
                | SessionUpdate::Edit { tentative: false }
        )
    }
}

/// Project a drained batch of scene changes into at most one plugin
/// broadcast. No-ops unless the batch carries a non-tentative observable
/// change; otherwise diffs the held snapshot against
/// `doc.head_assembly()` to produce a Full or coord-only Delta, fans it
/// out through the orchestrator sink, and adopts the new snapshot.
impl SessionUpdateConsumer<foldit_runner::Orchestrator> for RunnerProjector {
    fn consume(
        &mut self,
        changes: &[SessionUpdate],
        doc: &Session,
        orch: &mut foldit_runner::Orchestrator,
    ) {
        if !changes.iter().any(Self::is_observable) {
            // Tentative-only / empty batch: nothing the plugins should
            // see.
            return;
        }
        let new = doc.head_assembly();
        let Some(payload) = encode_payload(self.last_published.as_ref(), &new) else {
            // Serialize failure (currently impossible for in-memory
            // assemblies). Skip this drain and keep the prior snapshot so
            // the next drain diffs from the last payload plugins actually
            // received; STALE_GEN recovery covers the gap meanwhile.
            return;
        };
        // Exclude the plugin that sourced this edit from the fan-out: it
        // already holds the post-op assembly, so re-broadcasting it would
        // land back as a destructive self-delta. A host-internal edit
        // carries no plugin source, so it broadcasts to everyone ("").
        let source_plugin_id = head_plugin_source(doc);
        orch.broadcast_to_plugins(&payload, source_plugin_id);
        self.last_published = Some(new);
    }
}

/// The plugin id that sourced the session's current head edit, or `""`
/// for a host-internal edit with no plugin source. Reads the committed
/// graph head; a `PluginOp` checkpoint carries the originating plugin's
/// id, every other checkpoint kind is host-internal.
fn head_plugin_source(doc: &Session) -> &str {
    let history = doc.history();
    let head_id = history.checkpoints().head();
    match history.checkpoint(head_id) {
        Some(ckpt) => match &ckpt.kind {
            CheckpointKind::PluginOp { plugin_id, .. } => plugin_id,
            _ => "",
        },
        None => "",
    }
}

/// Encode the broadcast for one drain: a coord-only `Delta` when `prior`
/// and `new` share topology, otherwise a `Full`. Returns `None` only when
/// the Full serialize fails (currently impossible for in-memory
/// assemblies), at which point the caller skips the broadcast.
///
/// Mirrors the old `Session::queue_assembly_update_broadcast` fallback
/// chain: a delta-serialize failure (a topology edit slipping past the
/// coord-only check) also falls through to a Full.
fn encode_payload(
    prior: Option<&Assembly>,
    new: &Assembly,
) -> Option<foldit_runner::orchestrator::BroadcastPayload> {
    use foldit_runner::orchestrator::BroadcastPayload;
    if let Some(prior) = prior {
        if let Some(edits) = assembly_diff(prior, new) {
            if let Ok(bytes) = molex::ops::wire::delta::serialize_edits(&edits) {
                return Some(BroadcastPayload::Delta(bytes));
            }
            // Delta serialize rejected the edits - fall through to Full.
        }
    }
    molex::ops::wire::serialize_assembly(new)
        .ok()
        .map(BroadcastPayload::Full)
}

/// Whole-assembly diff, via [`molex::MoleculeEntity::diff`]. Returns
/// `Some(edits)` (possibly empty when nothing moved) when `prior` and `new`
/// carry the same entity-id set in the same order, so every entity is
/// pairwise-diffable; returns `None` when an entity was added, removed, or
/// reordered, or when any per-entity change isn't representable as an edit
/// (a residue insert/delete) - at which point the caller broadcasts a full
/// snapshot. Coord moves ride the delta as `SetEntityCoords`, mutations as
/// `MutateResidue`.
fn assembly_diff(prior: &Assembly, new: &Assembly) -> Option<Vec<AssemblyEdit>> {
    let prior_entities = prior.entities();
    let new_entities = new.entities();
    if prior_entities.len() != new_entities.len() {
        return None;
    }
    let mut edits = Vec::new();
    for (p, n) in prior_entities.iter().zip(new_entities.iter()) {
        if p.id() != n.id() {
            return None;
        }
        edits.extend(p.diff(n).ok()?);
    }
    Some(edits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{EntityOrigin, Session};
    use foldit_runner::orchestrator::BroadcastPayload;
    use molex::entity::molecule::atom::Atom;
    use molex::entity::molecule::bulk::BulkEntity;
    use molex::entity::molecule::id::{EntityId, EntityIdAllocator};
    use molex::{Element, MoleculeEntity, MoleculeType};

    fn mk_bulk(id: EntityId, pos: glam::Vec3) -> MoleculeEntity {
        let atom = Atom {
            position: pos,
            occupancy: 1.0,
            b_factor: 0.0,
            element: Element::O,
            name: *b"O   ",
            formal_charge: 0,
        };
        MoleculeEntity::Bulk(BulkEntity::new(id, MoleculeType::Water, vec![atom], *b"HOH", 1))
    }

    fn two_ids() -> (EntityId, EntityId) {
        let mut alloc = EntityIdAllocator::new();
        (alloc.allocate(), alloc.allocate())
    }

    // ── encode_payload: the Full vs Delta decision ──

    #[test]
    fn encode_payload_no_prior_is_full() {
        let (a, _) = two_ids();
        let new = Assembly::new(vec![mk_bulk(a, glam::Vec3::ZERO)]);
        assert!(matches!(
            encode_payload(None, &new),
            Some(BroadcastPayload::Full(_))
        ));
    }

    #[test]
    fn encode_payload_coord_only_is_delta() {
        let (a, _) = two_ids();
        let prior = Assembly::new(vec![mk_bulk(a, glam::Vec3::ZERO)]);
        let new = Assembly::new(vec![mk_bulk(a, glam::Vec3::new(1.0, 2.0, 3.0))]);
        let payload = encode_payload(Some(&prior), &new);
        let Some(BroadcastPayload::Delta(bytes)) = payload else {
            panic!("expected Delta, got {payload:?}");
        };
        let edits = molex::ops::wire::delta::deserialize_edits(&bytes).expect("delta decodes");
        assert!(matches!(
            edits.as_slice(),
            [AssemblyEdit::SetEntityCoords { .. }]
        ));
    }

    #[test]
    fn encode_payload_topology_change_is_full() {
        let (a, b) = two_ids();
        let prior = Assembly::new(vec![mk_bulk(a, glam::Vec3::ZERO)]);
        // A second entity → topology change → coord delta refuses → Full.
        let new = Assembly::new(vec![
            mk_bulk(a, glam::Vec3::ZERO),
            mk_bulk(b, glam::Vec3::ZERO),
        ]);
        assert!(matches!(
            encode_payload(Some(&prior), &new),
            Some(BroadcastPayload::Full(_))
        ));
    }

    #[test]
    fn encode_payload_coord_delta_round_trips_through_apply_edits() {
        let (a, _) = two_ids();
        let prior = Assembly::new(vec![mk_bulk(a, glam::Vec3::ZERO)]);
        let new = Assembly::new(vec![mk_bulk(a, glam::Vec3::new(4.0, 5.0, 6.0))]);
        let Some(BroadcastPayload::Delta(bytes)) = encode_payload(Some(&prior), &new) else {
            panic!("expected Delta");
        };
        let edits = molex::ops::wire::delta::deserialize_edits(&bytes).expect("decode");
        let mut replay = prior;
        replay.apply_edits(&edits).expect("apply_edits");
        assert_eq!(
            replay.entities()[0].positions(),
            new.entities()[0].positions(),
        );
    }

    // ── is_observable: tentative edits are filtered ──

    #[test]
    fn is_observable_filters_tentative() {
        assert!(RunnerProjector::is_observable(&SessionUpdate::PreviewAdded));
        assert!(RunnerProjector::is_observable(&SessionUpdate::PreviewDiscarded));
        assert!(RunnerProjector::is_observable(&SessionUpdate::Edit {
            tentative: false,
        }));
        assert!(!RunnerProjector::is_observable(&SessionUpdate::Edit {
            tentative: true,
        }));
    }

    // ── broadcast: gating + snapshot bookkeeping end-to-end ──

    #[test]
    fn broadcast_ignores_tentative_only_batch() {
        let changes = vec![SessionUpdate::Edit { tentative: true }];
        let doc = Session::new();
        let mut orch = foldit_runner::Orchestrator::new();
        let mut bc = RunnerProjector::new();
        bc.consume(&changes, &doc, &mut orch);
        assert_eq!(orch.broadcast_gen(), 0, "tentative-only batch broadcasts nothing");
        assert!(bc.last_published.is_none());
    }

    #[test]
    fn broadcast_ignores_empty_batch() {
        let doc = Session::new();
        let mut orch = foldit_runner::Orchestrator::new();
        let mut bc = RunnerProjector::new();
        bc.consume(&[], &doc, &mut orch);
        assert_eq!(orch.broadcast_gen(), 0);
    }

    #[test]
    fn broadcast_observable_batch_advances_gen_and_adopts_snapshot() {
        let mut doc = Session::new();
        let _ = doc.insert_preview(
            mk_bulk(mk_bulk_dummy_id(), glam::Vec3::ZERO),
            "p".to_owned(),
            EntityOrigin::Loaded,
        );
        let changes = doc.take_updates(); // [PreviewAdded]
        let mut orch = foldit_runner::Orchestrator::new();
        let mut bc = RunnerProjector::new();
        bc.consume(&changes, &doc, &mut orch);
        assert_eq!(orch.broadcast_gen(), 1, "observable batch broadcasts once");
        assert!(bc.last_published.is_some(), "projector adopts the new snapshot");
    }

    /// `insert_preview` overwrites the entity's id, so any valid id works.
    fn mk_bulk_dummy_id() -> EntityId {
        EntityIdAllocator::new().allocate()
    }
}
