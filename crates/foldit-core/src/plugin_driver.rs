//! Plugin driver: owns the orchestrator handle, the plugin broadcaster,
//! and the native stream bookkeeping that drives plugin operations.
//!
//! `PluginDriver` holds the `Orchestrator`, the [`PluginBroadcaster`]
//! (the spine's plugin projection), and (on native builds) the in-flight
//! `StreamHost` state, plus the orchestrator-lifecycle handlers that
//! touch only the orchestrator (`reset_for_new_structure`, `shutdown`).
//! The two big dispatch methods (`handle_dispatch_op` and
//! `apply_backend_updates`) interleave orchestrator I/O with store
//! mutations, so they stay on `App` until they are decomposed in RX8;
//! `App` reaches into `self.plugin_driver` for the orchestrator and
//! stream state they need.

use molex::ops::edit::AssemblyEdit;
use molex::{Assembly, MoleculeEntity};

use crate::document::{Document, SceneChange};

/// Owns the orchestrator handle, the plugin broadcaster, and the
/// native-only stream bookkeeping. `App` holds one of these and reaches
/// into its public fields by direct path so the orchestrator, broadcaster,
/// and stream state can be borrowed disjointly (the dispatch methods on
/// `App` rely on this).
pub struct PluginDriver {
    pub orchestrator: Option<foldit_runner::Orchestrator>,
    /// Plugin projection of the `SceneChange` spine: diffs its own
    /// last-published `Assembly` against the `Document` to fan Full/Delta
    /// broadcasts out. Disjoint from `orchestrator` so `App` can borrow
    /// both in `pump_scene_changes`.
    pub(crate) broadcaster: PluginBroadcaster,
    #[cfg(not(target_arch = "wasm32"))]
    pub stream_host: StreamHost,
}

impl PluginDriver {
    pub fn new() -> Self {
        Self {
            orchestrator: None,
            broadcaster: PluginBroadcaster::new(),
            #[cfg(not(target_arch = "wasm32"))]
            stream_host: StreamHost {
                active_streams: std::collections::HashMap::new(),
                pull_drag: None,
            },
        }
    }

    /// Release any lock state when puzzle topology changes.
    pub fn reset_for_new_structure(&mut self) {
        if let Some(ref mut orch) = self.orchestrator {
            for eid in orch.locked_entities() {
                orch.unlock(eid);
            }
        }
    }

    /// Shut down the orchestrator (and, through it, plugin workers).
    pub fn shutdown(&self) {
        if let Some(ref orch) = self.orchestrator {
            orch.shutdown();
        }
    }
}

// ── Native-only dispatch mechanism ──────────────────────────────────────
//
// These methods own the plugin-side bookkeeping of dispatch (orchestrator
// I/O + `StreamHost` table maintenance) and never touch `Document` or
// `VisoEngine` — the coordination boundary keeps those on `App`.

#[cfg(not(target_arch = "wasm32"))]
impl PluginDriver {
    /// Drain whatever the orchestrator has queued for the host. Returns an
    /// empty `Vec` when no orchestrator is wired up.
    pub(crate) fn drain_updates(
        &mut self,
    ) -> Vec<foldit_runner::orchestrator::PluginUpdate> {
        self.orchestrator
            .as_mut()
            .map(|orch| orch.drain_plugin_updates())
            .unwrap_or_default()
    }

    /// Read accessor for an in-flight stream entry. Used by the Pending
    /// arm of `apply_backend_updates` to look up the resolved
    /// `TransitionKind`, and by the Error arm to inspect
    /// `handle.entities` before the terminal cleanup releases the handle.
    pub(crate) fn stream_entry(&self, rid: u64) -> Option<&ActiveStreamEntry> {
        self.stream_host.active_streams.get(&rid)
    }

    /// Terminal stream cleanup (Cancelled / Final / Error): remove the
    /// entry from the active-streams table, release its dispatch locks
    /// on the orchestrator, and clear `pull_drag` if it pointed at this
    /// stream. Returns the entry's `(plugin_id, transition)` so callers
    /// can log or replay the manifest transition without re-querying.
    pub(crate) fn release_terminal_stream(
        &mut self,
        rid: u64,
    ) -> Option<(String, foldit_runner::orchestrator::TransitionKind)> {
        let entry = self.stream_host.active_streams.remove(&rid)?;
        let ActiveStreamEntry {
            handle,
            plugin_id,
            transition,
        } = entry;
        if let Some(orch) = self.orchestrator.as_mut() {
            orch.release_dispatch_locks(handle);
        }
        if matches!(&self.stream_host.pull_drag, Some(d) if d.request_id == rid) {
            self.stream_host.pull_drag = None;
        }
        Some((plugin_id, transition))
    }

    /// Send a cancel to every in-flight stream's plugin. Used by the
    /// ESC / `VisoCommand::ClearSelection` paths. Doesn't touch
    /// `active_streams`: the terminal cleanup runs when the plugin's
    /// `Cancelled` reply lands in the next drain.
    pub(crate) fn cancel_all_active_streams(&mut self) {
        let Some(orch) = self.orchestrator.as_mut() else {
            return;
        };
        for (rid, entry) in &self.stream_host.active_streams {
            if let Err(e) = orch.dispatch_cancel_stream(&entry.plugin_id, *rid) {
                log::warn!(
                    "dispatch_cancel_stream plugin={} rid={rid} failed: {e}",
                    entry.plugin_id,
                );
            }
        }
    }

    /// One-call dispatch: hand the orchestrator a pre-built session
    /// context + params, branch on `kind`, and for streams, insert the
    /// `ActiveStreamEntry` so the matching terminal arm can find it.
    /// `App` still owns the catalog lookup that produces `plugin_id` /
    /// `transition` and the post-processing (`begin_action`,
    /// `apply_invoke_result`, broadcaster pump, score poll).
    pub(crate) fn dispatch_op(
        &mut self,
        op_id: &str,
        kind: foldit_runner::orchestrator::OpKind,
        ctx: foldit_runner::orchestrator::SessionContext,
        params: std::collections::HashMap<
            String,
            foldit_runner::orchestrator::ParamValue,
        >,
        plugin_id: String,
        transition: foldit_runner::orchestrator::TransitionKind,
        entity_type_of: impl Fn(
            foldit_runner::orchestrator::EntityId,
        ) -> Option<foldit_runner::orchestrator::EntityType>,
    ) -> Result<OpOutcome, String> {
        use foldit_runner::orchestrator::OpKind;
        let Some(orch) = self.orchestrator.as_mut() else {
            return Err(String::from("orchestrator not initialized"));
        };
        match kind {
            OpKind::Invoke => orch
                .dispatch_invoke(op_id, ctx, params, entity_type_of)
                .map(OpOutcome::Invoke)
                .map_err(|e| e.to_string()),
            OpKind::Stream => {
                let (rid, handle) = orch
                    .dispatch_start_stream(op_id, ctx, params, entity_type_of)
                    .map_err(|e| e.to_string())?;
                let _ = self.stream_host.active_streams.insert(
                    rid,
                    ActiveStreamEntry {
                        handle,
                        plugin_id,
                        transition,
                    },
                );
                Ok(OpOutcome::Stream)
            }
        }
    }
}

/// Discriminated result of a dispatch — wraps the two return shapes
/// `dispatch_invoke` and `dispatch_start_stream` produce so
/// `App::handle_dispatch_op` can post-process either uniformly. Lives
/// here (rather than in `app.rs`) because [`PluginDriver::dispatch_op`]
/// is the producer and `App` is just one of two consumers.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) enum OpOutcome {
    /// Synchronous invoke completed; payload is the plugin's reply
    /// bytes, to feed into `apply_invoke_result`.
    Invoke(Vec<u8>),
    /// Stream dispatch succeeded; the `DispatchHandle` is already
    /// stored in `StreamHost::active_streams`, so the caller has
    /// nothing left to do for the dispatch itself. The matching
    /// terminal arm in `apply_backend_updates` performs the cleanup.
    Stream,
}

// ── Plugin broadcaster ──────────────────────────────────────────────────

/// Plugin projection of the [`SceneChange`] spine.
///
/// Holds its own last-published `Assembly` snapshot and diffs it against
/// the authoritative [`Document`] to fan a Full/Delta `UpdateAssembly`
/// broadcast out to peer plugins (whose Assembly mirrors must stay in
/// sync with the host). Cross-platform: the broadcast decision is the
/// same on native and wasm.
///
/// Per drain it coalesces the batch into one snapshot-diff broadcast
/// (vs the old one-broadcast-per-mutation queue), so the orchestrator's
/// generation advances once per drain. It ignores tentative `Edit`s:
/// plugins never see live frames. (Score updates are off-spine
/// entirely; canonical writes happen via `Document::set_head_scores`
/// and never reach the broadcaster.)
pub(crate) struct PluginBroadcaster {
    /// The `Assembly` last serialized and broadcast. `None` before the
    /// first broadcast (and after construction), which forces a Full.
    /// Deliberately **not** cleared on `Document::reset`: the post-reset
    /// empty-assembly diff still produces a Full that advances the
    /// orchestrator's gen counter, so plugins never see `from_gen` go
    /// backwards.
    last_published: Option<Assembly>,
}

impl PluginBroadcaster {
    fn new() -> Self {
        Self {
            last_published: None,
        }
    }

    /// Project a drained batch of scene changes into at most one plugin
    /// broadcast. No-ops unless the batch carries a non-tentative
    /// observable change; otherwise diffs the held snapshot against
    /// `doc.head_assembly()` to produce a Full or coord-only Delta,
    /// fans it out through `orch`, and adopts the new snapshot.
    pub(crate) fn broadcast(
        &mut self,
        changes: &[SceneChange],
        doc: &Document,
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
        orch.broadcast_to_plugins(&payload);
        self.last_published = Some(new);
    }

    /// Whether a change is a non-tentative observable mutation.
    /// Tentative edits (live per-cycle frames) are filtered out: plugins
    /// never see live frames. (Score updates aren't on the spine at all,
    /// so the match has no arm for them.)
    fn is_observable(change: &SceneChange) -> bool {
        matches!(
            change,
            SceneChange::HeadMoved { .. }
                | SceneChange::PreviewAdded { .. }
                | SceneChange::PreviewDiscarded { .. }
                | SceneChange::Edit {
                    tentative: false,
                    ..
                }
        )
    }
}

/// Encode the broadcast for one drain: a coord-only `Delta` when `prior`
/// and `new` share topology, otherwise a `Full`. Returns `None` only when
/// the Full serialize fails (currently impossible for in-memory
/// assemblies), at which point the caller skips the broadcast.
///
/// Mirrors the old `Document::queue_assembly_update_broadcast` fallback
/// chain: a delta-serialize failure (a topology edit slipping past the
/// coord-only check) also falls through to a Full.
fn encode_payload(
    prior: Option<&Assembly>,
    new: &Assembly,
) -> Option<foldit_runner::orchestrator::BroadcastPayload> {
    use foldit_runner::orchestrator::BroadcastPayload;
    if let Some(prior) = prior {
        if let Some(edits) = assembly_coord_delta(prior, new) {
            if let Ok(bytes) = molex::ops::wire::delta::serialize_edits(&edits) {
                return Some(BroadcastPayload::Delta(bytes));
            }
            // Delta serialize rejected the edits — fall through to Full.
        }
    }
    molex::ops::wire::serialize_assembly(new)
        .ok()
        .map(BroadcastPayload::Full)
}

/// Decide whether a single-entity mutation can be broadcast as a
/// coord-only `SetEntityCoords` delta. Returns `Some(edit)` when the
/// prior and new payloads share id, type, atom count, and full polymer
/// topology (residue layout + per-residue variants); returns `None` when
/// anything else changed — at which point the caller falls back to a
/// `Full` broadcast.
///
/// Atom-level fields beyond `position` (element, name, formal_charge,
/// occupancy, b_factor) are intentionally not re-checked: action
/// lifecycle mutations only touch positions, and a delta receiver that
/// applies `SetEntityCoords` preserves its own copy of those fields. A
/// receiver that needs non-position atom updates falls back through the
/// Full path.
fn coord_only_delta(prior: &MoleculeEntity, new: &MoleculeEntity) -> Option<AssemblyEdit> {
    if prior.id() != new.id() {
        return None;
    }
    if prior.molecule_type() != new.molecule_type() {
        return None;
    }
    if prior.atom_count() != new.atom_count() {
        return None;
    }
    match (prior.residues(), new.residues()) {
        (Some(p_res), Some(n_res)) => {
            if p_res.len() != n_res.len() {
                return None;
            }
            for (p, n) in p_res.iter().zip(n_res.iter()) {
                if p.name != n.name || p.atom_range != n.atom_range || p.variants != n.variants {
                    return None;
                }
            }
        }
        (None, None) => {}
        _ => return None,
    }
    Some(AssemblyEdit::SetEntityCoords {
        entity: new.id(),
        coords: new.positions(),
    })
}

/// Whole-assembly coord diff. Returns `Some(edits)` (possibly empty when
/// nothing moved) when every entity in `new` shares id + topology with
/// the entity at the same position in `prior`; returns `None` otherwise
/// (entity added, removed, reordered, or any per-entity topology
/// divergence). Same-topology head moves emit Delta; cross-topology jumps
/// fall back to Full.
fn assembly_coord_delta(prior: &Assembly, new: &Assembly) -> Option<Vec<AssemblyEdit>> {
    let prior_entities = prior.entities();
    let new_entities = new.entities();
    if prior_entities.len() != new_entities.len() {
        return None;
    }
    let mut edits = Vec::new();
    for (p, n) in prior_entities.iter().zip(new_entities.iter()) {
        let edit = coord_only_delta(p, n)?;
        if p.positions() == n.positions() {
            continue;
        }
        edits.push(edit);
    }
    Some(edits)
}

/// Owns the in-flight stream bookkeeping that only exists on native
/// builds: the plugin stream handle table plus the live pull-drag
/// state. Grouped so App's stream lifecycle touches one field.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) struct StreamHost {
    /// In-flight stream handles keyed by request_id. Populated by
    /// `handle_dispatch_op` on `StartStream`; the matching
    /// `release_dispatch_locks` runs in `apply_backend_updates` when
    /// the stream's terminal `PluginUpdate` arrives. The stored
    /// `plugin_id` is the dispatch target for `dispatch_cancel_stream`
    /// when the user hits ESC.
    pub(crate) active_streams: std::collections::HashMap<
        u64,
        ActiveStreamEntry,
    >,
    /// Live pull-drag state. `Some(...)` between pointer-down on an
    /// atom and pointer-up / stream-terminal / ESC cancel. The drag's
    /// stream id also lives in `active_streams` so Final/Error
    /// handling flows through the unified stream-cleanup path; this
    /// field carries the extra viso-side bookkeeping needed for
    /// pointer-move (PullInfo + op id).
    pub(crate) pull_drag: Option<crate::pull_drag::PullDrag>,
}

/// Bundle stored per running stream so `apply_backend_updates` /
/// `cancel_operations` can release locks and dispatch cancel against
/// the right plugin worker without re-querying the orchestrator.
/// `transition` is the viso animation preset to queue on each Pending
/// snapshot — resolved from the manifest catalog once at dispatch
/// time so per-poll handling stays orchestrator-free.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) struct ActiveStreamEntry {
    pub(crate) handle: foldit_runner::orchestrator::DispatchHandle,
    pub(crate) plugin_id: String,
    pub(crate) transition: foldit_runner::orchestrator::TransitionKind,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::{Document, EntityOrigin};
    use foldit_runner::orchestrator::BroadcastPayload;
    use molex::entity::molecule::atom::Atom;
    use molex::entity::molecule::bulk::BulkEntity;
    use molex::entity::molecule::id::{EntityId, EntityIdAllocator};
    use molex::{Element, MoleculeType};

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
        let mut replay = prior.clone();
        replay.apply_edits(&edits).expect("apply_edits");
        assert_eq!(
            replay.entities()[0].positions(),
            new.entities()[0].positions(),
        );
    }

    // ── is_observable: tentative edits are filtered ──

    #[test]
    fn is_observable_filters_tentative() {
        let (a, _) = two_ids();
        let coord_edit = AssemblyEdit::SetEntityCoords {
            entity: a,
            coords: Vec::new(),
        };
        assert!(PluginBroadcaster::is_observable(&SceneChange::PreviewAdded { entity: a }));
        assert!(PluginBroadcaster::is_observable(&SceneChange::PreviewDiscarded { entity: a }));
        assert!(PluginBroadcaster::is_observable(&SceneChange::Edit {
            edit: coord_edit.clone(),
            tentative: false,
        }));
        assert!(!PluginBroadcaster::is_observable(&SceneChange::Edit {
            edit: coord_edit,
            tentative: true,
        }));
    }

    // ── broadcast: gating + snapshot bookkeeping end-to-end ──

    #[test]
    fn broadcast_ignores_tentative_only_batch() {
        let (a, _) = two_ids();
        let changes = vec![SceneChange::Edit {
            edit: AssemblyEdit::SetEntityCoords {
                entity: a,
                coords: Vec::new(),
            },
            tentative: true,
        }];
        let doc = Document::new();
        let mut orch = foldit_runner::Orchestrator::new();
        let mut bc = PluginBroadcaster::new();
        bc.broadcast(&changes, &doc, &mut orch);
        assert_eq!(orch.broadcast_gen(), 0, "tentative-only batch broadcasts nothing");
        assert!(bc.last_published.is_none());
    }

    #[test]
    fn broadcast_ignores_empty_batch() {
        let doc = Document::new();
        let mut orch = foldit_runner::Orchestrator::new();
        let mut bc = PluginBroadcaster::new();
        bc.broadcast(&[], &doc, &mut orch);
        assert_eq!(orch.broadcast_gen(), 0);
    }

    #[test]
    fn broadcast_observable_batch_advances_gen_and_adopts_snapshot() {
        let mut doc = Document::new();
        let _ = doc.insert_preview(
            mk_bulk(mk_bulk_dummy_id(), glam::Vec3::ZERO),
            "p".to_string(),
            EntityOrigin::Loaded,
        );
        let changes = doc.take_scene_changes(); // [PreviewAdded]
        let mut orch = foldit_runner::Orchestrator::new();
        let mut bc = PluginBroadcaster::new();
        bc.broadcast(&changes, &doc, &mut orch);
        assert_eq!(orch.broadcast_gen(), 1, "observable batch broadcasts once");
        assert!(bc.last_published.is_some(), "broadcaster adopts the new snapshot");
    }

    /// `insert_preview` overwrites the entity's id, so any valid id works.
    fn mk_bulk_dummy_id() -> EntityId {
        EntityIdAllocator::new().allocate()
    }
}
