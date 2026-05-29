//! Infer the viso animation for an entity update from the structural
//! delta between the old and new pose.
//!
//! The right animation is derivable from what actually changed, so the
//! host computes it here rather than carrying a per-op declaration
//! across the plugin seam:
//!
//! - Sequence unchanged (same residue count and identities, so the atom
//!   sets line up index-for-index): ease coordinates A->B with
//!   [`viso::Transition::smooth`]. This is also the no-op default.
//! - Sequence changed (atom sets differ, so a position-wise ease is
//!   meaningless): rebuild rather than morph. When the backbone also
//!   moved (residue count held but CA positions shifted), play the
//!   backbone first via [`viso::Transition::backbone_then_expand`];
//!   otherwise [`viso::Transition::collapse_expand`].

use std::time::Duration;

use molex::entity::molecule::MoleculeEntity;

/// Largest CA displacement (Angstroms) still treated as "no backbone
/// movement". Streamed poses carry tiny float jitter even when the
/// backbone is held fixed; this keeps that jitter from misclassifying a
/// sidechain-only edit as a backbone move.
const CA_EPSILON: f32 = 1e-3;

/// Pick the viso transition that animates `prev` -> `new`.
///
/// See the module docs for the classification. Whole-entity only: this
/// never scopes to individual residues.
pub(crate) fn infer_transition(
    prev: &MoleculeEntity,
    new: &MoleculeEntity,
) -> viso::Transition {
    if !sequence_changed(prev, new) {
        // Atom sets match index-for-index; the coordinate ease is valid.
        return viso::Transition::smooth();
    }
    // Sequence changed => atom sets differ => can't ease across it.
    if backbone_moved(prev, new) {
        viso::Transition::backbone_then_expand(
            Duration::from_millis(200),
            Duration::from_millis(100),
        )
    } else {
        viso::Transition::collapse_expand(
            Duration::from_millis(150),
            Duration::from_millis(150),
        )
    }
}

/// True when the two entities differ in residue count or in any residue
/// identity (name). A non-polymer entity has no residues; two
/// non-polymers count as unchanged, a polymer/non-polymer pair as
/// changed.
fn sequence_changed(prev: &MoleculeEntity, new: &MoleculeEntity) -> bool {
    match (prev.residues(), new.residues()) {
        (Some(p), Some(n)) => {
            p.len() != n.len()
                || p.iter().zip(n.iter()).any(|(a, b)| a.name != b.name)
        }
        (None, None) => false,
        _ => true,
    }
}

/// True when the residue counts match but at least one residue's
/// backbone CA position moved beyond [`CA_EPSILON`]. Called only after
/// `sequence_changed`; a count mismatch (insertion/deletion) is not the
/// "backbone moved" case.
///
/// Per-residue CA positions come from molex's canonical
/// [`ProteinEntity::to_backbone`], which finds N/CA/C/O by name and
/// skips residues missing any of the four. A mutation preserves the
/// full backbone on both sides, so the skip set is identical and the
/// two vectors stay index-aligned; if they ever diverge in length we
/// can't align them, so report "not moved" (the collapse/expand
/// default). Non-protein entities have no protein backbone.
fn backbone_moved(prev: &MoleculeEntity, new: &MoleculeEntity) -> bool {
    if prev.residue_count() != new.residue_count() {
        return false;
    }
    let (Some(pp), Some(np)) = (prev.as_protein(), new.as_protein()) else {
        return false;
    };
    let (pb, nb) = (pp.to_backbone(), np.to_backbone());
    if pb.len() != nb.len() {
        return false;
    }
    pb.iter()
        .zip(nb.iter())
        .any(|(a, b)| a.ca.distance(b.ca) > CA_EPSILON)
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::Vec3;
    use molex::entity::molecule::atom::Atom;
    use molex::entity::molecule::id::EntityIdAllocator;
    use molex::entity::molecule::polymer::Residue;
    use molex::entity::molecule::protein::ProteinEntity;
    use molex::Element;

    fn atom(name: &str, element: Element, pos: Vec3) -> Atom {
        let mut n = [b' '; 4];
        for (i, b) in name.bytes().take(4).enumerate() {
            n[i] = b;
        }
        Atom {
            position: pos,
            occupancy: 1.0,
            b_factor: 0.0,
            element,
            name: n,
            formal_charge: 0,
        }
    }

    fn res_bytes(s: &str) -> [u8; 3] {
        let mut n = [b' '; 3];
        for (i, b) in s.bytes().take(3).enumerate() {
            n[i] = b;
        }
        n
    }

    fn residue(name: &str, seq: i32, range: std::ops::Range<usize>) -> Residue {
        Residue {
            name: res_bytes(name),
            label_seq_id: seq,
            auth_seq_id: None,
            auth_comp_id: None,
            ins_code: None,
            atom_range: range,
            variants: Vec::new(),
        }
    }

    /// Four-atom backbone-ish residue (N, CA, C, O), CA at `ca`.
    fn backbone_atoms(base: f32, ca: Vec3) -> Vec<Atom> {
        vec![
            atom("N", Element::N, Vec3::new(base, 0.0, 0.0)),
            atom("CA", Element::C, ca),
            atom("C", Element::C, Vec3::new(base + 2.0, 0.0, 0.0)),
            atom("O", Element::O, Vec3::new(base + 3.0, 0.0, 0.0)),
        ]
    }

    /// Two-residue protein; `names` gives the residue identities and
    /// `cas` the per-residue CA positions.
    fn protein(names: [&str; 2], cas: [Vec3; 2]) -> MoleculeEntity {
        let id = EntityIdAllocator::new().allocate();
        let mut atoms = backbone_atoms(0.0, cas[0]);
        atoms.extend(backbone_atoms(10.0, cas[1]));
        let residues =
            vec![residue(names[0], 1, 0..4), residue(names[1], 2, 4..8)];
        MoleculeEntity::Protein(ProteinEntity::new(
            id, atoms, residues, b'A', None,
        ))
    }

    // `viso::Transition` has no `PartialEq`; the two public flags
    // `(allows_size_change, suppress_initial_sidechains)` uniquely tag
    // the three presets this classifier returns:
    //   smooth               = (false, false)
    //   collapse_expand      = (true,  true)
    //   backbone_then_expand = (false, true)
    const SMOOTH: (bool, bool) = (false, false);
    const COLLAPSE_EXPAND: (bool, bool) = (true, true);
    const BACKBONE_THEN_EXPAND: (bool, bool) = (false, true);

    fn tag(t: &viso::Transition) -> (bool, bool) {
        (t.allows_size_change, t.suppress_initial_sidechains)
    }

    #[test]
    fn coords_only_change_is_smooth() {
        let prev = protein(["ALA", "ALA"], [Vec3::ZERO, Vec3::new(10.0, 0.0, 0.0)]);
        // Same sequence, CA positions shifted.
        let new = protein(
            ["ALA", "ALA"],
            [Vec3::new(0.0, 5.0, 0.0), Vec3::new(10.0, 5.0, 0.0)],
        );
        assert_eq!(tag(&infer_transition(&prev, &new)), SMOOTH);
    }

    #[test]
    fn sequence_change_fixed_backbone_is_collapse_expand() {
        let ca0 = Vec3::new(1.0, 0.0, 0.0);
        let ca1 = Vec3::new(11.0, 0.0, 0.0);
        let prev = protein(["ALA", "ALA"], [ca0, ca1]);
        // Residue 2 mutated; CA positions held fixed (sidechain-only).
        let new = protein(["ALA", "GLY"], [ca0, ca1]);
        assert_eq!(tag(&infer_transition(&prev, &new)), COLLAPSE_EXPAND);
    }

    #[test]
    fn sequence_change_with_backbone_move_is_backbone_then_expand() {
        let prev = protein(
            ["ALA", "ALA"],
            [Vec3::new(1.0, 0.0, 0.0), Vec3::new(11.0, 0.0, 0.0)],
        );
        // Residue 2 mutated AND its CA moved well past the epsilon.
        let new = protein(
            ["ALA", "GLY"],
            [Vec3::new(1.0, 0.0, 0.0), Vec3::new(11.0, 8.0, 0.0)],
        );
        assert_eq!(tag(&infer_transition(&prev, &new)), BACKBONE_THEN_EXPAND);
    }

    #[test]
    fn residue_count_change_is_collapse_expand() {
        let prev = protein(
            ["ALA", "ALA"],
            [Vec3::new(1.0, 0.0, 0.0), Vec3::new(11.0, 0.0, 0.0)],
        );
        // Single-residue protein: count differs, so the backbone-move
        // test is skipped and we collapse/expand.
        let id = EntityIdAllocator::new().allocate();
        let atoms = backbone_atoms(0.0, Vec3::new(1.0, 0.0, 0.0));
        let residues = vec![residue("ALA", 1, 0..4)];
        let new = MoleculeEntity::Protein(ProteinEntity::new(
            id, atoms, residues, b'A', None,
        ));
        assert_eq!(tag(&infer_transition(&prev, &new)), COLLAPSE_EXPAND);
    }
}
