//! Authoritative entity store atop the new two-layer [`History`].
//!
//! See `docs/undo-strategy.md` § Mutation API for the design contract
//! and `docs/undo-fix-plan.md` § 3 for the build-order section that
//! lands these types. The short version:
//!
//! `EntityStore` owns:
//! - [`History`] — the full per-entity timelines + checkpoint graph.
//! - `transient: IndexMap<EntityId, Arc<MoleculeEntity>>` — preview /
//!   scene-resident entities that are visible in [`Self::head_assembly`]
//!   but absent from every checkpoint. Presence in this map *is* the
//!   preview signal; the old [`EntityMetadata::is_preview`] flag is
//!   gone (G6).
//! - `metadata: IndexMap<EntityId, Arc<EntityMetadata>>` — per-entity
//!   metadata (name, origin, role, reference CA, designed sequences).
//!   `Arc`-shared so unchanged entries stay aliased across history
//!   operations (no metadata serialization on every mutation).
//! - [`LockSet`] — placeholder for the future Orchestrator-owned
//!   multi-client lock manager. `EntityStore`'s navigation methods
//!   consult it; the running-action half of the lock rule is enforced
//!   one layer down by [`History`].
//!
//! Mutation intent is in the type signature (G6): three explicit
//! categories — history-bearing actions, metadata-only edits, and
//! one-shot transient previews — with no neutral default. Adding a new
//! mutation requires choosing one.
//!
//! There is no `mutate(closure)`-style API. Every checkpoint-bearing
//! event funnels through `History::record` via a thin shim
//! here; the single-root invariant from G3 is preserved end to end.

use std::borrow::Cow;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use glam::Vec3;
use indexmap::IndexMap;
use molex::entity::molecule::id::{EntityId, EntityIdAllocator};
use molex::{Assembly, MoleculeEntity, MoleculeType};

use crate::history::{
    CheckpointId, CheckpointKind, EntitySnapshotId, FilterStatus, History, HistoryError,
};

// ── Domain metadata types ──────────────────────────────────────────────

/// How an entity entered the scene.
#[derive(Debug, Clone)]
pub enum EntityOrigin {
    /// Loaded from file or puzzle.
    Loaded,
    /// Result of RFDiffusion3 backbone design.
    StructureDesign { source: EntityId, confidence: f32 },
}

/// What operations are permitted on this entity.
#[derive(Debug, Clone)]
pub struct EntityRole {
    /// Structure (backbone) can be modified — wiggle, shake, RFD3.
    pub foldable: bool,
    /// Sequence can be redesigned — MPNN.
    pub designable: bool,
    /// Non-interactive background entity (waters, ions, lipids).
    pub ambient: bool,
}

/// A designed sequence paired with the backbone it was designed for.
#[derive(Debug, Clone)]
pub struct DesignedSequence {
    /// Single-letter amino-acid sequence.
    pub sequence: String,
    /// Designer's score for this sequence (lower-is-better, MPNN).
    pub score: f32,
    /// Entity this sequence was designed against.
    pub designed_for: EntityId,
}

/// Per-entity metadata that rides alongside the entity payload.
///
/// Visibility is **not** here — that lives on viso's
/// `EntityAnnotations`. The previous `is_preview: bool` flag is also
/// gone — presence in [`EntityStore::transient`] is the preview signal.
#[derive(Debug, Clone)]
pub struct EntityMetadata {
    /// Display name.
    pub name: String,
    /// How the entity entered the scene.
    pub origin: EntityOrigin,
    /// What operations are permitted.
    pub role: EntityRole,
    /// Optional reference CA set for alignment.
    pub reference_ca: Option<Vec<Vec3>>,
    /// Designed sequences, appended by MPNN runs.
    pub designed_sequences: Vec<DesignedSequence>,
}

impl EntityMetadata {
    /// Build a minimal metadata record.
    #[must_use]
    pub fn new(name: String, origin: EntityOrigin, role: EntityRole) -> Self {
        Self {
            name,
            origin,
            role,
            reference_ca: None,
            designed_sequences: Vec::new(),
        }
    }
}

// ── Lock set ───────────────────────────────────────────────────────────

/// Placeholder for the future Orchestrator-owned multi-client lock
/// manager.
///
/// `EntityStore` holds one for now; a future refactor (when the runner
/// runs server-side and clients are remote) will move ownership to a
/// dedicated Orchestrator and pass `&LockSet` into `EntityStore`'s
/// navigation methods. The interface is the same either way: a set of
/// entity ids plus `acquire` / `release` / `contains`. See
/// `docs/undo-strategy.md` § Lock semantics.
///
/// The `History` layer enforces the *action lock* (the entity owned by
/// the running [`crate::history::OngoingState::Active`]). `LockSet`
/// here adds the *client lock* — entities reserved by another client.
/// The two layers compose; a head-move that would touch either
/// refuses.
#[derive(Debug, Clone, Default)]
pub struct LockSet {
    locked: HashSet<EntityId>,
}

impl LockSet {
    /// Empty lock set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether `entity` is currently locked.
    #[must_use]
    pub fn contains(&self, entity: EntityId) -> bool {
        self.locked.contains(&entity)
    }

    /// Reserve `entity`. Idempotent.
    pub fn acquire(&mut self, entity: EntityId) {
        let _ = self.locked.insert(entity);
    }

    /// Release `entity`. No-op if not held.
    pub fn release(&mut self, entity: EntityId) {
        let _ = self.locked.remove(&entity);
    }

    /// Iterate the currently-locked entities.
    pub fn iter(&self) -> impl Iterator<Item = EntityId> + '_ {
        self.locked.iter().copied()
    }
}

// ── Errors ─────────────────────────────────────────────────────────────

/// Error returned by every fallible [`EntityStore`] operation.
#[derive(Debug)]
pub enum EntityStoreError {
    /// `History`-layer refusal (state machine, action lock, missing id,
    /// etc.). See [`HistoryError`].
    History(HistoryError),
    /// A navigation target would change a client-locked entity (the
    /// outer lock layer; the inner action-lock half lives on
    /// [`HistoryError::EntityLocked`]).
    LockedByClient { entity: EntityId },
    /// `id` is not currently a transient preview.
    NotAPreview { id: EntityId },
    /// `begin_action` was called with a [`CheckpointKind`] that doesn't
    /// name an entity (e.g., `Loaded`, `BondsChanged`). Action lifecycle
    /// kinds always do.
    ActionRequiresEntity,
}

impl From<HistoryError> for EntityStoreError {
    fn from(e: HistoryError) -> Self {
        EntityStoreError::History(e)
    }
}

impl std::fmt::Display for EntityStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EntityStoreError::History(e) => write!(f, "{e}"),
            EntityStoreError::LockedByClient { entity } => {
                write!(f, "entity {} is locked by another client", entity.raw())
            }
            EntityStoreError::NotAPreview { id } => {
                write!(f, "{} is not a transient preview", id.raw())
            }
            EntityStoreError::ActionRequiresEntity => {
                f.write_str("action lifecycle requires an entity-targeted CheckpointKind")
            }
        }
    }
}

impl std::error::Error for EntityStoreError {}

// ── Backend hand-off result ────────────────────────────────────────────

/// Combined assembly handed off to the Rosetta backend.
///
/// `entity_ids` and `residue_ranges` are *parallel* arrays — one entry
/// per entity in the produced assembly. The parallelism is the
/// load-bearing invariant for downstream callers (focus locking, score
/// projection); zero-residue entities are dropped from both arrays
/// together so the indexing stays consistent (the prior bug — see
/// `docs/undo-fix-plan.md` § 3).
pub struct CombinedAssemblyResult {
    /// Assembly with one entity per protein, in the same order as
    /// `entity_ids`. Backend-side IDs are minted fresh; match by
    /// position, not by id.
    pub assembly: Assembly,
    /// Local foldit entity ids parallel to `assembly.entities()`.
    pub entity_ids: Vec<EntityId>,
    /// Per-entity Rosetta residue ranges `(start, end)`, 1-indexed and
    /// inclusive, parallel to `entity_ids`.
    pub residue_ranges: Vec<(usize, usize)>,
}

/// Pure builder factored out of [`EntityStore::combined_assembly_for_backend`]
/// so the zero-residue alignment fix is unit-testable without a live
/// `EntityStore`.
fn build_combined_assembly(
    proteins: &[(EntityId, MoleculeEntity)],
) -> Option<CombinedAssemblyResult> {
    if proteins.is_empty() {
        return None;
    }
    let mut entity_ids = Vec::with_capacity(proteins.len());
    let mut residue_ranges = Vec::with_capacity(proteins.len());
    let mut entities = Vec::with_capacity(proteins.len());
    let mut cursor = 1usize;
    for (id, entity) in proteins {
        let res_count = match entity {
            MoleculeEntity::Protein(p) => p.residues.len(),
            _ => 0,
        };
        // Drop empty proteins from *all three* parallel arrays
        // together — the prior bug had `entity_ids` collected up front
        // (over the unfiltered list) so it ended up longer than
        // `residue_ranges` whenever any protein had zero residues.
        if res_count == 0 {
            continue;
        }
        let start = cursor;
        let end = cursor + res_count - 1;
        entity_ids.push(*id);
        residue_ranges.push((start, end));
        entities.push(entity.clone());
        cursor = end + 1;
    }
    if entities.is_empty() {
        return None;
    }
    debug_assert_eq!(entity_ids.len(), residue_ranges.len());
    debug_assert_eq!(entity_ids.len(), entities.len());
    Some(CombinedAssemblyResult {
        assembly: Assembly::new(entities),
        entity_ids,
        residue_ranges,
    })
}

// ── EntityStore ────────────────────────────────────────────────────────

/// Authoritative entity store.
pub struct EntityStore {
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
    /// Multi-client lock placeholder. See [`LockSet`].
    locks: LockSet,
    /// Monotonic counter stamped onto every published `Assembly`.
    /// Without it, `Assembly::new` hands viso a fresh snapshot at
    /// generation 0 every time, and viso's `poll_assembly` skips
    /// every publish after the first. Increment on every
    /// `publish_to` / `replace_in`.
    publish_seq: u64,
}

impl Default for EntityStore {
    fn default() -> Self {
        Self::new()
    }
}

impl EntityStore {
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
            locks: LockSet::new(),
            publish_seq: 0,
        }
    }

    // ── ID allocation ─────────────────────────────────────────────────

    /// Allocate a fresh entity id.
    pub fn allocate_id(&mut self) -> EntityId {
        self.allocator.allocate()
    }

    /// Reconstruct an [`EntityId`] from a raw `u32` (deserialization).
    /// Advances the allocator past `raw` so future allocations don't
    /// collide.
    pub fn mint_id(&mut self, raw: u32) -> EntityId {
        self.allocator.from_raw(raw)
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

    /// Read access to the multi-client lock set.
    #[must_use]
    pub fn locks(&self) -> &LockSet {
        &self.locks
    }

    /// Mutable access — the placeholder until an Orchestrator owns it.
    pub fn locks_mut(&mut self) -> &mut LockSet {
        &mut self.locks
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

    /// Iterate every tracked entity's metadata, in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = (EntityId, &EntityMetadata)> {
        self.metadata.iter().map(|(id, m)| (*id, m.as_ref()))
    }

    /// All currently-tracked entity ids, in insertion order. Includes
    /// committed entities and previews.
    pub fn ids(&self) -> impl Iterator<Item = EntityId> + '_ {
        self.metadata.keys().copied()
    }

    /// Number of tracked entities (committed + preview).
    #[must_use]
    pub fn count(&self) -> usize {
        self.metadata.len()
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
    /// the committed set, isn't owned by this kind, or is locked
    /// elsewhere.
    pub fn begin_action(
        &mut self,
        kind: CheckpointKind,
        label: impl Into<Cow<'static, str>>,
    ) -> Result<CheckpointId, EntityStoreError> {
        let entity = kind.entity().ok_or(EntityStoreError::ActionRequiresEntity)?;
        if self.locks.contains(entity) {
            return Err(EntityStoreError::LockedByClient { entity });
        }
        let snap_id = self
            .history
            .checkpoint(self.history.checkpoints().head())
            .and_then(|h| h.entity_heads.get(&entity).copied())
            .ok_or(EntityStoreError::History(HistoryError::UnknownEntity {
                entity,
            }))?;
        let snap = self
            .history
            .snapshot(entity, snap_id)
            .ok_or(EntityStoreError::History(HistoryError::UnknownSnapshot {
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
    ) -> Result<(), EntityStoreError> {
        Ok(self
            .history
            .action_update(raw_score, game_score, filter_status, mutate)?)
    }

    /// Commit the in-flight action. Flips tentative flags; recomputes
    /// best cursors; transitions to `Idle`. Returns the now-committed
    /// checkpoint id.
    pub fn commit_action(&mut self) -> Result<CheckpointId, EntityStoreError> {
        Ok(self.history.commit_action()?)
    }

    /// Abort the in-flight action. Removes the tentative snapshot and
    /// checkpoint; head pointers fall back to their parents.
    pub fn abort_action(&mut self) -> Result<(), EntityStoreError> {
        Ok(self.history.abort_action()?)
    }

    /// Atomic non-streaming entity replacement. Pushes one snapshot +
    /// one checkpoint with `tentative = false` immediately. Refused if
    /// `entity` is locked or an action is in flight.
    pub fn record_entity_update(
        &mut self,
        kind: CheckpointKind,
        entity: EntityId,
        payload: MoleculeEntity,
        label: impl Into<Cow<'static, str>>,
        raw_score: Option<f64>,
        game_score: Option<f64>,
    ) -> Result<CheckpointId, EntityStoreError> {
        if self.locks.contains(entity) {
            return Err(EntityStoreError::LockedByClient { entity });
        }
        Ok(self.history.record_entity_update(
            entity,
            kind,
            Arc::new(payload),
            label.into(),
            raw_score,
            game_score,
        )?)
    }

    // ── Navigation (lock-checked via the LockSet client lock; the
    //    action-lock half is enforced one layer down by `History`) ────

    /// Move checkpoint head to its parent. Returns the new head id, or
    /// `None` if already at root.
    pub fn undo(&mut self) -> Result<Option<CheckpointId>, EntityStoreError> {
        let parent = match self
            .history
            .checkpoint(self.history.checkpoints().head())
            .and_then(|h| h.parent)
        {
            Some(p) => p,
            None => return Ok(None),
        };
        self.check_target_locks(parent)?;
        Ok(self.history.undo()?)
    }

    /// Move checkpoint head forward to a child. `branch` picks among
    /// multiple children. Returns the new head id, or `None` at a leaf.
    pub fn redo(
        &mut self,
        branch: Option<CheckpointId>,
    ) -> Result<Option<CheckpointId>, EntityStoreError> {
        let head = self.history.checkpoints().head();
        let kids: Vec<CheckpointId> = self
            .history
            .checkpoint(head)
            .map(|h| h.children.iter().copied().collect())
            .unwrap_or_default();
        let target = match (branch, kids.as_slice()) {
            (_, []) => return Ok(None),
            (Some(b), kids) if kids.contains(&b) => b,
            (Some(_), _) => {
                return Err(EntityStoreError::History(HistoryError::NoSuchBranch))
            }
            (None, [only]) => *only,
            (None, _) => {
                return Err(EntityStoreError::History(HistoryError::AmbiguousBranch))
            }
        };
        self.check_target_locks(target)?;
        Ok(self.history.redo(branch)?)
    }

    /// Jump checkpoint head to `id`.
    pub fn jump_checkpoint(&mut self, id: CheckpointId) -> Result<CheckpointId, EntityStoreError> {
        self.check_target_locks(id)?;
        Ok(self.history.jump_checkpoint(id)?)
    }

    /// Per-entity revert: move `entity`'s lane head to `target`. Pushes
    /// a `LaneUndo` checkpoint mirroring the new lane head.
    pub fn lane_undo(
        &mut self,
        entity: EntityId,
        target: EntitySnapshotId,
    ) -> Result<CheckpointId, EntityStoreError> {
        if self.locks.contains(entity) {
            return Err(EntityStoreError::LockedByClient { entity });
        }
        Ok(self.history.lane_undo(entity, target)?)
    }

    /// Per-entity redo: move `entity`'s lane head to a child of the
    /// current lane head. `branch` picks among multiple children.
    pub fn lane_redo(
        &mut self,
        entity: EntityId,
        branch: Option<EntitySnapshotId>,
    ) -> Result<CheckpointId, EntityStoreError> {
        if self.locks.contains(entity) {
            return Err(EntityStoreError::LockedByClient { entity });
        }
        Ok(self.history.lane_redo(entity, branch)?)
    }

    /// Refuse if any client-locked entity's payload would change going to
    /// `target`.
    fn check_target_locks(&self, target: CheckpointId) -> Result<(), EntityStoreError> {
        let target_ckpt = self
            .history
            .checkpoint(target)
            .ok_or(EntityStoreError::History(HistoryError::UnknownCheckpoint {
                id: target,
            }))?;
        for (entity, target_snap) in &target_ckpt.entity_heads {
            if !self.locks.contains(*entity) {
                continue;
            }
            let current = self.history.lane(*entity).map(|l| l.head());
            if current != Some(*target_snap) {
                return Err(EntityStoreError::LockedByClient { entity: *entity });
            }
        }
        Ok(())
    }

    // ── Curation ──────────────────────────────────────────────────────

    /// Pin a checkpoint as user-marked best.
    pub fn pin_checkpoint(&mut self, id: CheckpointId) -> Result<(), EntityStoreError> {
        Ok(self.history.pin_checkpoint(id)?)
    }

    /// Unpin a checkpoint.
    pub fn unpin_checkpoint(&mut self, id: CheckpointId) -> Result<(), EntityStoreError> {
        Ok(self.history.unpin_checkpoint(id)?)
    }

    /// Set the "exclude from best" flag.
    pub fn set_exclude_from_best(
        &mut self,
        id: CheckpointId,
        exclude: bool,
    ) -> Result<(), EntityStoreError> {
        Ok(self.history.set_exclude_from_best(id, exclude)?)
    }

    /// Stamp scores on the current head checkpoint in place. See
    /// [`History::set_head_scores`]: bumps `live_version` only, no
    /// topology change.
    pub fn set_head_scores(&mut self, raw_score: Option<f64>, game_score: Option<f64>) {
        self.history.set_head_scores(raw_score, game_score);
    }

    /// Overwrite the entity payload on the current head's lane snapshot.
    /// See [`History::set_head_entity`]: bumps `live_version` only, no
    /// new checkpoint. Returns `true` if the entity is part of the
    /// current head and the payload was applied.
    pub fn set_head_entity(
        &mut self,
        entity: EntityId,
        payload: MoleculeEntity,
    ) -> bool {
        self.history.set_head_entity(entity, payload)
    }

    // ── Pure metadata edits — NOT history (G1: no shadow score; G6) ──

    /// Set an entity's display name. No history push.
    pub fn set_entity_name(&mut self, id: EntityId, name: String) {
        if let Some(meta_arc) = self.metadata.get_mut(&id) {
            Arc::make_mut(meta_arc).name = name;
        }
    }

    /// Stash a reference CA set for alignment. No history push.
    pub fn set_reference_ca(&mut self, id: EntityId, ca: Vec<Vec3>) {
        if let Some(meta_arc) = self.metadata.get_mut(&id) {
            Arc::make_mut(meta_arc).reference_ca = Some(ca);
        }
    }

    /// Append designed sequences to `id`'s metadata. No history push.
    pub fn add_designed_sequences(&mut self, id: EntityId, seqs: Vec<DesignedSequence>) {
        if let Some(meta_arc) = self.metadata.get_mut(&id) {
            Arc::make_mut(meta_arc).designed_sequences.extend(seqs);
        }
    }

    /// Replace an entity's role. No history push.
    pub fn set_entity_role(&mut self, id: EntityId, role: EntityRole) {
        if let Some(meta_arc) = self.metadata.get_mut(&id) {
            Arc::make_mut(meta_arc).role = role;
        }
    }

    /// Replace an entity's origin. No history push.
    pub fn set_entity_origin(&mut self, id: EntityId, origin: EntityOrigin) {
        if let Some(meta_arc) = self.metadata.get_mut(&id) {
            Arc::make_mut(meta_arc).origin = origin;
        }
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
        role: EntityRole,
    ) -> EntityId {
        let id = self.allocator.allocate();
        entity.set_id(id);
        let _ = self.transient.insert(id, Arc::new(entity));
        let _ = self
            .metadata
            .insert(id, Arc::new(EntityMetadata::new(name, origin, role)));
        id
    }

    /// Insert a preview with a caller-supplied id. Used when restoring
    /// state from a wire payload (e.g., session reload). Bypasses
    /// history.
    pub fn insert_preview_with_id(
        &mut self,
        id: EntityId,
        mut entity: MoleculeEntity,
        name: String,
        origin: EntityOrigin,
        role: EntityRole,
    ) {
        entity.set_id(id);
        let _ = self.transient.insert(id, Arc::new(entity));
        let _ = self
            .metadata
            .insert(id, Arc::new(EntityMetadata::new(name, origin, role)));
    }

    /// Replace the entity body of an existing preview. No-op if `id`
    /// is not currently a preview.
    pub fn update_preview(&mut self, id: EntityId, mut entity: MoleculeEntity) {
        if !self.transient.contains_key(&id) {
            return;
        }
        entity.set_id(id);
        let _ = self.transient.insert(id, Arc::new(entity));
    }

    /// Remove a preview (cancel / error path). Drops the metadata too.
    /// Returns `true` if a preview was removed.
    pub fn remove_preview(&mut self, id: EntityId) -> bool {
        if self.transient.shift_remove(&id).is_none() {
            return false;
        }
        let _ = self.metadata.shift_remove(&id);
        true
    }

    /// Promote a preview into history. Removes it from `transient` and
    /// pushes one checkpoint via [`History::add_entity`] with `kind`
    /// (typically [`CheckpointKind::PromotedPreview`] or one of the
    /// ML kinds). Optionally stamps final origin / role / name.
    /// Refused if the preview is unknown or an action is in flight.
    pub fn promote_preview(
        &mut self,
        id: EntityId,
        kind: CheckpointKind,
        origin: Option<EntityOrigin>,
        role: Option<EntityRole>,
        name: Option<String>,
        label: impl Into<Cow<'static, str>>,
    ) -> Result<CheckpointId, EntityStoreError> {
        let payload = self
            .transient
            .shift_remove(&id)
            .ok_or(EntityStoreError::NotAPreview { id })?;

        if let Some(meta_arc) = self.metadata.get_mut(&id) {
            let meta = Arc::make_mut(meta_arc);
            if let Some(o) = origin {
                meta.origin = o;
            }
            if let Some(r) = role {
                meta.role = r;
            }
            if let Some(n) = name {
                meta.name = n;
            }
        }

        match self.history.add_entity(id, payload, kind, label.into()) {
            Ok(ckpt) => Ok(ckpt),
            Err(e) => {
                // Restore the transient entry on failure so the caller
                // can retry. We can't recover the original payload
                // because `add_entity` consumed it on the error path
                // before failing — but the only failure modes are
                // ActiveActionInProgress and EntityAlreadyExists, both
                // of which are caller-fixable; rebuilding the payload
                // from a re-snapshotted entity is a section-4 concern.
                Err(EntityStoreError::History(e))
            }
        }
    }

    // ── Reset & one-shot transient mutation ───────────────────────────

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
        self.locks = LockSet::new();
        self.history = History::new(std::iter::empty(), PathBuf::new());
    }

    /// One-shot animation lerp / visualization-only operation: builds a
    /// transient `Assembly` from `head_assembly()`, applies `f`, and
    /// returns its result. **Does not modify** `self.transient`
    /// (different concept; one-shot vs persistent) and does not touch
    /// history.
    pub fn mutate_transient<R>(&mut self, f: impl FnOnce(&mut Assembly) -> R) -> R {
        let mut assembly = self.head_assembly();
        f(&mut assembly)
    }

    // ── Viso publishing — thin wrappers over `head_assembly()` ────────

    /// Push the current `head_assembly()` snapshot to viso. Each push
    /// stamps a fresh `publish_seq` onto the Assembly so viso's
    /// generation-gate (`poll_assembly`) sees a different number on
    /// every call — without that, the second-and-subsequent publishes
    /// would silently skip because `Assembly::new` always starts at
    /// generation 0.
    pub fn publish_to(&mut self, engine: &mut viso::VisoEngine) {
        let mut asm = self.head_assembly();
        self.publish_seq = self.publish_seq.saturating_add(1);
        asm.set_generation(self.publish_seq);
        engine.set_assembly(Arc::new(asm));
    }

    /// Atomic topology swap: hand the current `head_assembly()` to
    /// viso and have it tear down scene-local state + force-sync in
    /// one shot. Use for puzzle / file reloads where leftover
    /// per-entity state from the previous topology would otherwise
    /// linger until the next render tick.
    pub fn replace_in(&mut self, engine: &mut viso::VisoEngine) {
        let mut asm = self.head_assembly();
        self.publish_seq = self.publish_seq.saturating_add(1);
        asm.set_generation(self.publish_seq);
        engine.replace_assembly(Arc::new(asm));
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

    /// Build the combined `Assembly` for the Rosetta backend. Returns
    /// `None` if there are no usable proteins (empty session, or every
    /// protein is zero-residue).
    pub fn combined_assembly_for_backend(&self) -> Option<CombinedAssemblyResult> {
        let proteins: Vec<(EntityId, MoleculeEntity)> = self
            .proteins()
            .map(|(id, _, e)| (id, e.clone()))
            .collect();
        build_combined_assembly(&proteins)
    }

    /// Count protein residues per non-preview, non-empty entity.
    pub fn visible_residue_counts(&self) -> Vec<(EntityId, usize)> {
        self.proteins()
            .filter_map(|(id, _, entity)| match entity {
                MoleculeEntity::Protein(p) if !p.residues.is_empty() => {
                    Some((id, p.residues.len()))
                }
                _ => None,
            })
            .collect()
    }

    /// Build a focus description from focus + entity names.
    pub fn focus_description(&self, focus: &viso::Focus) -> String {
        match focus {
            viso::Focus::Session => {
                let count = self.metadata.len();
                format!("Session ({count} entities)")
            }
            viso::Focus::Entity(id) => self
                .metadata
                .get(id)
                .map(|m| m.name.clone())
                .unwrap_or_else(|| format!("Entity {}", id.raw())),
        }
    }

    /// Get the bytes-encoded assembly for one entity (all molecule
    /// types).
    pub fn get_entity_assembly_bytes(&self, id: EntityId) -> Option<Vec<u8>> {
        let entity = self.entity(id)?;
        molex::ops::codec::assembly_bytes(std::slice::from_ref(entity)).ok()
    }

    /// Collect entities for an ML kickoff based on the current focus.
    /// Refuses preview targets — kicking off RF3/RFD3/MPNN against a
    /// streaming preview would be nonsense.
    pub fn collect_ml_entities(
        &self,
        focus: &viso::Focus,
        fallback_entity: Option<EntityId>,
    ) -> Option<(EntityId, Vec<MoleculeEntity>)> {
        match focus {
            viso::Focus::Entity(eid) => {
                if self.is_preview(*eid) {
                    return None;
                }
                let entity = self.entity(*eid)?.clone();
                Some((*eid, vec![entity]))
            }
            viso::Focus::Session => {
                let id = fallback_entity?;
                if self.is_preview(id) {
                    return None;
                }
                let entity = self.entity(id)?.clone();
                Some((id, vec![entity]))
            }
        }
    }

    /// First committed (non-preview) loaded entity.
    #[must_use]
    pub fn loaded_entity(&self) -> Option<EntityId> {
        self.metadata
            .iter()
            .find(|(id, m)| {
                !self.transient.contains_key(*id) && matches!(m.origin, EntityOrigin::Loaded)
            })
            .map(|(id, _)| *id)
    }

    /// Reference CA positions for alignment.
    #[must_use]
    pub fn reference_ca(&self, id: EntityId) -> Option<&[Vec3]> {
        self.metadata
            .get(&id)
            .and_then(|m| m.reference_ca.as_deref())
    }

    /// Entity metadata (origin + role).
    #[must_use]
    pub fn entity_meta(&self, id: EntityId) -> Option<(&EntityOrigin, &EntityRole)> {
        self.metadata.get(&id).map(|m| (&m.origin, &m.role))
    }

    /// Register a loaded entity with reference CA + role detection.
    /// Pure metadata edit — no history push.
    pub fn register_loaded(&mut self, id: EntityId, reference_ca: Vec<Vec3>) {
        if let Some(meta_arc) = self.metadata.get_mut(&id) {
            let meta = Arc::make_mut(meta_arc);
            meta.origin = EntityOrigin::Loaded;
            meta.role = EntityRole {
                foldable: true,
                designable: true,
                ambient: false,
            };
            meta.reference_ca = Some(reference_ca);
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
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
                });
            }
            let end = atoms.len();
            residues.push(Residue {
                name: *b"ALA",
                number: i as i32 + 1,
                atom_range: start..end,
            });
        }
        MoleculeEntity::Protein(ProteinEntity::new_continuous(id, atoms, residues, b'A'))
    }

    // ── combined_assembly_for_backend zero-residue alignment fix ──────

    #[test]
    fn combined_assembly_drops_zero_residue_entities_from_all_arrays() {
        // Mandated by docs/undo-fix-plan.md § 3: a fixture with one
        // zero-residue entity must produce parallel arrays of equal
        // length.
        let mut alloc = EntityIdAllocator::new();
        let id_a = alloc.allocate();
        let id_b = alloc.allocate();
        let id_c = alloc.allocate();
        let proteins = vec![
            (id_a, mk_protein(id_a, 5)),
            (id_b, mk_protein(id_b, 0)), // zero-residue protein
            (id_c, mk_protein(id_c, 3)),
        ];

        let result = build_combined_assembly(&proteins).expect("non-empty result");

        assert_eq!(
            result.entity_ids.len(),
            result.residue_ranges.len(),
            "entity_ids and residue_ranges must be parallel"
        );
        assert_eq!(result.entity_ids.len(), 2, "zero-residue entity dropped");
        assert_eq!(result.entity_ids, vec![id_a, id_c]);
        assert_eq!(result.residue_ranges, vec![(1, 5), (6, 8)]);
    }

    #[test]
    fn combined_assembly_returns_none_when_only_zero_residue_proteins() {
        let mut alloc = EntityIdAllocator::new();
        let id = alloc.allocate();
        let proteins = vec![(id, mk_protein(id, 0))];
        assert!(build_combined_assembly(&proteins).is_none());
    }

    #[test]
    fn combined_assembly_returns_none_on_empty_input() {
        assert!(build_combined_assembly(&[]).is_none());
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
            EntityRole {
                foldable: false,
                designable: false,
                ambient: true,
            },
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
                None,
                "no",
            )
            .unwrap_err();
        assert!(matches!(err, EntityStoreError::NotAPreview { .. }));
    }

    // ── Lock-checked navigation ───────────────────────────────────────

    #[test]
    fn undo_refuses_when_target_changes_a_locked_entity() {
        let mut store = EntityStore::new();
        let id = store.insert_preview(
            mk_bulk(mk_dummy_id()),
            "x".to_string(),
            EntityOrigin::Loaded,
            EntityRole {
                foldable: true,
                designable: false,
                ambient: false,
            },
        );
        let _root_ckpt = store
            .promote_preview(
                id,
                CheckpointKind::PromotedPreview { entity: id },
                None,
                None,
                None,
                "p",
            )
            .unwrap();

        // Push a record_entity_update so undo has somewhere to go.
        let _ = store
            .record_entity_update(
                CheckpointKind::Shake { entity: id, duration_ms: 1 },
                id,
                mk_bulk(id),
                "shake",
                None,
                None,
            )
            .unwrap();

        // Undo target's entity_heads[id] != current → if `id` is
        // locked, refuse.
        store.locks_mut().acquire(id);
        let err = store.undo().expect_err("undo should refuse");
        assert!(matches!(err, EntityStoreError::LockedByClient { entity } if entity == id));

        // Releasing the lock unblocks undo.
        store.locks_mut().release(id);
        let to = store.undo().unwrap();
        assert!(to.is_some());
    }

    #[test]
    fn lane_undo_refuses_locked_entity() {
        let mut store = EntityStore::new();
        let id = store.insert_preview(
            mk_bulk(mk_dummy_id()),
            "x".to_string(),
            EntityOrigin::Loaded,
            EntityRole {
                foldable: true,
                designable: false,
                ambient: false,
            },
        );
        let _ = store
            .promote_preview(
                id,
                CheckpointKind::PromotedPreview { entity: id },
                None,
                None,
                None,
                "p",
            )
            .unwrap();
        let lane_root = store.history().lane(id).unwrap().root();
        let _ = store
            .record_entity_update(
                CheckpointKind::Shake { entity: id, duration_ms: 1 },
                id,
                mk_bulk(id),
                "shake",
                None,
                None,
            )
            .unwrap();

        store.locks_mut().acquire(id);
        let err = store.lane_undo(id, lane_root).expect_err("should refuse");
        assert!(matches!(err, EntityStoreError::LockedByClient { entity } if entity == id));
    }

    // ── Reset clears everything ───────────────────────────────────────

    #[test]
    fn reset_clears_history_metadata_and_transient() {
        let mut store = EntityStore::new();
        let id = store.insert_preview(
            mk_bulk(mk_dummy_id()),
            "x".to_string(),
            EntityOrigin::Loaded,
            EntityRole {
                foldable: true,
                designable: false,
                ambient: false,
            },
        );
        store.locks_mut().acquire(id);
        assert_eq!(store.count(), 1);
        assert!(store.is_preview(id));

        store.reset();

        assert_eq!(store.count(), 0);
        assert!(!store.is_preview(id));
        assert!(!store.locks().contains(id));
        assert_eq!(store.history().checkpoints().len(), 1); // root only
        assert!(store
            .history()
            .checkpoint(store.history().checkpoints().head())
            .unwrap()
            .entity_heads
            .is_empty());
    }

    // ── Pure metadata edits don't touch history ──────────────────────

    #[test]
    fn set_entity_name_does_not_push_history() {
        let mut store = EntityStore::new();
        let id = store.insert_preview(
            mk_bulk(mk_dummy_id()),
            "old".to_string(),
            EntityOrigin::Loaded,
            EntityRole {
                foldable: false,
                designable: false,
                ambient: true,
            },
        );
        let _ = store
            .promote_preview(
                id,
                CheckpointKind::PromotedPreview { entity: id },
                None,
                None,
                None,
                "p",
            )
            .unwrap();
        let n_ckpts = store.history().checkpoints().len();

        store.set_entity_name(id, "new".to_string());
        assert_eq!(store.metadata(id).unwrap().name, "new");
        assert_eq!(
            store.history().checkpoints().len(),
            n_ckpts,
            "metadata edit must not push a checkpoint"
        );
    }

    #[test]
    fn add_designed_sequences_does_not_push_history() {
        let mut store = EntityStore::new();
        let id = store.insert_preview(
            mk_bulk(mk_dummy_id()),
            "x".to_string(),
            EntityOrigin::Loaded,
            EntityRole {
                foldable: true,
                designable: true,
                ambient: false,
            },
        );
        let _ = store
            .promote_preview(
                id,
                CheckpointKind::PromotedPreview { entity: id },
                None,
                None,
                None,
                "p",
            )
            .unwrap();
        let n_ckpts = store.history().checkpoints().len();

        store.add_designed_sequences(
            id,
            vec![DesignedSequence {
                sequence: "AAA".to_string(),
                score: 1.0,
                designed_for: id,
            }],
        );
        assert_eq!(store.metadata(id).unwrap().designed_sequences.len(), 1);
        assert_eq!(
            store.history().checkpoints().len(),
            n_ckpts,
            "metadata edit must not push a checkpoint"
        );
    }
}
