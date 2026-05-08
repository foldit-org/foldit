//! Domain enums for the history module — action vocabulary, action
//! lifecycle effect kinds, scoring filter status. Pure data, no
//! behavior beyond a couple of small helpers on `CheckpointKind`.

use std::path::PathBuf;

use molex::entity::molecule::id::EntityId;
use molex::MoleculeType;

use super::EntitySnapshotId;

// ── Domain enums ───────────────────────────────────────────────────────

/// Per-snapshot effect on a single entity's lane.
///
/// Distinct from [`CheckpointKind`]: a checkpoint kind names the
/// user-visible action across the whole assembly, while an action kind
/// names only the entity-level effect on this lane. Most checkpoint kinds
/// have a 1:1 mapping; `LaneUndo` is checkpoint-only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntityActionKind {
    /// Initial state — the snapshot the entity was loaded with.
    Loaded,
    /// Backbone wiggle (Rosetta).
    Wiggle,
    /// Sidechain shake (Rosetta).
    Shake,
    /// Energy minimization (Rosetta).
    Minimize,
    /// Hand-dragged residues.
    ManualMove,
    /// Mutated a residue's amino acid identity.
    Mutate,
    /// Backbone-design (RFdiffusion).
    Rfd3,
    /// Sequence-design (MPNN).
    Mpnn,
    /// Structure-prediction (RoseTTAFold) result accepted.
    Predict,
    /// Promoted from a transient preview (e.g., a streamed ML result).
    PromotedPreview,
}

/// User-visible action across the whole assembly. Carried on a
/// [`Checkpoint`].
///
/// Inner field types follow the strategy doc; placeholder shapes
/// (`WiggleMask`, [`AminoAcid`]) are intentionally minimal in this
/// section and refined as the action wiring lands in later sections.
///
/// `Eq` is intentionally not derived: `Rfd3.confidence` is `f32`.
#[derive(Debug, Clone, PartialEq)]
pub enum CheckpointKind {
    /// Puzzle / file load — the root checkpoint.
    Loaded { source: PathBuf },
    /// Rosetta wiggle on `entity`.
    Wiggle { entity: EntityId, mask: WiggleMask, duration_ms: u32 },
    /// Rosetta shake on `entity`.
    Shake { entity: EntityId, duration_ms: u32 },
    /// Rosetta minimize on `entity`.
    Minimize { entity: EntityId, iterations: u32 },
    /// User dragged residues on `entity`.
    ManualMove { entity: EntityId, residues: std::ops::Range<u32> },
    /// User mutated a residue on `entity`.
    Mutate {
        entity: EntityId,
        residue: u32,
        from: AminoAcid,
        to: AminoAcid,
    },
    /// RFdiffusion result accepted on `entity`.
    Rfd3 { entity: EntityId, confidence: f32 },
    /// MPNN result accepted on `entity`.
    Mpnn { entity: EntityId, sequence: String },
    /// Promoted a transient preview (e.g., RF3 / RFD3 / MPNN stream).
    PromotedPreview { entity: EntityId },
    /// New entity added to the assembly.
    AddEntity { entity: EntityId, kind: MoleculeType },
    /// Entity removed from the assembly.
    RemoveEntity { entity: EntityId },
    /// Per-entity revert to an older snapshot. Lane head moves to
    /// `target`; no new snapshot pushed; this checkpoint references the
    /// existing target snapshot.
    LaneUndo { entity: EntityId, target: EntitySnapshotId },
}

impl CheckpointKind {
    /// Whether this kind pushes a new snapshot on its entity's lane.
    /// `LaneUndo` only moves a head pointer; the snapshot it points at
    /// already exists.
    #[must_use]
    pub fn pushes_snapshot(&self) -> bool {
        !matches!(self, CheckpointKind::LaneUndo { .. })
    }

    /// The entity this checkpoint primarily targets, if any. `Loaded`
    /// is the only non-entity-targeted variant.
    #[must_use]
    pub fn entity(&self) -> Option<EntityId> {
        match self {
            CheckpointKind::Loaded { .. } => None,
            CheckpointKind::Wiggle { entity, .. }
            | CheckpointKind::Shake { entity, .. }
            | CheckpointKind::Minimize { entity, .. }
            | CheckpointKind::ManualMove { entity, .. }
            | CheckpointKind::Mutate { entity, .. }
            | CheckpointKind::Rfd3 { entity, .. }
            | CheckpointKind::Mpnn { entity, .. }
            | CheckpointKind::PromotedPreview { entity, .. }
            | CheckpointKind::AddEntity { entity, .. }
            | CheckpointKind::RemoveEntity { entity, .. }
            | CheckpointKind::LaneUndo { entity, .. } => Some(*entity),
        }
    }

    /// Translate into the matching [`EntityActionKind`] for the
    /// snapshot pushed on the lane (when [`pushes_snapshot`] is true).
    #[must_use]
    pub fn entity_action_kind(&self) -> Option<EntityActionKind> {
        match self {
            CheckpointKind::Loaded { .. } => Some(EntityActionKind::Loaded),
            CheckpointKind::Wiggle { .. } => Some(EntityActionKind::Wiggle),
            CheckpointKind::Shake { .. } => Some(EntityActionKind::Shake),
            CheckpointKind::Minimize { .. } => Some(EntityActionKind::Minimize),
            CheckpointKind::ManualMove { .. } => Some(EntityActionKind::ManualMove),
            CheckpointKind::Mutate { .. } => Some(EntityActionKind::Mutate),
            CheckpointKind::Rfd3 { .. } => Some(EntityActionKind::Rfd3),
            CheckpointKind::Mpnn { .. } => Some(EntityActionKind::Mpnn),
            CheckpointKind::PromotedPreview { .. } => Some(EntityActionKind::PromotedPreview),
            CheckpointKind::AddEntity { .. } => Some(EntityActionKind::Loaded),
            CheckpointKind::RemoveEntity { .. } | CheckpointKind::LaneUndo { .. } => None,
        }
    }
}

/// Wiggle mask placeholder. Refined in section 4 when wiggle wiring lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WiggleMask {
    /// Backbone DOFs.
    pub backbone: bool,
    /// Sidechain DOFs.
    pub sidechains: bool,
}

/// Amino acid placeholder (single-letter ASCII). Refined in section 4
/// when mutate wiring lands.
pub type AminoAcid = u8;

/// Filter evaluation status for a checkpoint.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum FilterStatus {
    /// Every filter passed.
    Pass,
    /// One or more filters failed.
    Fail(Vec<String>),
    /// Not yet evaluated.
    #[default]
    NotEvaluated,
}
