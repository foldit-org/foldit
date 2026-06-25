//! Plugin-sourced viz channels: the pure proto -> struct decoders (`clashes`,
//! `connections`, `exposed_hydrophobics`, `voids`), plus the at-rest
//! `refresh` coordinator that drives them.

pub mod clashes;
pub mod connections;
pub mod exposed_hydrophobics;
pub mod refresh;
pub mod voids;
