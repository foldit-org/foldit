use crate::app::App;
use crate::history::CheckpointId;

/// Sum the RAW score bonus of every `exposed_count` objective met at the
/// given exposed-hydrophobic `count`. An `exposed_count` objective awards
/// its `bonus` when `count < max` (the legacy `ExposedCount` filter: max=1
/// means the win is `count == 0`), else `0`. An objective with no `max`
/// (malformed) and any non-`exposed_count` kind contribute nothing
/// (forward-compatible: an unknown objective type parses but is inert). The
/// result is a RAW delta the score path folds in before the raw->game map.
#[must_use]
pub(in crate::app) fn exposed_count_bonus(
    objectives: &[crate::puzzle::Objective],
    count: u32,
) -> f64 {
    objectives
        .iter()
        .filter(|o| o.kind == "exposed_count")
        .filter_map(|o| o.max.map(|max| (max, o.bonus)))
        .map(|(max, bonus)| if count < max { f64::from(bonus) } else { 0.0 })
        .sum()
}

impl App {
    /// Fire a non-blocking `score` query at every provider with no query
    /// already in flight. The reply lands on a stored receiver drained by
    /// [`Self::poll_async_scores`]; the render thread never blocks. One
    /// outstanding query per provider coalesces a fast pose stream
    /// against a slow scorer.
    #[cfg(not(target_arch = "wasm32"))]
    pub(in crate::app) fn request_scores(&mut self) {
        self.runner_client.request_scores();
    }

    /// Drain whatever async `score` replies have arrived and apply them.
    /// Non-blocking; no-op when nothing is ready.
    #[cfg(not(target_arch = "wasm32"))]
    pub(in crate::app) fn poll_async_scores(&mut self) {
        let reports = self.runner_client.poll_score_results();
        self.apply_score_reports(reports);
    }

    /// Weight a score report and stamp the weighted total + the RAW per-term
    /// breakdown onto the current composition node. Shared tail of the
    /// async whole-assembly and composition score paths; no-op on an empty
    /// report set. Stamping emits `ScoresChanged`; the render projector
    /// re-derives the displayed per-residue colors from the session-owned
    /// breakdown on that signal (no direct viso push here anymore).
    #[cfg(not(target_arch = "wasm32"))]
    pub(in crate::app) fn apply_score_reports(
        &mut self,
        reports: std::collections::HashMap<String, crate::scores::ScoreReport>,
    ) {
        if reports.is_empty() {
            return;
        }

        // Today only one plugin (Rosetta) returns a non-trivial report, and
        // the session holds a single `term_names` alignment key, so a single
        // breakdown is the source of truth. Pick the first report: its
        // weighted total drives the score widget and its RAW terms become the
        // session-owned breakdown the render projector re-derives colors from.
        // (When multiple plugins score per-residue a merge strategy will be
        // needed; until then the first report wins, matching the previous
        // total selection.)
        let mut chosen: Option<crate::scores::ScoreReport> = None;
        for (plugin_id, report) in reports {
            let weighted_total = report.weighted_total(self.store.term_weights());
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

        // A content-empty report carries nothing to stamp. The session goes
        // live before the scorer's pose is built, so an early query lands an
        // empty report in that window; stamping it would mint a hollow
        // breakdown (no terms, no per-residue colors) that flips the "scored"
        // state and leaves the backbone gray until the next real score. Skip
        // it and leave the gauge at "not scored yet". Same predicate the
        // blocking load-time scorer uses.
        if report.term_names.is_empty() && report.per_residue_terms.is_empty() {
            return;
        }

        let (raw, game, breakdown) = self.prepare_score_stamp(report);
        // Whole-assembly score of the worker's live pose. With exactly one
        // edit open, the live pose IS that edit's composition (its tentative +
        // peers' committed heads), so the total + breakdown are the edit's →
        // stamp the edit. With zero or >=2 edits open, stamp the committed
        // head; the >=2 case is transiently imperfect for live display (each
        // open edit keeps its last value) but exact per-edit values still land
        // at commit via the commit-stamp.
        match self.store.sole_pending_request_id() {
            Some(rid) => self
                .store
                .set_edit_scores(rid, Some(raw), Some(game), Some(breakdown)),
            None => self
                .store
                .set_head_scores(Some(raw), Some(game), Some(breakdown)),
        }
    }

    /// Weight a report and resolve it into the `(raw, game, breakdown)` triple
    /// the score mutators stamp. Installs the alignment key (`set_term_names`,
    /// idempotent in steady state) before the breakdown is built so the
    /// write-time alignment invariant holds. Shared by `apply_score_reports`
    /// and the synchronous load-time stamp.
    #[cfg(not(target_arch = "wasm32"))]
    pub(in crate::app) fn prepare_score_stamp(
        &mut self,
        report: crate::scores::ScoreReport,
    ) -> (f64, f64, crate::scores::StoredBreakdown) {
        let raw = report.weighted_total(self.store.term_weights());
        // Fold the loaded puzzle's met-objective RAW bonus into the headline
        // game score before the raw->game map (game = raw_to_game(raw +
        // bonus)); `raw` itself stays the true rosetta value (free-form
        // display), only `game` carries the bonus. `0.0` outside a puzzle.
        let game =
            crate::scores::rosetta_raw_to_game(raw + self.store.objective_bonus());
        self.store.set_term_names(report.term_names);
        let breakdown = crate::scores::StoredBreakdown {
            whole_pose_terms: report.whole_pose_terms,
            per_residue_terms: report.per_residue_terms,
        };
        (raw, game, breakdown)
    }

    /// Fire a composition score for the committed union of `ckpt_id` under a
    /// fresh `request_id`, routing the reply to stamp that (now-immutable)
    /// checkpoint. Called right after a user-action commit so the new
    /// checkpoint gets a correctly-attributed score even when a peer edit is
    /// still open (so the idle whole-assembly path is not the one running).
    #[cfg(not(target_arch = "wasm32"))]
    pub(in crate::app) fn score_committed_checkpoint(&mut self, ckpt_id: CheckpointId) {
        let Some(rid) = self.runner_client.alloc_request_id() else {
            return;
        };
        let Some(assembly) = self.store.checkpoint_assembly(ckpt_id) else {
            return;
        };
        let Ok(bytes) = molex::ops::wire::serialize_assembly(&assembly) else {
            log::warn!("[App] commit-stamp serialize failed for checkpoint {ckpt_id:?}");
            return;
        };
        self.runner_client.score_composition(bytes, rid);
        let _ = self.score_targets.insert(rid, ckpt_id);
    }

    /// Drain composition-score replies and stamp each commit-time checkpoint
    /// (its weighted total + RAW per-term breakdown) via the `request_id`
    /// map (`set_checkpoint_scores`). A `request_id` absent from the map is a
    /// stale reply (its target was aborted/reset before the score returned)
    /// and is dropped: there is no node to stamp it on, and pushing its colors
    /// would be the off-display push this inversion removes. Stamping emits
    /// `ScoresChanged`; the render projector re-derives the displayed colors
    /// from whichever node is displayed. The raw REU → game-points map applies
    /// here too, so composition scores never display raw REU.
    #[cfg(not(target_arch = "wasm32"))]
    pub(in crate::app) fn poll_composition_scores(&mut self) {
        let replies = self.runner_client.poll_composition_scores();
        if replies.is_empty() {
            return;
        }
        for (rid, report) in replies {
            let Some(ckpt_id) = self.score_targets.get(&rid).copied() else {
                continue;
            };
            let raw = report.weighted_total(self.store.term_weights());
            // Fold the met-objective RAW bonus into the game score (same as
            // `prepare_score_stamp`), so a composition / commit-stamp score
            // and the at-rest headline agree on the bonus. `raw` stays the
            // true rosetta value; `0.0` outside a puzzle.
            let game =
                crate::scores::rosetta_raw_to_game(raw + self.store.objective_bonus());
            // Install the alignment key before stamping the breakdown.
            self.store.set_term_names(report.term_names);
            let breakdown = crate::scores::StoredBreakdown {
                whole_pose_terms: report.whole_pose_terms,
                per_residue_terms: report.per_residue_terms,
            };
            self.store
                .set_checkpoint_scores(ckpt_id, Some(raw), Some(game), Some(breakdown));
            let _ = self.score_targets.remove(&rid);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::float_cmp,
        reason = "exact-constant assertions on deterministic bonus returns"
    )]

    use super::exposed_count_bonus;
    use crate::puzzle::Objective;

    fn exposed_count(max: u32, bonus: f32) -> Objective {
        Objective {
            kind: "exposed_count".to_owned(),
            max: Some(max),
            bonus,
        }
    }

    #[test]
    fn bonus_awarded_below_max() {
        // max=1: the win is count==0; count 0 < 1 awards the bonus.
        let objs = [exposed_count(1, -100.0)];
        assert_eq!(exposed_count_bonus(&objs, 0), -100.0);
    }

    #[test]
    fn no_bonus_at_or_above_max() {
        let objs = [exposed_count(1, -100.0)];
        assert_eq!(exposed_count_bonus(&objs, 1), 0.0);
        assert_eq!(exposed_count_bonus(&objs, 5), 0.0);
    }

    #[test]
    fn unknown_kind_is_inert() {
        let objs = [Objective {
            kind: "some_future_kind".to_owned(),
            max: Some(1),
            bonus: -100.0,
        }];
        assert_eq!(exposed_count_bonus(&objs, 0), 0.0);
    }

    #[test]
    fn empty_objectives_yield_zero() {
        assert_eq!(exposed_count_bonus(&[], 0), 0.0);
    }
}
