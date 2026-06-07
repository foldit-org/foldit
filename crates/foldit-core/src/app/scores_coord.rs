use crate::app::App;
use crate::history::CheckpointId;

impl App {
    /// Query every plugin's `score` op, merge totals into the head
    /// checkpoint (bumping `live_version` for the `GuiProjector` to pick
    /// up), and push per-residue scores directly to viso for
    /// color-by-score display modes. Off the `SessionUpdate` stream
    /// entirely: scores have two consumers (the `GuiProjector` via
    /// `HistorySyncCursor` and viso via a direct overlay push) and
    /// neither needs to ride the `SessionUpdate` stream.
    ///
    /// Synchronous (blocking) score poll. `tick` calls this each frame
    /// only until the first score lands, so the `InSession` gate
    /// flips promptly; once a score exists `tick` switches to the async
    /// path (`request_scores` + `poll_async_scores`). Dirty flags are set
    /// by `apply_score_reports` when a report actually applies.
    #[cfg(not(target_arch = "wasm32"))]
    pub(in crate::app) fn poll_plugin_scores(&mut self) {
        if !self.runner_client.has_orchestrator() {
            return;
        }
        self.refresh_scores();
    }

    /// Fan out the well-known `score` query across every plugin that
    /// registered it, merge totals into the head checkpoint, and push
    /// per-residue scores to the render engine for color-by-score modes.
    ///
    /// Called once at bootstrap (flips `has_initial_score()`, opening the
    /// loading gate) and again after every host-originated broadcast (so
    /// post-edit rescores update both the score widget and the residue
    /// colors).
    ///
    /// Today only Rosetta returns a non-trivial report. When more scorers
    /// come online the merge becomes app-wide -- the host stays generic
    /// either way.
    #[cfg(not(target_arch = "wasm32"))]
    fn refresh_scores(&mut self) {
        // Blocking score round-trip. Used only until the first score
        // lands, where a synchronous result keeps the InSession
        // flip deterministic. Once a score exists the caller switches to
        // `request_scores` + `poll_async_scores` so the render thread
        // never blocks on the worker.
        let reports = self.runner_client.collect_scores_blocking();
        self.apply_score_reports(reports);
    }

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
    /// blocking (bootstrap) and async (steady-state) score paths; no-op on an
    /// empty report set. Stamping emits `ScoresChanged`; the render projector
    /// re-derives the displayed per-residue colors from the session-owned
    /// breakdown on that signal (no direct viso push here anymore).
    #[cfg(not(target_arch = "wasm32"))]
    fn apply_score_reports(
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

        let raw = report.weighted_total(self.store.term_weights());
        let game = crate::scores::rosetta_raw_to_game(raw);
        // Install the alignment key before stamping the breakdown (idempotent
        // in steady state) so the write-time alignment invariant holds.
        self.store.set_term_names(report.term_names);
        let breakdown = crate::scores::StoredBreakdown {
            whole_pose_terms: report.whole_pose_terms,
            per_residue_terms: report.per_residue_terms,
        };
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
            let game = crate::scores::rosetta_raw_to_game(raw);
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
