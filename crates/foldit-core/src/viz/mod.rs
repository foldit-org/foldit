//! Plugin-sourced viz channels: the pure proto -> struct decoders, plus the
//! at-rest coordinator that drives them.
//!
//! The `clashes` / `connections` / `exposed_hydrophobics` / `voids`
//! submodules each turn one plugin query's wire payload into the host-side
//! struct the viso engine consumes, keeping every proto-decode in one
//! dependency-free place. None of those decoders reaches into core state; they
//! map bytes/reports to plain data.
//!
//! The `refresh` submodule is the at-rest coordinator: unlike the decoders it
//! DOES take core borrows (`RunnerClient`, `Session`, `ViewOptions`), firing
//! each query, decoding the reply through the decoders above, and writing the
//! result into the session-held viz state for the render projector to push.

pub mod clashes;
pub mod connections;
pub mod exposed_hydrophobics;
pub mod refresh;
pub mod voids;
