//! Per-entity triple buffers and metadata: lock-free coord buffering between
//! backends and the render loop, plus per-entity origin/role tracking.
//!
//! The Orchestrator (in foldit-runner) owns the writer side of each triple buffer.
//! The main crate's frame loop calls `drain_updates()` once per frame to read the
//! latest values and push them to viso's engine. Intermediate updates between
//! frames are automatically discarded (latest-wins semantics).

use std::collections::HashMap;
use glam::Vec3;
use triple_buffer::Output;
use foldit_runner::orchestrator::BackendUpdate;
use viso::scene::{Focus, GroupId};

/// Reader side of a per-entity triple buffer.
struct EntityBuffer {
    reader: Output<Option<BackendUpdate>>,
}

// ── Per-entity metadata ──

/// How an entity entered the scene.
#[derive(Debug, Clone)]
pub enum EntityOrigin {
    /// Loaded from file or puzzle.
    Loaded,
    /// Result of RFDiffusion3 backbone design.
    StructureDesign { source: GroupId, confidence: f32 },
    /// Transient animation entity during ML operation.
    Animation { source: GroupId },
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
    pub designed_for: GroupId,
}

/// Per-entity metadata.
#[derive(Debug, Clone)]
pub struct EntityMeta {
    pub origin: EntityOrigin,
    pub role: EntityRole,
    /// Backbone CA positions at load time, for Kabsch alignment of ML outputs.
    pub reference_ca: Option<Vec<Vec3>>,
    /// MPNN-designed sequences associated with this entity's backbone.
    pub designed_sequences: Vec<DesignedSequence>,
}

// ── SharedState ──

/// Shared state: owns all per-entity triple buffer readers and metadata.
/// The frame loop calls `drain_updates()` each frame.
pub struct SharedState {
    buffers: HashMap<u64, EntityBuffer>,
    entities: HashMap<GroupId, EntityMeta>,
    /// Transient: in-flight ML animation structure (at most one).
    animation_id: Option<GroupId>,
}

impl SharedState {
    pub fn new() -> Self {
        Self {
            buffers: HashMap::new(),
            entities: HashMap::new(),
            animation_id: None,
        }
    }

    // ── Triple buffer management ──

    /// Store a triple buffer reader for an entity.
    /// The writer side is held by the Orchestrator.
    pub fn register_entity(&mut self, id: GroupId, reader: Output<Option<BackendUpdate>>) {
        self.buffers.insert(id.0, EntityBuffer { reader });
    }

    /// Remove an entity's buffer.
    pub fn unregister_entity(&mut self, id: GroupId) {
        self.buffers.remove(&id.0);
    }

    /// Read latest updates from all entity buffers.
    /// Returns only entities that have new data since last read.
    pub fn drain_updates(&mut self) -> Vec<(GroupId, BackendUpdate)> {
        let mut updates = Vec::new();
        for (&id, buf) in &mut self.buffers {
            if buf.reader.update() {
                if let Some(update) = buf.reader.output_buffer_mut().take() {
                    updates.push((GroupId(id), update));
                }
            }
        }
        updates
    }

    // ── Registration methods (replace Session lifecycle hooks) ──

    /// Register a loaded entity (from puzzle/file).
    pub fn register_loaded(&mut self, id: GroupId, reference_ca: Vec<Vec3>) {
        self.entities.insert(id, EntityMeta {
            origin: EntityOrigin::Loaded,
            role: EntityRole { foldable: true, designable: true, ambient: false },
            reference_ca: Some(reference_ca),
            designed_sequences: Vec::new(),
        });
    }

    /// Register a loaded entity with roles derived from its molecule entities.
    ///
    /// Inspects the entity types to determine capabilities:
    /// - Groups containing protein are foldable and designable
    /// - Groups that are entirely water/ion/solvent are ambient (non-interactive)
    pub fn register_loaded_with_entities(
        &mut self,
        id: GroupId,
        reference_ca: Vec<Vec3>,
        entities: &[foldit_conv::coords::entity::MoleculeEntity],
    ) {
        use foldit_conv::coords::entity::MoleculeType;

        let has_protein = entities.iter().any(|e| e.molecule_type == MoleculeType::Protein);
        let all_ambient = !entities.is_empty() && entities.iter().all(|e| {
            matches!(e.molecule_type, MoleculeType::Water | MoleculeType::Ion | MoleculeType::Solvent)
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

        self.entities.insert(id, EntityMeta {
            origin: EntityOrigin::Loaded,
            role,
            reference_ca: Some(reference_ca),
            designed_sequences: Vec::new(),
        });
    }

    /// Get entity metadata (for building EntityContext).
    pub fn entity_meta(&self, id: GroupId) -> Option<&EntityMeta> {
        self.entities.get(&id)
    }

    /// Register an animation entity (transient ML intermediate).
    pub fn register_animation(&mut self, id: GroupId, source: GroupId) {
        self.entities.insert(id, EntityMeta {
            origin: EntityOrigin::Animation { source },
            role: EntityRole { foldable: false, designable: false, ambient: false },
            reference_ca: None,
            designed_sequences: Vec::new(),
        });
        self.animation_id = Some(id);
    }

    /// Promote animation entity to a structure design result.
    pub fn promote_animation_to_design(&mut self, id: GroupId, confidence: f32) {
        if let Some(meta) = self.entities.get_mut(&id) {
            if let EntityOrigin::Animation { source } = meta.origin {
                meta.origin = EntityOrigin::StructureDesign { source, confidence };
                meta.role = EntityRole { foldable: true, designable: true, ambient: false };
            }
        }
        if self.animation_id == Some(id) {
            self.animation_id = None;
        }
    }

    /// Remove the current animation entity.
    pub fn remove_animation(&mut self) {
        if let Some(id) = self.animation_id.take() {
            self.entities.remove(&id);
        }
    }

    /// Store designed sequences for an entity.
    pub fn add_designed_sequences(&mut self, for_entity: GroupId, sequences: Vec<String>, scores: Vec<f32>) {
        if let Some(meta) = self.entities.get_mut(&for_entity) {
            for (seq, score) in sequences.into_iter().zip(scores.into_iter()) {
                meta.designed_sequences.push(DesignedSequence {
                    sequence: seq,
                    score,
                    designed_for: for_entity,
                });
            }
        }
    }

    /// Clear all entity metadata and animation state.
    pub fn reset_entities(&mut self) {
        self.entities.clear();
        self.animation_id = None;
    }

    // ── Query methods (replace Session targeting logic) ──

    /// First loaded entity (replaces session.original).
    pub fn loaded_entity(&self) -> Option<GroupId> {
        self.entities.iter()
            .find(|(_, m)| matches!(m.origin, EntityOrigin::Loaded))
            .map(|(id, _)| *id)
    }

    /// Current animation entity.
    pub fn animation(&self) -> Option<GroupId> {
        self.animation_id
    }

    /// Reference CA positions for a loaded entity (for Kabsch alignment).
    pub fn reference_ca(&self, id: GroupId) -> Option<&[Vec3]> {
        self.entities.get(&id).and_then(|m| m.reference_ca.as_deref())
    }

    /// Whether focus is session-wide.
    pub fn is_session_mode(focus: &Focus) -> bool {
        matches!(focus, Focus::Session)
    }

    /// Which entity a backend operation should target.
    /// Returns None in session mode (use combined coords).
    pub fn operation_target(focus: &Focus) -> Option<GroupId> {
        match focus {
            Focus::Session | Focus::Entity(..) => None,
            Focus::Group(id) => Some(*id),
        }
    }

    /// Entity to lock for backend operations. Falls back to loaded entity in session mode.
    pub fn lock_target(&self, focus: &Focus) -> Option<GroupId> {
        match focus {
            Focus::Session | Focus::Entity(..) => self.loaded_entity(),
            Focus::Group(id) => Some(*id),
        }
    }

}
