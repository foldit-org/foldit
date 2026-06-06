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
//! projectors (the `RunnerProjector` owns the Full/Delta plugin
//! fan-out; the render + GUI projectors follow). Because `pending_updates`
//! is private and `apply` is its sole pusher, "one emit per mutator" is a
//! structural invariant, not a runtime assertion.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;

use indexmap::IndexMap;
use molex::entity::molecule::id::{EntityId, EntityIdAllocator};
use molex::{Assembly, MoleculeEntity, MoleculeType};
use viso::Focus;
use viso::options::VisoOptions;

use crate::history::{
    CheckpointId, CheckpointKind, EntitySnapshotId, FilterStatus, History, HistoryError,
};

mod apply;
mod change;
pub use change::SessionUpdate;
pub(crate) use change::SessionUpdateConsumer;
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
}

// ── Puzzle ─────────────────────────────────────────────────────────────

/// Puzzle-shaped session add-on. `None` on the [`Session`] is the default
/// free-form ("scientist") session with no objective; `Some` is a loaded
/// campaign/intro puzzle. Populated from the puzzle TOML on a puzzle load.
///
/// `start_energy` / `completion_energy` are the objective energies handed
/// to the GUI (the same numbers, in the same units, that the puzzle TOML
/// supplies). `bubbles` / `current_bubble` carry the tutorial sequence and
/// its cursor; they move together — a puzzle with a tutorial sequence is
/// `bubbles: Some(seq)` + `current_bubble: Some(0)`, and a puzzle with no
/// sequence is both `None`.
#[derive(Debug)]
pub struct Puzzle {
    pub id: u32,
    pub start_energy: f64,
    pub completion_energy: f64,
    pub bubbles: Option<Vec<crate::puzzle::Bubble>>,
    pub current_bubble: Option<usize>,
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
    /// Ambient residue selection, keyed by entity. A first-class scene
    /// field beside `history`, but *not* history-versioned: undo / redo /
    /// jump leave it untouched. Empty inner sets are never stored —
    /// removing the last residue on an entity removes the entity entry,
    /// so iterating yields only entities that currently have at least one
    /// selected residue. [`Self::reset`] clears it on a topology swap.
    selection: BTreeMap<EntityId, BTreeSet<u32>>,
    /// Ambient session focus (Tab-cycle target), a first-class scene
    /// field beside `selection`. Not history-versioned: undo / redo / jump
    /// leave it untouched. [`Self::reset`] returns it to [`Focus::All`] on
    /// a topology swap. viso keeps a mirror for camera framing only (focus
    /// drives no GPU highlight); the `App` tick pushes the mirror on each
    /// [`SessionUpdate::FocusChanged`].
    focus: Focus,
    /// Display title for the current session: the file stem on a free-form
    /// load, the puzzle name on a puzzle load. Plain session state derived
    /// from the load source; never empty in practice (a structure with no
    /// derivable name gets `"Unknown"` at create time). [`Self::reset`]
    /// leaves it untouched — the following load's create seam
    /// ([`Self::start`]) overwrites it.
    title: String,
    /// Puzzle-shaped session state. `None` is the default free-form
    /// ("scientist") session; `Some` is a loaded campaign/intro puzzle
    /// carrying its objective energies and tutorial-bubble cursor. Ambient
    /// session state, not history-versioned; [`Self::reset`] clears it on a
    /// topology swap. Installing or clearing the objective emits
    /// [`SessionUpdate::PuzzleChanged`]; stepping the bubble cursor emits
    /// [`SessionUpdate::BubbleChanged`].
    puzzle: Option<Puzzle>,
    /// Active view options (render settings). Ambient session state, not
    /// history-versioned; the source of truth for what viso renders. The
    /// `App` tick applies these to the engine on every
    /// [`SessionUpdate::ViewOptionsChanged`]. [`Self::reset`] returns them to
    /// [`VisoOptions::default`] on a topology swap (view options reset per
    /// session). Holding `VisoOptions` directly relaxes the otherwise
    /// viso-free `Session` boundary for this one field.
    view_options: VisoOptions,
    /// Name of the preset whose options are currently loaded, or `None` when
    /// the active options were set manually (a manual edit no longer matches
    /// any preset) or at startup. Ambient session state; [`Self::reset`]
    /// clears it.
    active_preset: Option<String>,
    /// Score-term weight map (`term_name -> weight`) core multiplies the
    /// plugin's raw per-term energies by to produce the weighted total +
    /// per-residue scalars. Session-lifetime ambient state, not
    /// history-versioned and never on the `SessionUpdate` stream: it changes
    /// only at load, before the first score, so no consumer needs a change
    /// signal. Default empty; the App loads `ref2015_cart` into it once at
    /// init. [`Self::reset`] leaves it untouched (the `title` pattern): a
    /// reload re-sets it via the same init seam, so it carries across swaps.
    term_weights: std::collections::HashMap<String, f32>,
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
            selection: BTreeMap::new(),
            focus: Focus::default(),
            title: "Unknown".to_string(),
            puzzle: None,
            view_options: VisoOptions::default(),
            active_preset: None,
            term_weights: std::collections::HashMap::new(),
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

    // ── Action lifecycle (G6: typed mutation intent) ──────────────────

    /// Begin a streaming action over `entities` under the caller-supplied
    /// `request_id` (allocated by the orchestrator, the single id
    /// authority). Opens one tentative lane per entity, each forked from
    /// its own current lane head. Opens the edit under `request_id` (the
    /// caller already holds it). A single-entity action passes a
    /// one-element set; the multi-entity post-Init normalization passes
    /// the full touched set. Refused if any named entity has no committed
    /// lane or already holds an open tentative.
    pub fn begin_action(
        &mut self,
        entities: impl IntoIterator<Item = EntityId>,
        kind: CheckpointKind,
        label: impl Into<Cow<'static, str>>,
        request_id: u64,
    ) -> Result<(), SessionError> {
        self.history
            .begin_action(entities, kind, label.into(), request_id)?;
        Ok(())
    }

    /// Per-cycle update of the in-flight action. Mutates the tentative
    /// snapshot's payload via `Arc::make_mut` and updates the tentative
    /// checkpoint's score / filter status. Bumps `live_version` only
    /// (no DAG topology change).
    ///
    /// Emits one tentative [`SessionUpdate::Edit`] carrying the locked
    /// entity's post-mutation coordinates. The runner projector skips
    /// tentative edits (plugins don't see live frames); it completes the
    /// `SessionUpdate` stream for the render projector.
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
        // `SessionUpdate` stream and rebuilds from `head_assembly`. Payload-less because
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
    /// score write: updates the head checkpoint, bumps the History's
    /// `live_version` (the history panel's live cursor), and emits one
    /// [`SessionUpdate::ScoresChanged`] when a value was actually written
    /// (the GUI score-widget channel). Plugins compute their own scores
    /// and never observe this signal.
    pub fn set_head_scores(&mut self, raw_score: Option<f64>, game_score: Option<f64>) {
        if self.history.set_head_scores(raw_score, game_score) {
            self.apply(SessionUpdate::ScoresChanged);
        }
    }

    /// Stamp a composition score on the open edit `request_id`. Targets the
    /// named edit so two concurrent edits' scores never collide; the score
    /// transfers onto the checkpoint that edit mints at commit. Emits one
    /// [`SessionUpdate::ScoresChanged`] when a value was actually written.
    pub fn set_edit_scores(
        &mut self,
        request_id: u64,
        raw_score: Option<f64>,
        game_score: Option<f64>,
    ) {
        if self.history.set_edit_scores(request_id, raw_score, game_score) {
            self.apply(SessionUpdate::ScoresChanged);
        }
    }

    /// Stamp a composition score on the committed checkpoint `id` (the
    /// commit-time stamp once the reply for its composed union returns).
    /// Emits one [`SessionUpdate::ScoresChanged`] when a value was actually
    /// written.
    pub fn set_checkpoint_scores(
        &mut self,
        id: CheckpointId,
        raw_score: Option<f64>,
        game_score: Option<f64>,
    ) {
        if self.history.set_checkpoint_scores(id, raw_score, game_score) {
            self.apply(SessionUpdate::ScoresChanged);
        }
    }

    /// Read the `(raw, game)` score of the current composition node (first
    /// open pending edit if any, else the committed head). The live-score
    /// read surface for the score widget.
    #[must_use]
    pub fn current_composition_scores(&self) -> (Option<f64>, Option<f64>) {
        self.history.current_composition_scores()
    }

    /// The request ids of every open edit, in insertion order.
    pub fn pending_request_ids(&self) -> impl Iterator<Item = u64> + '_ {
        self.history.pending_request_ids()
    }

    /// The lone open edit's request id, or `None` if zero or >1 edits are
    /// open.
    #[must_use]
    pub fn sole_pending_request_id(&self) -> Option<u64> {
        self.history.sole_pending_request_id()
    }

    /// Build the assembly composing the open edit `request_id` (its
    /// tentative lanes over its peers' committed heads), for a composition
    /// score targeted at that edit. `None` if `request_id` names no open
    /// edit.
    #[must_use]
    pub fn edit_composition_assembly(&self, request_id: u64) -> Option<Assembly> {
        self.history
            .edit_composition_entities(request_id)
            .map(Assembly::from_arcs)
    }

    /// Build the assembly composing committed checkpoint `id` (its
    /// `entity_heads`), for a commit-time composition score. `None` if `id`
    /// is unknown.
    #[must_use]
    pub fn checkpoint_assembly(&self, id: CheckpointId) -> Option<Assembly> {
        self.history
            .checkpoint_composition_entities(id)
            .map(Assembly::from_arcs)
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

    // ── Selection ─────────────────────────────────────────────────────
    //
    // Ambient residue selection (not history-versioned). Invariant
    // maintained across every mutator: per-entity sets are never left
    // empty in the outer map. Removing the last residue on an entity
    // removes the entity entry, so `selected_entities` yields only
    // entities that currently have at least one selected residue. Each
    // mutator that changes the selection emits exactly one
    // [`SessionUpdate::SelectionChanged`] through the funnel.

    /// The current residue selection, keyed by entity. Empty inner sets
    /// are never present (see the invariant above), so every entry
    /// carries at least one residue.
    #[must_use]
    pub fn selection(&self) -> &BTreeMap<EntityId, BTreeSet<u32>> {
        &self.selection
    }

    /// Mark a single residue on `entity` as selected. Idempotent:
    /// re-selecting an already-selected residue is a no-op (but still
    /// emits, matching the prior behavior where the mutator raised its
    /// dirty bits unconditionally).
    pub fn select_residue(&mut self, entity: EntityId, residue_index: u32) {
        self.selection
            .entry(entity)
            .or_default()
            .insert(residue_index);
        self.apply(SessionUpdate::SelectionChanged);
    }

    /// Mark a single residue on `entity` as deselected. Idempotent on
    /// already-empty state. If this empties the per-entity set, the
    /// entity entry is removed from the outer map (sets are never
    /// left empty).
    pub fn deselect_residue(&mut self, entity: EntityId, residue_index: u32) {
        if let Some(set) = self.selection.get_mut(&entity) {
            set.remove(&residue_index);
            if set.is_empty() {
                self.selection.remove(&entity);
            }
        }
        self.apply(SessionUpdate::SelectionChanged);
    }

    /// Bulk-replace the selection on a single entity. The provided
    /// residues become the entity's full set (not merged into the
    /// existing one). An empty input removes the entity entry.
    pub fn set_residues_on(
        &mut self,
        entity: EntityId,
        residues: impl IntoIterator<Item = u32>,
    ) {
        let set: BTreeSet<u32> = residues.into_iter().collect();
        if set.is_empty() {
            self.selection.remove(&entity);
        } else {
            self.selection.insert(entity, set);
        }
        self.apply(SessionUpdate::SelectionChanged);
    }

    /// Drop the entire selection across all entities.
    pub fn clear_selection(&mut self) {
        self.selection.clear();
        self.apply(SessionUpdate::SelectionChanged);
    }

    /// Flip the selected state of `(entity, residue_index)` and return
    /// the new state (`true` if now selected, `false` if now
    /// deselected). Maintains the empty-set-removal invariant.
    pub fn toggle_residue(&mut self, entity: EntityId, residue_index: u32) -> bool {
        let set = self.selection.entry(entity).or_default();
        let now_selected = set.insert(residue_index);
        if !now_selected {
            set.remove(&residue_index);
            if set.is_empty() {
                self.selection.remove(&entity);
            }
        }
        self.apply(SessionUpdate::SelectionChanged);
        now_selected
    }

    /// Selected residues on a specific entity, or `None` if the entity
    /// has no selection. Sets are never empty by invariant, so
    /// `Some(_)` always carries at least one residue.
    #[must_use]
    pub fn selected_residues_on(&self, entity: EntityId) -> Option<&BTreeSet<u32>> {
        self.selection.get(&entity)
    }

    /// Point-query: is `(entity, residue_index)` selected?
    #[must_use]
    pub fn is_residue_selected(&self, entity: EntityId, residue_index: u32) -> bool {
        self.selection
            .get(&entity)
            .is_some_and(|set| set.contains(&residue_index))
    }

    /// Iterator over the entities that currently have at least one
    /// residue selected. Order is `BTreeMap`'s natural key order.
    pub fn selected_entities(&self) -> impl Iterator<Item = EntityId> + '_ {
        self.selection.keys().copied()
    }

    /// True when no residue is selected on any entity.
    #[must_use]
    pub fn selection_is_empty(&self) -> bool {
        self.selection.is_empty()
    }

    /// Total number of selected residues across all entities (sum of
    /// per-entity set sizes).
    #[must_use]
    pub fn selection_total_count(&self) -> usize {
        self.selection.values().map(|set| set.len()).sum()
    }

    // ── Focus ─────────────────────────────────────────────────────────
    //
    // Ambient session focus (not history-versioned). Mutating it emits
    // exactly one [`SessionUpdate::FocusChanged`], and only when the value
    // actually changes — an idempotent re-focus is silent.

    /// The current session focus.
    #[must_use]
    pub fn focus(&self) -> Focus {
        self.focus
    }

    /// Set the session focus. Emits exactly one
    /// [`SessionUpdate::FocusChanged`] when the value changes; an
    /// idempotent re-focus emits nothing.
    pub fn set_focus(&mut self, focus: Focus) {
        if self.focus != focus {
            self.focus = focus;
            self.apply(SessionUpdate::FocusChanged);
        }
    }

    // ── Session title ─────────────────────────────────────────────────

    /// Display title for the current session (file stem on a free-form
    /// load, puzzle name on a puzzle load). Always a real string; set by
    /// the create seam ([`Self::start`]).
    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }

    // ── Puzzle objective + tutorial bubbles ───────────────────────────
    //
    // Ambient session state (not history-versioned). The puzzle add-on
    // carries the objective energies and the tutorial-bubble cursor. A
    // puzzle load installs it; a free-form load clears it. Installing or
    // clearing the objective emits exactly one
    // [`SessionUpdate::PuzzleChanged`]; stepping the bubble cursor emits
    // exactly one [`SessionUpdate::BubbleChanged`].

    /// The loaded puzzle, or `None` in the default free-form session.
    #[must_use]
    pub fn puzzle(&self) -> Option<&Puzzle> {
        self.puzzle.as_ref()
    }

    /// Install a puzzle objective (a puzzle load). Always emits
    /// [`SessionUpdate::PuzzleChanged`].
    pub fn set_puzzle(&mut self, puzzle: Puzzle) {
        self.puzzle = Some(puzzle);
        self.apply(SessionUpdate::PuzzleChanged);
    }

    /// Drop the puzzle objective and revert to the free-form session (a
    /// free-form structure load). Emits [`SessionUpdate::PuzzleChanged`]
    /// only when there was a puzzle to clear.
    pub fn clear_puzzle(&mut self) {
        let changed = self.puzzle.is_some();
        self.puzzle = None;
        if changed {
            self.apply(SessionUpdate::PuzzleChanged);
        }
    }

    /// Begin a session over a freshly-loaded structure: install its display
    /// `title` and `puzzle` add-on in one funnel. `puzzle` is `Some` for a
    /// campaign/intro puzzle load (carrying its objective + tutorial
    /// bubbles) and `None` for a free-form structure load. The single
    /// create seam every load path routes through. The `PuzzleChanged`
    /// comes from the inner [`Self::set_puzzle`] / [`Self::clear_puzzle`];
    /// a title-only change (a free-form reload that leaves `puzzle` `None`)
    /// is silent here, so its caller raises the puzzle-panel refresh
    /// explicitly.
    pub fn start(&mut self, title: String, puzzle: Option<Puzzle>) {
        self.title = title;
        match puzzle {
            Some(p) => self.set_puzzle(p),
            None => self.clear_puzzle(),
        }
    }

    /// Step the tutorial-bubble cursor of the active puzzle. No-op when no
    /// puzzle is loaded or the puzzle carries no tutorial sequence. Forward
    /// saturates at the sequence length (one past the last bubble; the GUI
    /// then shows no bubble); back saturates at 0. Emits
    /// [`SessionUpdate::BubbleChanged`] only when the cursor actually moves
    /// — a step at either clamp is silent.
    pub fn advance_bubble(&mut self, back: bool) {
        let Some(puzzle) = self.puzzle.as_mut() else {
            return;
        };
        let Some(cursor) = puzzle.current_bubble else {
            return;
        };
        let len = puzzle.bubbles.as_ref().map_or(0, Vec::len);
        let next = if back {
            cursor.saturating_sub(1)
        } else if cursor < len {
            cursor + 1
        } else {
            cursor
        };
        if next == cursor {
            return;
        }
        puzzle.current_bubble = Some(next);
        self.apply(SessionUpdate::BubbleChanged);
    }

    // ── View options + active preset ──────────────────────────────────
    //
    // Ambient session state (not history-versioned). The active options are
    // the source of truth for what viso renders; the `App` tick applies
    // them to the engine on each [`SessionUpdate::ViewOptionsChanged`]. A
    // manual option edit clears the active preset (the options no longer
    // match a named preset); applying a preset sets both together. Each
    // mutator emits exactly one `ViewOptionsChanged`, and only when
    // something actually changes.

    /// The active view options.
    #[must_use]
    pub fn view_options(&self) -> &VisoOptions {
        &self.view_options
    }

    /// The name of the currently-loaded preset, or `None` when the active
    /// options were set manually.
    #[must_use]
    pub fn active_preset(&self) -> Option<&str> {
        self.active_preset.as_deref()
    }

    /// Set the active view options (a manual edit). Clears the active preset
    /// — manually-set options no longer match any named preset. Emits
    /// [`SessionUpdate::ViewOptionsChanged`] when the options or the active
    /// preset actually change; an idempotent set emits nothing.
    pub fn set_view_options(&mut self, options: VisoOptions) {
        let changed = self.view_options != options || self.active_preset.is_some();
        self.view_options = options;
        self.active_preset = None;
        if changed {
            self.apply(SessionUpdate::ViewOptionsChanged);
        }
    }

    /// Apply a named preset: install its `options` and record `name` as the
    /// active preset. Emits [`SessionUpdate::ViewOptionsChanged`] when the
    /// options or the active preset actually change.
    pub fn apply_preset(&mut self, name: String, options: VisoOptions) {
        let changed =
            self.view_options != options || self.active_preset.as_deref() != Some(name.as_str());
        self.view_options = options;
        self.active_preset = Some(name);
        if changed {
            self.apply(SessionUpdate::ViewOptionsChanged);
        }
    }

    // ── Score-term weights ────────────────────────────────────────────

    /// The active score-term weight map core multiplies raw per-term
    /// energies by. Empty until the App loads the default at init.
    #[must_use]
    pub fn term_weights(&self) -> &std::collections::HashMap<String, f32> {
        &self.term_weights
    }

    /// Install the score-term weight map. Silent (no `SessionUpdate`): the
    /// weights change only at load, before the first score, so no consumer
    /// needs a change signal. Called once at App init; survives reloads
    /// because [`Self::reset`] leaves `term_weights` untouched.
    pub fn set_term_weights(&mut self, weights: std::collections::HashMap<String, f32>) {
        self.term_weights = weights;
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
        // Selection keys are entity ids from the outgoing assembly; the
        // incoming one may reuse those ids without referring to the same
        // entities (the allocator restarts), so a stale selection must be
        // dropped on every topology swap. Co-located here so both load
        // paths (puzzle + free-form structure) clear it.
        self.selection.clear();
        // Focus is ambient session state; a topology swap returns it to
        // the all-entities view. Set silently (no `FocusChanged`): viso
        // independently resets its mirror to `All` on the assembly
        // replace, and the reset's `HeadMoved` below drives the reframe.
        self.focus = Focus::default();
        // The puzzle add-on (objective + tutorial bubbles) is ambient
        // session state tied to the outgoing structure. Clear it silently
        // here (the load path that follows a reset re-installs it via the
        // `start` create seam, whose `PuzzleChanged` drives the panel); the
        // reset's own `HeadMoved` below stands in for the topology swap.
        // `title` is left untouched: the following load's `start` overwrites
        // it, and nothing reads it between the reset and that overwrite.
        // `term_weights` is likewise left untouched: the load re-sets it via
        // the App-init seam, so it carries across the topology swap.
        self.puzzle = None;
        // View options + active preset are ambient session state; a topology
        // swap resets both to defaults (view options reset per session, not
        // persist). Unlike focus (which viso re-derives on the assembly
        // replace), nothing pushes default options to the engine on its own,
        // so this emits `ViewOptionsChanged` below when there was a non-
        // default state to clear, driving the tick's `set_options` + the
        // GUI panel refresh. A load path that wants a preset then re-applies
        // it via `apply_preset`, whose own emit reads the latest options.
        let view_changed =
            self.view_options != VisoOptions::default() || self.active_preset.is_some();
        self.view_options = VisoOptions::default();
        self.active_preset = None;
        // Drop any changes emitted before the reset — they describe state
        // that no longer exists. Cleared BEFORE the reset's own emit below
        // so that change survives. The runner projector's published snapshot is
        // intentionally NOT cleared (it lives on `RunnerProjector`): the
        // post-reset empty-assembly diff still advances the host's gen
        // counter, so plugins never see `from_gen` go backwards.
        self.pending_updates.clear();
        if view_changed {
            self.apply(SessionUpdate::ViewOptionsChanged);
        }
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
