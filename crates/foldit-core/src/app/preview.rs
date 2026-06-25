//! Preview-streaming, commit, and entity-adoption helpers for dispatched ops.

use super::App;
#[cfg(not(target_arch = "wasm32"))]
use molex::entity::molecule::id::EntityId;

#[cfg(not(target_arch = "wasm32"))]
use crate::history::CheckpointKind;

impl App {
    /// Adopt every entity in an entity-creating op's terminal `assembly`
    /// as a new committed entity. Each is inserted as a transient preview
    /// (which allocates a fresh id, so it can't collide with the focused
    /// target) and immediately promoted into history via
    /// [`CheckpointKind::PromotedPreview`]. The focused target is never
    /// touched: creates-entities ops open no edit over it, so this is
    /// purely additive.
    #[cfg(not(target_arch = "wasm32"))]
    pub(in crate::app) fn stream_preview_frame(&mut self, token: u64, assembly: &molex::Assembly) {
        let Some(entity) = assembly.entities().first() else {
            return;
        };
        // Draw the in-progress diffusion frame faithfully: rebuild protein
        // chains as one continuous segment so noisy intermediate coordinates
        // render as a connected backbone instead of fragmenting into a
        // per-residue segment at every C->N gap.
        let payload: molex::MoleculeEntity = entity.to_continuous();
        let atoms = payload.atom_count();
        match self.creates_previews.get(&token).copied() {
            // Same topology: cheap in-place coord update (animates).
            Some((preview_id, prev_atoms)) if prev_atoms == atoms => {
                let _ = self.store.update_preview(preview_id, payload);
            }
            // Atom count changed: a same-id coord update would desync viso's
            // topology vs positions (hard panic). Rebuild under a fresh id so
            // the render projector does a topology `replace_assembly`.
            Some((preview_id, _)) => {
                let _ = self.store.remove_preview(preview_id);
                let id = self.insert_design_preview(payload);
                let _ = self.creates_previews.insert(token, (id, atoms));
            }
            None => {
                let id = self.insert_design_preview(payload);
                let _ = self.creates_previews.insert(token, (id, atoms));
            }
        }
    }

    /// Seed a preview-style op's discardable ghost from the target `lane_id`:
    /// clone that lane's geometry into a transient preview marked provisional
    /// (viso renders a provisional entity as a flat gray tube), name it after
    /// the op, and track it under `token` so the stream's frames update the
    /// ghost while the real lane stays frozen. Rebuild edits a single entity;
    /// when an op resolves to several lanes, only the first carries a ghost.
    #[cfg(not(target_arch = "wasm32"))]
    pub(in crate::app) fn seed_inplace_preview(&mut self, token: u64, lane_id: EntityId, name: String) {
        // Clone the lane geometry to seed an independent preview entity that
        // animates without touching the real lane.
        let Some(clone) = self.store.entity(lane_id).cloned() else {
            return;
        };
        let preview_id =
            self.store
                .insert_preview(clone, name, crate::session::EntityOrigin::Generated);
        self.store.set_entity_provisional(preview_id, true);
        let atom_count = self
            .store
            .entity(preview_id)
            .map_or(0, molex::MoleculeEntity::atom_count);
        let _ = self.inplace_previews.insert(token, (preview_id, atom_count));
    }

    /// Apply a terminal [`OpEvent::Commit`]: drop a preview ghost, adopt an
    /// entity-creating op's terminal entities, or commit the open edit to the
    /// real lane and score the resulting checkpoint. For a checkpoint-driven
    /// preview op this commits the final segment (earlier segments already
    /// committed); the retained begin args are dropped here.
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
                if let Some((preview_id, _)) = self.inplace_previews.remove(&token) {
                    let _ = self.store.remove_preview(preview_id);
                }
                // The checkpoint-driven re-open ends here; drop the retained
                // begin args for this edit.
                let _ = self.inplace_edits.remove(&token);
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
                let _ = self.score_targets.remove(&token);
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
                    Ok(ckpt) => match score.filter(|_| sole) {
                        Some(report) => {
                            let (raw, game, breakdown) = self.prepare_score_stamp(report);
                            self.store.set_checkpoint_scores(
                                ckpt,
                                Some(raw),
                                Some(game),
                                Some(breakdown),
                            );
                        }
                        None => self.score_committed_checkpoint(ckpt),
                    },
                    Err(e) => log::warn!("commit_action failed: {e}"),
                }
                // The edit's correlation id is now spent; drop any lingering
                // composition target.
                let _ = self.score_targets.remove(&token);
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
    /// union. The re-`begin_action` reuses the same token (the commit freed
    /// the lane and dropped the token; the re-open re-forks from the now-
    /// committed head under that token). A non-preview op or one with no open
    /// edit / no retained begin args never reaches here as a commit: it logs
    /// and skips so it cannot disturb a lane it does not own.
    #[cfg(not(target_arch = "wasm32"))]
    pub(in crate::app) fn promote_inplace_checkpoint(
        &mut self,
        token: u64,
        assembly: &molex::Assembly,
        score: Option<crate::scores::ScoreReport>,
    ) {
        let Some((lanes, kind, display)) = self.inplace_edits.get(&token).cloned() else {
            log::trace!("promote checkpoint rid={token}: no open in-place edit, skipped");
            return;
        };
        let sole = self.store.sole_pending_request_id() == Some(token);
        if !self.store.apply_streaming_assembly(assembly, None, token) {
            return;
        }
        match self.store.commit_action(token) {
            Ok(ckpt) => match score.filter(|_| sole) {
                Some(report) => {
                    let (raw, game, breakdown) = self.prepare_score_stamp(report);
                    self.store.set_checkpoint_scores(
                        ckpt,
                        Some(raw),
                        Some(game),
                        Some(breakdown),
                    );
                }
                None => self.score_committed_checkpoint(ckpt),
            },
            Err(e) => {
                log::warn!("promote checkpoint rid={token}: commit_action failed: {e}");
                return;
            }
        }
        // Re-open the edit under the same token for the next segment; it
        // re-forks each lane from its just-committed head.
        if let Err(e) = self
            .store
            .begin_action(lanes, kind, display, token)
        {
            log::warn!("promote checkpoint rid={token}: re-begin_action failed: {e}");
            let _ = self.inplace_edits.remove(&token);
        }
    }

    /// Apply one streaming frame of a preview-style op to its discardable
    /// ghost. The ghost is seeded at dispatch (a provisional clone of the
    /// target lane); this updates it in place from the frame's first entity,
    /// leaving the real lane untouched. No-op when no ghost is tracked for the
    /// token (a streaming frame never moves the lane; only a commit does).
    ///
    /// The frame carries the op's full fixed topology (unlike the backbone-
    /// only diffusion frames `stream_preview_frame` continuous-rebuilds), so
    /// the payload is used as-is. A same-atom-count frame updates in place
    /// (the override persists across `update_preview`: the id is unchanged); a
    /// changed atom count - not expected for a fixed-topology rebuild, but
    /// guarded - rebuilds under a fresh id so the render projector does a
    /// topology `replace_assembly`, then re-marks it provisional.
    #[cfg(not(target_arch = "wasm32"))]
    pub(in crate::app) fn stream_inplace_preview_frame(&mut self, token: u64, assembly: &molex::Assembly) {
        let Some((preview_id, prev_atoms)) = self.inplace_previews.get(&token).copied() else {
            return;
        };
        let Some(entity) = assembly.entities().first() else {
            return;
        };
        let payload: molex::MoleculeEntity = (**entity).clone();
        let atoms = payload.atom_count();
        if prev_atoms == atoms {
            let _ = self.store.update_preview(preview_id, payload);
        } else {
            let name = self
                .store
                .metadata(preview_id)
                .map_or_else(String::new, |m| m.name.clone());
            let _ = self.store.remove_preview(preview_id);
            let id = self.store.insert_preview(
                payload,
                name,
                crate::session::EntityOrigin::Generated,
            );
            self.store.set_entity_provisional(id, true);
            let _ = self.inplace_previews.insert(token, (id, atoms));
        }
    }

    /// Insert a streamed design frame as a transient preview, returning its
    /// allocated id.
    #[cfg(not(target_arch = "wasm32"))]
    fn insert_design_preview(&mut self, payload: molex::MoleculeEntity) -> molex::EntityId {
        self.store.insert_preview(
            payload,
            String::from("RFdiffusion3 design"),
            crate::session::EntityOrigin::Generated,
        )
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
            if let Some((preview_id, _)) = self.creates_previews.remove(&t) {
                let _ = self.store.remove_preview(preview_id);
            }
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
    #[cfg(not(target_arch = "wasm32"))]
    fn adopt_one_entity(&mut self, payload: molex::MoleculeEntity) {
        let id = self.insert_design_preview(payload);
        self.promote_adopted(id);
    }

    /// Promote an already-inserted preview `id` into history as a new
    /// committed entity, scoring the resulting checkpoint. Discards the
    /// preview on failure.
    #[cfg(not(target_arch = "wasm32"))]
    fn promote_adopted(&mut self, id: molex::EntityId) {
        match self.store.promote_preview(
            id,
            CheckpointKind::PromotedPreview { entity: id },
            None,
            None,
            "RFdiffusion3",
        ) {
            Ok(ckpt) => self.score_committed_checkpoint(ckpt),
            Err(e) => {
                log::warn!("promote_adopted: promote failed: {e}");
                let _ = self.store.remove_preview(id);
            }
        }
    }
}
