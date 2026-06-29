//! Best-cursor recompute: `best` / `best_that_counts` over the
//! checkpoint graph.

use super::{CheckpointId, FilterStatus, History};

impl History {
    /// Recompute `best` and `best_that_counts` cursors.
    /// `best` = highest `raw_score` across non-tentative, non-excluded
    /// checkpoints. `best_that_counts` adds the constraint
    /// `filter_status == Pass`.
    pub(super) fn recompute_best(&mut self) {
        let mut best: Option<(CheckpointId, f64)> = None;
        let mut best_counts: Option<(CheckpointId, f64)> = None;
        for (id, ckpt) in &self.checkpoints.checkpoints {
            if ckpt.exclude_from_best {
                continue;
            }
            if let Some(score) = ckpt.raw_score {
                if best.is_none_or(|(_, b)| score > b) {
                    best = Some((id, score));
                }
                if matches!(ckpt.filter_status, FilterStatus::Pass)
                    && best_counts.is_none_or(|(_, b)| score > b)
                {
                    best_counts = Some((id, score));
                }
            }
        }
        self.checkpoints.best = best.map(|(id, _)| id);
        self.checkpoints.best_that_counts = best_counts.map(|(id, _)| id);
    }
}
