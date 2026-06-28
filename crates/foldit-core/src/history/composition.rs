//! Composition-score reads and writes: stamping per-edit and committed
//! checkpoint scores, and reading the current composition node's scores,
//! breakdown, and entity payloads.

use std::sync::Arc;

use molex::MoleculeEntity;

use super::{CheckpointId, History};

impl History {
    /// Stamp scores on the open edit identified by `request_id`. The edit's
    /// accumulated score is what `do_commit` transfers onto the checkpoint
    /// it mints, so this is how a per-edit composition score reaches the
    /// committed node. Targeting the named edit (not "the first open one")
    /// keeps two concurrent edits' scores from colliding. Bumps
    /// `live_version` only; no DAG topology change. No-op when `request_id`
    /// names no open edit, or on `(None, None)`. Returns `true` when a
    /// value was actually written (so the caller can emit a score-changed
    /// signal only on a real change).
    pub fn set_edit_scores(
        &mut self,
        request_id: u64,
        raw_score: Option<f64>,
        game_score: Option<f64>,
        breakdown: Option<crate::scores::StoredBreakdown>,
    ) -> bool {
        if raw_score.is_none() && game_score.is_none() {
            return false;
        }
        if let Some(edit) = self.pending.get_mut(&request_id) {
            if let Some(s) = raw_score {
                edit.raw_score = Some(s);
            }
            if let Some(s) = game_score {
                edit.game_score = Some(s);
            }
            // The breakdown rides the scalar-score write (same node, same
            // call); a `Some` overwrites, a `None` leaves the prior one.
            if breakdown.is_some() {
                edit.breakdown = breakdown;
            }
            self.live_version = self.live_version.saturating_add(1);
            return true;
        }
        false
    }

    /// Stamp scores on the committed checkpoint `id` in place. Used by the
    /// commit-time composition score: the checkpoint composes the committed
    /// union at commit, the score lands once the reply returns, and this
    /// stamps the now-immutable node it was scored for. Bumps `live_version`
    /// only. No-op on unknown `id` or `(None, None)`. Returns `true` when a
    /// value was actually written (so the caller can emit a score-changed
    /// signal only on a real change).
    pub fn set_checkpoint_scores(
        &mut self,
        id: CheckpointId,
        raw_score: Option<f64>,
        game_score: Option<f64>,
        breakdown: Option<crate::scores::StoredBreakdown>,
    ) -> bool {
        if raw_score.is_none() && game_score.is_none() {
            return false;
        }
        if let Some(ckpt) = self.checkpoints.checkpoints.get_mut(id) {
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
            self.recompute_best();
            return true;
        }
        false
    }

    /// Read the `(raw, game)` score of the current composition node: the
    /// first open pending edit if one exists, else the committed head
    /// checkpoint. The live-score read surface for the score widget; with
    /// per-edit composition scores each open edit holds its own correctly
    /// attributed score, so the first one is a meaningful display value.
    #[must_use]
    pub fn current_composition_scores(&self) -> (Option<f64>, Option<f64>) {
        self.pending.values().next().map_or_else(
            || {
                let head = &self.checkpoints.checkpoints[self.checkpoints.head];
                (head.raw_score, head.game_score)
            },
            |edit| (edit.raw_score, edit.game_score),
        )
    }

    /// The RAW per-term breakdown of the current composition node: the
    /// first open pending edit if one exists, else the committed head
    /// checkpoint. Same node-selection rule as
    /// [`Self::current_composition_scores`]; the render projector re-derives
    /// the displayed per-residue colors from it. `None` until a score with a
    /// breakdown has been stamped on that node.
    #[must_use]
    pub fn current_composition_breakdown(&self) -> Option<&crate::scores::StoredBreakdown> {
        self.pending.values().next().map_or_else(
            || {
                let head = &self.checkpoints.checkpoints[self.checkpoints.head];
                head.breakdown.as_ref()
            },
            |edit| edit.breakdown.as_ref(),
        )
    }

    /// The entities composing committed checkpoint `id` (its `entity_heads`
    /// snapshots), in canonical order. `None` when `id` is unknown.
    #[must_use]
    pub fn checkpoint_composition_entities(
        &self,
        id: CheckpointId,
    ) -> Option<Vec<Arc<MoleculeEntity>>> {
        let ckpt = self.checkpoints.checkpoint(id)?;
        let mut out = Vec::with_capacity(ckpt.entity_heads.len());
        for (eid, snap_id) in &ckpt.entity_heads {
            if let Some(lane) = self.lanes.get(eid) {
                if let Some(snap) = lane.snapshot(*snap_id) {
                    out.push(Arc::clone(&snap.payload));
                }
            }
        }
        Some(out)
    }
}
