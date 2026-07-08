//! Owns the score-term weights/names + met-filter bonus, the in-flight
//! composition-score targets, and the score-stamp methods.

use std::collections::HashMap;

use crate::history::CheckpointId;
use crate::runner_client::RunnerClient;
use crate::scores::{ScoreReport, StoredBreakdown};
use crate::session::Session;

/// Score-term state plus the request-id -> checkpoint join for in-flight
/// composition scores, and the methods that weight a score report and stamp
/// it onto the session.
pub struct ScoreCoordinator {
    score_targets: HashMap<u64, CheckpointId>,
    /// Score-term weight map.
    term_weights: HashMap<String, f32>,
    /// Score-term name list (alignment key for every stored breakdown).
    term_names: Vec<String>,
    /// Labeled raw score bonus from the loaded puzzle's met filters.
    filter_bonus: Vec<(String, f64)>,
}

impl Default for ScoreCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

impl ScoreCoordinator {
    pub(in crate::app) fn new() -> Self {
        Self {
            score_targets: HashMap::new(),
            term_weights: HashMap::new(),
            term_names: Vec::new(),
            filter_bonus: Vec::new(),
        }
    }

    /// The active score-term weight map. Empty until the App loads the default
    /// at init.
    pub const fn term_weights(&self) -> &HashMap<String, f32> {
        &self.term_weights
    }

    /// The score-term name list. Empty until the first score report lands.
    pub fn term_names(&self) -> &[String] {
        &self.term_names
    }

    /// The labeled met-filter raw bonus breakdown.
    pub(in crate::app) fn filter_bonus(&self) -> &[(String, f64)] {
        &self.filter_bonus
    }

    /// The summed raw score bonus across every met filter.
    pub(in crate::app) fn filter_bonus_total(&self) -> f64 {
        self.filter_bonus.iter().map(|(_, v)| v).sum()
    }

    /// Install the score-term weight map.
    pub(in crate::app) fn set_term_weights(&mut self, weights: HashMap<String, f32>) {
        self.term_weights = weights;
    }

    /// Overlay a puzzle's weight patch onto a base term-weight map: every
    /// patched term replaces (or adds) its base entry.
    #[cfg(not(target_arch = "wasm32"))]
    pub(in crate::app) fn overlay_weights(
        mut base: HashMap<String, f32>,
        patch: Option<&HashMap<String, f32>>,
    ) -> HashMap<String, f32> {
        if let Some(patch) = patch {
            for (name, &w) in patch {
                base.insert(name.clone(), w);
            }
        }
        base
    }

    /// Install the score-term name list.
    #[cfg(not(target_arch = "wasm32"))]
    fn set_term_names(&mut self, names: Vec<String>) {
        self.term_names = names;
    }

    /// Upsert one labeled raw-bonus entry, replacing an existing entry with the
    /// same label or appending a new one; a `0.0` value drops the label. Every
    /// other filter's entry stays intact, so objectives updated on independent
    /// schedules (the exposed-count reply and the async R-free result) coexist
    /// in the one summed raw-bonus channel. Every writer of this channel must go
    /// through here, never a bulk replace, or it would clobber the others.
    pub(crate) fn set_filter_bonus_entry(&mut self, label: &str, value: f64) {
        self.filter_bonus.retain(|(k, _)| k != label);
        if value != 0.0 {
            self.filter_bonus.push((label.to_owned(), value));
        }
    }

    /// Clear the met-filter raw bonus.
    pub(in crate::app) fn clear_filter_bonus(&mut self) {
        self.filter_bonus.clear();
    }

    /// Drop every composition target.
    pub(in crate::app) fn clear_targets(&mut self) {
        self.score_targets.clear();
    }

    /// Remove and return the checkpoint a spent `request_id` was scoring.
    pub(in crate::app) fn remove_target(&mut self, request_id: u64) -> Option<CheckpointId> {
        self.score_targets.remove(&request_id)
    }

    /// Drain this tick's async whole-assembly and composition score replies
    /// and stamp them. Non-blocking; no-op when nothing is ready.
    #[cfg(not(target_arch = "wasm32"))]
    pub(in crate::app) fn poll(&mut self, rc: &mut RunnerClient, s: &mut Session) {
        let reports = rc.poll_score_results();
        self.apply_score_reports(s, reports);

        for (rid, report) in rc.poll_composition_scores() {
            let Some(ckpt_id) = self.score_targets.get(&rid).copied() else {
                continue;
            };
            let raw = report.weighted_total(self.term_weights());
            let forwarded_bonus: f64 = report
                .bonus_breakdown
                .iter()
                .map(|(_, v)| f64::from(*v))
                .sum();
            let game = crate::scores::rosetta_raw_to_game(
                raw + self.filter_bonus_total() + forwarded_bonus,
            );
            self.set_term_names(report.term_names);
            let breakdown = StoredBreakdown {
                whole_pose_terms: report.whole_pose_terms,
                per_residue_terms: report.per_residue_terms,
            };
            self.debug_assert_breakdown_alignment(&breakdown);
            s.set_checkpoint_scores(ckpt_id, Some(raw), Some(game), Some(breakdown));
            let _ = self.score_targets.remove(&rid);
        }
    }

    /// Weight a score report set and stamp the chosen report's weighted total
    /// and RAW per-term breakdown onto the open edit (sole pending) or the
    /// committed head. No-op on an empty or content-empty report set.
    #[cfg(not(target_arch = "wasm32"))]
    pub(in crate::app) fn apply_score_reports(
        &mut self,
        s: &mut Session,
        reports: HashMap<String, ScoreReport>,
    ) {
        if reports.is_empty() {
            return;
        }

        // Pick the first report.
        let mut chosen: Option<ScoreReport> = None;
        for (plugin_id, report) in reports {
            let weighted_total = report.weighted_total(self.term_weights());
            log::info!(
                "[App] score from {plugin_id}: total={weighted_total} terms={} per_residue={}",
                report.term_names.len(),
                report.per_residue_terms.len()
            );
            if chosen.is_none() {
                chosen = Some(report);
            }
        }
        let Some(report) = chosen else {
            return;
        };

        // Skip a content-empty report so it never mints a hollow breakdown.
        if report.term_names.is_empty() && report.per_residue_terms.is_empty() {
            return;
        }

        let (raw, game, breakdown) = self.prepare_score_stamp(report);
        match s.sole_pending_request_id() {
            Some(rid) => s.set_edit_scores(rid, Some(raw), Some(game), Some(breakdown)),
            None => s.set_head_scores(Some(raw), Some(game), Some(breakdown)),
        }
    }

    /// Weight a report into the `(raw, game, breakdown)` triple the score
    /// mutators stamp, installing the term-name alignment key first. The
    /// met-filter RAW bonus (native + forwarded) folds into `game` before the
    /// raw->game map; `raw` stays the true rosetta value.
    #[cfg(not(target_arch = "wasm32"))]
    pub(in crate::app) fn prepare_score_stamp(
        &mut self,
        report: ScoreReport,
    ) -> (f64, f64, StoredBreakdown) {
        let raw = report.weighted_total(self.term_weights());
        let forwarded_bonus: f64 = report
            .bonus_breakdown
            .iter()
            .map(|(_, v)| f64::from(*v))
            .sum();
        let filter_bonus = self.filter_bonus_total() + forwarded_bonus;
        if !self.filter_bonus().is_empty() || !report.bonus_breakdown.is_empty() {
            log::debug!(
                "[App] filter bonus: native={:?} forwarded={:?} (sum={filter_bonus})",
                self.filter_bonus(),
                report.bonus_breakdown,
            );
        }
        let game = crate::scores::rosetta_raw_to_game(raw + filter_bonus);
        self.set_term_names(report.term_names);
        let breakdown = StoredBreakdown {
            whole_pose_terms: report.whole_pose_terms,
            per_residue_terms: report.per_residue_terms,
        };
        self.debug_assert_breakdown_alignment(&breakdown);
        (raw, game, breakdown)
    }

    /// Debug-only: a stored breakdown's term rows must match `term_names`
    /// length.
    #[cfg(not(target_arch = "wasm32"))]
    fn debug_assert_breakdown_alignment(&self, breakdown: &StoredBreakdown) {
        debug_assert_eq!(
            breakdown.whole_pose_terms.len(),
            self.term_names.len(),
            "stored whole_pose_terms must align to term_names",
        );
        for rts in &breakdown.per_residue_terms {
            debug_assert_eq!(
                rts.terms.len(),
                self.term_names.len(),
                "stored per-residue terms must align to term_names",
            );
        }
    }

    /// Fire a composition score for the committed union of `ckpt_id` under a
    /// fresh request id, routing the reply to stamp that now-immutable
    /// checkpoint.
    #[cfg(not(target_arch = "wasm32"))]
    pub(in crate::app) fn score_committed_checkpoint(
        &mut self,
        rc: &mut RunnerClient,
        s: &Session,
        ckpt_id: CheckpointId,
    ) {
        let Some(rid) = rc.alloc_request_id() else {
            return;
        };
        let Some(assembly) = s.checkpoint_assembly(ckpt_id) else {
            return;
        };
        let Ok(bytes) = assembly.to_bytes() else {
            log::warn!("[App] commit-stamp serialize failed for checkpoint {ckpt_id:?}");
            return;
        };
        rc.score_composition(bytes, rid);
        let _ = self.score_targets.insert(rid, ckpt_id);
    }
}
