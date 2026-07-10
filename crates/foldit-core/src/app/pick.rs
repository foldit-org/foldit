//! Cursor-pick resolution helpers shared across the input paths, kept
//! target-agnostic so both the wasm and native builds can reach them.

use molex::entity::molecule::id::EntityId;
use viso::VisoEngine;

use crate::session::Session;

/// Resolve the residue currently under the cursor to its session
/// `(EntityId, residue)`, or `None` when the hover is empty / an atom or the
/// flat index does not map to a live entity (before the first rebuild, or out
/// of range). The raw entity id is mapped to the session [`EntityId`] the same
/// way the pull-drag path does, matching on `id.raw()`.
pub(in crate::app) fn hovered_segment_target(
    engine: &VisoEngine,
    store: &Session,
) -> Option<(EntityId, usize)> {
    let viso::PickTarget::Residue(flat) = engine.hovered_target() else {
        return None;
    };
    let (raw_entity, local_residue) = engine.flat_to_entity_residue(flat)?;
    let eid = store.ids().find(|id| id.raw() == raw_entity)?;
    Some((eid, local_residue as usize))
}
