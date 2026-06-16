//! Authoritative document atop the two-layer [`History`].
//!
//! `Session` owns:
//! - [`History`] - the full per-entity timelines + checkpoint graph.
//! - `transient: IndexMap<EntityId, Arc<MoleculeEntity>>` - preview /
//!   scene-resident entities that are visible in [`Self::head_assembly`]
//!   but absent from every checkpoint. Presence in this map *is* the
//!   preview signal; the old [`EntityMetadata::is_preview`] flag is
//!   gone.
//! - `metadata: IndexMap<EntityId, Arc<EntityMetadata>>` - per-entity
//!   metadata (name, origin).
//!   `Arc`-shared so unchanged entries stay aliased across history
//!   operations (no metadata serialization on every mutation).
//!
//! Mutation intent is in the type signature: three explicit
//! categories - history-bearing actions, metadata-only edits, and
//! one-shot transient previews - with no neutral default. Adding a new
//! mutation requires choosing one.
//!
//! There is no `mutate(closure)`-style API. Every checkpoint-bearing
//! event funnels through `History::record` via a thin shim
//! here; the single-root invariant is preserved end to end.
//!
//! **Emit invariant.** Every public mutator is a shim: it performs its
//! state change, then emits exactly one [`SessionUpdate`] (or none, where
//! the change is unobservable) through the [`Self::apply`] funnel. The
//! `Session` holds no projection logic - it neither serializes
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
use molex::{Assembly, MoleculeEntity};
use viso::Focus;

use crate::history::{CheckpointId, History, HistoryError};

mod apply;
mod change;
pub use change::SessionUpdate;
pub use change::SessionUpdateConsumer;
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
/// free-form ("scientist") session with no puzzle goal; `Some` is a loaded
/// campaign/intro puzzle. Populated from the puzzle TOML on a puzzle load.
///
/// `start_energy` / `completion_energy` are the target energies handed
/// to the GUI (the same numbers, in the same units, that the puzzle TOML
/// supplies). `bubbles` / `current_bubble` carry the tutorial sequence and
/// its cursor; they move together - a puzzle with a tutorial sequence is
/// `bubbles: Some(seq)` + `current_bubble: Some(0)`, and a puzzle with no
/// sequence is both `None`.
#[derive(Debug)]
pub struct Puzzle {
    pub id: u32,
    pub start_energy: f64,
    pub completion_energy: f64,
    /// Optional per-puzzle scorefunction weight patch (`scoretype_name ->
    /// weight`) from the puzzle TOML's `[puzzle.weights]` table. `None` when
    /// the puzzle declares no patch. The host overlays it onto its display
    /// weight map at load, and threads it to the bridge through the normalize
    /// dispatch params so the patched terms ship and are optimized against.
    pub weight_patch: Option<std::collections::HashMap<String, f32>>,
    /// Scored filters from the puzzle TOML's `[[puzzle.filter]]` tables.
    /// Empty when the puzzle declares none. The native `ExposedCount`
    /// filter is evaluated by the exposed-hydro coordinator, whose met-bonus
    /// breakdown is stored on [`Session::filter_bonus`] and folded into the
    /// headline game score.
    pub filters: Vec<crate::puzzle::FilterSpec>,
    pub bubbles: Option<Vec<crate::puzzle::Bubble>>,
    pub current_bubble: Option<usize>,
    /// Catalytic constraints parsed from the puzzle's `.cnstr` file. Empty
    /// when the puzzle declares none. Carried here so the session-init kick
    /// can deliver them to the worker.
    pub constraints: Vec<crate::puzzle_setup::Constraint>,
    /// Ligand asset bytes read from the puzzle dir. Empty for protein-only
    /// puzzles. Carried here so the session-init kick can deliver them to the
    /// worker.
    pub ligands: Vec<crate::puzzle::LigandAsset>,
    /// Per-entity design gating, keyed by the loaded `EntityId`. `None` means
    /// the puzzle declares no gating (e.g. a free-edit fold puzzle); `Some`
    /// carries one mask per designable entity (resolved from the puzzle TOML's
    /// per-chain masks at load). The query is secure-by-default: an entity
    /// absent from the map - or any residue outside its mask - is not
    /// designable, so the ligand (and any chain without an entry) is locked.
    pub design_gating: Option<BTreeMap<EntityId, crate::puzzle_setup::DesignMask>>,
}

impl Puzzle {
    /// Whether residue `res` on `entity` may be designed (mutated).
    ///
    /// Secure-by-default: `None` gating answers `false` for every residue,
    /// and an entity absent from the gating map - or a residue outside its
    /// mask - is not designable.
    #[must_use]
    pub fn is_designable(&self, entity: EntityId, res: u32) -> bool {
        self.design_gating
            .as_ref()
            .is_some_and(|map| map.get(&entity).is_some_and(|m| m.is_designable(res)))
    }
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
    /// jump leave it untouched. Empty inner sets are never stored -
    /// removing the last residue on an entity removes the entity entry,
    /// so iterating yields only entities that currently have at least one
    /// selected residue. [`Self::reset`] clears it on a topology swap.
    selection: BTreeMap<EntityId, BTreeSet<u32>>,
    /// Ambient per-entity render overrides, keyed by entity. A first-class
    /// scene field beside `selection`, and likewise *not* history-versioned:
    /// undo / redo / jump leave it untouched. Authoritative and
    /// session-scoped (entity ids are session-specific and reused across
    /// puzzles), so [`Self::reset`] clears it on a topology swap for id-reuse
    /// safety. The render projector reads it via [`Self::appearance`] and
    /// pushes it into the viso engine, which holds the resolved working copy
    /// the GUI reads back. Empty override entries are never stored - merging
    /// a field that leaves an entry empty removes the entity entry.
    appearance: BTreeMap<EntityId, viso::DisplayOverrides>,
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
    /// leaves it untouched - the following load's create seam
    /// ([`Self::start`]) overwrites it.
    title: String,
    /// Puzzle-shaped session state. `None` is the default free-form
    /// ("scientist") session; `Some` is a loaded campaign/intro puzzle
    /// carrying its target energies and tutorial-bubble cursor. Ambient
    /// session state, not history-versioned; [`Self::reset`] clears it on a
    /// topology swap. Installing or clearing the puzzle emits
    /// [`SessionUpdate::PuzzleChanged`]; stepping the bubble cursor emits
    /// [`SessionUpdate::BubbleChanged`].
    puzzle: Option<Puzzle>,
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
    /// Labeled breakdown of the RAW score bonus from the loaded puzzle's met
    /// filters: each entry is `(filter label, bonus value)` in RAW score
    /// units (e.g. the native `ExposedCount` filter contributes
    /// `("exposed_count", bonus)` when the exposed-hydrophobic count is below
    /// its threshold). The summed total is a RAW delta folded into the headline game
    /// score before the raw->game map, so a met filter can push the displayed
    /// score across `completion_score`; the breakdown is also surfaced to the
    /// dev readout. Ambient session state, not history-versioned and never on
    /// the `SessionUpdate` stream: the exposed-hydro coordinator recomputes it
    /// at rest each geometry change. Empty by default; [`Self::reset`] clears
    /// it on a topology swap (a new puzzle re-derives it from its own filters).
    filter_bonus: Vec<(String, f64)>,
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
            appearance: BTreeMap::new(),
            focus: Focus::default(),
            title: "Unknown".to_owned(),
            puzzle: None,
            term_weights: std::collections::HashMap::new(),
            term_names: Vec::new(),
            filter_bonus: Vec::new(),
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
    pub const fn history(&self) -> &History {
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
        self.transient.get(&id).map(std::convert::AsRef::as_ref)
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

    /// Whether any edit is currently open.
    #[must_use]
    pub(crate) fn has_pending(&self) -> bool {
        self.history.has_pending()
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
    /// without ever reading the committed parent (derive, don't store).
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
    pub const fn selection(&self) -> &BTreeMap<EntityId, BTreeSet<u32>> {
        &self.selection
    }

    /// The current per-entity appearance overrides, keyed by entity. Empty
    /// override entries are never present (merging a field that leaves an
    /// entry empty removes it), so every entry carries at least one set
    /// field. The render projector reads this and reconciles the engine's
    /// working copy against it.
    #[must_use]
    pub const fn appearance(&self) -> &BTreeMap<EntityId, viso::DisplayOverrides> {
        &self.appearance
    }

    /// Selected residues on a specific entity, or `None` if the entity
    /// has no selection. Sets are never empty by invariant, so
    /// `Some(_)` always carries at least one residue. Selection query
    /// API; currently only exercised by tests.
    #[allow(dead_code)]
    #[must_use]
    pub fn selected_residues_on(&self, entity: EntityId) -> Option<&BTreeSet<u32>> {
        self.selection.get(&entity)
    }

    /// Point-query: is `(entity, residue_index)` selected? Selection
    /// query API; currently only exercised by tests.
    #[allow(dead_code)]
    #[must_use]
    pub fn is_residue_selected(&self, entity: EntityId, residue_index: u32) -> bool {
        self.selection
            .get(&entity)
            .is_some_and(|set| set.contains(&residue_index))
    }

    /// Iterator over the entities that currently have at least one
    /// residue selected. Order is `BTreeMap`'s natural key order.
    /// Selection query API; currently only exercised by tests.
    #[allow(dead_code)]
    pub fn selected_entities(&self) -> impl Iterator<Item = EntityId> + '_ {
        self.selection.keys().copied()
    }

    /// True when no residue is selected on any entity. Selection query
    /// API; currently only exercised by tests.
    #[allow(dead_code)]
    #[must_use]
    pub fn selection_is_empty(&self) -> bool {
        self.selection.is_empty()
    }

    /// Total number of selected residues across all entities (sum of
    /// per-entity set sizes).
    #[must_use]
    pub fn selection_total_count(&self) -> usize {
        self.selection.values().map(std::collections::BTreeSet::len).sum()
    }

    // ── Focus read ────────────────────────────────────────────────────

    /// The current session focus.
    #[must_use]
    pub const fn focus(&self) -> Focus {
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
    pub const fn puzzle(&self) -> Option<&Puzzle> {
        self.puzzle.as_ref()
    }

    /// Whether residue `res` on `entity` may be designed (mutated) in the
    /// current session. Secure-by-default: a free-form session (no puzzle),
    /// a puzzle with no design gating, or an entity/residue outside the mask
    /// all answer `false`. Forwards to [`Puzzle::is_designable`].
    #[must_use]
    pub fn is_designable(&self, entity: EntityId, res: u32) -> bool {
        self.puzzle
            .as_ref()
            .is_some_and(|p| p.is_designable(entity, res))
    }

    /// Whether the current focus-scoped selection is fully designable.
    ///
    /// Mirrors the runner's selection-spec focus scoping: with
    /// [`Focus::Entity`] only that entity's selected residues are checked;
    /// with [`Focus::All`] every selected `(entity, residue)` is checked.
    /// A design-gated action is enabled only when this holds. The empty
    /// selection is vacuously designable (the action's `selection_spec`
    /// min-residues gate handles the empty case separately).
    ///
    /// Secure-by-default through [`Self::is_designable`]: any residue in
    /// scope that is not designable (including a free-form session or an
    /// ungated puzzle) makes the result `false`.
    #[must_use]
    pub fn selection_is_designable(&self) -> bool {
        match self.focus {
            Focus::Entity(eid) => self
                .selection
                .get(&eid)
                .into_iter()
                .flatten()
                .all(|&res| self.is_designable(eid, res)),
            Focus::All => self.selection.iter().all(|(&eid, residues)| {
                residues.iter().all(|&res| self.is_designable(eid, res))
            }),
        }
    }

    /// Whether the loaded puzzle gates design per entity (its `design_gating`
    /// is `Some`). Distinguishes a design puzzle (overlay the lock visuals)
    /// from a free-edit fold puzzle or free-form session (no gating). Consumed
    /// by the design overlay (a later pass); currently only exercised by tests.
    #[allow(dead_code)]
    #[must_use]
    pub fn design_gating_active(&self) -> bool {
        self.puzzle
            .as_ref()
            .is_some_and(|p| p.design_gating.is_some())
    }

    /// The labeled breakdown of the RAW score bonus from the loaded puzzle's
    /// met filters, each entry `(filter label, bonus value)`. Empty in a
    /// free-form session or when no filter is met. The dev readout lists it;
    /// the score path folds [`Self::filter_bonus_total`] into the raw value
    /// before the raw->game map.
    #[must_use]
    pub fn filter_bonus(&self) -> &[(String, f64)] {
        &self.filter_bonus
    }

    /// The summed RAW score bonus across every met filter. `0.0` in a
    /// free-form session or when no filter is met. The score path folds this
    /// into the raw value before the raw->game map.
    #[must_use]
    pub fn filter_bonus_total(&self) -> f64 {
        self.filter_bonus.iter().map(|(_, v)| v).sum()
    }

    /// Replace the met-filter RAW bonus breakdown. Silent (no `SessionUpdate`):
    /// it rides the `ScoresChanged` that the score write following it emits.
    /// Recomputed by the exposed-hydro coordinator at rest each geometry
    /// change; [`Self::reset`] clears it on a topology swap.
    pub fn set_filter_bonus(&mut self, breakdown: Vec<(String, f64)>) {
        self.filter_bonus = breakdown;
    }

    // ── Score-term weight reads ───────────────────────────────────────

    /// The active score-term weight map core multiplies raw per-term
    /// energies by. Empty until the App loads the default at init.
    #[must_use]
    pub const fn term_weights(&self) -> &std::collections::HashMap<String, f32> {
        &self.term_weights
    }

    /// The score-term name list (alignment key for every stored breakdown).
    /// Empty until the first score report lands.
    #[must_use]
    pub fn term_names(&self) -> &[String] {
        &self.term_names
    }

}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests;
