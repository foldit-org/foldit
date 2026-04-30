//! Authoritative entity store: owns all entity data, assigns IDs,
//! and holds per-entity metadata (name, origin, role, reference CA).
//!
//! Viso is a downstream renderer — foldit pushes entity data to it.
//! Both sides share the same ID space.

use molex::entity::molecule::id::{EntityId, EntityIdAllocator};
use molex::{Assembly, MoleculeEntity, MoleculeType};
use glam::Vec3;
use indexmap::IndexMap;

/// Combined assembly handed off to the Rosetta backend.
///
/// The runner returns updates as an [`Assembly`] in the same entity
/// order as `entity_ids` here. Callers (e.g. `apply_combined_update`)
/// match returned entities to local entities by position.
pub struct CombinedAssemblyResult {
    /// Assembly with one entity per protein in the same order as
    /// `entity_ids`. Backend-side IDs are minted fresh and meaningless;
    /// match by position, not by id.
    pub assembly: Assembly,
    /// Local foldit entity ids in the same order as
    /// `assembly.entities()`.
    pub entity_ids: Vec<u32>,
    /// Per-entity Rosetta residue ranges `(start, end)`, 1-indexed and
    /// inclusive, computed from each entity's residue count and the
    /// concatenation order in `entity_ids`. Used to populate
    /// `RosettaSessionState` for focus locking.
    pub residue_ranges: Vec<(usize, usize)>,
}

/// How an entity entered the scene.
#[derive(Debug, Clone)]
pub enum EntityOrigin {
    /// Loaded from file or puzzle.
    Loaded,
    /// Result of RFDiffusion3 backbone design.
    StructureDesign { source: u32, confidence: f32 },
    /// Transient animation entity during ML operation.
    Animation { source: u32 },
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
    pub sequence: String,
    pub score: f32,
    pub designed_for: u32,
}

/// A tracked entity with metadata.
///
/// Visibility is **not** tracked here — it lives on viso's
/// `EntityAnnotations` (mutate via `engine.set_entity_visible`, query
/// via `engine.is_entity_visible`).
pub struct TrackedEntity {
    pub entity: MoleculeEntity,
    pub name: String,
    pub origin: EntityOrigin,
    pub role: EntityRole,
    pub reference_ca: Option<Vec<Vec3>>,
    pub designed_sequences: Vec<DesignedSequence>,
}

/// Authoritative entity store.
///
/// Assigns entity IDs, owns all entity data, and pushes to viso.
/// Holds a long-lived [`molex::Assembly`] that mirrors the structural
/// state of the entities map. Incremental mutations (add/remove/
/// replace) bump the assembly's generation counter, which is what
/// viso uses to gate its rederive work — rebuilding a fresh
/// `Assembly` per publish would reset generation to 0 and viso
/// would never see updates.
pub struct EntityStore {
    entities: IndexMap<u32, TrackedEntity>,
    allocator: EntityIdAllocator,
    assembly: molex::Assembly,
    animation_id: Option<u32>,
}

impl EntityStore {
    pub fn new() -> Self {
        Self {
            entities: IndexMap::new(),
            allocator: EntityIdAllocator::new(),
            assembly: molex::Assembly::new(Vec::new()),
            animation_id: None,
        }
    }

    // -- ID assignment --

    /// Mint an [`EntityId`] for a known raw u32 id. Advances the
    /// allocator past `raw` so future `insert` calls don't collide.
    pub fn mint_id(&mut self, raw: u32) -> EntityId {
        self.allocator.from_raw(raw)
    }

    /// Insert a new entity. Assigns the next available ID.
    /// Returns the assigned ID.
    pub fn insert(
        &mut self,
        mut entity: MoleculeEntity,
        name: String,
        origin: EntityOrigin,
        role: EntityRole,
    ) -> u32 {
        let eid = self.allocator.allocate();
        let id = eid.raw();
        entity.set_id(eid);
        self.assembly.add_entity(entity.clone());
        self.entities.insert(id, TrackedEntity {
            entity,
            name,
            origin,
            role,
            reference_ca: None,
            designed_sequences: Vec::new(),
        });
        id
    }

    /// Insert with a specific ID (used when restoring state).
    pub fn insert_with_id(
        &mut self,
        mut entity: MoleculeEntity,
        id: u32,
        name: String,
        origin: EntityOrigin,
        role: EntityRole,
    ) {
        entity.set_id(self.allocator.from_raw(id));
        self.assembly.add_entity(entity.clone());
        self.entities.insert(id, TrackedEntity {
            entity,
            name,
            origin,
            role,
            reference_ca: None,
            designed_sequences: Vec::new(),
        });
    }

    // -- Accessors --

    pub fn get(&self, id: u32) -> Option<&TrackedEntity> {
        self.entities.get(&id)
    }

    pub fn get_mut(&mut self, id: u32) -> Option<&mut TrackedEntity> {
        self.entities.get_mut(&id)
    }

    pub fn ids(&self) -> impl Iterator<Item = u32> + '_ {
        self.entities.keys().copied()
    }

    pub fn iter(&self) -> impl Iterator<Item = (u32, &TrackedEntity)> {
        self.entities.iter().map(|(&id, te)| (id, te))
    }

    pub fn count(&self) -> usize {
        self.entities.len()
    }

    // -- Mutation --

    pub fn remove(&mut self, id: u32) -> bool {
        if self.animation_id == Some(id) {
            self.animation_id = None;
        }
        let removed = self.entities.shift_remove(&id).is_some();
        if removed {
            self.assembly.remove_entity(self.allocator.from_raw(id));
        }
        removed
    }

    pub fn clear(&mut self) {
        self.entities.clear();
        self.animation_id = None;
        self.assembly = molex::Assembly::new(Vec::new());
    }

    pub fn set_name(&mut self, id: u32, name: String) {
        if let Some(te) = self.entities.get_mut(&id) {
            te.name = name;
        }
    }

    pub fn update_entity(&mut self, id: u32, entity: MoleculeEntity) {
        if let Some(te) = self.entities.get_mut(&id) {
            te.entity = entity.clone();
        }
        // Replace in the long-lived Assembly. remove_entity + add_entity
        // both bump the generation counter, which is what viso uses to
        // gate rederive work.
        let eid = self.allocator.from_raw(id);
        self.assembly.remove_entity(eid);
        self.assembly.add_entity(entity);
    }

    pub fn set_reference_ca(&mut self, id: u32, ca: Vec<Vec3>) {
        if let Some(te) = self.entities.get_mut(&id) {
            te.reference_ca = Some(ca);
        }
    }

    // -- Viso integration --

    /// Push the current `Assembly` snapshot to viso. The `Assembly`
    /// is long-lived and incrementally mutated by `insert`/`remove`/
    /// `update_entity`, so each push has a fresh generation and viso
    /// will rederive.
    pub fn publish_to(&self, engine: &mut viso::VisoEngine) {
        engine.set_assembly(std::sync::Arc::new(self.assembly.clone()));
    }

    /// Replace an entity in-place, queue an animation transition for
    /// the next sync, and publish the updated `Assembly` to the engine.
    /// The transition stages a per-sync animation (start = current
    /// visual position, target = the new entity's positions); it does
    /// NOT set a persistent behavior override.
    pub fn update_entity_and_publish(
        &mut self,
        engine: &mut viso::VisoEngine,
        id: u32,
        entity: MoleculeEntity,
        transition: viso::Transition,
    ) {
        self.update_entity(id, entity);
        engine.queue_entity_transition(id, transition);
        self.publish_to(engine);
    }

    /// Iterate over all protein entities (molecule_type filter only).
    /// Backend ops should pick targets via focus/role, not visibility.
    pub fn proteins(&self) -> impl Iterator<Item = (u32, &TrackedEntity)> {
        self.entities.iter().filter_map(|(&id, te)| {
            if te.entity.molecule_type() == MoleculeType::Protein {
                Some((id, te))
            } else {
                None
            }
        })
    }

    // -- Animation management --

    pub fn animation(&self) -> Option<u32> {
        self.animation_id
    }

    pub fn register_animation(&mut self, id: u32, source: u32) {
        self.animation_id = Some(id);
        if let Some(te) = self.entities.get_mut(&id) {
            te.origin = EntityOrigin::Animation { source };
            te.role = EntityRole { foldable: false, designable: false, ambient: false };
        }
    }

    pub fn remove_animation(&mut self) {
        if let Some(id) = self.animation_id.take() {
            // Route through `remove` so the long-lived Assembly stays
            // in sync (it's the structural source of truth for viso).
            let _ = self.remove(id);
        }
    }

    pub fn promote_animation_to_design(&mut self, id: u32, confidence: f32) {
        if let Some(te) = self.entities.get_mut(&id) {
            if let EntityOrigin::Animation { source } = te.origin {
                te.origin = EntityOrigin::StructureDesign { source, confidence };
                te.role = EntityRole { foldable: true, designable: true, ambient: false };
            }
        }
        if self.animation_id == Some(id) {
            self.animation_id = None;
        }
    }

    // -- Legacy query helpers (moved from SharedState) --

    /// First loaded entity.
    pub fn loaded_entity(&self) -> Option<u32> {
        self.entities.iter()
            .find(|(_, te)| matches!(te.origin, EntityOrigin::Loaded))
            .map(|(&id, _)| id)
    }

    /// Reference CA positions for alignment.
    pub fn reference_ca(&self, id: u32) -> Option<&[Vec3]> {
        self.entities.get(&id).and_then(|te| te.reference_ca.as_deref())
    }

    /// Entity metadata (origin + role).
    pub fn entity_meta(&self, id: u32) -> Option<(&EntityOrigin, &EntityRole)> {
        self.entities.get(&id).map(|te| (&te.origin, &te.role))
    }

    /// Store designed sequences.
    pub fn add_designed_sequences(&mut self, for_entity: u32, sequences: Vec<String>, scores: Vec<f32>) {
        if let Some(te) = self.entities.get_mut(&for_entity) {
            for (seq, score) in sequences.into_iter().zip(scores.into_iter()) {
                te.designed_sequences.push(DesignedSequence {
                    sequence: seq,
                    score,
                    designed_for: for_entity,
                });
            }
        }
    }

    // -- Backend coord helpers --

    /// Combine all protein entities into an [`Assembly`] for the
    /// Rosetta backend. Returns `None` if there are no proteins.
    pub fn combined_assembly_for_backend(&self) -> Option<CombinedAssemblyResult> {
        let proteins: Vec<(u32, &TrackedEntity)> = self.proteins().collect();
        if proteins.is_empty() {
            return None;
        }
        let entity_ids: Vec<u32> = proteins.iter().map(|(id, _)| *id).collect();
        let mut residue_ranges = Vec::with_capacity(proteins.len());
        let mut cursor = 1usize;
        let mut entities = Vec::with_capacity(proteins.len());
        for (_, te) in &proteins {
            let res_count = match &te.entity {
                MoleculeEntity::Protein(p) => p.residues.len(),
                _ => 0,
            };
            if res_count == 0 {
                continue;
            }
            let start = cursor;
            let end = cursor + res_count - 1;
            residue_ranges.push((start, end));
            cursor = end + 1;
            entities.push(te.entity.clone());
        }
        if entities.is_empty() {
            return None;
        }
        Some(CombinedAssemblyResult {
            assembly: Assembly::new(entities),
            entity_ids,
            residue_ranges,
        })
    }

    /// Count protein residues per entity.
    pub fn visible_residue_counts(&self) -> Vec<(u32, usize)> {
        self.proteins()
            .filter_map(|(id, te)| match &te.entity {
                MoleculeEntity::Protein(p) if !p.residues.is_empty() => {
                    Some((id, p.residues.len()))
                }
                _ => None,
            })
            .collect()
    }

    /// Build focus description from focus + entity names.
    pub fn focus_description(&self, focus: &viso::Focus) -> String {
        match focus {
            viso::Focus::Session => {
                let count = self.entities.len();
                format!("Session ({count} entities)")
            }
            viso::Focus::Entity(id) => {
                let raw = id.raw();
                self.entities.get(&raw)
                    .map(|te| te.name.clone())
                    .unwrap_or_else(|| format!("Entity {raw}"))
            }
        }
    }

    /// Get entity assembly bytes (all molecule types).
    pub fn get_entity_assembly_bytes(&self, id: u32) -> Option<Vec<u8>> {
        let te = self.entities.get(&id)?;
        molex::ops::codec::assembly_bytes(std::slice::from_ref(&te.entity)).ok()
    }

    /// Collect entities for ML based on current focus.
    pub fn collect_ml_entities(
        &self,
        focus: &viso::Focus,
        fallback_entity: Option<u32>,
    ) -> Option<(u32, Vec<MoleculeEntity>)> {
        match focus {
            viso::Focus::Entity(eid) => {
                let raw = eid.raw();
                let te = self.entities.get(&raw)?;
                Some((raw, vec![te.entity.clone()]))
            }
            viso::Focus::Session => {
                let id = fallback_entity?;
                let te = self.entities.get(&id)?;
                Some((id, vec![te.entity.clone()]))
            }
        }
    }

    /// Register a loaded entity with reference CA and role detection.
    pub fn register_loaded(&mut self, id: u32, reference_ca: Vec<Vec3>) {
        if let Some(te) = self.entities.get_mut(&id) {
            te.origin = EntityOrigin::Loaded;
            te.role = EntityRole { foldable: true, designable: true, ambient: false };
            te.reference_ca = Some(reference_ca);
        }
    }

    /// Register a loaded entity with roles derived from molecule types.
    pub fn register_loaded_with_entities(
        &mut self,
        id: u32,
        reference_ca: Vec<Vec3>,
        entities: &[MoleculeEntity],
    ) {
        let has_protein = entities.iter().any(|e| e.molecule_type() == MoleculeType::Protein);
        let all_ambient = !entities.is_empty() && entities.iter().all(|e| {
            matches!(e.molecule_type(), MoleculeType::Water | MoleculeType::Ion | MoleculeType::Solvent)
        });

        let role = if all_ambient {
            EntityRole { foldable: false, designable: false, ambient: true }
        } else {
            EntityRole {
                foldable: has_protein,
                designable: has_protein,
                ambient: false,
            }
        };

        if let Some(te) = self.entities.get_mut(&id) {
            te.origin = EntityOrigin::Loaded;
            te.role = role;
            te.reference_ca = Some(reference_ca);
        }
    }
}
