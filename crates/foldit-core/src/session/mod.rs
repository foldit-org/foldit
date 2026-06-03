//! Authoritative document atop the two-layer [`History`].
//!
//! `Session` owns:
//! - [`History`] — the full per-entity timelines + checkpoint graph.
//! - `transient: IndexMap<EntityId, Arc<MoleculeEntity>>` — preview /
//!   scene-resident entities that are visible in [`Self::head_assembly`]
//!   but absent from every checkpoint. Presence in this map *is* the
//!   preview signal; the old [`EntityMetadata::is_preview`] flag is
//!   gone (G6).
//! - `metadata: IndexMap<EntityId, Arc<EntityMetadata>>` — per-entity
//!   metadata (name, origin).
//!   `Arc`-shared so unchanged entries stay aliased across history
//!   operations (no metadata serialization on every mutation).
//!
//! Mutation intent is in the type signature (G6): three explicit
//! categories — history-bearing actions, metadata-only edits, and
//! one-shot transient previews — with no neutral default. Adding a new
//! mutation requires choosing one.
//!
//! There is no `mutate(closure)`-style API. Every checkpoint-bearing
//! event funnels through `History::record` via a thin shim
//! here; the single-root invariant from G3 is preserved end to end.
//!
//! **Emit invariant.** Every public mutator is a shim: it performs its
//! state change, then emits exactly one [`SessionUpdate`] (or none, where
//! the change is unobservable) through the [`Self::apply`] funnel. The
//! `Session` holds no projection logic — it neither serializes
//! assemblies nor knows about plugins or viso. `App` drains the emitted
//! changes via [`Self::take_updates`] and routes them to the
//! projectors (the `PluginBroadcaster` owns the Full/Delta plugin
//! fan-out; the render + GUI projectors follow). Because `pending_updates`
//! is private and `apply` is its sole pusher, "one emit per mutator" is a
//! structural invariant, not a runtime assertion.

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::Arc;

use indexmap::IndexMap;
use molex::entity::molecule::id::{EntityId, EntityIdAllocator};
use molex::{Assembly, MoleculeEntity, MoleculeType};

use crate::history::{
    CheckpointId, CheckpointKind, EntitySnapshotId, FilterStatus, History, HistoryError,
};

mod apply;
mod change;
pub use change::SessionUpdate;
mod metadata;
pub use metadata::{EntityMetadata, EntityOrigin};

// ── Errors ─────────────────────────────────────────────────────────────

/// Error returned by every fallible [`Session`] operation.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// `History`-layer refusal (state machine, action lock, missing id,
    /// etc.). See [`HistoryError`].
    #[error("{0}")]
    History(#[from] HistoryError),
    /// `id` is not currently a transient preview.
    #[error("{} is not a transient preview", id.raw())]
    NotAPreview { id: EntityId },
    /// `begin_action` was called with a [`CheckpointKind`] that doesn't
    /// name an entity (e.g., `Loaded`, `BondsChanged`). Action lifecycle
    /// kinds always do.
    #[error("action lifecycle requires an entity-targeted CheckpointKind")]
    ActionRequiresEntity,
}

// ── Session ───────────────────────────────────────────────────────────

/// Authoritative document over the whole scene.
pub struct Session {
    /// Per-entity metadata. `Arc`-shared so unchanged entries alias
    /// across history operations (no metadata serialization fan-out
    /// per mutation).
    metadata: IndexMap<EntityId, Arc<EntityMetadata>>,
    /// Preview / scene-resident entities. Visible in
    /// [`Self::head_assembly`] but absent from every checkpoint.
    /// `promote_preview` moves entries into history; `remove_preview`
    /// drops them.
    transient: IndexMap<EntityId, Arc<MoleculeEntity>>,
    /// Id allocator. Stable across history navigation.
    allocator: EntityIdAllocator,
    /// The full two-layer history.
    history: History,
    /// Drain queue of [`SessionUpdate`]s emitted by this store's mutators
    /// through [`Self::apply`]. `App` drains it once per tick via
    /// [`Self::take_updates`] and routes the batch to the
    /// projectors. Always empty in steady state.
    pending_updates: Vec<SessionUpdate>,
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

impl Session {
    /// Build an empty store. The internal [`History`] is seeded with no
    /// entities and an empty bonds set; call [`Self::reset`] when a
    /// puzzle loads.
    #[must_use]
    pub fn new() -> Self {
        Self {
            metadata: IndexMap::new(),
            transient: IndexMap::new(),
            allocator: EntityIdAllocator::new(),
            history: History::new(std::iter::empty(), PathBuf::new()),
            pending_updates: Vec::new(),
        }
    }

    // ── Read accessors ────────────────────────────────────────────────

    /// Build the current view of the assembly: the lane heads of every
    /// entity in the checkpoint head's `entity_heads` (in canonical
    /// order), followed by every transient preview (also in insertion
    /// order). Collects the entity `Arc`s and hands them to
    /// [`Assembly::from_arcs`], so a per-frame call is O(entities) of
    /// refcount bumps rather than the old O(atoms) deep clone per
    /// entity. The returned `Assembly` shares its `Arc<MoleculeEntity>`s
    /// with the history snapshots (and the transient map); that aliasing
    /// is safe because consumers only read the assembly, and history
    /// forks its own copy via `Arc::make_mut` before any in-place edit,
    /// so a published snapshot never observes a later mutation.
    #[must_use]
    pub fn head_assembly(&self) -> Assembly {
        let head_id = self.history.checkpoints().head();
        let mut entities: Vec<Arc<MoleculeEntity>> = Vec::new();
        if let Some(head) = self.history.checkpoint(head_id) {
            // Membership (which entities, in what order) comes from the
            // committed head; the snapshot read comes from each lane's
            // head, which is the open tentative when an action holds the
            // lane and the committed snapshot otherwise. This makes the
            // live view follow an in-flight action; an action never
            // changes membership.
            for eid in head.entity_heads.keys() {
                if let Some(lane) = self.history.lane(*eid) {
                    if let Some(snap) = lane.snapshot(lane.head()) {
                        entities.push(Arc::clone(&snap.payload));
                    }
                }
            }
        }
        for arc in self.transient.values() {
            entities.push(Arc::clone(arc));
        }
        Assembly::from_arcs(entities)
    }

    /// Read access to the history graph.
    #[must_use]
    pub fn history(&self) -> &History {
        &self.history
    }

    /// Look up an entity by id. Reads the lane head (the open tentative
    /// when an action holds the lane, else the committed snapshot) for any
    /// entity in the committed membership, then falls back to transient
    /// previews.
    #[must_use]
    pub fn entity(&self, id: EntityId) -> Option<&MoleculeEntity> {
        let head_id = self.history.checkpoints().head();
        if let Some(head) = self.history.checkpoint(head_id) {
            if head.entity_heads.contains_key(&id) {
                if let Some(lane) = self.history.lane(id) {
                    if let Some(snap) = lane.snapshot(lane.head()) {
                        return Some(snap.payload.as_ref());
                    }
                }
            }
        }
        self.transient.get(&id).map(|arc| arc.as_ref())
    }

    /// The structural kind of a live entity, or `None` if no entity with
    /// this id exists in the session head / previews.
    #[must_use]
    pub fn entity_type(&self, id: EntityId) -> Option<molex::EntityKind> {
        self.entity(id).map(MoleculeEntity::entity_kind)
    }

    /// Look up an entity's metadata.
    #[must_use]
    pub fn metadata(&self, id: EntityId) -> Option<&EntityMetadata> {
        self.metadata.get(&id).map(Arc::as_ref)
    }

    /// Live entity membership: the head checkpoint's committed entities
    /// (canonical `entity_heads` order), followed by the transient
    /// previews (insertion order). The two sets are disjoint
    /// (`promote_preview` moves an entity from `transient` into
    /// history), so concatenating committed-then-preview needs no dedup.
    /// This is the membership source for `ids` / `count` / `iter`; the
    /// `metadata` map is now a pure side table, not a membership oracle
    /// (it is never GC'd, so it over-reports).
    fn live_ids(&self) -> impl Iterator<Item = EntityId> + '_ {
        let head_id = self.history.checkpoints().head();
        let entity_heads = self.history.checkpoint(head_id).map(|h| &h.entity_heads);
        entity_heads
            .into_iter()
            .flat_map(|heads| heads.keys().copied())
            .chain(self.transient.keys().copied())
    }

    /// Iterate every live (committed ∪ preview) entity's metadata, in
    /// canonical order (committed first, then preview). Live ids with no
    /// side-table entry are skipped.
    pub fn iter(&self) -> impl Iterator<Item = (EntityId, &EntityMetadata)> {
        self.live_ids()
            .filter_map(move |id| self.metadata.get(&id).map(|m| (id, m.as_ref())))
    }

    /// All live (committed ∪ preview) entity ids, in canonical order
    /// (committed first, then preview).
    pub fn ids(&self) -> impl Iterator<Item = EntityId> + '_ {
        self.live_ids()
    }

    /// Number of live (committed ∪ preview) entities.
    #[must_use]
    pub fn count(&self) -> usize {
        self.live_ids().count()
    }

    /// All current preview ids, in insertion order.
    pub fn preview_ids(&self) -> impl Iterator<Item = EntityId> + '_ {
        self.transient.keys().copied()
    }

    /// Whether the action identified by `request_id` is in flight.
    #[must_use]
    pub fn is_pending(&self, request_id: u64) -> bool {
        self.history.is_pending(request_id)
    }

    /// The request id of the sole in-flight action, if exactly one is
    /// open; `None` otherwise.
    #[must_use]
    pub fn sole_pending_request_id(&self) -> Option<u64> {
        self.history.sole_pending_request_id()
    }

    // ── Action lifecycle (G6: typed mutation intent) ──────────────────

    /// Begin a streaming action under the caller-supplied `request_id`
    /// (allocated by the orchestrator, the single id authority). The kind
    /// determines the entity (via `kind.entity()`); the current lane
    /// head's payload is forked into the tentative snapshot. Opens the
    /// edit under `request_id` (the caller already holds it). Refused if
    /// the action's entity isn't in the committed set or isn't owned by
    /// this kind.
    pub fn begin_action(
        &mut self,
        kind: CheckpointKind,
        label: impl Into<Cow<'static, str>>,
        request_id: u64,
    ) -> Result<(), SessionError> {
        let entity = kind.entity().ok_or(SessionError::ActionRequiresEntity)?;
        let snap_id = self
            .history
            .checkpoint(self.history.checkpoints().head())
            .and_then(|h| h.entity_heads.get(&entity).copied())
            .ok_or(SessionError::History(HistoryError::UnknownEntity {
                entity,
            }))?;
        let snap = self
            .history
            .snapshot(entity, snap_id)
            .ok_or(SessionError::History(HistoryError::UnknownSnapshot {
                entity,
                id: snap_id,
            }))?;
        let payload = Arc::clone(&snap.payload);
        self.history
            .begin_action(entity, kind, payload, label.into(), request_id)?;
        Ok(())
    }

    /// Per-cycle update of the in-flight action. Mutates the tentative
    /// snapshot's payload via `Arc::make_mut` and updates the tentative
    /// checkpoint's score / filter status. Bumps `live_version` only
    /// (no DAG topology change).
    ///
    /// Emits one tentative [`SessionUpdate::Edit`] carrying the locked
    /// entity's post-mutation coordinates. The plugin broadcaster skips
    /// tentative edits (plugins don't see live frames); it completes the
    /// spine for the render projector.
    pub fn action_update(
        &mut self,
        request_id: u64,
        raw_score: Option<f64>,
        game_score: Option<f64>,
        filter_status: Option<FilterStatus>,
        mutate: impl FnMut(&mut MoleculeEntity),
    ) -> Result<(), SessionError> {
        self.history
            .action_update(request_id, raw_score, game_score, filter_status, mutate)?;
        // A tentative live frame: render projector picks it up via the
        // spine and rebuilds from `head_assembly`. Payload-less because
        // the projectors re-derive from `Session` anyway.
        self.apply(SessionUpdate::Edit { tentative: true });
        Ok(())
    }

    /// Commit the action identified by `request_id`. Composes and mints
    /// the committed checkpoint from the committed graph head plus the
    /// edit's lanes; recomputes best cursors. Returns the now-committed
    /// checkpoint id.
    pub fn commit_action(&mut self, request_id: u64) -> Result<CheckpointId, SessionError> {
        let ckpt = self.history.commit_action(request_id)?;
        self.apply(SessionUpdate::HeadMoved);
        Ok(ckpt)
    }

    /// Abort the action identified by `request_id`. Removes its tentative
    /// snapshot(s); lane heads fall back to their parents.
    pub fn abort_action(&mut self, request_id: u64) -> Result<(), SessionError> {
        self.history.abort_action(request_id)?;
        self.apply(SessionUpdate::HeadMoved);
        Ok(())
    }

    // ── Navigation (the action-lock half is enforced one layer down by
    //    `History`) ──────────────────────────────────────────────────

    /// Move checkpoint head to its parent. Returns the new head id, or
    /// `None` if already at root (in which case nothing is emitted).
    pub fn undo(&mut self) -> Result<Option<CheckpointId>, SessionError> {
        let moved = self.history.undo()?;
        if moved.is_some() {
            self.apply(SessionUpdate::HeadMoved);
        }
        Ok(moved)
    }

    /// Move checkpoint head forward to a child. `branch` picks among
    /// multiple children. Returns the new head id, or `None` at a leaf
    /// (in which case nothing is emitted).
    pub fn redo(
        &mut self,
        branch: Option<CheckpointId>,
    ) -> Result<Option<CheckpointId>, SessionError> {
        let head = self.history.checkpoints().head();
        let kids: Vec<CheckpointId> = self
            .history
            .checkpoint(head)
            .map(|h| h.children.iter().copied().collect())
            .unwrap_or_default();
        match (branch, kids.as_slice()) {
            (_, []) => return Ok(None),
            (Some(b), kids) if kids.contains(&b) => {}
            (Some(_), _) => {
                return Err(SessionError::History(HistoryError::NoSuchBranch))
            }
            (None, [_]) => {}
            (None, _) => {
                return Err(SessionError::History(HistoryError::AmbiguousBranch))
            }
        }
        let moved = self.history.redo(branch)?;
        if moved.is_some() {
            self.apply(SessionUpdate::HeadMoved);
        }
        Ok(moved)
    }

    /// Jump checkpoint head to `id`.
    pub fn jump_checkpoint(&mut self, id: CheckpointId) -> Result<CheckpointId, SessionError> {
        let ckpt = self.history.jump_checkpoint(id)?;
        self.apply(SessionUpdate::HeadMoved);
        Ok(ckpt)
    }

    /// Per-entity revert: move `entity`'s lane head to `target`. Pushes
    /// a `LaneUndo` checkpoint mirroring the new lane head.
    pub fn lane_undo(
        &mut self,
        entity: EntityId,
        target: EntitySnapshotId,
    ) -> Result<CheckpointId, SessionError> {
        let ckpt = self.history.lane_undo(entity, target)?;
        self.apply(SessionUpdate::HeadMoved);
        Ok(ckpt)
    }

    /// Per-entity redo: move `entity`'s lane head to a child of the
    /// current lane head. `branch` picks among multiple children.
    pub fn lane_redo(
        &mut self,
        entity: EntityId,
        branch: Option<EntitySnapshotId>,
    ) -> Result<CheckpointId, SessionError> {
        let ckpt = self.history.lane_redo(entity, branch)?;
        self.apply(SessionUpdate::HeadMoved);
        Ok(ckpt)
    }

    // ── Curation ──────────────────────────────────────────────────────

    /// Pin a checkpoint as user-marked best.
    pub fn pin_checkpoint(&mut self, id: CheckpointId) -> Result<(), SessionError> {
        Ok(self.history.pin_checkpoint(id)?)
    }

    /// Unpin a checkpoint.
    pub fn unpin_checkpoint(&mut self, id: CheckpointId) -> Result<(), SessionError> {
        Ok(self.history.unpin_checkpoint(id)?)
    }

    /// Set the "exclude from best" flag.
    pub fn set_exclude_from_best(
        &mut self,
        id: CheckpointId,
        exclude: bool,
    ) -> Result<(), SessionError> {
        Ok(self.history.set_exclude_from_best(id, exclude)?)
    }

    /// Stamp scores on the current head checkpoint in place. Canonical
    /// score write: updates the head checkpoint and bumps the History's
    /// `live_version`; emits no `SessionUpdate`. Scores are off-spine:
    /// plugins compute their own, and the GUI projector picks up the
    /// new score by polling `live_version` through its
    /// `HistorySyncCursor`.
    pub fn set_head_scores(&mut self, raw_score: Option<f64>, game_score: Option<f64>) {
        self.history.set_head_scores(raw_score, game_score);
    }

    /// Stamp scores on the current composition node: the open pending edit
    /// if an action is in flight, else the committed head checkpoint. This
    /// is the score-write the per-tick poll uses so the live score follows
    /// an in-flight action without ever touching the committed parent.
    pub fn set_current_composition_scores(
        &mut self,
        raw_score: Option<f64>,
        game_score: Option<f64>,
    ) {
        self.history
            .set_current_composition_scores(raw_score, game_score);
    }

    /// Read the `(raw, game)` score of the current composition node (open
    /// pending edit if any, else the committed head). The live-score read
    /// surface, mirroring the geometry read surface in [`Self::entity`].
    #[must_use]
    pub fn current_composition_scores(&self) -> (Option<f64>, Option<f64>) {
        self.history.current_composition_scores()
    }

    // ── Preview API — transient, never in history ─────────────────────

    /// Insert a new preview entity. Allocates a fresh id, sets the
    /// entity's id to it, and stores it in `transient` plus
    /// `metadata`. Bypasses history.
    pub fn insert_preview(
        &mut self,
        mut entity: MoleculeEntity,
        name: String,
        origin: EntityOrigin,
    ) -> EntityId {
        let id = self.allocator.allocate();
        entity.set_id(id);
        let _ = self.transient.insert(id, Arc::new(entity));
        let _ = self
            .metadata
            .insert(id, Arc::new(EntityMetadata::new(name, origin)));
        self.apply(SessionUpdate::PreviewAdded);
        id
    }

    /// Remove a preview (cancel / error path). Drops the metadata too.
    /// Returns `true` if a preview was removed.
    pub fn remove_preview(&mut self, id: EntityId) -> bool {
        if self.transient.shift_remove(&id).is_none() {
            return false;
        }
        let _ = self.metadata.shift_remove(&id);
        self.apply(SessionUpdate::PreviewDiscarded);
        true
    }

    /// Promote a preview into history. Removes it from `transient` and
    /// pushes one checkpoint via [`History::add_entity`] with `kind`
    /// (typically [`CheckpointKind::PromotedPreview`] or one of the
    /// ML kinds). Optionally stamps final origin / name.
    /// Refused if the preview is unknown or an action is in flight.
    pub fn promote_preview(
        &mut self,
        id: EntityId,
        kind: CheckpointKind,
        origin: Option<EntityOrigin>,
        name: Option<String>,
        label: impl Into<Cow<'static, str>>,
    ) -> Result<CheckpointId, SessionError> {
        let payload = self
            .transient
            .shift_remove(&id)
            .ok_or(SessionError::NotAPreview { id })?;

        if let Some(meta_arc) = self.metadata.get_mut(&id) {
            let meta = Arc::make_mut(meta_arc);
            if let Some(o) = origin {
                meta.origin = o;
            }
            if let Some(n) = name {
                meta.name = n;
            }
        }

        match self.history.add_entity(id, payload, kind, label.into()) {
            Ok(ckpt) => {
                self.apply(SessionUpdate::HeadMoved);
                Ok(ckpt)
            }
            Err(e) => {
                // Restore the transient entry on failure so the caller
                // can retry. We can't recover the original payload
                // because `add_entity` consumed it on the error path
                // before failing — but the only failure modes are
                // ActiveActionInProgress and EntityAlreadyExists, both
                // of which are caller-fixable; rebuilding the payload
                // from a re-snapshotted entity is a section-4 concern.
                Err(SessionError::History(e))
            }
        }
    }

    // ── Reset ─────────────────────────────────────────────────────────

    /// Drop the entire history graph, clear metadata and transient.
    /// After `reset`, the store is back to the empty initial state;
    /// callers populate it via the preview API + `promote_preview`
    /// (the puzzle-load path used to be a one-shot insert; now it's a
    /// preview-then-promote flow that runs through the same recorded
    /// path as RF3 / RFD3 / MPNN promotions, by design).
    pub fn reset(&mut self) {
        self.metadata.clear();
        self.transient.clear();
        self.allocator = EntityIdAllocator::new();
        self.history = History::new(std::iter::empty(), PathBuf::new());
        // Drop any changes emitted before the reset — they describe state
        // that no longer exists. Cleared BEFORE the reset's own emit below
        // so that change survives. The broadcaster's published snapshot is
        // intentionally NOT cleared (it lives on `PluginDriver`): the
        // post-reset empty-assembly diff still advances the host's gen
        // counter, so plugins never see `from_gen` go backwards.
        self.pending_updates.clear();
        self.apply(SessionUpdate::HeadMoved);
    }

    // ── Backend helpers ───────────────────────────────────────────────

    /// Iterate committed (non-preview) protein entities together with
    /// their metadata. Backend ops drive their work from this iterator;
    /// previews are filtered out by construction (they're not in
    /// `entity_heads`).
    pub fn proteins(&self) -> impl Iterator<Item = (EntityId, &EntityMetadata, &MoleculeEntity)> {
        let head_id = self.history.checkpoints().head();
        let head = self.history.checkpoint(head_id);
        let entity_heads = head.map(|h| &h.entity_heads);
        entity_heads
            .into_iter()
            .flat_map(move |heads| heads.iter())
            .filter_map(move |(eid, snap_id)| {
                let meta = self.metadata.get(eid)?.as_ref();
                let snap = self.history.snapshot(*eid, *snap_id)?;
                let entity: &MoleculeEntity = snap.payload.as_ref();
                if entity.molecule_type() != MoleculeType::Protein {
                    return None;
                }
                Some((*eid, meta, entity))
            })
    }

}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests;
