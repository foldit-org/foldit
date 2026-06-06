//! Core-owned score types. Mirror what a plugin score report carries
//! today: a whole-report total, a per-term breakdown, and a flat list of
//! per-residue scores. The runner facade converts the wire/proto report
//! into these at the `RunnerClient` boundary so the rest of the core never
//! names the runner's proto types.
//!
//! Cross-platform: the blocking score path is reachable on wasm, so these
//! types and their conversion must build on every target.

/// One plugin's score for the assembly (or a scored composition): the
/// total, the per-term breakdown, and the per-residue scores.
pub(crate) struct ScoreReport {
    pub total: f32,
    pub terms: std::collections::HashMap<String, f32>,
    pub per_residue: Vec<ResidueScore>,
}

/// A single residue's score, addressed by `(entity_id, residue_index)`.
pub(crate) struct ResidueScore {
    pub entity_id: u64,
    pub residue_index: u32,
    pub score: f32,
}
