//! Curation: pin / unpin / exclude / budget / head-score writes that
//! mutate checkpoint fields in place without changing DAG topology.

#[cfg(test)]
use super::HistoryBudget;
use super::{CheckpointId, History, HistoryError};

impl History {
    /// Pin a checkpoint as user-marked best.
    pub fn pin_checkpoint(&mut self, id: CheckpointId) -> Result<(), HistoryError> {
        if !self.checkpoints.checkpoints.contains_key(id) {
            return Err(HistoryError::UnknownCheckpoint { id });
        }
        let _ = self.checkpoints.pinned.insert(id);
        Ok(())
    }

    /// Unpin a checkpoint.
    pub fn unpin_checkpoint(&mut self, id: CheckpointId) -> Result<(), HistoryError> {
        if !self.checkpoints.checkpoints.contains_key(id) {
            return Err(HistoryError::UnknownCheckpoint { id });
        }
        let _ = self.checkpoints.pinned.remove(&id);
        Ok(())
    }

    /// Set the "exclude from best" flag.
    pub fn set_exclude_from_best(
        &mut self,
        id: CheckpointId,
        exclude: bool,
    ) -> Result<(), HistoryError> {
        let ckpt = self
            .checkpoints
            .checkpoints
            .get_mut(id)
            .ok_or(HistoryError::UnknownCheckpoint { id })?;
        ckpt.exclude_from_best = exclude;
        Ok(())
    }

    /// Replace the eviction budget.
    #[cfg(test)]
    pub const fn set_budget(&mut self, budget: HistoryBudget) {
        self.checkpoints.budget = budget;
    }

    /// Stamp `raw_score` / `game_score` on the current head checkpoint
    /// in place. Bumps `live_version` only - DAG topology unchanged, no
    /// new checkpoint, no new snapshot. Idempotent on `(None, None)`.
    ///
    /// This is the right call for cycle-zero scoring during session init
    /// (Rosetta streams a score before the user takes any action). It
    /// avoids the pre-fix behavior where every init cycle pushed a fresh
    /// checkpoint on top of root + `AddEntity`. Returns `true` when a value
    /// was actually written (so the caller can emit a score-changed signal
    /// only on a real change); `false` on the `(None, None)` no-op.
    // The head checkpoint id is a maintained invariant; it always resolves.
    #[allow(clippy::expect_used)]
    pub fn set_head_scores(
        &mut self,
        raw_score: Option<f64>,
        game_score: Option<f64>,
        breakdown: Option<crate::scores::StoredBreakdown>,
    ) -> bool {
        if raw_score.is_none() && game_score.is_none() {
            return false;
        }
        let head_id = self.checkpoints.head;
        let ckpt = self
            .checkpoints
            .checkpoints
            .get_mut(head_id)
            .expect("head checkpoint must exist");
        if let Some(s) = raw_score {
            ckpt.raw_score = Some(s);
        }
        if let Some(s) = game_score {
            ckpt.game_score = Some(s);
        }
        // The breakdown rides the scalar-score write (same node, same
        // call); a `Some` overwrites, a `None` leaves the prior one.
        if breakdown.is_some() {
            ckpt.breakdown = breakdown;
        }
        self.live_version = self.live_version.saturating_add(1);

        if cfg!(debug_assertions) {
            self.assert_invariant();
        }
        true
    }
}
