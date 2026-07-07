//! Session is intended to encapsulate/own all App state that is tied
//! to the lifecycle of a Session (and optionally Puzzle)
//!
//! `Session` owns:
//! - [`History`] - the full per-entity timelines + checkpoint graph.
//! - `transient: IndexMap<EntityId, Arc<MoleculeEntity>>` - preview /
//!   scene-resident entities that are visible in [`Self::head_assembly`]
//!   but absent from every checkpoint. Presence in this map *is* the
//!   preview signal.
//! - `metadata: IndexMap<EntityId, Arc<str>>` - per-entity display name.
//!   `Arc`-shared so unchanged entries stay aliased across history
//!   operations.
//!

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;

use indexmap::IndexMap;

use molex::entity::molecule::id::{EntityId, EntityIdAllocator};
use molex::{Assembly, MoleculeEntity};
use viso::Focus;

mod apply;
mod change;
mod commands;
mod load;
mod mutators;
mod previews;

pub use change::{HeadMoveCause, SessionUpdate, SessionUpdateConsumer};
use previews::Previews;

use crate::history::{CheckpointId, History, HistoryError};

/// Error returned by every fallible [`Session`] operation.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// `History`-layer refusal
    #[error("{0}")]
    History(#[from] HistoryError),
    /// `id` is not currently a transient preview.
    #[error("{} is not a transient preview", id.raw())]
    NotAPreview { id: EntityId },
}

/// Puzzle-shaped session add-on. `None` on the [`Session`] is the default
/// free-form ("scientist") session with no puzzle goal; `Some` is a loaded
/// campaign/intro puzzle. Populated from the puzzle TOML on a puzzle load.
#[derive(Debug)]
pub struct Puzzle {
    pub id: u32,
    pub start_energy: f64,
    pub completion_energy: f64,
    pub weight_patch: Option<std::collections::HashMap<String, f32>>,
    pub filters: Vec<crate::puzzle_toml::FilterSpec>,
    pub bubbles: Option<Vec<crate::puzzle_toml::Bubble>>,
    pub current_bubble: Option<usize>,
    pub constraints: Vec<crate::puzzle_setup::Constraint>,
    pub ligands: Vec<crate::puzzle_load::LigandAsset>,
    pub density: Option<crate::puzzle_load::DensityAsset>,
    pub design_gating: Option<BTreeMap<EntityId, crate::puzzle_setup::DesignMask>>,
}

impl Puzzle {
    /// Whether residue `res` on `entity` may be designed (mutated).
    #[must_use]
    pub fn is_designable(&self, entity: EntityId, res: u32) -> bool {
        self.design_gating
            .as_ref()
            .is_some_and(|map| map.get(&entity).is_some_and(|m| m.is_designable(res)))
    }

    /// Install the resolved per-entity design gating.
    pub(crate) fn set_design_gating(
        &mut self,
        gating: Option<BTreeMap<EntityId, crate::puzzle_setup::DesignMask>>,
    ) {
        self.design_gating = gating;
    }

    /// Mark `entity`'s whole residue range `0..=residue_count-1` designable.
    /// A no-op unless gating is already active (`design_gating` is `Some`).
    pub(crate) fn register_full_designable_entity(
        &mut self,
        entity: EntityId,
        residue_count: usize,
    ) {
        let Some(gating) = self.design_gating.as_mut() else {
            return;
        };
        let Some(last) = residue_count.checked_sub(1) else {
            return;
        };
        let end = u32::try_from(last).unwrap_or(u32::MAX);
        gating.insert(
            entity,
            crate::puzzle_setup::DesignMask {
                ranges: vec![0..=end],
            },
        );
    }

    /// Step the tutorial-bubble cursor; returns whether it moved. Forward
    /// saturates one past the last bubble, back saturates at 0. No-op when
    /// the puzzle carries no tutorial sequence.
    pub(crate) fn advance_bubble(&mut self, back: bool) -> bool {
        let Some(cursor) = self.current_bubble else {
            return false;
        };
        let len = self.bubbles.as_ref().map_or(0, Vec::len);
        let next = if back {
            cursor.saturating_sub(1)
        } else if cursor < len {
            cursor + 1
        } else {
            cursor
        };
        if next == cursor {
            return false;
        }
        self.current_bubble = Some(next);
        true
    }
}

/// Authoritative document over the whole scene.
pub struct Session {
    /// Per-entity display name.
    metadata: IndexMap<EntityId, Arc<str>>,
    /// Preview / scene-resident entities.
    /// Visible in [`Self::head_assembly`] but absent from checkpoints.
    transient: IndexMap<EntityId, Arc<MoleculeEntity>>,
    /// Id allocator. Stable across history navigation.
    allocator: EntityIdAllocator,
    /// The full two-layer history.
    history: History,
    /// Ambient residue selection, keyed by entity.
    selection: BTreeMap<EntityId, BTreeSet<u32>>,
    /// Ambient per-entity render overrides, keyed by entity.
    appearance: BTreeMap<EntityId, viso::DisplayOverrides>,
    /// Ambient session focus (Tab-cycle target).
    focus: Focus,
    /// Display title for the current session.
    title: String,
    /// Session state specific to game or intro puzzles.
    puzzle: Option<Puzzle>,
    /// Experimental density for a free-form (no-puzzle) load, computed on a
    /// `--with-density` pdbid load. The puzzle's own `density` takes
    /// precedence when a puzzle is installed.
    density: Option<crate::puzzle_load::DensityAsset>,
    /// Queue of [`SessionUpdate`]s emitted by this store's mutators
    pending_updates: Vec<SessionUpdate>,
    /// In-flight op-stream preview token maps.
    previews: Previews,
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
            density: None,
            pending_updates: Vec::new(),
            previews: Previews::new(),
        }
    }

    /// Build the current view of the assembly: the lane heads of every
    /// entity in the checkpoint head's `entity_heads`, followed by
    /// every transient preview
    #[must_use]
    pub fn head_assembly(&self) -> Assembly {
        let head_id = self.history.checkpoints().head();
        let mut entities: Vec<Arc<MoleculeEntity>> = Vec::new();
        if let Some(head) = self.history.checkpoint(head_id) {
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

    /// Look up an entity by id.
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

    /// Look up an entity's display name.
    #[must_use]
    pub fn name(&self, id: EntityId) -> Option<&str> {
        self.metadata.get(&id).map(Arc::as_ref)
    }

    fn live_ids(&self) -> impl Iterator<Item = EntityId> + '_ {
        let head_id = self.history.checkpoints().head();
        let entity_heads = self.history.checkpoint(head_id).map(|h| &h.entity_heads);
        entity_heads
            .into_iter()
            .flat_map(|heads| heads.keys().copied())
            .chain(self.transient.keys().copied())
    }

    /// Iterate every live entity's display name
    pub fn iter(&self) -> impl Iterator<Item = (EntityId, &str)> {
        self.live_ids()
            .filter_map(move |id| self.metadata.get(&id).map(|m| (id, m.as_ref())))
    }

    /// All live entity ids
    /// (committed first, then preview).
    pub fn ids(&self) -> impl Iterator<Item = EntityId> + '_ {
        self.live_ids()
    }

    /// Resolve a proto `entity_id` (`u64`) to a live molex `EntityId` against
    /// the current session. Returns `None` when no live entity matches.
    pub(crate) fn resolve_entity(&self, entity_id: u64) -> Option<EntityId> {
        self.ids().find(|id| u64::from(id.raw()) == entity_id)
    }

    /// Number of live entities.
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

    /// Read the `(raw, game)` score of the current composition node (first
    /// open pending edit if any, else the committed head). The live-score
    /// read surface for the score widget.
    #[must_use]
    pub fn current_composition_scores(&self) -> (Option<f64>, Option<f64>) {
        self.history.current_composition_scores()
    }

    /// The raw per-term breakdown of the current composition node
    #[must_use]
    pub fn current_composition_breakdown(&self) -> Option<&crate::scores::StoredBreakdown> {
        self.history.current_composition_breakdown()
    }

    pub(crate) fn display_score(&self) -> Option<f64> {
        let (raw, game) = self.current_composition_scores();

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

    // Getter for selection, as per entity map
    #[must_use]
    pub const fn selection(&self) -> &BTreeMap<EntityId, BTreeSet<u32>> {
        &self.selection
    }

    // Getter for appearance, as per entity map
    #[must_use]
    pub const fn appearance(&self) -> &BTreeMap<EntityId, viso::DisplayOverrides> {
        &self.appearance
    }

    /// Total number of selected residues across all entities
    #[must_use]
    pub fn selection_total_count(&self) -> usize {
        self.selection
            .values()
            .map(std::collections::BTreeSet::len)
            .sum()
    }

    #[must_use]
    pub const fn focus(&self) -> Focus {
        self.focus
    }

    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }

    #[must_use]
    pub const fn puzzle(&self) -> Option<&Puzzle> {
        self.puzzle.as_ref()
    }

    /// The free-form session density (a `--with-density` load). `None` for a
    /// puzzle load or when no density was computed.
    #[must_use]
    pub const fn session_density(&self) -> Option<&crate::puzzle_load::DensityAsset> {
        self.density.as_ref()
    }

    // Is Residue N of Entity X designable?
    #[must_use]
    pub fn is_designable(&self, entity: EntityId, res: u32) -> bool {
        self.puzzle
            .as_ref()
            .is_some_and(|p| p.is_designable(entity, res))
    }

    /// Whether the current focus-scoped selection is fully designable.
    /// Used to gate action availability
    #[must_use]
    pub fn selection_is_designable(&self) -> bool {
        match self.focus {
            Focus::Entity(eid) => self
                .selection
                .get(&eid)
                .into_iter()
                .flatten()
                .all(|&res| self.is_designable(eid, res)),
            Focus::All => self
                .selection
                .iter()
                .all(|(&eid, residues)| residues.iter().all(|&res| self.is_designable(eid, res))),
        }
    }

    /// Whether the loaded puzzle gates design per entity
    #[must_use]
    pub fn design_gating_active(&self) -> bool {
        self.puzzle
            .as_ref()
            .is_some_and(|p| p.design_gating.is_some())
    }

    /// Per-entity set of residues the loaded puzzle permits redesign at, read
    /// off the session's design mask over the live head entities. Empty when
    /// no design gating is active (free-form session, fold puzzle). Carried on
    /// the [`DispatchIntent`] so the plugin can gate identity changes; the
    /// engine intersects it with the resolved selection, so computing it over
    /// every live protein entity is sufficient.
    ///
    /// [`DispatchIntent`]: crate::runner_client::DispatchIntent
    #[cfg(not(target_arch = "wasm32"))]
    #[must_use]
    pub(crate) fn designable_residues(&self) -> BTreeMap<EntityId, BTreeSet<u32>> {
        let mut designable: BTreeMap<EntityId, BTreeSet<u32>> = BTreeMap::new();
        if !self.design_gating_active() {
            return designable;
        }
        for entity in self.head_assembly().entities() {
            let eid = entity.id();
            let count = u32::try_from(entity.residue_count()).unwrap_or(u32::MAX);
            let residues: BTreeSet<u32> = (0..count)
                .filter(|&res| self.is_designable(eid, res))
                .collect();
            if !residues.is_empty() {
                let _ = designable.insert(eid, residues);
            }
        }
        designable
    }
}

#[cfg(test)]
mod tests;
