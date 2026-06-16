//! Exposed-hydrophobic decode.
//!
//! The `exposed_hydrophobics` query returns a plugin's detected
//! solvent-exposed hydrophobic residues as opaque bytes: a proto
//! `ExposedHydrophobicReport`, a list of flagged residues. This module turns
//! those bytes into the per-entity structural refs the viso engine resolves
//! directly, keeping the proto-decode in one pure place the `RunnerClient`
//! facade and the at-rest trigger both call. The proto type is named only
//! here; the rest of the core sees [`ExposedHydroData`].

/// One flagged residue: the proto-side `entity_id` (orchestrator scope,
/// mapped to a molex `EntityId` by the trigger) and the entity-local
/// `residue_index`. No flat residue index is computed; viso resolves the
/// per-entity ref itself.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExposedResidue {
    pub entity_id: u64,
    pub residue_index: u32,
}

/// A decoded exposed-hydrophobic report, ready for the trigger to map entity
/// ids and hand to [`viso::VisoEngine::update_exposed_hydrophobics`].
///
/// An empty/cleared report (empty bytes or undecodable bytes) maps to an
/// empty `exposed`, which viso reads as the signal to clear the set.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExposedHydroData {
    pub exposed: Vec<ExposedResidue>,
}

/// Decode opaque `exposed_hydrophobics`-query bytes into an
/// [`ExposedHydroData`] the trigger maps and feeds the viso engine.
///
/// Returns an empty report when the bytes are empty (the query came back with
/// no payload) or fail to decode. The empty result is the caller's signal to
/// clear the engine's exposed-hydrophobic set.
#[cfg(not(target_arch = "wasm32"))]
pub fn exposed_from_bytes(bytes: &[u8]) -> ExposedHydroData {
    if bytes.is_empty() {
        return ExposedHydroData::default();
    }
    <foldit_runner::proto::plugin::ExposedHydrophobicReport as prost::Message>::decode(bytes)
        .map_or_else(|_| ExposedHydroData::default(), |report| report_data(&report))
}

/// Map a decoded `ExposedHydrophobicReport` into an [`ExposedHydroData`].
/// Split out from [`exposed_from_bytes`] so the decode and the field mapping
/// can be exercised independently.
#[cfg(not(target_arch = "wasm32"))]
fn report_data(report: &foldit_runner::proto::plugin::ExposedHydrophobicReport) -> ExposedHydroData {
    let exposed = report
        .exposed
        .iter()
        .map(|residue| ExposedResidue {
            entity_id: residue.entity_id,
            residue_index: residue.residue_index,
        })
        .collect();
    ExposedHydroData { exposed }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use foldit_runner::proto::plugin::{ExposedHydrophobicReport, ResidueRef};

    const fn residue_ref(entity_id: u64, residue_index: u32) -> ResidueRef {
        ResidueRef {
            entity_id,
            residue_index,
        }
    }

    /// Decode yields the per-residue entity/residue refs the trigger maps
    /// into `update_exposed_hydrophobics`. The engine push + render is
    /// runtime-only (no headless `VisoEngine`), so this pins the pure decode
    /// step.
    #[test]
    fn decodes_report() {
        let report = ExposedHydrophobicReport {
            exposed: vec![residue_ref(0, 5), residue_ref(1, 12)],
        };
        let bytes = prost::Message::encode_to_vec(&report);

        let data = exposed_from_bytes(&bytes);
        assert_eq!(data.exposed.len(), 2);
        assert_eq!(data.exposed[0].entity_id, 0);
        assert_eq!(data.exposed[0].residue_index, 5);
        assert_eq!(data.exposed[1].entity_id, 1);
        assert_eq!(data.exposed[1].residue_index, 12);
    }

    /// Empty bytes (the inert pre-implementation case) and undecodable bytes
    /// both yield an empty report, the signal to clear the set.
    #[test]
    fn empty_and_garbage_bytes_yield_empty_report() {
        assert!(exposed_from_bytes(&[]).exposed.is_empty());
        // A short non-protobuf byte string fails to decode.
        assert!(exposed_from_bytes(&[0xff, 0xff, 0xff, 0xff]).exposed.is_empty());
    }
}
