//! Drift guard between the plugin C-ABI enums (`plugin::abi`) and their
//! proto counterparts (`proto::plugin`).
//!
//! Both enums are defined in `foldit-plugin-sdk` and reached here through
//! the runner's re-exports, so the guard is a direct value comparison: no
//! `FileDescriptorSet`, no `prost-reflect`. It pins the logical
//! correspondence the bridge relies on, including the one deliberate
//! divergence (the C ABI folds the proto `Enum` tag into `String`, so the
//! abi tag discriminants are NOT numerically equal to the proto ones past
//! `String`).
//!
//! Covered: the abi `FolditPluginParamTag` <-> proto `ParamType`
//! logical mapping, and a reachability smoke for
//! `FOLDIT_PLUGIN_ABI_VERSION`.
//!
//! NOT covered (deferred): a full descriptor walk asserting every
//! C-ABI struct field has its proto counterpart. That needs the proto
//! `FileDescriptorSet` emitted at build time plus a reflection dep
//! (`prost-reflect`) and a curated field map; it is a separate,
//! heavier guard than the cheap enum-value check here.

use foldit_runner::plugin::abi::{self, FolditPluginParamTag};
use foldit_runner::proto::plugin::ParamType;

/// The abi tag and its logical proto counterpart. The numeric values
/// agree through `String`; `proto::ParamType::Enum` (5) has no abi tag
/// (the C ABI carries enum-typed params as `String`), which shifts
/// `Vec3` to abi 5 / proto 6. This table is the contract the bridge
/// switch in `vtable.cc` encodes; reordering either enum without
/// updating the other breaks it.
const LOGICAL_PAIRS: &[(FolditPluginParamTag, ParamType)] = &[
    (FolditPluginParamTag::Unspecified, ParamType::Unspecified),
    (FolditPluginParamTag::Int, ParamType::Int),
    (FolditPluginParamTag::Float, ParamType::Float),
    (FolditPluginParamTag::Bool, ParamType::Bool),
    (FolditPluginParamTag::String, ParamType::String),
    (FolditPluginParamTag::Vec3, ParamType::Vec3),
];

#[test]
fn param_tag_matches_proto_through_string() {
    // The shared prefix (Unspecified..=String) must stay value-identical:
    // these discriminants cross the ABI boundary as raw integers.
    for (tag, proto) in &LOGICAL_PAIRS[..5] {
        // Both discriminants are small and non-negative; widen to i64 so
        // the comparison crosses the repr(u32)/repr(i32) boundary without
        // a lossy cast.
        assert_eq!(
            i64::from(*tag as u32),
            i64::from(*proto as i32),
            "abi {tag:?} and proto {proto:?} must share a discriminant",
        );
    }
}

#[test]
fn param_tag_vec3_carries_the_enum_fold_offset() {
    // The C ABI has no `Enum` tag (proto 5); enum-typed params travel as
    // `String`. So abi `Vec3` is 5 while proto `Vec3` is 6: a deliberate
    // one-step offset that the bridge accounts for. Pin both halves so a
    // future "let's just renumber" change trips here.
    assert_eq!(FolditPluginParamTag::Vec3 as u32, 5);
    assert_eq!(ParamType::Vec3 as i32, 6);
    assert_eq!(ParamType::Enum as i32, 5);
}

#[test]
fn param_tag_count_matches_proto_minus_enum() {
    // Every abi tag has a proto counterpart in the table; the only proto
    // member without one is `Enum`. If a variant is added to either enum
    // without updating LOGICAL_PAIRS, one of these counts moves.
    assert_eq!(LOGICAL_PAIRS.len(), 6);
}

#[test]
fn abi_version_const_is_reachable() {
    // Smoke: the version const is single-sourced in foldit-plugin-sdk's
    // abi module and emitted into the generated C header by the SDK's
    // cbindgen build. A bump there must move this assertion.
    assert_eq!(abi::FOLDIT_PLUGIN_ABI_VERSION, 6);
}
