//! Off-thread crystallographic B-factor refine: the background thread runs
//! molex only and reports over a channel; every session / engine / gui change
//! happens here on the main thread when the tick drains those events.

use molex::MoleculeEntity;
use molex::entity::molecule::id::EntityId;

use crate::app::App;
use crate::history::CheckpointKind;

/// A message from the background refine thread to the main thread.
pub(in crate::app) enum RefineEvent {
    /// One L-BFGS inner iteration ticked. `macro_cycle` is the 1-based outer
    /// cycle; `inner_iter` is the 1-based inner iteration within it, out of
    /// `inner_total`. Both loops converge early, so neither count is a fixed
    /// denominator.
    Progress {
        macro_cycle: usize,
        inner_iter: usize,
        inner_total: usize,
    },
    /// Refinement succeeded. `full_b` is aligned 1:1 to
    /// `AtomTable::from_entities` flat order over the dispatched snapshot.
    Done {
        full_b: Vec<f32>,
        r_work: f64,
        r_free: f64,
    },
    Failed(String),
    /// The user cancelled the refine (its toast X or ESC). Clears the toast
    /// without applying a partial result or raising an error.
    Cancelled,
}

impl App {
    /// Drain every pending refine event and act on it: update the toast on
    /// progress, apply the refined B and clear the toast on completion, or
    /// report failure.
    #[allow(
        clippy::cast_precision_loss,
        reason = "inner_iter / inner_total are tiny iteration counts; the f32 fraction only drives a progress bar"
    )]
    pub(in crate::app) fn drain_refine_events(&mut self) {
        while let Ok(event) = self.refine_rx.try_recv() {
            match event {
                RefineEvent::Progress {
                    macro_cycle,
                    inner_iter,
                    inner_total,
                } => {
                    // Fraction fills within a cycle and resets when the cycle
                    // advances (inner_iter restarts at 1); the label carries no
                    // fixed total because both loops stop on convergence.
                    self.gui.actions.refine_progress = Some(foldit_gui::state::RefineProgress {
                        fraction: inner_iter as f32 / inner_total.max(1) as f32,
                        label: format!("Refining B-factors - cycle {macro_cycle}"),
                    });
                    self.mark_dirty(foldit_gui::DirtyFlags::ACTIONS);
                }
                RefineEvent::Done {
                    full_b,
                    r_work,
                    r_free,
                } => {
                    if self.apply_refined_b(&full_b) {
                        self.gui.push_notification(
                            foldit_gui::NotificationLevel::Info,
                            format!(
                                "B-factor refine complete - R-work {r_work:.3} / R-free {r_free:.3}"
                            ),
                        );
                    }
                    self.gui.actions.refine_progress = None;
                    self.refine_in_flight = false;
                    self.runner_client.unlock_global_native();
                    self.mark_dirty(foldit_gui::DirtyFlags::ACTIONS);
                }
                RefineEvent::Failed(msg) => {
                    self.gui.actions.refine_progress = None;
                    self.refine_in_flight = false;
                    self.runner_client.unlock_global_native();
                    self.gui
                        .push_notification(foldit_gui::NotificationLevel::Error, msg);
                    self.mark_dirty(foldit_gui::DirtyFlags::ACTIONS);
                }
                RefineEvent::Cancelled => {
                    // User-initiated: drop the toast quietly, no error, no
                    // partial apply.
                    self.gui.actions.refine_progress = None;
                    self.refine_in_flight = false;
                    self.runner_client.unlock_global_native();
                    self.mark_dirty(foldit_gui::DirtyFlags::ACTIONS);
                }
            }
        }
    }

    /// Request cancellation of the running refine (its toast X, or ESC). Flips
    /// the shared flag the background thread's progress callback polls; the
    /// thread then unwinds molex and reports [`RefineEvent::Cancelled`]. A
    /// no-op when no refine is in flight.
    #[cfg(not(target_arch = "wasm32"))]
    pub(in crate::app) fn request_refine_cancel(&self) {
        if self.refine_in_flight {
            self.refine_cancel
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Scatter the refined B column back onto the committed head losslessly and
    /// commit it as a `NativeEdit` checkpoint. Returns `false` (without opening
    /// any edit) when the committed model changed under the refine or the
    /// result does not line up, so the caller skips the success toast.
    fn apply_refined_b(&mut self, full_b: &[f32]) -> bool {
        let current = self.committed_head_entities();

        // Race guard: the committed model must be the one the refine ran
        // against (same entities, same per-entity atom counts, same order).
        let current_fp: Vec<(u32, usize)> = current
            .iter()
            .map(|e| (e.id().raw(), e.atom_count()))
            .collect();
        if current_fp != self.refine_fingerprint {
            self.gui.push_notification(
                foldit_gui::NotificationLevel::Error,
                "Model changed during refine; result discarded".to_owned(),
            );
            return false;
        }

        // `flat_source_indices` walks the same order as `from_entities`, so
        // `prov[i]` is the `(entity, storage index)` source of `full_b[i]`.
        let prov = molex::adapters::table::AtomTable::flat_source_indices(&current);
        if prov.len() != full_b.len() {
            self.gui.push_notification(
                foldit_gui::NotificationLevel::Error,
                "Refine result size mismatch; discarded".to_owned(),
            );
            return false;
        }
        let mut per_entity: std::collections::HashMap<u32, Vec<(usize, f32)>> =
            std::collections::HashMap::new();
        for (&(entity_raw, raw_idx), &b) in prov.iter().zip(full_b.iter()) {
            per_entity
                .entry(entity_raw)
                .or_default()
                .push((raw_idx as usize, b));
        }

        let head_ids: Vec<EntityId> = current
            .iter()
            .map(MoleculeEntity::id)
            .filter(|id| self.store.history().lane(*id).is_some())
            .collect();
        let Some(rid) = self.runner_client.alloc_request_id() else {
            self.gui.push_notification(
                foldit_gui::NotificationLevel::Error,
                "Refine apply failed: no request id".to_owned(),
            );
            return false;
        };
        let kind = CheckpointKind::NativeEdit {
            op_id: "refine_b".to_owned(),
            display: "Refine B".to_owned(),
        };
        if let Err(e) =
            self.store
                .begin_action(head_ids, kind, "Refine B", rid, std::collections::BTreeMap::new())
        {
            log::warn!("refine apply: begin_action failed: {e}");
            self.gui.push_notification(
                foldit_gui::NotificationLevel::Error,
                "Refine apply failed".to_owned(),
            );
            return false;
        }

        // Light in-place lane update: rewrite only the B column, preserving
        // every other atom field. `action_update` fans the closure across each
        // locked lane's entity.
        let update = self.store.action_update(rid, None, None, None, |em| {
            if let Some(cells) = per_entity.get(&em.id().raw()) {
                let col = &mut em.columns_mut().b_factor;
                for &(k, b) in cells {
                    if let Some(slot) = col.get_mut(k) {
                        *slot = b;
                    }
                }
            }
        });
        if let Err(e) = update {
            log::warn!("refine apply: action_update failed: {e}");
            let _ = self.store.abort_action(rid);
            self.gui.push_notification(
                foldit_gui::NotificationLevel::Error,
                "Refine apply failed".to_owned(),
            );
            return false;
        }

        match self.store.commit_action(rid) {
            Ok(ckpt) => {
                self.scores
                    .score_committed_checkpoint(&mut self.runner_client, &self.store, ckpt);
                self.spawn_rfree_compute(ckpt);
            }
            Err(e) => {
                log::warn!("refine apply: commit_action failed: {e}");
                self.gui.push_notification(
                    foldit_gui::NotificationLevel::Error,
                    "Refine commit failed".to_owned(),
                );
                return false;
            }
        }

        // Recompute the map from the refined B. Reuse the prior asset name so
        // the scoring lane keeps its label.
        let map_name = self.store.session_density().map_or_else(
            || "refined-density.mrc".to_owned(),
            |a| a.name.clone(),
        );
        let new_head = self.committed_head_entities();
        self.refresh_density(&new_head, &map_name);
        true
    }

    /// Clone the committed head entities, excluding transient previews. The
    /// refine input and validation set: previews are gated out so a refine
    /// never runs against a discardable ghost.
    pub(in crate::app) fn committed_head_entities(&self) -> Vec<MoleculeEntity> {
        let previews: std::collections::HashSet<EntityId> = self.store.preview_ids().collect();
        self.store
            .ids()
            .filter(|id| !previews.contains(id))
            .filter_map(|id| self.store.entity(id).cloned())
            .collect()
    }

    /// Acquire the inputs both crystallographic jobs (the R-free objective and
    /// the B-factor refine) hand to molex: the retained experimental data, the
    /// shared GPU device, a snapshot of the committed head, and its atom table.
    /// `None` when the density or device is absent, or the head is empty. The
    /// snapshot rides alongside the table so a caller can fingerprint the input
    /// geometry. Callers that need distinct bail messaging per missing input do
    /// their own presence checks first; the empty-head `None` is then the only
    /// case left for them to handle.
    pub(in crate::app) fn xtal_job_inputs(
        &self,
    ) -> Option<(
        std::sync::Arc<molex::xtal::ExperimentalData>,
        molex::xtal::WgpuDevice,
        Vec<MoleculeEntity>,
        molex::adapters::table::AtomTable,
    )> {
        let data = self.experimental_data.as_ref().map(std::sync::Arc::clone)?;
        let dev = self.shared_device.clone()?;
        let snapshot = self.committed_head_entities();
        if snapshot.is_empty() {
            return None;
        }
        let table = molex::adapters::table::AtomTable::from_entities(&snapshot);
        Some((data, dev, snapshot, table))
    }
}
