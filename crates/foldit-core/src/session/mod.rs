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
//!
//! The mutating surface (every `&mut self` shim) lives in the sibling
//! `mutators` child module; this file holds the struct, its construction,
//! the read accessors, and the backend helpers.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;

use indexmap::IndexMap;
use molex::entity::molecule::id::{EntityId, EntityIdAllocator};
use molex::{Assembly, MoleculeEntity, MoleculeType};
use viso::Focus;
use viso::options::VisoOptions;

use crate::history::{CheckpointId, History, HistoryError};

mod apply;
mod change;
pub use change::SessionUpdate;
pub(crate) use change::SessionUpdateConsumer;
mod metadata;
pub use metadata::{EntityMetadata, EntityOrigin};
mod mutators;

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
    /// Score-term name list (the alignment key for every stored breakdown's
    /// `whole_pose_terms` and each residue's `terms`). Session-lifetime
    /// ambient state, like [`Self::term_weights`]: it changes only when a
    /// score report lands (the App re-sets it from each report, idempotent),
    /// and [`Self::reset`] leaves it untouched so the next session's first
    /// score overwrites it. Lives once on the session rather than being
    /// duplicated on every checkpoint's breakdown.
    term_names: Vec<String>,
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
            term_names: Vec::new(),
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

    // ── Score + composition reads ─────────────────────────────────────

    /// Read the `(raw, game)` score of the current composition node (first
    /// open pending edit if any, else the committed head). The live-score
    /// read surface for the score widget.
    #[must_use]
    pub fn current_composition_scores(&self) -> (Option<f64>, Option<f64>) {
        self.history.current_composition_scores()
    }

    /// The RAW per-term breakdown of the current composition node (first
    /// open pending edit if any, else the committed head). The render
    /// projector re-derives the displayed per-residue colors from it ×
    /// [`Self::term_weights`] (zipping [`Self::term_names`]) on every
    /// `ScoresChanged`. `None` until a score with a breakdown is stamped.
    #[must_use]
    pub fn current_composition_breakdown(&self) -> Option<&crate::scores::StoredBreakdown> {
        self.history.current_composition_breakdown()
    }

    /// Read the score for the *current composition node* (the open pending
    /// edit when an action is in flight, else the committed head checkpoint),
    /// projected through the active scoring mode. Following the composition
    /// node keeps the displayed score on an in-flight action's streamed score
    /// without ever reading the committed parent (G1: derive, don't store).
    pub(crate) fn display_score(&self) -> Option<f64> {
        let (raw, game) = self.current_composition_scores();
        // A loaded puzzle displays the foldit game score; the free-form
        // session displays the raw Rosetta score.
        if self.puzzle().is_some() {
            game
        } else {
            raw
        }
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

    // ── Selection reads ───────────────────────────────────────────────
    //
    // Ambient residue selection (not history-versioned); the mutators live
    // in [`mutators`]. Invariant maintained across every mutator: per-entity
    // sets are never left empty in the outer map, so `selected_entities`
    // yields only entities that currently have at least one selected residue.

    /// The current residue selection, keyed by entity. Empty inner sets
    /// are never present (see the invariant above), so every entry
    /// carries at least one residue.
    #[must_use]
    pub fn selection(&self) -> &BTreeMap<EntityId, BTreeSet<u32>> {
        &self.selection
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

    // ── Focus read ────────────────────────────────────────────────────

    /// The current session focus.
    #[must_use]
    pub fn focus(&self) -> Focus {
        self.focus
    }

    // ── Session title ─────────────────────────────────────────────────

    /// Display title for the current session (file stem on a free-form
    /// load, puzzle name on a puzzle load). Always a real string; set by
    /// the create seam ([`Self::start`]).
    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }

    // ── Puzzle read ───────────────────────────────────────────────────

    /// The loaded puzzle, or `None` in the default free-form session.
    #[must_use]
    pub fn puzzle(&self) -> Option<&Puzzle> {
        self.puzzle.as_ref()
    }

    // ── View options + active preset reads ────────────────────────────

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

    // ── Score-term weight reads ───────────────────────────────────────

    /// The active score-term weight map core multiplies raw per-term
    /// energies by. Empty until the App loads the default at init.
    #[must_use]
    pub fn term_weights(&self) -> &std::collections::HashMap<String, f32> {
        &self.term_weights
    }

    /// The score-term name list (alignment key for every stored breakdown).
    /// Empty until the first score report lands.
    #[must_use]
    pub fn term_names(&self) -> &[String] {
        &self.term_names
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
