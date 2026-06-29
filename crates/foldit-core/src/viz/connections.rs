//! Plugin-sourced rendering connections: the `connections` query's proto
//! `ConnectionReport` decoded into the molex `AtomLink` map the assembly
//! carries. The proto type is named only here.

#[cfg(not(target_arch = "wasm32"))]
use std::collections::HashMap;

/// Decode a proto `ConnectionReport` into the molex `AtomLink` map an
/// `Assembly` carries, resolving each endpoint against `asm`. Only `HBond`
/// and `Disulfide` are kept (`Clash`, `Band`, `Unspecified` skipped). A
/// connection whose endpoint cannot be resolved is skipped, so no
/// half-resolved link is emitted. `magnitude` is carried through.
#[cfg(not(target_arch = "wasm32"))]
#[must_use]
pub fn connections_from_report(
    report: &foldit_runner::proto::plugin::ConnectionReport,
    asm: &molex::Assembly,
) -> HashMap<molex::ConnectionType, Vec<molex::AtomLink>> {
    use foldit_runner::proto::plugin::ConnectionType as ProtoType;

    let mut map: HashMap<molex::ConnectionType, Vec<molex::AtomLink>> = HashMap::new();
    let mut skipped = 0_usize;
    for conn in &report.connections {
        let kind = match ProtoType::try_from(conn.r#type) {
            Ok(ProtoType::Hbond) => molex::ConnectionType::HBond,
            Ok(ProtoType::Disulfide) => molex::ConnectionType::Disulfide,
            // Clash has its own viz channel; Band has no producer;
            // Unspecified is inert. None of them enter this map.
            Ok(ProtoType::Clash | ProtoType::Band | ProtoType::Unspecified) | Err(_) => continue,
        };
        let (Some(a), Some(b)) = (
            conn.a.as_ref().and_then(|e| resolve_end(e, asm)),
            conn.b.as_ref().and_then(|e| resolve_end(e, asm)),
        ) else {
            skipped += 1;
            continue;
        };
        let link = molex::AtomLink {
            a,
            b,
            magnitude: conn.magnitude,
        };
        map.entry(kind).or_default().push(link);
    }
    if skipped > 0 {
        log::trace!("[viz] skipped {skipped} connection(s) with unresolvable endpoints");
    }
    map
}

/// Resolve one proto `AtomEnd` to a molex [`molex::AtomEnd`]. Returns
/// `None` when the `end` oneof is unset or the atom endpoint does not
/// resolve against `asm`.
#[cfg(not(target_arch = "wasm32"))]
fn resolve_end(
    end: &foldit_runner::proto::plugin::AtomEnd,
    asm: &molex::Assembly,
) -> Option<molex::AtomEnd> {
    use foldit_runner::proto::plugin::atom_end::End;
    match end.end.as_ref()? {
        End::Atom(atom) => {
            let id = resolve_atom(atom, asm)?;
            Some(molex::AtomEnd::Atom(id))
        }
        End::Anchor(v) => Some(molex::AtomEnd::Anchor(glam::Vec3::new(v.x, v.y, v.z))),
    }
}

/// Resolve a proto `ConnectionAtom` to a stable molex [`molex::AtomId`].
///
/// Maps `residue.entity_id` to the current entity, indexes
/// `residues()[residue_index]`, and scans that residue's `atom_range` for
/// the atom whose trimmed name matches the wire `atom_name`. Returns
/// `None` when the residue ref is absent, the entity / residue index is
/// out of range, or no atom in the residue matches the name.
#[cfg(not(target_arch = "wasm32"))]
fn resolve_atom(
    atom: &foldit_runner::proto::plugin::ConnectionAtom,
    asm: &molex::Assembly,
) -> Option<molex::AtomId> {
    let residue_ref = atom.residue.as_ref()?;
    let entity = asm
        .entities()
        .iter()
        .find(|e| u64::from(e.id().raw()) == residue_ref.entity_id)?;
    let residue = entity
        .residues()?
        .get(usize::try_from(residue_ref.residue_index).ok()?)?;
    let names = &entity.columns().name;
    let wire = atom.atom_name.as_bytes();
    let index = residue
        .atom_range
        .clone()
        .find(|&i| names.get(i).is_some_and(|n| trimmed_atom_name(n) == wire))?;
    Some(molex::AtomId {
        entity: entity.id(),
        index: u32::try_from(index).ok()?,
    })
}

/// Trim trailing space / NUL padding and leading space / NUL off a 4-byte
/// PDB atom name. Mirrors molex's own internal `trimmed_atom_name`
/// (which is crate-private), so the byte compare here matches the
/// convention molex uses everywhere it normalizes atom names.
#[cfg(not(target_arch = "wasm32"))]
fn trimmed_atom_name(name: &[u8; 4]) -> &[u8] {
    let mut end = 4;
    while end > 0 && (name[end - 1] == b' ' || name[end - 1] == 0) {
        end -= 1;
    }
    let mut start = 0;
    while start < end && (name[start] == b' ' || name[start] == 0) {
        start += 1;
    }
    &name[start..end]
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use foldit_runner::proto::plugin::{
        atom_end::End, AtomEnd as ProtoAtomEnd, Connection, ConnectionAtom, ConnectionReport,
        ConnectionType as ProtoType, ResidueRef,
    };
    use molex::entity::molecule::atom::Atom;
    use molex::entity::molecule::id::{EntityId, EntityIdAllocator};
    use molex::entity::molecule::polymer::Residue;
    use molex::entity::molecule::protein::ProteinEntity;
    use molex::{Element, MoleculeEntity};

    const fn atom(name: [u8; 4], element: Element) -> Atom {
        Atom {
            position: glam::Vec3::ZERO,
            occupancy: 1.0,
            b_factor: 0.0,
            element,
            name,
            formal_charge: 0,
            observed: true,
        }
    }

    const fn residue(name: [u8; 3], label_seq_id: i32, atom_range: std::ops::Range<usize>) -> Residue {
        Residue {
            name,
            label_seq_id,
            auth_seq_id: None,
            auth_comp_id: None,
            ins_code: None,
            atom_range,
            variants: Vec::new(),
        }
    }

    /// Build a single-entity assembly with two CYS residues, each a full
    /// canonical backbone (`N`, `CA`, `C`, `O`) plus `CB` and `SG`, so a
    /// disulfide between their `SG`s and an `HBond` between backbone atoms
    /// resolve. The residues survive protein canonicalization (which drops
    /// residues missing backbone atoms).
    fn two_cys_assembly() -> (molex::Assembly, EntityId) {
        let mut alloc = EntityIdAllocator::new();
        let id = alloc.allocate();
        let cys_atoms = || {
            vec![
                atom(*b"N   ", Element::N),
                atom(*b"CA  ", Element::C),
                atom(*b"C   ", Element::C),
                atom(*b"O   ", Element::O),
                atom(*b"CB  ", Element::C),
                atom(*b"SG  ", Element::S),
            ]
        };
        let mut atoms = cys_atoms();
        atoms.extend(cys_atoms());
        let residues = vec![residue(*b"CYS", 1, 0..6), residue(*b"CYS", 2, 6..12)];
        let entity = ProteinEntity::new(id, atoms, residues, "A".to_owned());
        let asm = molex::Assembly::new(vec![MoleculeEntity::Protein(entity)]);
        (asm, id)
    }

    /// The molex `AtomId` a direct trimmed-name scan of `residue_index`
    /// resolves `atom_name` to, for cross-checking the decoder output.
    fn expect_atom_id(asm: &molex::Assembly, residue_index: usize, atom_name: &str) -> molex::AtomId {
        let entity = &asm.entities()[0];
        let residue = &entity.residues().unwrap()[residue_index];
        let names = &entity.columns().name;
        let index = residue
            .atom_range
            .clone()
            .find(|&i| trimmed_atom_name(&names[i]) == atom_name.as_bytes())
            .unwrap();
        molex::AtomId {
            entity: entity.id(),
            index: u32::try_from(index).unwrap(),
        }
    }

    fn atom_end(entity_id: u64, residue_index: u32, atom_name: &str) -> ProtoAtomEnd {
        ProtoAtomEnd {
            end: Some(End::Atom(ConnectionAtom {
                residue: Some(ResidueRef {
                    entity_id,
                    residue_index,
                }),
                atom_name: atom_name.to_owned(),
            })),
        }
    }

    /// A disulfide between the two residues' `SG` atoms decodes into a
    /// single `Disulfide` link, and an `HBond` between backbone atoms decodes
    /// into a single `HBond` link, both carrying the resolved `AtomId`s a
    /// direct name scan finds.
    #[test]
    fn decodes_hbond_and_disulfide() {
        let (asm, id) = two_cys_assembly();
        let raw = u64::from(id.raw());
        let report = ConnectionReport {
            connections: vec![
                Connection {
                    r#type: ProtoType::Disulfide as i32,
                    a: Some(atom_end(raw, 0, "SG")),
                    b: Some(atom_end(raw, 1, "SG")),
                    magnitude: None,
                },
                Connection {
                    r#type: ProtoType::Hbond as i32,
                    a: Some(atom_end(raw, 0, "N")),
                    b: Some(atom_end(raw, 1, "CA")),
                    magnitude: None,
                },
            ],
        };

        let map = connections_from_report(&report, &asm);

        let disulfides = map.get(&molex::ConnectionType::Disulfide).unwrap();
        assert_eq!(disulfides.len(), 1);
        assert_eq!(
            disulfides[0].a,
            molex::AtomEnd::Atom(expect_atom_id(&asm, 0, "SG"))
        );
        assert_eq!(
            disulfides[0].b,
            molex::AtomEnd::Atom(expect_atom_id(&asm, 1, "SG"))
        );

        let hbonds = map.get(&molex::ConnectionType::HBond).unwrap();
        assert_eq!(hbonds.len(), 1);
        assert_eq!(
            hbonds[0].a,
            molex::AtomEnd::Atom(expect_atom_id(&asm, 0, "N"))
        );
        assert_eq!(
            hbonds[0].b,
            molex::AtomEnd::Atom(expect_atom_id(&asm, 1, "CA"))
        );
    }

    /// Clash, Band, and Unspecified are skipped: none enter the map.
    #[test]
    fn skips_non_hbond_disulfide_types() {
        let (asm, id) = two_cys_assembly();
        let raw = u64::from(id.raw());
        let report = ConnectionReport {
            connections: vec![
                Connection {
                    r#type: ProtoType::Clash as i32,
                    a: Some(atom_end(raw, 0, "SG")),
                    b: Some(atom_end(raw, 1, "SG")),
                    magnitude: Some(3.5),
                },
                Connection {
                    r#type: ProtoType::Band as i32,
                    a: Some(atom_end(raw, 0, "N")),
                    b: Some(atom_end(raw, 1, "CA")),
                    magnitude: None,
                },
                Connection {
                    r#type: ProtoType::Unspecified as i32,
                    a: Some(atom_end(raw, 0, "N")),
                    b: Some(atom_end(raw, 1, "CA")),
                    magnitude: None,
                },
            ],
        };

        let map = connections_from_report(&report, &asm);
        assert!(map.is_empty());
    }

    /// An endpoint that does not resolve (unknown entity, out-of-range
    /// residue, or absent atom name) drops the whole connection.
    #[test]
    fn skips_unresolvable_endpoints() {
        let (asm, id) = two_cys_assembly();
        let raw = u64::from(id.raw());
        let report = ConnectionReport {
            connections: vec![
                // Unknown entity id.
                Connection {
                    r#type: ProtoType::Disulfide as i32,
                    a: Some(atom_end(raw + 999, 0, "SG")),
                    b: Some(atom_end(raw, 1, "SG")),
                    magnitude: None,
                },
                // Out-of-range residue.
                Connection {
                    r#type: ProtoType::Disulfide as i32,
                    a: Some(atom_end(raw, 0, "SG")),
                    b: Some(atom_end(raw, 99, "SG")),
                    magnitude: None,
                },
                // Atom name not present in the residue.
                Connection {
                    r#type: ProtoType::Disulfide as i32,
                    a: Some(atom_end(raw, 0, "ZZ")),
                    b: Some(atom_end(raw, 1, "SG")),
                    magnitude: None,
                },
            ],
        };

        let map = connections_from_report(&report, &asm);
        assert!(map.is_empty());
        let _ = id;
    }
}
