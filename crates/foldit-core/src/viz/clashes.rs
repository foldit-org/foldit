//! Steric-clash decode.
//!
//! The `clashes` query returns a plugin's detected steric clashes as opaque
//! bytes: a proto `ClashReport`, a list of atom-atom clashes. This module
//! turns those bytes into the per-endpoint structural refs the viso engine
//! resolves directly, keeping the proto-decode in one pure place the
//! `RunnerClient` facade and the at-rest trigger both call. The proto type is
//! named only here; the rest of the core sees [`ClashData`].

/// One endpoint of a decoded clash: the proto-side `entity_id` (orchestrator
/// scope, mapped to a molex `EntityId` by the trigger), the entity-local
/// `residue_index`, and the PDB `atom_name`. No flat residue index is
/// computed; viso resolves the per-entity ref itself.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClashEnd {
    pub entity_id: u64,
    pub residue_index: u32,
    pub atom_name: String,
}

/// One decoded clash: both endpoints and the per-pair `severity` (LJ
/// repulsion).
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, PartialEq)]
pub struct DecodedClash {
    pub a: ClashEnd,
    pub b: ClashEnd,
    pub severity: f32,
}

/// A decoded clash report, ready for the trigger to map entity ids and hand
/// to [`viso::VisoEngine::update_clashes`].
///
/// An empty/cleared report (empty bytes or undecodable bytes) maps to an
/// empty `clashes`, which viso reads as the signal to clear the set.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ClashData {
    pub clashes: Vec<DecodedClash>,
}

/// Decode opaque `clashes`-query bytes into a [`ClashData`] the trigger maps
/// and feeds the viso engine.
///
/// Returns an empty report when the bytes are empty (the query came back with
/// no payload) or fail to decode. A clash whose `a`/`b` atom or that atom's
/// `residue` ref is `None` is skipped (it cannot be resolved to a viso
/// endpoint). The empty result is the caller's signal to clear the engine's
/// clash set.
#[cfg(not(target_arch = "wasm32"))]
pub fn clashes_from_bytes(bytes: &[u8]) -> ClashData {
    if bytes.is_empty() {
        return ClashData::default();
    }
    <foldit_runner::proto::plugin::ClashReport as prost::Message>::decode(bytes)
        .map_or_else(|_| ClashData::default(), |report| report_data(&report))
}

/// Map a decoded `ClashReport` into a [`ClashData`]. Split out from
/// [`clashes_from_bytes`] so the decode and the field mapping can be exercised
/// independently. A clash with a `None` atom or residue ref on either endpoint
/// is dropped.
#[cfg(not(target_arch = "wasm32"))]
fn report_data(report: &foldit_runner::proto::plugin::ClashReport) -> ClashData {
    let clashes = report
        .clashes
        .iter()
        .filter_map(|clash| {
            let a = endpoint(clash.a.as_ref()?)?;
            let b = endpoint(clash.b.as_ref()?)?;
            Some(DecodedClash {
                a,
                b,
                severity: clash.severity,
            })
        })
        .collect();
    ClashData { clashes }
}

/// Map a proto `ClashAtom` into a [`ClashEnd`]. Returns `None` when the
/// `residue` ref is absent (the endpoint cannot be resolved).
#[cfg(not(target_arch = "wasm32"))]
fn endpoint(atom: &foldit_runner::proto::plugin::ClashAtom) -> Option<ClashEnd> {
    let residue = atom.residue.as_ref()?;
    Some(ClashEnd {
        entity_id: residue.entity_id,
        residue_index: residue.residue_index,
        atom_name: atom.atom_name.clone(),
    })
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use foldit_runner::proto::plugin::{Clash, ClashAtom, ClashReport, ResidueRef};

    fn clash_atom(entity_id: u64, residue_index: u32, atom_name: &str) -> ClashAtom {
        ClashAtom {
            residue: Some(ResidueRef {
                entity_id,
                residue_index,
            }),
            atom_name: atom_name.to_owned(),
        }
    }

    /// Decode yields the per-endpoint entity/residue/atom refs the trigger
    /// maps into `update_clashes`. The engine push + render is runtime-only
    /// (no headless `VisoEngine`), so this pins the pure decode step.
    // The severity is an exactly-representable f32 constant round-tripped
    // through prost bit-exactly, so exact equality is correct here (an epsilon
    // compare would hide a real round-trip regression).
    #[allow(clippy::float_cmp)]
    #[test]
    fn decodes_report() {
        let report = ClashReport {
            clashes: vec![Clash {
                a: Some(clash_atom(0, 5, "CB")),
                b: Some(clash_atom(1, 12, "NE2")),
                severity: 3.5,
            }],
        };
        let bytes = prost::Message::encode_to_vec(&report);

        let data = clashes_from_bytes(&bytes);
        assert_eq!(data.clashes.len(), 1);
        let clash = &data.clashes[0];
        assert_eq!(clash.a.entity_id, 0);
        assert_eq!(clash.a.residue_index, 5);
        assert_eq!(clash.a.atom_name, "CB");
        assert_eq!(clash.b.entity_id, 1);
        assert_eq!(clash.b.residue_index, 12);
        assert_eq!(clash.b.atom_name, "NE2");
        assert_eq!(clash.severity, 3.5);
    }

    /// A clash whose atom or residue ref is `None` on either endpoint is
    /// dropped: it cannot be resolved to a viso endpoint.
    #[test]
    fn skips_clashes_with_missing_refs() {
        let report = ClashReport {
            clashes: vec![
                // Endpoint `a` has no atom at all.
                Clash {
                    a: None,
                    b: Some(clash_atom(0, 1, "CA")),
                    severity: 1.0,
                },
                // Endpoint `b`'s atom has no residue ref.
                Clash {
                    a: Some(clash_atom(0, 1, "CA")),
                    b: Some(ClashAtom {
                        residue: None,
                        atom_name: "CB".to_owned(),
                    }),
                    severity: 1.0,
                },
                // Fully populated: kept.
                Clash {
                    a: Some(clash_atom(0, 2, "N")),
                    b: Some(clash_atom(0, 3, "O")),
                    severity: 2.0,
                },
            ],
        };
        let bytes = prost::Message::encode_to_vec(&report);

        let data = clashes_from_bytes(&bytes);
        assert_eq!(data.clashes.len(), 1);
        assert_eq!(data.clashes[0].a.residue_index, 2);
    }

    /// Empty bytes (the inert pre-implementation case) and undecodable bytes
    /// both yield an empty report, the signal to clear the set.
    #[test]
    fn empty_and_garbage_bytes_yield_empty_report() {
        assert!(clashes_from_bytes(&[]).clashes.is_empty());
        // A short non-protobuf byte string fails to decode.
        assert!(clashes_from_bytes(&[0xff, 0xff, 0xff, 0xff]).clashes.is_empty());
    }
}
