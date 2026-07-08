//! Core-owned score types. Mirror what a plugin score report carries: the
//! RAW (unweighted) per-term energies (whole-pose and per-residue) plus the
//! term-name alignment key.
//!
//! Core owns the weighting: it multiplies the raw per-term energies by the
//! coordinator-held weight map to produce the weighted total and per-residue
//! scalars itself, which are what the app displays and colors by.
//!
//! Cross-platform: the blocking score path is reachable on wasm, so these
//! types, their conversion, and the weighting methods must build on every
//! target. Only the file-IO weight loader is native-gated.

use std::collections::HashMap;

/// One plugin's score for the assembly (or a scored composition): the RAW
/// (unweighted) per-term energies that core weights itself, plus the
/// term-name alignment key.
pub struct ScoreReport {
    /// Names of the raw terms, the alignment key for `whole_pose_terms` and
    /// each `ResidueTermScores::terms`. Same order, same length.
    pub term_names: Vec<String>,
    /// Raw (unweighted) whole-pose energy per term, aligned to `term_names`.
    pub whole_pose_terms: Vec<f32>,
    /// Raw (unweighted) per-residue energies, each `terms` aligned to
    /// `term_names`.
    pub per_residue_terms: Vec<ResidueTermScores>,
    /// Labeled puzzle-filter bonuses forwarded by the plugin, each
    /// `(kind, value)` in RAW rosetta energy (same sign convention as the
    /// energy terms). Separate from the weighted raw terms: the score path
    /// adds their sum into the headline game total alongside the native
    /// filter bonus. Empty for a free-form session or a puzzle that
    /// forwarded no filters.
    pub bonus_breakdown: Vec<(String, f32)>,
}

/// A single residue's RAW per-term energies, addressed by
/// `(entity_id, residue_index)`. `terms` is aligned to
/// [`ScoreReport::term_names`].
#[derive(Debug, Clone)]
pub struct ResidueTermScores {
    pub entity_id: molex::EntityId,
    pub residue_index: u32,
    pub terms: Vec<f32>,
}

/// The RAW (unweighted) per-term breakdown retained on a history node, the
/// session-owned source of truth for per-residue coloring: whole-pose and
/// per-residue energies aligned to the session's `term_names`.
#[derive(Debug, Clone)]
pub struct StoredBreakdown {
    /// Raw (unweighted) whole-pose energy per term, aligned to the
    /// session's `term_names`.
    pub whole_pose_terms: Vec<f32>,
    /// Raw (unweighted) per-residue energies, each `terms` aligned to the
    /// session's `term_names`.
    pub per_residue_terms: Vec<ResidueTermScores>,
}

impl StoredBreakdown {
    /// Core-weighted per-residue scalars, taking `term_names` externally
    /// (the alignment key lives on the `Session`, not on the stored form).
    /// One `(entity_id, residue_index, score)` per [`ResidueTermScores`],
    /// where `score = Σ terms[i] * weights[term_names[i]]` (missing weights
    /// `0.0`).
    pub fn weighted_per_residue(
        &self,
        term_names: &[String],
        weights: &HashMap<String, f32>,
    ) -> Vec<(molex::EntityId, u32, f64)> {
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
pub fn rosetta_raw_to_game(raw: f64) -> f64 {
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
pub fn parse_wts(src: &str) -> HashMap<String, f32> {
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
            weights.insert(tokens[0].to_owned(), value);
        }
    }
    weights
}

/// Load + parse the default `ref2015_cart` weight map. Resolves
/// `assets/scoring/ref2015_cart.wts` by walking up from the running
/// executable (same shape as [`crate::puzzle_load::levels_root`], covering test,
/// dev, and installed binary layouts). Returns an `Err` string the caller
/// can log if no ancestor carries the asset or the file is unreadable.
#[cfg(not(target_arch = "wasm32"))]
pub fn load_default_term_weights() -> Result<HashMap<String, f32>, String> {
    // Explicit override (set by a packaged bundle whose assets are not an
    // ancestor of the exe, e.g. a macOS .app's Contents/Resources). Points at
    // the `scoring` dir.
    if let Some(dir) = std::env::var_os("FOLDIT_SCORING_DIR") {
        let candidate = std::path::PathBuf::from(dir).join("ref2015_cart.wts");
        if candidate.is_file() {
            let src = std::fs::read_to_string(&candidate)
                .map_err(|e| format!("reading {}: {e}", candidate.display()))?;
            return Ok(parse_wts(&src));
        }
    }
    let exe = std::env::current_exe().map_err(|e| format!("current_exe lookup failed: {e}"))?;
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

/// Read a `toml::Value` as an `f64`, accepting an integer or float literal.
/// Anything else (string, bool, ...) yields `None`. Filter thresholds and
/// bonuses are small magnitudes, so the integer widening is exact in practice.
#[allow(
    clippy::cast_precision_loss,
    reason = "filter thresholds/bonuses are small integers; widening is exact"
)]
const fn toml_number(value: &toml::Value) -> Option<f64> {
    match value {
        toml::Value::Integer(n) => Some(*n as f64),
        toml::Value::Float(f) => Some(*f),
        _ => None,
    }
}

/// Sum the RAW score bonus of every native `ExposedCount` filter met at the
/// given exposed-hydrophobic `count`. A native `ExposedCount` filter awards its
/// `bonus` param when `count` is below `max_exposed_hydrophobics`
/// (`max_exposed_hydrophobics = 1` means the win is `count == 0`), else `0`. A
/// filter that names a `plugin` (forwarded, not scored here), one missing the
/// threshold or the bonus param, and any non-`ExposedCount` kind all contribute
/// nothing (forward-compatible: an unknown filter type parses but is inert). The
/// result is a RAW delta the score path folds in before the raw->game map.
#[must_use]
pub fn exposed_count_bonus(filters: &[crate::puzzle_toml::FilterSpec], count: u32) -> f64 {
    filters
        .iter()
        .filter(|f| f.kind == "ExposedCount" && f.plugin.is_none())
        .filter_map(|f| {
            let max = toml_number(f.params.get("max_exposed_hydrophobics")?)?;
            let bonus = toml_number(f.params.get("bonus")?)?;
            Some((max, bonus))
        })
        .map(|(max, bonus)| if f64::from(count) < max { bonus } else { 0.0 })
        .sum()
}

/// A native `rfree_bonus` filter's parameters: the game-score objective that
/// rewards driving the crystallographic R-free below `target`. `weight` scales
/// the reward and `exponent` shapes the power curve.
#[cfg(not(target_arch = "wasm32"))]
pub struct RFreeBonus {
    pub target: f64,
    pub weight: f64,
    pub exponent: f64,
}

#[cfg(not(target_arch = "wasm32"))]
impl RFreeBonus {
    /// Game-score points this objective awards at the given `r_free`:
    /// `weight * (target - r_free)^exponent` while `r_free` is below `target`,
    /// and `0` once `r_free` reaches or exceeds it. Always non-negative.
    #[must_use]
    pub fn reward_game(&self, r_free: f64) -> f64 {
        self.weight * (self.target - r_free).max(0.0).powf(self.exponent)
    }
}

/// The native `rfree_bonus` filter's parameters, or `None` when the puzzle
/// declares no such native filter. Reads `target` and `weight` (both required);
/// `exponent` defaults to `2.0` when absent. A filter that names a `plugin` is
/// forwarded, not scored here, so it yields `None`; a non-`rfree_bonus` kind and
/// one missing `target` or `weight` yield `None` too (forward-compatible: an
/// unknown filter type parses but is inert).
#[cfg(not(target_arch = "wasm32"))]
#[must_use]
pub fn rfree_bonus_spec(filters: &[crate::puzzle_toml::FilterSpec]) -> Option<RFreeBonus> {
    filters
        .iter()
        .filter(|f| f.kind == "rfree_bonus" && f.plugin.is_none())
        .find_map(|f| {
            let target = toml_number(f.params.get("target")?)?;
            let weight = toml_number(f.params.get("weight")?)?;
            let exponent = f.params.get("exponent").and_then(toml_number).unwrap_or(2.0);
            Some(RFreeBonus {
                target,
                weight,
                exponent,
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Weighting anchor: core multiplies the raw per-term energies by the
    /// weight set to produce the displayed whole-pose total and per-residue
    /// scalars.
    // Exact comparison is correct: every input is exactly representable and
    // the products/sums land with no rounding (25.0, 8.0).
    #[allow(clippy::float_cmp)]
    #[test]
    fn weighting_matches_preweighted_fields() {
        let weights: HashMap<String, f32> = [("a".to_owned(), 0.5_f32), ("b".to_owned(), 1.0_f32)]
            .into_iter()
            .collect();

        // whole pose: 10*0.5 + 20*1.0 = 25.0.
        // per residue: 4*0.5 + 6*1.0 = 8.0.
        let report = ScoreReport {
            term_names: vec!["a".to_owned(), "b".to_owned()],
            whole_pose_terms: vec![10.0, 20.0],
            per_residue_terms: vec![ResidueTermScores {
                entity_id: molex::EntityId::from_raw(7),
                residue_index: 3,
                terms: vec![4.0, 6.0],
            }],
            bonus_breakdown: Vec::new(),
        };

        assert_eq!(report.weighted_total(&weights), 25.0);

        let stored = StoredBreakdown {
            whole_pose_terms: report.whole_pose_terms,
            per_residue_terms: report.per_residue_terms,
        };
        let per_residue = stored.weighted_per_residue(&report.term_names, &weights);
        assert_eq!(per_residue.len(), 1);
        let (entity_id, residue_index, score) = per_residue[0];
        assert_eq!(entity_id, molex::EntityId::from_raw(7));
        assert_eq!(residue_index, 3);
        assert_eq!(score, 8.0);
    }

    /// Per-residue scalars from a `StoredBreakdown` against an external
    /// `term_names`: `score = Σ terms[i] * weights[term_names[i]]`, one entry
    /// per residue in lane order.
    #[test]
    fn stored_breakdown_weighting_matches_report() {
        let weights: HashMap<String, f32> = [
            ("fa_atr".to_owned(), 1.0_f32),
            ("fa_rep".to_owned(), 0.55_f32),
            ("hbond".to_owned(), -0.5_f32),
        ]
        .into_iter()
        .collect();
        let term_names = vec!["fa_atr".to_owned(), "fa_rep".to_owned(), "hbond".to_owned()];

        let report = ScoreReport {
            term_names: term_names.clone(),
            whole_pose_terms: vec![1.0, 2.0, 3.0],
            per_residue_terms: vec![
                ResidueTermScores {
                    entity_id: molex::EntityId::from_raw(0),
                    residue_index: 5,
                    terms: vec![1.5, -2.0, 0.25],
                },
                ResidueTermScores {
                    entity_id: molex::EntityId::from_raw(2),
                    residue_index: 11,
                    terms: vec![-3.0, 4.0, 8.0],
                },
            ],
            bonus_breakdown: Vec::new(),
        };
        let stored = StoredBreakdown {
            whole_pose_terms: report.whole_pose_terms,
            per_residue_terms: report.per_residue_terms,
        };

        let weighted = stored.weighted_per_residue(&term_names, &weights);
        assert_eq!(weighted.len(), 2);

        // 1.5*1.0 + (-2.0)*0.55 + 0.25*(-0.5) = 0.275
        assert_eq!(weighted[0].0, molex::EntityId::from_raw(0));
        assert_eq!(weighted[0].1, 5);
        assert!((weighted[0].2 - 0.275).abs() < 1e-6);

        // -3.0*1.0 + 4.0*0.55 + 8.0*(-0.5) = -4.8
        assert_eq!(weighted[1].0, molex::EntityId::from_raw(2));
        assert_eq!(weighted[1].1, 11);
        assert!((weighted[1].2 - (-4.8)).abs() < 1e-6);
    }

    /// An unweighted term (absent from the map) contributes nothing.
    // Exact comparison is correct: 3*2.0 + 100*0.0 = 6.0 with no rounding.
    #[allow(clippy::float_cmp)]
    #[test]
    fn missing_weight_is_zero() {
        let weights: HashMap<String, f32> = std::iter::once(("a".to_owned(), 2.0_f32)).collect();
        let report = ScoreReport {
            term_names: vec!["a".to_owned(), "unknown".to_owned()],
            whole_pose_terms: vec![3.0, 100.0],
            per_residue_terms: Vec::new(),
            bonus_breakdown: Vec::new(),
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

    mod exposed_count {
        #![allow(
            clippy::float_cmp,
            reason = "exact-constant assertions on deterministic bonus returns"
        )]

        use super::exposed_count_bonus;
        use crate::puzzle_toml::FilterSpec;
        use std::collections::BTreeMap;

        /// Build an `ExposedCount` filter with its threshold + bonus params, the
        /// way the transcribed `[[puzzle.filter]]` block stores them.
        fn exposed_count(max: i64, bonus: i64) -> FilterSpec {
            let mut params = BTreeMap::new();
            params.insert(
                "max_exposed_hydrophobics".to_owned(),
                toml::Value::Integer(max),
            );
            params.insert("bonus".to_owned(), toml::Value::Integer(bonus));
            FilterSpec {
                kind: "ExposedCount".to_owned(),
                plugin: None,
                params,
            }
        }

        #[test]
        fn bonus_awarded_below_max() {
            // max=1: the win is count==0; count 0 < 1 awards the bonus.
            let filters = [exposed_count(1, -100)];
            assert_eq!(exposed_count_bonus(&filters, 0), -100.0);
        }

        #[test]
        fn no_bonus_at_or_above_max() {
            let filters = [exposed_count(1, -100)];
            assert_eq!(exposed_count_bonus(&filters, 1), 0.0);
            assert_eq!(exposed_count_bonus(&filters, 5), 0.0);
        }

        #[test]
        fn unknown_kind_is_inert() {
            let mut filter = exposed_count(1, -100);
            filter.kind = "some_future_kind".to_owned();
            assert_eq!(exposed_count_bonus(&[filter], 0), 0.0);
        }

        #[test]
        fn forwarded_filter_is_inert() {
            // A filter that names a plugin is forwarded for scoring, not
            // evaluated here, so it contributes no native bonus.
            let mut filter = exposed_count(1, -100);
            filter.plugin = Some("rosetta".to_owned());
            assert_eq!(exposed_count_bonus(&[filter], 0), 0.0);
        }

        #[test]
        fn empty_filters_yield_zero() {
            assert_eq!(exposed_count_bonus(&[], 0), 0.0);
        }
    }
}
