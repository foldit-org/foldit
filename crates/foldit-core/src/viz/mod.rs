//! Pure proto -> struct decoders for the plugin-sourced viz channels.
//!
//! Each submodule turns one plugin query's wire payload into the
//! host-side struct the viso engine consumes, keeping every proto-decode
//! in one dependency-free place the `RunnerClient` facade and the at-rest
//! triggers call. None of these decoders reaches into core state; they map
//! bytes/reports to plain data the `_coord` glue then feeds to viso.

pub mod clashes;
pub mod connections;
pub mod exposed_hydrophobics;
pub mod voids;
