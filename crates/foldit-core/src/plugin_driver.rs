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
use molex::Assembly;

use crate::session::{Session, SessionUpdate};

/// Owns the orchestrator handle, the plugin broadcaster, and the
/// native-only stream bookkeeping. `App` holds one of these and reaches
/// into its public fields by direct path so the orchestrator, broadcaster,
/// and stream state can be borrowed disjointly (the dispatch methods on
/// `App` rely on this).
pub struct PluginDriver {
    pub orchestrator: Option<foldit_runner::Orchestrator>,
    /// Plugin projection of the `SessionUpdate` spine: diffs its own
    /// last-published `Assembly` against the `Session` to fan Full/Delta
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

    /// Construct and install a fresh orchestrator handle. Called on
    /// structure load (and again on the load-error path) to replace any
    /// prior handle before plugin discovery runs.
    pub(crate) fn init_orchestrator(&mut self) {
        self.orchestrator = Some(foldit_runner::Orchestrator::new());
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
// I/O + `StreamHost` table maintenance) and never touch `Session` or
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

    /// Read accessor for an in-flight stream entry. Used by the Error
    /// arm of `apply_backend_updates` to inspect `handle.entities`
    /// before the terminal cleanup releases the handle.
    pub(crate) fn stream_entry(&self, rid: u64) -> Option<&ActiveStreamEntry> {
        self.stream_host.active_streams.get(&rid)
    }

    /// Terminal stream cleanup (Cancelled / Final / Error): remove the
    /// entry from the active-streams table, release its dispatch locks
    /// on the orchestrator, and clear `pull_drag` if it pointed at this
    /// stream. Returns the entry's `plugin_id` so callers can log
    /// without re-querying.
    pub(crate) fn release_terminal_stream(&mut self, rid: u64) -> Option<String> {
        let entry = self.stream_host.active_streams.remove(&rid)?;
        let ActiveStreamEntry {
            handle, plugin_id, ..
        } = entry;
        if let Some(orch) = self.orchestrator.as_mut() {
            orch.release_dispatch_locks(handle);
        }
        if matches!(&self.stream_host.pull_drag, Some(d) if d.request_id == rid) {
            self.stream_host.pull_drag = None;
        }
        Some(plugin_id)
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
    /// `App` still owns the catalog lookup that produces `plugin_id`
    /// and the post-processing (`begin_action`, `apply_invoke_result`,
    /// broadcaster pump, score poll).
    pub(crate) fn dispatch_op(
        &mut self,
        op_id: &str,
        kind: foldit_runner::orchestrator::OpKind,
        ctx: foldit_runner::orchestrator::DispatchContext,
        params: std::collections::HashMap<
            String,
            foldit_runner::orchestrator::ParamValue,
        >,
        plugin_id: String,
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
                        core_token: None,
                    },
                );
                Ok(OpOutcome::Stream { request_id: rid })
            }
        }
    }

    // ── Score paths ─────────────────────────────────────────────────────
    //
    // Forward the well-known `score` query to the orchestrator, building
    // the default dispatch context internally so the score query covers
    // the whole assembly. App owns merging the returned reports into the
    // head checkpoint and pushing per-residue colors.

    /// Blocking score round-trip: fan the `score` query across every
    /// provider and return one report per provider that replied. Used
    /// until the first score lands, where a synchronous result keeps the
    /// load gate deterministic. Empty map when no orchestrator is wired up.
    pub(crate) fn collect_scores_blocking(
        &mut self,
    ) -> std::collections::HashMap<String, foldit_runner::proto::plugin::ScoreReport>
    {
        use foldit_runner::orchestrator::DispatchContext;
        self.orchestrator
            .as_mut()
            .map(|orch| orch.collect_scores(&DispatchContext::default()))
            .unwrap_or_default()
    }

    /// Fire a non-blocking `score` query at every provider with none
    /// already in flight. Replies land on stored receivers drained by
    /// [`Self::poll_score_results`]. No-op when no orchestrator exists.
    pub(crate) fn request_scores(&mut self) {
        use foldit_runner::orchestrator::DispatchContext;
        if let Some(orch) = self.orchestrator.as_mut() {
            orch.request_scores(&DispatchContext::default());
        }
    }

    /// Drain whatever async `score` replies have arrived. Non-blocking;
    /// empty map when nothing is ready or no orchestrator exists.
    pub(crate) fn poll_score_results(
        &mut self,
    ) -> std::collections::HashMap<String, foldit_runner::proto::plugin::ScoreReport>
    {
        self.orchestrator
            .as_mut()
            .map(|orch| orch.poll_score_results())
            .unwrap_or_default()
    }

    // ── Bootstrap discover + register ───────────────────────────────────

    /// Discover plugins under `root` and register each against the given
    /// initial assembly, bringing up its worker session. Returns the
    /// `(plugin_id, post_init_bytes)` pair for every plugin that
    /// registered successfully; the post-Init bytes carry each plugin's
    /// normalized assembly for the caller to apply. Empty `Vec` when no
    /// orchestrator is wired up or discovery fails — both degrade the app
    /// to viewer-only rather than erroring.
    pub(crate) fn discover_and_register(
        &mut self,
        root: &std::path::Path,
        initial_assembly: Vec<u8>,
    ) -> Vec<(String, Vec<u8>)> {
        let Some(orch) = self.orchestrator.as_mut() else {
            return Vec::new();
        };
        let discovered = match orch.discover_plugins(root) {
            Ok(ids) => ids,
            Err(e) => {
                log::warn!(
                    "[PluginDriver] discover_plugins({}) failed: {e}; plugins disabled",
                    root.display()
                );
                return Vec::new();
            }
        };
        log::info!("[PluginDriver] discovered plugins: {discovered:?}");
        let mut registered = Vec::with_capacity(discovered.len());
        for plugin_id in &discovered {
            match orch.ensure_plugin_registered(plugin_id, initial_assembly.clone())
            {
                Ok(bytes) => {
                    log::info!("[PluginDriver] {plugin_id} plugin ready");
                    registered.push((plugin_id.clone(), bytes));
                }
                Err(e) => {
                    log::warn!(
                        "[PluginDriver] ensure_plugin_registered('{plugin_id}') \
                         failed: {e}; {plugin_id} plugin disabled"
                    );
                }
            }
        }
        registered
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
    /// stored in `StreamHost::active_streams` under `request_id` (the
    /// runner rid), so the caller has nothing left to do for the
    /// dispatch itself beyond recording the core token on the entry. The
    /// matching terminal arm in `apply_backend_updates` performs the
    /// cleanup.
    Stream { request_id: u64 },
}

// ── Plugin broadcaster ──────────────────────────────────────────────────

/// Plugin projection of the [`SessionUpdate`] spine.
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
/// plugins never see live frames. (Score updates are off-spine
/// entirely; canonical writes happen via `Session::set_head_scores`
/// and never reach the broadcaster.)
pub(crate) struct PluginBroadcaster {
    /// The `Assembly` last serialized and broadcast. `None` before the
    /// first broadcast (and after construction), which forces a Full.
    /// Deliberately **not** cleared on `Session::reset`: the post-reset
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
        orch.broadcast_to_plugins(&payload);
        self.last_published = Some(new);
    }

    /// Whether a change is a non-tentative observable mutation.
    /// Tentative edits (live per-cycle frames) are filtered out: plugins
    /// never see live frames. (Score updates aren't on the spine at all,
    /// so the match has no arm for them.)
    fn is_observable(change: &SessionUpdate) -> bool {
        matches!(
            change,
            SessionUpdate::HeadMoved
                | SessionUpdate::PreviewAdded
                | SessionUpdate::PreviewDiscarded
                | SessionUpdate::Edit { tentative: false }
        )
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
            // Delta serialize rejected the edits — fall through to Full.
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
/// (a residue insert/delete) — at which point the caller broadcasts a full
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
#[cfg(not(target_arch = "wasm32"))]
pub(crate) struct ActiveStreamEntry {
    pub(crate) handle: foldit_runner::orchestrator::DispatchHandle,
    pub(crate) plugin_id: String,
    /// Core-side `request_id` from `Session::begin_action`, set after the
    /// dispatch's history side-effect runs. `None` when no action was
    /// begun for this stream (e.g. a multi-entity dispatch with no focus).
    /// The terminal arm looks this up to commit / abort the right edit,
    /// replacing the old entity-coincidence join.
    pub(crate) core_token: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{Session, EntityOrigin};
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
        assert!(PluginBroadcaster::is_observable(&SessionUpdate::PreviewAdded));
        assert!(PluginBroadcaster::is_observable(&SessionUpdate::PreviewDiscarded));
        assert!(PluginBroadcaster::is_observable(&SessionUpdate::Edit {
            tentative: false,
        }));
        assert!(!PluginBroadcaster::is_observable(&SessionUpdate::Edit {
            tentative: true,
        }));
    }

    // ── broadcast: gating + snapshot bookkeeping end-to-end ──

    #[test]
    fn broadcast_ignores_tentative_only_batch() {
        let changes = vec![SessionUpdate::Edit { tentative: true }];
        let doc = Session::new();
        let mut orch = foldit_runner::Orchestrator::new();
        let mut bc = PluginBroadcaster::new();
        bc.broadcast(&changes, &doc, &mut orch);
        assert_eq!(orch.broadcast_gen(), 0, "tentative-only batch broadcasts nothing");
        assert!(bc.last_published.is_none());
    }

    #[test]
    fn broadcast_ignores_empty_batch() {
        let doc = Session::new();
        let mut orch = foldit_runner::Orchestrator::new();
        let mut bc = PluginBroadcaster::new();
        bc.broadcast(&[], &doc, &mut orch);
        assert_eq!(orch.broadcast_gen(), 0);
    }

    #[test]
    fn broadcast_observable_batch_advances_gen_and_adopts_snapshot() {
        let mut doc = Session::new();
        let _ = doc.insert_preview(
            mk_bulk(mk_bulk_dummy_id(), glam::Vec3::ZERO),
            "p".to_string(),
            EntityOrigin::Loaded,
        );
        let changes = doc.take_updates(); // [PreviewAdded]
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
