//! Per-entity domain metadata: how an entity entered the scene plus the
//! design artifacts attached to it. Owned by [`super::EntityStore`] in an
//! `Arc`-shared `IndexMap`; re-exported through the parent module.

use glam::Vec3;
use molex::entity::molecule::id::EntityId;

/// How an entity entered the scene.
#[derive(Debug, Clone)]
pub enum EntityOrigin {
    /// Loaded from file or puzzle.
    Loaded,
    /// Result of RFDiffusion3 backbone design.
    StructureDesign { source: EntityId, confidence: f32 },
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
/// gone — presence in [`super::EntityStore::transient`] is the preview signal.
#[derive(Debug, Clone)]
pub struct EntityMetadata {
    /// Display name.
    pub name: String,
    /// How the entity entered the scene.
    pub origin: EntityOrigin,
    /// Optional reference CA set for alignment.
    pub reference_ca: Option<Vec<Vec3>>,
    /// Designed sequences, appended by MPNN runs.
    pub designed_sequences: Vec<DesignedSequence>,
}

impl EntityMetadata {
    /// Build a minimal metadata record.
    #[must_use]
    pub fn new(name: String, origin: EntityOrigin) -> Self {
        Self {
            name,
            origin,
            reference_ca: None,
            designed_sequences: Vec::new(),
        }
    }
}
