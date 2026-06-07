//! Score paths.
//!
//! Forward the well-known `score` query to the orchestrator, building
//! the default dispatch context internally so the score query covers
//! the whole assembly. App owns merging the returned reports into the
//! head checkpoint and pushing per-residue colors. Reports cross the
//! facade as the core-owned `crate::scores::ScoreReport`; the proto type
//! is named only inside this module's `From` conversion below.

#[cfg(not(target_arch = "wasm32"))]
use super::RunnerClient;

#[cfg(not(target_arch = "wasm32"))]
impl RunnerClient {
    /// Blocking score round-trip: fan the `score` query across every
    /// provider and return one report per provider that replied. Used
    /// until the first score lands, where a synchronous result keeps the
    /// load gate deterministic. Empty map when no orchestrator is wired up.
    pub(crate) fn collect_scores_blocking(
        &mut self,
    ) -> std::collections::HashMap<String, crate::scores::ScoreReport> {
        use foldit_runner::orchestrator::DispatchContext;
        self.orchestrator
            .as_mut()
            .map(|orch| {
                orch.collect_scores(&DispatchContext::default())
                    .into_iter()
                    .map(|(id, report)| (id, report.into()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Fire a non-blocking `score` query at every provider with none
    /// already in flight. Replies land on stored receivers drained by
    /// [`Self::poll_score_results`]. No-op when no orchestrator exists.
    pub(crate) fn request_scores(&mut self) {
        use foldit_runner::orchestrator::DispatchContext;
        if let Some(orch) = self.orchestrator.as_mut() {
            orch.request_scores(&DispatchContext::default());
        }
    }

    /// Drain whatever async `score` replies have arrived. Non-blocking;
    /// empty map when nothing is ready or no orchestrator exists.
    pub(crate) fn poll_score_results(
        &mut self,
    ) -> std::collections::HashMap<String, crate::scores::ScoreReport> {
        self.orchestrator
            .as_mut()
            .map(|orch| {
                orch.poll_score_results()
                    .into_iter()
                    .map(|(id, report)| (id, report.into()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Fire a composition-score request for `request_id`, carrying the
    /// ASSEM02 bytes of the composition to score (one open edit's lanes
    /// over its peers' committed heads, or a committed checkpoint's union).
    /// Replies land on receivers drained by
    /// [`Self::poll_composition_scores`]. No-op when no orchestrator exists.
    pub(crate) fn score_composition(&mut self, assembly: Vec<u8>, request_id: u64) {
        if let Some(orch) = self.orchestrator.as_mut() {
            orch.score_composition(assembly, request_id);
        }
    }

    /// Drain whatever composition-score replies have arrived, each tagged
    /// with the `request_id` the host correlated it under. Non-blocking;
    /// empty when nothing is ready or no orchestrator exists.
    pub(crate) fn poll_composition_scores(&mut self) -> Vec<(u64, crate::scores::ScoreReport)> {
        self.orchestrator
            .as_mut()
            .map(|orch| {
                orch.poll_composition_scores()
                    .into_iter()
                    .map(|(rid, report)| (rid, report.into()))
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Convert the runner's proto score report into the core-owned twin at the
/// facade boundary. Structural copy: the raw `term_names` / `whole_pose_terms`
/// move as-is; `per_residue_terms` is rebuilt, dropping any entry with no
/// residue ref (the proto field is optional). That skip preserves the
/// historical `residue.as_ref() else continue` behavior, relocated from the
/// consumer.
impl From<foldit_runner::proto::plugin::ScoreReport> for crate::scores::ScoreReport {
    fn from(report: foldit_runner::proto::plugin::ScoreReport) -> Self {
        Self {
            term_names: report.term_names,
            whole_pose_terms: report.whole_pose_terms,
            per_residue_terms: report
                .per_residue_terms
                .into_iter()
                .filter_map(|rts| {
                    rts.residue.map(|rref| {
                        // proto entity ids are uint64 on the wire;
                        // molex::EntityId is u32.
                        #[allow(clippy::cast_possible_truncation)]
                        let entity_id =
                            molex::EntityId::from_raw(rref.entity_id as u32);
                        crate::scores::ResidueTermScores {
                            entity_id,
                            residue_index: rref.residue_index,
                            terms: rts.terms,
                        }
                    })
                })
                .collect(),
        }
    }
}
