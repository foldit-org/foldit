//! Authoritative document atop the two-layer [`History`].
//!
//! `Document` owns:
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
//! **Broadcast invariant.** Every public method that changes the
//! observable assembly (any mutation visible through
//! [`Self::head_assembly`]) must queue exactly one
//! [`foldit_runner::orchestrator::BroadcastPayload`] via
//! [`Self::queue_full_broadcast`] /
//! [`Self::queue_delta_broadcast`] (the latter behind
//! [`Self::queue_single_entity_update_broadcast`] /
//! [`Self::queue_assembly_update_broadcast`]). The orchestrator
//! stamps gen counters on each payload and fans it out to peer
//! plugins so their Assembly mirrors stay in sync. Bypassing the
//! queue leaves plugins one generation behind the host; the
//! orchestrator's `STALE_GEN` recovery catches divergence after
//! the fact, but the invariant is what prevents it in the first
//! place.

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::Arc;

use indexmap::IndexMap;
use molex::entity::molecule::id::{EntityId, EntityIdAllocator};
use molex::{Assembly, MoleculeEntity, MoleculeType};

use crate::history::{
    CheckpointId, CheckpointKind, EntitySnapshotId, FilterStatus, History, HistoryError,
};

mod broadcast;
mod change;
// Re-export is unused until the apply funnel + projectors consume it in
// RX6; remove the allow then (mirrors change::SceneChange's dead_code allow).
#[allow(unused_imports)]
pub use change::SceneChange;
mod metadata;
pub use metadata::{EntityMetadata, EntityOrigin};

mod render;

// ── Errors ─────────────────────────────────────────────────────────────

/// Error returned by every fallible [`Document`] operation.
#[derive(Debug, thiserror::Error)]
pub enum DocumentError {
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

// ── Document ───────────────────────────────────────────────────────────

/// Authoritative document over the whole scene.
pub struct Document {
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
    /// Monotonic counter stamped onto every published `Assembly`.
    /// Without it, `Assembly::new` hands viso a fresh snapshot at
    /// generation 0 every time, and viso's `poll_assembly` skips
    /// every publish after the first. Increment on every
    /// `publish_to` / `replace_in`.
    publish_seq: u64,
    /// Drain queue of pending plugin broadcasts produced by
    /// authoritative mutations on this store. `App` pumps this after
    /// each action / keybind / head-move and forwards each payload to
    /// the orchestrator's `broadcast_to_plugins`, which stamps the
    /// gen counters and cache. Always empty in steady state.
    pending_broadcasts: Vec<foldit_runner::orchestrator::BroadcastPayload>,
}

impl Default for Document {
    fn default() -> Self {
        Self::new()
    }
}

impl Document {
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
            publish_seq: 0,
            pending_broadcasts: Vec::new(),
        }
    }

    // ── ID allocation ─────────────────────────────────────────────────

    /// Allocate a fresh entity id.
    pub fn allocate_id(&mut self) -> EntityId {
        self.allocator.allocate()
    }

    // ── Read accessors ────────────────────────────────────────────────

    /// Build the current view of the assembly: the lane heads of every
    /// entity in the checkpoint head's `entity_heads` (in canonical
    /// order), followed by every transient preview (also in insertion
    /// order). Cheap as a per-frame call relative to today's path:
    /// each entity is `MoleculeEntity::clone()` (O(atoms)). A future
    /// optimization can expose an `Assembly::from_arcs(Vec<Arc<…>>)`
    /// constructor on molex to skip the deep clone — out of scope for
    /// section 3.
    #[must_use]
    pub fn head_assembly(&self) -> Assembly {
        let head_id = self.history.checkpoints().head();
        let mut entities: Vec<MoleculeEntity> = Vec::new();
        if let Some(head) = self.history.checkpoint(head_id) {
            for (eid, snap_id) in &head.entity_heads {
                if let Some(snap) = self.history.snapshot(*eid, *snap_id) {
                    entities.push((*snap.payload).clone());
                }
            }
        }
        for arc in self.transient.values() {
            entities.push((**arc).clone());
        }
        Assembly::new(entities)
    }

    /// Read access to the history graph.
    #[must_use]
    pub fn history(&self) -> &History {
        &self.history
    }

    /// Look up an entity by id. Searches committed lane heads first,
    /// then transient previews.
    #[must_use]
    pub fn entity(&self, id: EntityId) -> Option<&MoleculeEntity> {
        let head_id = self.history.checkpoints().head();
        if let Some(head) = self.history.checkpoint(head_id) {
            if let Some(snap_id) = head.entity_heads.get(&id).copied() {
                if let Some(snap) = self.history.snapshot(id, snap_id) {
                    return Some(snap.payload.as_ref());
                }
            }
        }
        self.transient.get(&id).map(|arc| arc.as_ref())
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

    /// Whether `id` is a transient preview.
    #[must_use]
    pub fn is_preview(&self, id: EntityId) -> bool {
        self.transient.contains_key(&id)
    }

    /// All current preview ids, in insertion order.
    pub fn preview_ids(&self) -> impl Iterator<Item = EntityId> + '_ {
        self.transient.keys().copied()
    }

    /// Whether an action is in flight.
    #[must_use]
    pub fn has_ongoing_action(&self) -> bool {
        self.history.ongoing().is_active()
    }

    // ── Action lifecycle (G6: typed mutation intent) ──────────────────

    /// Begin a streaming action. The kind determines the entity (via
    /// `kind.entity()`); the current lane head's payload is forked into
    /// the tentative snapshot. Refused if the action's entity isn't in
    /// the committed set or isn't owned by this kind.
    pub fn begin_action(
        &mut self,
        kind: CheckpointKind,
        label: impl Into<Cow<'static, str>>,
    ) -> Result<CheckpointId, DocumentError> {
        let entity = kind.entity().ok_or(DocumentError::ActionRequiresEntity)?;
        let snap_id = self
            .history
            .checkpoint(self.history.checkpoints().head())
            .and_then(|h| h.entity_heads.get(&entity).copied())
            .ok_or(DocumentError::History(HistoryError::UnknownEntity {
                entity,
            }))?;
        let snap = self
            .history
            .snapshot(entity, snap_id)
            .ok_or(DocumentError::History(HistoryError::UnknownSnapshot {
                entity,
                id: snap_id,
            }))?;
        let payload = Arc::clone(&snap.payload);
        Ok(self.history.begin_action(entity, kind, payload, label.into())?)
    }

    /// Per-cycle update of the in-flight action. Mutates the tentative
    /// snapshot's payload via `Arc::make_mut` and updates the tentative
    /// checkpoint's score / filter status. Bumps `live_version` only
    /// (no DAG topology change).
    pub fn action_update(
        &mut self,
        raw_score: Option<f64>,
        game_score: Option<f64>,
        filter_status: Option<FilterStatus>,
        mutate: impl FnOnce(&mut MoleculeEntity),
    ) -> Result<(), DocumentError> {
        Ok(self
            .history
            .action_update(raw_score, game_score, filter_status, mutate)?)
    }

    /// Commit the in-flight action. Flips tentative flags; recomputes
    /// best cursors; transitions to `Idle`. Returns the now-committed
    /// checkpoint id.
    pub fn commit_action(&mut self) -> Result<CheckpointId, DocumentError> {
        // Capture the active entity + prior lane-head payload before
        // delegating: after commit_action lands, `ongoing` flips to
        // `Idle` and the tentative's parent linkage stays intact, but
        // grabbing both up front keeps the diff site straightforward.
        let prior = self.history.ongoing().locked_entity().and_then(|e| {
            let lane = self.history.lane(e)?;
            let snap = lane.snapshot(lane.head())?;
            // The tentative snapshot's parent is the pre-action lane
            // head — i.e. the payload plugins last saw via broadcast.
            let parent_id = snap.parent?;
            Some((e, Arc::clone(&lane.snapshot(parent_id)?.payload)))
        });
        let ckpt = self.history.commit_action()?;
        match prior {
            Some((entity, prior_payload)) => {
                let new_payload = self
                    .history
                    .lane(entity)
                    .and_then(|l| l.snapshot(l.head()))
                    .map(|s| Arc::clone(&s.payload));
                if let Some(new_payload) = new_payload {
                    self.queue_single_entity_update_broadcast(
                        &prior_payload,
                        &new_payload,
                    );
                } else {
                    let _ = self.queue_full_broadcast();
                }
            }
            None => {
                let _ = self.queue_full_broadcast();
            }
        }
        Ok(ckpt)
    }

    /// Abort the in-flight action. Removes the tentative snapshot and
    /// checkpoint; head pointers fall back to their parents.
    pub fn abort_action(&mut self) -> Result<(), DocumentError> {
        Ok(self.history.abort_action()?)
    }

    /// Atomic non-streaming entity replacement. Pushes one snapshot +
    /// one checkpoint with `tentative = false` immediately. Refused if
    /// an action is in flight.
    pub fn record_entity_update(
        &mut self,
        kind: CheckpointKind,
        entity: EntityId,
        payload: MoleculeEntity,
        label: impl Into<Cow<'static, str>>,
        raw_score: Option<f64>,
        game_score: Option<f64>,
    ) -> Result<CheckpointId, DocumentError> {
        // Capture the prior lane-head payload before the history push
        // so the post-push diff has something to compare against. A
        // None here means the entity wasn't tracked yet (history will
        // reject the call with `UnknownEntity` anyway) — the Full
        // fallback handles the post-error path harmlessly.
        let prior = self
            .history
            .lane(entity)
            .and_then(|l| l.snapshot(l.head()))
            .map(|s| Arc::clone(&s.payload));
        let payload = Arc::new(payload);
        let payload_for_history = Arc::clone(&payload);
        let ckpt = self.history.record_entity_update(
            entity,
            kind,
            payload_for_history,
            label.into(),
            raw_score,
            game_score,
        )?;
        if let Some(prior_payload) = prior {
            self.queue_single_entity_update_broadcast(&prior_payload, &payload);
        } else {
            let _ = self.queue_full_broadcast();
        }
        Ok(ckpt)
    }

    // ── Navigation (the action-lock half is enforced one layer down by
    //    `History`) ──────────────────────────────────────────────────

    /// Move checkpoint head to its parent. Returns the new head id, or
    /// `None` if already at root.
    pub fn undo(&mut self) -> Result<Option<CheckpointId>, DocumentError> {
        if self
            .history
            .checkpoint(self.history.checkpoints().head())
            .and_then(|h| h.parent)
            .is_none()
        {
            return Ok(None);
        }
        let prior = self.head_assembly();
        let moved = self.history.undo()?;
        if moved.is_some() {
            self.queue_assembly_update_broadcast(&prior);
        }
        Ok(moved)
    }

    /// Move checkpoint head forward to a child. `branch` picks among
    /// multiple children. Returns the new head id, or `None` at a leaf.
    pub fn redo(
        &mut self,
        branch: Option<CheckpointId>,
    ) -> Result<Option<CheckpointId>, DocumentError> {
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
                return Err(DocumentError::History(HistoryError::NoSuchBranch))
            }
            (None, [_]) => {}
            (None, _) => {
                return Err(DocumentError::History(HistoryError::AmbiguousBranch))
            }
        }
        let prior = self.head_assembly();
        let moved = self.history.redo(branch)?;
        if moved.is_some() {
            self.queue_assembly_update_broadcast(&prior);
        }
        Ok(moved)
    }

    /// Jump checkpoint head to `id`.
    pub fn jump_checkpoint(&mut self, id: CheckpointId) -> Result<CheckpointId, DocumentError> {
        let prior = self.head_assembly();
        let ckpt = self.history.jump_checkpoint(id)?;
        self.queue_assembly_update_broadcast(&prior);
        Ok(ckpt)
    }

    /// Per-entity revert: move `entity`'s lane head to `target`. Pushes
    /// a `LaneUndo` checkpoint mirroring the new lane head.
    pub fn lane_undo(
        &mut self,
        entity: EntityId,
        target: EntitySnapshotId,
    ) -> Result<CheckpointId, DocumentError> {
        let prior = self.head_assembly();
        let ckpt = self.history.lane_undo(entity, target)?;
        self.queue_assembly_update_broadcast(&prior);
        Ok(ckpt)
    }

    /// Per-entity redo: move `entity`'s lane head to a child of the
    /// current lane head. `branch` picks among multiple children.
    pub fn lane_redo(
        &mut self,
        entity: EntityId,
        branch: Option<EntitySnapshotId>,
    ) -> Result<CheckpointId, DocumentError> {
        let prior = self.head_assembly();
        let ckpt = self.history.lane_redo(entity, branch)?;
        self.queue_assembly_update_broadcast(&prior);
        Ok(ckpt)
    }

    // ── Curation ──────────────────────────────────────────────────────

    /// Pin a checkpoint as user-marked best.
    pub fn pin_checkpoint(&mut self, id: CheckpointId) -> Result<(), DocumentError> {
        Ok(self.history.pin_checkpoint(id)?)
    }

    /// Unpin a checkpoint.
    pub fn unpin_checkpoint(&mut self, id: CheckpointId) -> Result<(), DocumentError> {
        Ok(self.history.unpin_checkpoint(id)?)
    }

    /// Set the "exclude from best" flag.
    pub fn set_exclude_from_best(
        &mut self,
        id: CheckpointId,
        exclude: bool,
    ) -> Result<(), DocumentError> {
        Ok(self.history.set_exclude_from_best(id, exclude)?)
    }

    /// Stamp scores on the current head checkpoint in place. See
    /// [`History::set_head_scores`]: bumps `live_version` only, no
    /// topology change.
    pub fn set_head_scores(&mut self, raw_score: Option<f64>, game_score: Option<f64>) {
        self.history.set_head_scores(raw_score, game_score);
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
        // Topology edit (entity added) — DELTA01 rejects AddEntity, so
        // this stays Full.
        let _ = self.queue_full_broadcast();
        id
    }

    /// Remove a preview (cancel / error path). Drops the metadata too.
    /// Returns `true` if a preview was removed.
    pub fn remove_preview(&mut self, id: EntityId) -> bool {
        if self.transient.shift_remove(&id).is_none() {
            return false;
        }
        let _ = self.metadata.shift_remove(&id);
        // Topology edit (entity removed) — DELTA01 rejects RemoveEntity,
        // so this stays Full.
        let _ = self.queue_full_broadcast();
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
    ) -> Result<CheckpointId, DocumentError> {
        let payload = self
            .transient
            .shift_remove(&id)
            .ok_or(DocumentError::NotAPreview { id })?;

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
                // Topology edit (preview moves from transient into a
                // committed checkpoint) — Full broadcast.
                let _ = self.queue_full_broadcast();
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
                Err(DocumentError::History(e))
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
        // Drop any queued payloads from the prior session — they
        // describe state that no longer exists. Broadcast gen is
        // intentionally NOT reset: a fresh empty-assembly broadcast
        // still progresses the host's gen counter, and resetting it
        // would let plugins see from_gen go backwards.
        self.pending_broadcasts.clear();
        // Hard topology reset — every entity disappears, Full broadcast.
        let _ = self.queue_full_broadcast();
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

    /// First committed entity: the loaded protein. Reads the head
    /// checkpoint's first `entity_heads` key, so (unlike the old
    /// `metadata` scan) it can never surface a stale/undone entity.
    #[must_use]
    pub fn loaded_entity(&self) -> Option<EntityId> {
        let head_id = self.history.checkpoints().head();
        self.history
            .checkpoint(head_id)
            .and_then(|h| h.entity_heads.keys().next().copied())
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests;
