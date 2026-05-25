//! Mouse-drag → Rosetta pull plumbing.
//!
//! Owns the routing decision (backbone vs sidechain, hydrogen/non-protein
//! rejection) and bridges the GUI's pointer lifecycle to the unified
//! plugin op-id surface plus viso's pull-geom feed. Lives outside
//! `app.rs` because the routing is non-trivial and worth isolating from
//! the rest of the app lifecycle.
//!
//! The lifecycle is:
//!   - pointer-down on an atom: `begin` classifies the pick into one of
//!     two op ids, dispatches `StartStream`, and returns the viso
//!     `PullInfo` plus a `Pending` entry the host attaches to its
//!     `active_streams` map.
//!   - pointer-move: `update_endpoint` re-resolves the world-space drag
//!     target through viso (it owns the camera + atom positions) and
//!     hands it to the orchestrator as a single-key `endpoint` Vec3
//!     update.
//!   - pointer-up / ESC / stream Final/Error: caller drops the
//!     `PullDrag` and dispatches `CancelStream` (host already cancels
//!     other streams the same way).
//!
//! Single-stream invariant: only one `PullDrag` is alive at a time. The
//! caller enforces this by checking `Option<PullDrag>` before
//! constructing a new one.

#[cfg(not(target_arch = "wasm32"))]
use std::collections::HashMap;

use molex::chemistry::is_protein_backbone_atom_name;
use molex::entity::molecule::id::EntityId as MolexEntityId;
use molex::{Element, MoleculeEntity};
#[cfg(not(target_arch = "wasm32"))]
use viso::{AtomRef, PullInfo};

use crate::document::Document;

/// Op id for the residue-anchored cart-pull (backbone pull).
pub(crate) const OP_PULL_BACKBONE: &str = "ActionLocalMinimizePull";
/// Op id for the atom-anchored sidechain pull.
pub(crate) const OP_PULL_SIDECHAIN: &str = "ActionPullSidechain";

/// Resolved pull-route decision. The two variants map 1:1 to the two op
/// ids registered with the rosetta bridge; param shape differs (backbone
/// needs the residue only, sidechain needs residue + atom name) so the
/// caller dispatches accordingly.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) struct PullRoute {
    /// Which op-id to dispatch (one of [`OP_PULL_BACKBONE`] /
    /// [`OP_PULL_SIDECHAIN`]).
    pub op_id: &'static str,
    /// PDB atom name the user picked (`"CA"`, `"CB"`, etc.). For
    /// backbone pulls the bridge ignores the name (residue-anchored);
    /// for sidechain pulls it's the dispatch key.
    pub atom_name: String,
    /// 0-based residue index within the entity. Bridge expects
    /// 1-indexed pose residue, so the caller converts at dispatch.
    pub residue_in_entity: u32,
    /// Entity-flat 0-based residue index (matches `viso::AtomRef`).
    pub flat_residue: u32,
    /// Molex entity id of the picked entity. Used both for the
    /// SessionContext focus and to compute the rosetta-pose residue
    /// from `residue_in_entity` once multi-entity routing lands.
    pub entity_id: MolexEntityId,
}

/// Live pull-drag state. One per active drag at most (single-stream
/// invariant). Owned by the host alongside its `active_streams` map.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) struct PullDrag {
    /// The stream's request id from `dispatch_start_stream`.
    pub request_id: u64,
    /// Plugin id that owns the stream (always `"rosetta"` today).
    pub plugin_id: String,
    /// viso pull-geom spec; the host re-feeds this to
    /// `engine.update_pull` every frame so the capsule + arrow render.
    pub pull_info: PullInfo,
}

/// Classify a `PickTarget::Atom { entity_id, atom_idx }` against the
/// host policy and produce a pull route, or `None` to reject (no pull
/// dispatched).
///
/// Rejection cases:
///   - entity not found in the store
///   - entity is not a protein (no pull on NA / small molecules / bulk)
///   - picked atom is a hydrogen (atom name starts with `H`; covers
///     PDB v3 `HA`, `H`, `HB1`, `HG21`, …)
#[cfg(not(target_arch = "wasm32"))]
#[must_use]
pub(crate) fn route_atom_pick(
    store: &Document,
    entity_id: u32,
    atom_idx: u32,
) -> Option<PullRoute> {
    let molex_id = store
        .ids()
        .find(|id| id.raw() == entity_id)?;
    let protein = match store.entity(molex_id)? {
        MoleculeEntity::Protein(p) => p,
        _ => return None,
    };
    let atom = protein.atoms.get(atom_idx as usize)?;
    // Hydrogen reject — element field is the authoritative check; the
    // atom_name prefix gate in the cartoon-pick path is a fallback for
    // when we don't have the Atom in hand.
    if atom.element == Element::H {
        return None;
    }
    let atom_name = trim_atom_name(&atom.name);
    let residue_in_entity = protein
        .residues
        .iter()
        .position(|r| r.atom_range.contains(&(atom_idx as usize)))?
        as u32;

    let op_id = if is_protein_backbone_atom_name(&atom_name) {
        OP_PULL_BACKBONE
    } else {
        OP_PULL_SIDECHAIN
    };
    Some(PullRoute {
        op_id,
        atom_name,
        residue_in_entity,
        flat_residue: residue_in_entity,
        entity_id: molex_id,
    })
}

/// Classify a `PickTarget::Residue(flat_idx)` (cartoon-mode pick)
/// against the host policy. Defers to viso for the "closest atom by
/// screen distance" lookup, then applies the same hydrogen / non-protein
/// gating as [`route_atom_pick`].
///
/// `entity_for_flat_residue` is the host's view of which entity owns a
/// given flat residue index; the host has the multi-entity layout, this
/// module does not.
#[cfg(not(target_arch = "wasm32"))]
#[must_use]
pub(crate) fn route_residue_pick(
    store: &Document,
    flat_residue: u32,
    atom_name: &str,
    entity_id: MolexEntityId,
    residue_in_entity: u32,
) -> Option<PullRoute> {
    if atom_name.starts_with('H') {
        return None;
    }
    if !matches!(store.entity(entity_id)?, MoleculeEntity::Protein(_)) {
        return None;
    }
    let op_id = if is_protein_backbone_atom_name(atom_name) {
        OP_PULL_BACKBONE
    } else {
        OP_PULL_SIDECHAIN
    };
    Some(PullRoute {
        op_id,
        atom_name: atom_name.to_owned(),
        residue_in_entity,
        flat_residue,
        entity_id,
    })
}

/// Build the StartStream `params` map for a pull route. Backbone pulls
/// carry only the 1-indexed pose residue; sidechain pulls add the
/// `atom_name` so the bridge can resolve `name → atomno` against the
/// live pose.
///
/// The conversion to 1-indexed rosetta-pose residue assumes a
/// single-protein-entity layout (the common Foldit case). Multi-entity
/// support requires the bridge to expose its entity → pose-offset map
/// to the host; left as a v1 limitation.
#[cfg(not(target_arch = "wasm32"))]
#[must_use]
pub(crate) fn build_start_params(
    route: &PullRoute,
) -> HashMap<String, foldit_runner::orchestrator::ParamValue> {
    use foldit_runner::orchestrator::ParamValue;
    // ParamValue::Int is i32; rosetta-pose residue is 1-indexed
    // core::Size on the bridge side. `as i32` is safe for any
    // realistic foldit pose (max residues ≪ i32::MAX).
    let pose_residue = (route.residue_in_entity as i32) + 1;
    let mut params = HashMap::new();
    let _ = params.insert(
        String::from("residue"),
        ParamValue::Int(pose_residue),
    );
    if route.op_id == OP_PULL_SIDECHAIN {
        let _ = params.insert(
            String::from("atom_name"),
            ParamValue::String(route.atom_name.clone()),
        );
    }
    params
}

/// Build the viso `PullInfo` spec used to drive the pull-geom capsule
/// + arrow render. The atom reference must match what viso resolves
/// on its side; for protein picks the flat residue + PDB atom name
/// land in viso's `ConstraintContext::resolve_atom_ref`.
#[cfg(not(target_arch = "wasm32"))]
#[must_use]
pub(crate) fn build_pull_info(
    route: &PullRoute,
    screen_target: (f32, f32),
) -> PullInfo {
    PullInfo {
        atom: AtomRef {
            residue: route.flat_residue,
            atom_name: route.atom_name.clone(),
        },
        screen_target,
    }
}

/// Trim the trailing zero / space padding off a PDB atom name. Atoms
/// in molex carry the raw 4-byte buffer; the on-wire / classifier
/// representation drops the padding.
fn trim_atom_name(raw: &[u8; 4]) -> String {
    let end = raw
        .iter()
        .position(|b| *b == 0 || *b == b' ')
        .unwrap_or(raw.len());
    String::from_utf8_lossy(&raw[..end]).into_owned()
}
