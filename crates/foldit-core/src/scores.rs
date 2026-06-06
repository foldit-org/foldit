//! Core-owned score types. Mirror what a plugin score report carries: the
//! RAW (unweighted) per-term energies (whole-pose and per-residue), the
//! term-name alignment key, and nothing else. The runner facade converts
//! the wire/proto report into these at the `RunnerClient` boundary so the
//! rest of the core never names the runner's proto types.
//!
//! Core owns the weighting: it multiplies the raw per-term energies by a
//! session-held weight map ([`Session::term_weights`]) to produce the
//! weighted total and per-residue scalars itself, which are what the app
//! displays and colors by.
//!
//! Cross-platform: the blocking score path is reachable on wasm, so these
//! types, their conversion, and the weighting methods must build on every
//! target. Only the file-IO weight loader is native-gated.

use std::collections::HashMap;

/// One plugin's score for the assembly (or a scored composition): the RAW
/// (unweighted) per-term energies that core weights itself, plus the
/// term-name alignment key.
pub(crate) struct ScoreReport {
    /// Names of the raw terms, the alignment key for `whole_pose_terms` and
    /// each `ResidueTermScores::terms`. Same order, same length.
    pub term_names: Vec<String>,
    /// Raw (unweighted) whole-pose energy per term, aligned to `term_names`.
    pub whole_pose_terms: Vec<f32>,
    /// Raw (unweighted) per-residue energies, each `terms` aligned to
    /// `term_names`.
    pub per_residue_terms: Vec<ResidueTermScores>,
}

/// A single residue's RAW per-term energies, addressed by
/// `(entity_id, residue_index)`. `terms` is aligned to
/// [`ScoreReport::term_names`].
#[derive(Debug, Clone)]
pub(crate) struct ResidueTermScores {
    pub entity_id: u64,
    pub residue_index: u32,
    pub terms: Vec<f32>,
}

/// The RAW (unweighted) per-term breakdown retained on a history node
/// (a [`crate::history::Checkpoint`] or in-flight `PendingEdit`) as the
/// session-owned source of truth for per-residue coloring. Mirrors the
/// energy half of a [`ScoreReport`] minus `term_names`: the alignment key
/// lives once on the [`crate::session::Session`] (its `term_names`),
/// shared by every stored breakdown, rather than being duplicated on each
/// node. The render projector re-derives the displayed per-residue colors
/// from the current composition node's breakdown × the session weights on
/// every `ScoresChanged`.
///
/// Cross-platform like the rest of this module: stamped only on the native
/// score path today, but the type and its weighting helper build on every
/// target (the breakdown is simply `None` on every node on wasm).
#[derive(Debug, Clone)]
pub(crate) struct StoredBreakdown {
    /// Raw (unweighted) whole-pose energy per term, aligned to the
    /// session's `term_names`.
    pub whole_pose_terms: Vec<f32>,
    /// Raw (unweighted) per-residue energies, each `terms` aligned to the
    /// session's `term_names`.
    pub per_residue_terms: Vec<ResidueTermScores>,
}

impl StoredBreakdown {
    /// Core-weighted per-residue scalars, identical in shape and value to
    /// [`ScoreReport::weighted_per_residue`] but taking `term_names`
    /// externally (the alignment key lives on the `Session`, not on the
    /// stored form). One `(entity_id, residue_index, score)` per
    /// [`ResidueTermScores`], where `score = Σ terms[i] *
    /// weights[term_names[i]]` (missing weights `0.0`).
    pub fn weighted_per_residue(
        &self,
        term_names: &[String],
        weights: &HashMap<String, f32>,
    ) -> Vec<(u64, u32, f64)> {
        self.per_residue_terms
            .iter()
            .map(|rts| {
                let score: f64 = term_names
                    .iter()
                    .zip(&rts.terms)
                    .map(|(name, raw)| {
                        let w = weights.get(name).copied().unwrap_or(0.0);
                        f64::from(*raw) * f64::from(w)
                    })
                    .sum();
                (rts.entity_id, rts.residue_index, score)
            })
            .collect()
    }
}

impl ScoreReport {
    /// Core-weighted whole-pose total: `Σ whole_pose_terms[i] * weights[name]`,
    /// missing weights treated as `0.0`. Replaces the plugin's pre-weighted
    /// `total` for display; value-identical when `weights` matches the
    /// plugin's own weight set.
    pub fn weighted_total(&self, weights: &HashMap<String, f32>) -> f64 {
        self.term_names
            .iter()
            .zip(&self.whole_pose_terms)
            .map(|(name, raw)| {
                let w = weights.get(name).copied().unwrap_or(0.0);
                f64::from(*raw) * f64::from(w)
            })
            .sum()
    }

    /// Core-weighted per-residue scalars: one `(entity_id, residue_index,
    /// score)` per [`ResidueTermScores`], where `score = Σ terms[i] *
    /// weights[term_names[i]]` (missing weights `0.0`). The production path
    /// now weights via [`StoredBreakdown::weighted_per_residue`] (term names
    /// supplied externally from the session); this report-local form is
    /// retained as the value-identity oracle that test pins the stored form's
    /// output against, hence `#[allow(dead_code)]` for non-test builds.
    #[allow(dead_code)]
    pub fn weighted_per_residue(&self, weights: &HashMap<String, f32>) -> Vec<(u64, u32, f64)> {
        self.per_residue_terms
            .iter()
            .map(|rts| {
                let score: f64 = self
                    .term_names
                    .iter()
                    .zip(&rts.terms)
                    .map(|(name, raw)| {
                        let w = weights.get(name).copied().unwrap_or(0.0);
                        f64::from(*raw) * f64::from(w)
                    })
                    .sum();
                (rts.entity_id, rts.residue_index, score)
            })
            .collect()
    }
}

/// Convert a rosetta raw score (REU) to foldit's game-mode display number.
/// Verbatim port of `rosetta_score_to_game_score_either(use_minimum=true,
/// internal=false)` (`rosetta_util.cc:2702`, constants at lines 2662-2664).
/// The linear map is universal foldit policy, not rosetta-specific, so it
/// lives next to the score-view selector that picks which representation
/// reaches the GUI (game when a puzzle is loaded, raw otherwise). Applied
/// to both whole-assembly and composition scores so neither ever displays
/// raw REU.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn rosetta_raw_to_game(raw: f64) -> f64 {
    const SCORE_OFFSET: f64 = 800.0;
    const SCORE_SCALE: f64 = 10.0;
    const SCORE_MINIMUM: f64 = 0.0;
    ((-raw + SCORE_OFFSET) * SCORE_SCALE).max(SCORE_MINIMUM)
}

/// Parse a Rosetta `.wts` weights file into `term_name -> weight`. Keeps
/// only lines that split into exactly two whitespace tokens whose second
/// parses as `f32` (the `term value` rows). Blank lines, `#` comments,
/// `METHOD_WEIGHTS …` rows (more than two tokens), and bare flags
/// (`INCLUDE_INTRA_RES_PROTEIN`, `NO_HB_ENV_DEP`; one token) are skipped.
pub(crate) fn parse_wts(src: &str) -> HashMap<String, f32> {
    let mut weights = HashMap::new();
    for line in src.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let tokens: Vec<&str> = line.split_whitespace().collect();
        if tokens.len() != 2 {
            continue;
        }
        if let Ok(value) = tokens[1].parse::<f32>() {
            weights.insert(tokens[0].to_string(), value);
        }
    }
    weights
}

/// Load + parse the default `ref2015_cart` weight map. Resolves
/// `assets/scoring/ref2015_cart.wts` by walking up from the running
/// executable (same shape as [`crate::puzzle::levels_root`], covering test,
/// dev, and installed binary layouts). Returns an `Err` string the caller
/// can log if no ancestor carries the asset or the file is unreadable.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn load_default_term_weights() -> Result<HashMap<String, f32>, String> {
    let exe = std::env::current_exe()
        .map_err(|e| format!("current_exe lookup failed: {e}"))?;
    let mut dir = exe.parent();
    while let Some(d) = dir {
        let candidate = d.join("assets/scoring/ref2015_cart.wts");
        if candidate.is_file() {
            let src = std::fs::read_to_string(&candidate)
                .map_err(|e| format!("reading {}: {e}", candidate.display()))?;
            return Ok(parse_wts(&src));
        }
        dir = d.parent();
    }
    Err(format!(
        "load_default_term_weights: no `assets/scoring/ref2015_cart.wts` found \
         in any ancestor of {}",
        exe.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Weighting anchor: core multiplies the raw per-term energies by the
    /// weight set to produce the displayed whole-pose total and per-residue
    /// scalars.
    #[test]
    fn weighting_matches_preweighted_fields() {
        let weights: HashMap<String, f32> =
            [("a".to_string(), 0.5_f32), ("b".to_string(), 1.0_f32)]
                .into_iter()
                .collect();

        // whole pose: 10*0.5 + 20*1.0 = 25.0.
        // per residue: 4*0.5 + 6*1.0 = 8.0.
        let report = ScoreReport {
            term_names: vec!["a".to_string(), "b".to_string()],
            whole_pose_terms: vec![10.0, 20.0],
            per_residue_terms: vec![ResidueTermScores {
                entity_id: 7,
                residue_index: 3,
                terms: vec![4.0, 6.0],
            }],
        };

        assert_eq!(report.weighted_total(&weights), 25.0);

        let per_residue = report.weighted_per_residue(&weights);
        assert_eq!(per_residue.len(), 1);
        let (entity_id, residue_index, score) = per_residue[0];
        assert_eq!(entity_id, 7);
        assert_eq!(residue_index, 3);
        assert_eq!(score, 8.0);
    }

    /// Re-derivation equality: weighting a `StoredBreakdown` against an
    /// external `term_names` produces the same per-residue scalars as
    /// weighting the equivalent `ScoreReport` directly. This is the value-
    /// identity proof for the session-owned breakdown swap: the render
    /// projector re-derives colors from the stored form and must land on
    /// the same numbers the old direct push produced from the report.
    #[test]
    fn stored_breakdown_weighting_matches_report() {
        let weights: HashMap<String, f32> = [
            ("fa_atr".to_string(), 1.0_f32),
            ("fa_rep".to_string(), 0.55_f32),
            ("hbond".to_string(), -0.5_f32),
        ]
        .into_iter()
        .collect();
        let term_names =
            vec!["fa_atr".to_string(), "fa_rep".to_string(), "hbond".to_string()];

        let report = ScoreReport {
            term_names: term_names.clone(),
            whole_pose_terms: vec![1.0, 2.0, 3.0],
            per_residue_terms: vec![
                ResidueTermScores {
                    entity_id: 0,
                    residue_index: 5,
                    terms: vec![1.5, -2.0, 0.25],
                },
                ResidueTermScores {
                    entity_id: 2,
                    residue_index: 11,
                    terms: vec![-3.0, 4.0, 8.0],
                },
            ],
        };
        let stored = StoredBreakdown {
            whole_pose_terms: report.whole_pose_terms.clone(),
            per_residue_terms: report.per_residue_terms.clone(),
        };

        assert_eq!(
            stored.weighted_per_residue(&term_names, &weights),
            report.weighted_per_residue(&weights),
        );
    }

    /// An unweighted term (absent from the map) contributes nothing.
    #[test]
    fn missing_weight_is_zero() {
        let weights: HashMap<String, f32> =
            [("a".to_string(), 2.0_f32)].into_iter().collect();
        let report = ScoreReport {
            term_names: vec!["a".to_string(), "unknown".to_string()],
            whole_pose_terms: vec![3.0, 100.0],
            per_residue_terms: Vec::new(),
        };
        // 3*2.0 + 100*0.0 = 6.0; the unknown term is dropped.
        assert_eq!(report.weighted_total(&weights), 6.0);
    }

    #[test]
    fn parse_wts_keeps_only_term_value_rows() {
        let src = "\
# beta_nov15
#   comment line
#METHOD_WEIGHTS ref 1.82 3.75
METHOD_WEIGHTS ref 1.32 3.25 -2.14
fa_atr 1
fa_rep 0.55
pro_close 0.0
INCLUDE_INTRA_RES_PROTEIN
NO_HB_ENV_DEP

";
        let weights = parse_wts(src);
        assert_eq!(weights.len(), 3);
        assert_eq!(weights.get("fa_atr"), Some(&1.0));
        assert_eq!(weights.get("fa_rep"), Some(&0.55));
        assert_eq!(weights.get("pro_close"), Some(&0.0));
        assert!(!weights.contains_key("METHOD_WEIGHTS"));
        assert!(!weights.contains_key("INCLUDE_INTRA_RES_PROTEIN"));
        assert!(!weights.contains_key("NO_HB_ENV_DEP"));
    }
}
