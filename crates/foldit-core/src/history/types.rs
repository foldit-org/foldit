//! Domain enums for the history module - action vocabulary, scoring
//! filter status. Pure data, no behavior beyond a small helper on
//! `CheckpointKind`.

use std::path::PathBuf;

use molex::entity::molecule::id::EntityId;
use molex::MoleculeType;

use super::EntitySnapshotId;

/// User-visible action across the whole assembly. Carried on a
/// [`Checkpoint`].
///
/// Plugin-driven edits all share the single plugin-agnostic
/// [`CheckpointKind::PluginOp`] shape: foldit-core records the op by its
/// `(plugin_id, op_id)` identity without naming any plugin or enumerating
/// plugin internals. The remaining variants are host structural events
/// (load, add entity, per-lane revert, preview promotion).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckpointKind {
    /// Puzzle / file load - the root checkpoint.
    Loaded { source: PathBuf },
    /// Promoted a transient preview (e.g., a streamed ML result).
    PromotedPreview { entity: EntityId },
    /// New entity added to the assembly.
    AddEntity { entity: EntityId, kind: MoleculeType },
    /// Per-entity revert to an older snapshot. Lane head moves to
    /// `target`; no new snapshot pushed; this checkpoint references the
    /// existing target snapshot.
    LaneUndo { entity: EntityId, target: EntitySnapshotId },
    /// Plugin-dispatched op. Identity carried by (`plugin_id`, `op_id`);
    /// `display` is the manifest-supplied label captured at dispatch time
    /// so the history projection doesn't have to look the plugin up later
    /// (and so the label survives plugin reload / removal). The touched
    /// entity set is recorded on the checkpoint's `entity_heads` (and, for
    /// an in-flight edit, on the pending edit's lanes), not on this kind -
    /// a single op may span several entities.
    PluginOp {
        plugin_id: String,
        op_id: String,
        display: String,
    },
    /// Host-native edit applied without any plugin dispatch (e.g. a
    /// residue mutation performed in-process via molex). Distinct from
    /// [`Self::PluginOp`] because that variant's `plugin_id` is read as a
    /// broadcast routing key; a native edit has no originating plugin and
    /// must not fabricate one. The touched lane set rides on the
    /// checkpoint's `entity_heads`, like `PluginOp`.
    NativeEdit { op_id: String, display: String },
}

impl CheckpointKind {
    /// The single entity this checkpoint targets, if it has one.
    /// `Loaded` carries no entity; `PluginOp` may span several and so
    /// reports its touched set through `entity_heads` rather than here.
    #[must_use]
    pub const fn entity(&self) -> Option<EntityId> {
        match self {
            Self::Loaded { .. } | Self::PluginOp { .. } | Self::NativeEdit { .. } => None,
            Self::PromotedPreview { entity, .. }
            | Self::AddEntity { entity, .. }
            | Self::LaneUndo { entity, .. } => Some(*entity),
        }
    }
}

/// Filter evaluation status for a checkpoint.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum FilterStatus {
    /// Every filter passed. Set once filter evaluation is wired;
    /// eviction already protects checkpoints in this state.
    #[allow(dead_code)]
    Pass,
    /// One or more filters failed. Set once filter evaluation is wired.
    #[allow(dead_code)]
    Fail(Vec<String>),
    /// Not yet evaluated.
    #[default]
    NotEvaluated,
}
