//! Preview-streaming, commit, and entity-adoption helpers for dispatched ops.

use super::App;
#[cfg(not(target_arch = "wasm32"))]
use crate::history::CheckpointKind;

impl App {
    /// Apply a terminal [`OpEvent::Commit`]: drop a preview ghost, adopt an
    /// entity-creating op's terminal entities, or commit the open edit to the
    /// real lane and score the resulting checkpoint. For a checkpoint-driven
    /// preview op this commits the final segment (earlier segments already
    /// committed via [`App::promote_inplace_checkpoint`]).
    #[cfg(not(target_arch = "wasm32"))]
    pub(in crate::app) fn apply_commit_event(
        &mut self,
        token: Option<u64>,
        assembly: &molex::Assembly,
        score: Option<crate::scores::ScoreReport>,
        creates_entities: bool,
        preview: bool,
    ) {
        if preview {
            // Preview-style op: drop the ghost. The terminal then applies to
            // the real lane via the ordinary edit-commit path below
            // (committing the final segment; for a checkpoint-driven op the
            // earlier segments already committed); the ghost is never
            // promoted. Both effects land in this one drain so the projector
            // sees the lane update and the ghost removal together.
            if let Some(token) = token {
                self.store.discard_inplace_ghost(token);
            }
        }
        if creates_entities {
            // Entity-creating op (e.g. RFdiffusion3 design): no edit was
            // opened over the focused target. If a live preview animated the
            // stream, snap it to the final geometry and promote it in place;
            // else adopt the terminal assembly fresh. Either way the focused
            // target is untouched.
            self.commit_created_entities(token, assembly);
            if let Some(token) = token {
                let _ = self.scores.remove_target(token);
            }
        } else if let Some(token) = token {
            // Capture sole-open-ness while the edit is still pending: the
            // commit below clears it.
            let sole = self.store.sole_pending_request_id() == Some(token);
            if self.store.apply_streaming_assembly(assembly, None, token) {
                // Stream finished: commit the tentative so the partial result
                // becomes a permanent undo entry. A sole open edit's terminal
                // frame already carries this checkpoint's score, so stamp it
                // directly. With a peer edit still open the live pose is a
                // blend, so re-score the committed union for correct
                // attribution.
                match self.store.commit_action(token) {
                    Ok(ckpt) => {
                        match score.filter(|_| sole) {
                            Some(report) => {
                                let (raw, game, breakdown) =
                                    self.scores.prepare_score_stamp(report);
                                self.store.set_checkpoint_scores(
                                    ckpt,
                                    Some(raw),
                                    Some(game),
                                    Some(breakdown),
                                );
                            }
                            None => self.scores.score_committed_checkpoint(
                                &mut self.runner_client,
                                &self.store,
                                ckpt,
                            ),
                        }
                        self.spawn_rfree_compute(ckpt);
                    }
                    Err(e) => log::warn!("commit_action failed: {e}"),
                }
                // The edit's correlation id is now spent; drop any lingering
                // composition target.
                let _ = self.scores.remove_target(token);
            }
        }
    }

    /// Commit one accepted candidate of a preview-style op to the real lane,
    /// then re-open the same edit for the next segment. Driven by a
    /// non-terminal checkpoint: the leading ghost keeps animating the
    /// in-flight preview (untouched here) while each accept mints a history
    /// checkpoint on the lane, so the lane advances accept-by-accept and the
    /// stream stays open.
    ///
    /// Scoring mirrors the terminal commit: a sole open edit's candidate
    /// already carries this checkpoint's score, so stamp it directly; with a
    /// peer edit open the live pose is a blend, so re-score the committed
    /// union. [`Session::commit_and_reopen`] mints the segment's checkpoint
    /// and re-forks the same lanes under the same token from the now-
    /// committed head, reusing the edit's own kind and label. A non-preview
    /// op or one with no open edit under this token never reaches here as a
    /// commit: it logs and skips so it cannot disturb a lane it does not own.
    #[cfg(not(target_arch = "wasm32"))]
    pub(in crate::app) fn promote_inplace_checkpoint(
        &mut self,
        token: u64,
        assembly: &molex::Assembly,
        score: Option<crate::scores::ScoreReport>,
    ) {
        if !self.store.is_pending(token) {
            log::trace!("promote checkpoint rid={token}: no open in-place edit, skipped");
            return;
        }
        let sole = self.store.sole_pending_request_id() == Some(token);
        if !self.store.apply_streaming_assembly(assembly, None, token) {
            return;
        }
        match self.store.commit_and_reopen(token) {
            Ok(ckpt) => {
                match score.filter(|_| sole) {
                    Some(report) => {
                        let (raw, game, breakdown) = self.scores.prepare_score_stamp(report);
                        self.store
                            .set_checkpoint_scores(ckpt, Some(raw), Some(game), Some(breakdown));
                    }
                    None => self.scores.score_committed_checkpoint(
                        &mut self.runner_client,
                        &self.store,
                        ckpt,
                    ),
                }
                self.spawn_rfree_compute(ckpt);
            }
            Err(e) => {
                log::warn!("promote checkpoint rid={token}: commit_and_reopen failed: {e}");
            }
        }
    }

    /// Terminal handling for a creates-entities stream. Tears down the
    /// streaming preview (if any) and adopts the terminal assembly fresh.
    ///
    /// The preview is NOT promoted in place: streaming frames are
    /// backbone-only, but the terminal entity carries full atoms (a
    /// different topology). The render projector routes a same-id change
    /// as a coord-only `set_assembly` (it detects topology change only via
    /// id-set membership), which would index the new positions against the
    /// old backbone topology and panic viso. Removing the preview and
    /// adopting fresh (new id) forces a `replace_assembly` topology
    /// rebuild. Both updates land in one drain, so the projector sees a
    /// single net id-set change -- no flicker.
    #[cfg(not(target_arch = "wasm32"))]
    fn commit_created_entities(&mut self, token: Option<u64>, assembly: &molex::Assembly) {
        if let Some(t) = token {
            self.store.discard_created_preview(t);
        }
        self.adopt_created_entities(assembly);
    }

    /// Adopt every entity in an entity-creating op's terminal `assembly`
    /// as a new committed entity (the no-live-preview path).
    #[cfg(not(target_arch = "wasm32"))]
    fn adopt_created_entities(&mut self, assembly: &molex::Assembly) {
        for entity in assembly.entities() {
            self.adopt_one_entity((**entity).clone());
        }
    }

    /// Insert `payload` as a transient preview (fresh id) and promote it.
    /// On a design-gated puzzle the promoted entity is registered as fully
    /// designable, making the created design itself the designable target.
    #[cfg(not(target_arch = "wasm32"))]
    fn adopt_one_entity(&mut self, payload: molex::MoleculeEntity) {
        let residue_count = payload.residue_count();
        let id = self.store.insert_design_preview(payload);
        if self.promote_adopted(id) {
            self.store
                .register_full_designable_entity(id, residue_count);
        }
    }

    /// Promote an already-inserted preview `id` into history as a new
    /// committed entity, scoring the resulting checkpoint. Discards the
    /// preview on failure. Returns whether promotion succeeded.
    #[cfg(not(target_arch = "wasm32"))]
    fn promote_adopted(&mut self, id: molex::EntityId) -> bool {
        match self.store.promote_preview(
            id,
            CheckpointKind::PromotedPreview { entity: id },
            None,
            "RFdiffusion3",
        ) {
            Ok(ckpt) => {
                self.scores
                    .score_committed_checkpoint(&mut self.runner_client, &self.store, ckpt);
                self.spawn_rfree_compute(ckpt);
                true
            }
            Err(e) => {
                log::warn!("promote_adopted: promote failed: {e}");
                let _ = self.store.remove_preview(id);
                false
            }
        }
    }
}
