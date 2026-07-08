//! Off-thread crystallographic R-free objective: the background thread runs
//! molex on the shared GPU device and reports `(r_work, r_free)` over a
//! channel; the game-score fold and the live readout happen here on the main
//! thread when the tick drains those events. Mirrors the b-factor refine
//! channel in [`crate::app::refine`].

use crate::app::App;
use crate::history::CheckpointId;

/// A committed checkpoint's R-free result from the background compute thread.
pub(in crate::app) struct RFreeEvent {
    pub checkpoint: CheckpointId,
    pub r_work: f64,
    pub r_free: f64,
}

impl App {
    /// Kick an R-free compute for a freshly committed checkpoint, when the
    /// loaded puzzle enables a native `rfree_bonus` filter and both the
    /// experimental data and the shared GPU device are present. Snapshots the
    /// committed head and builds the atom table on the main thread; the spawned
    /// thread runs molex only and sends one `RFreeEvent`. A no-op when any
    /// precondition is unmet, so every commit seam can call it unconditionally.
    pub(in crate::app) fn spawn_rfree_compute(&self, checkpoint: CheckpointId) {
        if self
            .store
            .puzzle()
            .and_then(|p| crate::scores::rfree_bonus_spec(&p.filters))
            .is_none()
        {
            return;
        }
        let Some((data, dev, _snapshot, table)) = self.xtal_job_inputs() else {
            return;
        };

        let tx = self.rfree_tx.clone();
        std::thread::spawn(move || {
            if let Some((r_work, r_free)) =
                molex::xtal::r_factors_from_atom_table_gpu(&data, &table, &dev)
            {
                let _ = tx.send(RFreeEvent {
                    checkpoint,
                    r_work,
                    r_free,
                });
            }
        });
    }

    /// Drain every pending R-free event on the main thread. The result for the
    /// current head folds its reward into the game score through the labeled
    /// filter-bonus channel and refreshes the live readout under the score bar;
    /// a result for a checkpoint a later commit has already superseded is
    /// dropped, since its own compute is in flight.
    pub(in crate::app) fn drain_rfree_events(&mut self) {
        let head = self.store.history().checkpoints().head();
        while let Ok(event) = self.rfree_rx.try_recv() {
            if event.checkpoint != head {
                continue;
            }
            log::debug!(
                "[App] R-free result: r_work {:.3} / r_free {:.3}",
                event.r_work,
                event.r_free
            );
            self.apply_rfree_result(event.r_free);
        }
    }

    /// Fold one R-free value into the game score and publish the subheader
    /// readout. The reward is a positive game-point contribution; converting it
    /// to the raw filter-bonus delta with the negative scale of
    /// [`crate::scores::rosetta_raw_to_game`] means a lower R-free raises the
    /// displayed game score.
    #[allow(
        clippy::cast_possible_truncation,
        reason = "the R-free readout and its game-point bonus are display scalars; f32 is the wire type"
    )]
    fn apply_rfree_result(&mut self, r_free: f64) {
        let Some(spec) = self
            .store
            .puzzle()
            .and_then(|p| crate::scores::rfree_bonus_spec(&p.filters))
        else {
            // The puzzle changed out from under an in-flight compute; the load
            // path already cleared any stale readout.
            return;
        };
        let reward_game = spec.reward_game(r_free);
        // `rosetta_raw_to_game` scales raw REU by -10, so a game increase of
        // `reward_game` needs a raw delta of `-reward_game / 10`.
        let raw_delta = -reward_game / 10.0;
        self.scores.set_filter_bonus_entry("r_free", raw_delta);
        self.restamp_head_game();

        self.gui.score.r_free = Some(foldit_gui::state::RFreeStatus {
            value: r_free as f32,
            bonus: reward_game as f32,
        });
        self.gui.mark_dirty(foldit_gui::DirtyFlags::SCORE);
    }

    /// Re-stamp the current head game score from its retained raw and the
    /// updated filter-bonus total, so a changed R-free bonus moves the headline
    /// without waiting for the next score cycle. The raw stays the true rosetta
    /// value; only the derived game number changes. A no-op until the head
    /// carries a raw score.
    fn restamp_head_game(&mut self) {
        let (raw, _) = self.store.current_composition_scores();
        if let Some(raw) = raw {
            let game = crate::scores::rosetta_raw_to_game(raw + self.scores.filter_bonus_total());
            self.store.set_head_scores(Some(raw), Some(game), None);
        }
    }
}
