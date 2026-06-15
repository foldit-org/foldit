//! Packing-void decode.
//!
//! The `voids` query returns a plugin's detected packing voids as opaque
//! bytes: a proto `VoidField`, a void distance field sampled on a regular
//! grid. This module turns those bytes into the dims/origin/spacing/phi the
//! viso engine meshes directly, keeping the proto-decode in one pure place
//! the `RunnerClient` facade and the at-rest trigger both call. The proto
//! type is named only here; the rest of the core sees [`VoidFieldData`].

/// A decoded void distance field, ready to hand to
/// [`viso::VisoEngine::set_external_void_field`]. Carries the grid `dims`
/// (from nx/ny/nz), the world-space `origin` of cell (0,0,0), the per-axis
/// `spacing`, the row-major x-major `phi` samples, and the iso `threshold`.
///
/// An empty/cleared field (empty bytes, undecodable bytes, or a `None`
/// origin) maps to zero `dims` and an empty `phi`, which viso reads as the
/// signal to clear the external set.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, PartialEq)]
pub struct VoidFieldData {
    pub dims: [usize; 3],
    pub origin: [f32; 3],
    pub spacing: [f32; 3],
    pub phi: Vec<f32>,
    pub threshold: f32,
}

#[cfg(not(target_arch = "wasm32"))]
impl VoidFieldData {
    /// A cleared field: zero dims, empty `phi`. viso reads this as "clear
    /// the external set".
    const fn empty() -> Self {
        Self {
            dims: [0; 3],
            origin: [0.0; 3],
            spacing: [0.0; 3],
            phi: Vec::new(),
            threshold: 0.0,
        }
    }
}

/// Decode opaque `voids`-query bytes into a [`VoidFieldData`] the viso
/// engine meshes directly.
///
/// Returns a cleared field when the bytes are empty (the query came back
/// with no payload), fail to decode, or carry no grid origin (the
/// proto's `origin`/`spacing` are optional; a missing origin marks a
/// cleared/empty field). The cleared result is the caller's signal to clear
/// the engine's external set.
#[cfg(not(target_arch = "wasm32"))]
pub fn void_field_from_bytes(bytes: &[u8]) -> VoidFieldData {
    if bytes.is_empty() {
        return VoidFieldData::empty();
    }
    <foldit_runner::proto::plugin::VoidField as prost::Message>::decode(bytes)
        .map_or_else(|_| VoidFieldData::empty(), |field| field_data(&field))
}

/// Map a decoded `VoidField` into a [`VoidFieldData`]. Split out from
/// [`void_field_from_bytes`] so the decode and the field mapping can be
/// exercised independently. A `None` origin marks a cleared field.
#[cfg(not(target_arch = "wasm32"))]
fn field_data(field: &foldit_runner::proto::plugin::VoidField) -> VoidFieldData {
    let Some(origin) = field.origin.as_ref() else {
        return VoidFieldData::empty();
    };
    let spacing = field
        .spacing
        .as_ref()
        .map_or([0.0; 3], |s| [s.x, s.y, s.z]);
    VoidFieldData {
        dims: [field.nx as usize, field.ny as usize, field.nz as usize],
        origin: [origin.x, origin.y, origin.z],
        spacing,
        phi: field.phi.clone(),
        threshold: field.threshold,
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use foldit_runner::proto::plugin::{Vec3, VoidField};

    /// Decode yields the dims/origin/spacing/phi the trigger feeds into
    /// `set_external_void_field`. The engine push + render is runtime-only
    /// (no headless `VisoEngine`), so this pins the pure decode step.
    // The origin/spacing/threshold values are exactly-representable f32
    // constants round-tripped through prost bit-exactly, so exact equality
    // is the correct assertion (an epsilon compare would hide a real
    // round-trip regression).
    #[allow(clippy::float_cmp)]
    #[test]
    fn decodes_field() {
        let field = VoidField {
            nx: 2,
            ny: 3,
            nz: 4,
            origin: Some(Vec3 { x: 1.0, y: -2.0, z: 0.5 }),
            spacing: Some(Vec3 { x: 0.5, y: 0.5, z: 0.5 }),
            phi: vec![0.0; 24],
            threshold: 1.0,
        };
        let bytes = prost::Message::encode_to_vec(&field);

        let data = void_field_from_bytes(&bytes);
        assert_eq!(data.dims, [2, 3, 4]);
        assert_eq!(data.origin, [1.0, -2.0, 0.5]);
        assert_eq!(data.spacing, [0.5, 0.5, 0.5]);
        assert_eq!(data.phi.len(), 24);
        assert_eq!(data.threshold, 1.0);
    }

    /// A `None` origin marks a cleared field, the signal to clear the set.
    #[test]
    fn none_origin_yields_cleared_field() {
        let field = VoidField {
            nx: 2,
            ny: 2,
            nz: 2,
            origin: None,
            spacing: Some(Vec3 { x: 1.0, y: 1.0, z: 1.0 }),
            phi: vec![0.0; 8],
            threshold: 1.0,
        };
        let bytes = prost::Message::encode_to_vec(&field);

        let data = void_field_from_bytes(&bytes);
        assert_eq!(data.dims, [0, 0, 0]);
        assert!(data.phi.is_empty());
    }

    /// Empty bytes (the inert pre-implementation case) and undecodable
    /// bytes both yield a cleared field, the signal to clear the set.
    #[test]
    fn empty_and_garbage_bytes_yield_cleared_field() {
        assert!(void_field_from_bytes(&[]).phi.is_empty());
        // A short non-protobuf byte string fails to decode.
        assert!(void_field_from_bytes(&[0xff, 0xff, 0xff, 0xff]).phi.is_empty());
    }
}
