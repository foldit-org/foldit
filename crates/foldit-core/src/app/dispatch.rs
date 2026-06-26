use foldit_gui::HistoryCommand;
use molex::entity::molecule::id::EntityId;

use crate::app::App;
use crate::gui_projector::project_history;
#[cfg(not(target_arch = "wasm32"))]
use crate::history::CheckpointKind;
#[cfg(not(target_arch = "wasm32"))]
use crate::runner_client::{DispatchError, DispatchIntent, EditScope, OpEvent, OpOutcome};
use crate::session::SessionError;
#[cfg(not(target_arch = "wasm32"))]
use viso::Focus;

/// Outcome of a [`HistoryCommand`] dispatch - drives the per-frame
/// follow-up the dispatcher must run (republish to viso, mark dirty,
/// or nothing at all).
enum HistoryOutcome {
    /// Checkpoint head moved; rerun [`App::after_head_move`].
    HeadMoved,
    /// Curation flag changed (pin / unpin / exclude from best). No
    /// head move; just mark `ACTIONS` dirty so the GUI reflects it.
    Curated,
    /// The command was a no-op (e.g., undo at root). No follow-up.
    Noop,
}

impl App {
    pub fn apply_backend_updates(&mut self) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let events = self.runner_client.drain_op_events();
            if events.is_empty() {
                return;
            }

            // Every branch below routes through the mutation funnel, which
            // emits the covering `SessionUpdate`: a mid-stream update emits
            // a tentative `Edit`, a commit emits `HeadMoved` (+ a deferred
            // `ScoresChanged` from the commit-stamp), an abort emits its own
            // update. The GUI consumer derives its dirty set from that batch,
            // so no explicit raise is needed here.
            for event in events {
                match event {
                    OpEvent::Update {
                        token,
                        assembly,
                        score,
                        creates_entities,
                        preview,
                    } => {
                        if creates_entities {
                            // Entity-creating op: stream the diffusion frame
                            // into a live preview entity (created on the first
                            // frame, updated in place after) so the viewport
                            // animates the binder forming. Promoted at commit.
                            self.stream_preview_frame(token, &assembly);
                        } else if preview {
                            // Preview-style op: the frame animates the
                            // discardable ghost, never the frozen lane.
                            self.stream_inplace_preview_frame(token, &assembly);
                        } else {
                            let applied =
                                self.store.apply_streaming_assembly(&assembly, None, token);
                            // The frame carries the warm score of its own
                            // geometry; stamp the open edit directly from it so
                            // the displayed score stays coupled to the frame
                            // instead of trailing it.
                            if applied {
                                if let Some(report) = score {
                                    let (raw, game, breakdown) = self.prepare_score_stamp(report);
                                    self.store.set_edit_scores(
                                        token,
                                        Some(raw),
                                        Some(game),
                                        Some(breakdown),
                                    );
                                }
                            }
                        }
                    }
                    OpEvent::Promote {
                        token,
                        assembly,
                        score,
                        creates_entities,
                        preview,
                    } => {
                        // A checkpoint commits an accepted candidate to the
                        // real lane and re-opens the edit for the next segment;
                        // the stream keeps running and the leading ghost is
                        // untouched. Only a preview op drives this loop; a
                        // checkpoint for any other op (no open edit, no stash)
                        // is a safe no-op so it never commits a stray lane.
                        if preview && !creates_entities {
                            self.promote_inplace_checkpoint(token, &assembly, score);
                        }
                    }
                    OpEvent::Commit {
                        token,
                        assembly,
                        score,
                        creates_entities,
                        preview,
                    } => self.apply_commit_event(
                        token,
                        &assembly,
                        score,
                        creates_entities,
                        preview,
                    ),
                    OpEvent::Abort { token, reason } => {
                        // Spontaneous failure: never commits; aborts
                        // exactly the edit this stream owns. A terminal
                        // with no open edit, or whose edit already
                        // committed, is a no-op.
                        if let Some(token) = token {
                            // Discard any in-progress creates-entities preview
                            // or in-place ghost this stream was animating.
                            if let Some((preview_id, _)) = self.creates_previews.remove(&token) {
                                let _ = self.store.remove_preview(preview_id);
                            }
                            if let Some((preview_id, _)) = self.inplace_previews.remove(&token) {
                                let _ = self.store.remove_preview(preview_id);
                            }
                            let _ = self.inplace_edits.remove(&token);
                            if self.store.is_pending(token) {
                                if let Err(e) = self.store.abort_action(token) {
                                    log::warn!("abort_action failed: {e}");
                                }
                            }
                        }
                        log::warn!("plugin op aborted: {reason}");
                    }
                }
            }
        }
    }

    /// Dispatch a plugin op by op-id. Resolves the op against the
    /// orchestrator's `PluginRegistry` to pick Invoke vs `Start_stream`;
    /// builds a `DispatchContext` from the GUI-provided focus and the
    /// authoritative in-core `App.selection`. Op-ids unknown to the
    /// registry are logged and dropped (the catalog couldn't have
    /// surfaced them, so this is either a stale GUI cache or a
    /// misrouted message).
    pub fn handle_dispatch_op(&mut self, op: foldit_gui::OpDispatch) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            // Drain pending terminals so rapid follow-up dispatches
            // see released locks.
            self.apply_backend_updates();

            // Source the focused entity authoritatively from the session's
            // current focus, not the GUI-supplied `op.focused_entity_id`
            // (which the hotkey paths leave as None). This makes every
            // dispatch path -- button or hotkey -- carry the live focus to
            // the worker, paired with the authoritative `App.selection`
            // read into the intent below. The molex `EntityId` passes
            // straight through, the shape `DispatchIntent` expects.
            let focused_entity_id: Option<molex::EntityId> = match self.store.focus() {
                Focus::Entity(eid) => Some(eid),
                Focus::All => None,
            };

            // Resolve the op-id to its owning plugin via the registry behind
            // an accessor (names no orchestrator type); `plugin_id` is needed
            // below for `begin_action`. A miss here means either no
            // orchestrator is installed or the op-id isn't registered (a stale
            // GUI cache or misrouted message) -- either way the op can't run.
            let Some(plugin_id) = self.runner_client.resolve_op_plugin_id(&op.op_id) else {
                log::warn!(
                    "handle_dispatch_op({:?}): op not resolvable (no orchestrator or op-id not in registry)",
                    op.op_id
                );
                return;
            };

            // Resolve the display label from the manifest catalog. Falls
            // back to the op id when the op isn't surfaced as a button
            // (the dispatcher still routes; the history entry just shows
            // the op id).
            let display = self
                .runner_client
                .op_display(&plugin_id, &op.op_id)
                .unwrap_or_else(|| op.op_id.clone());
            // Hand the driver a core-shaped intent: the selection flatten,
            // param conversion, and `DispatchContext` build all live behind
            // `dispatch_op` now, so this path names no orchestrator type.
            let intent = DispatchIntent {
                selection: self.store.selection().clone(),
                designable: self.designable_residues(),
                focused_entity_id,
                op_id: op.op_id.clone(),
                params: op.params,
            };
            // Hoist a shared borrow of the store so the lookup closure
            // can capture it alongside the upcoming `&mut self.runner_client`
            // call (disjoint field paths).
            let store = &self.store;
            let dispatch_outcome =
                self.runner_client
                    .dispatch_op(intent, plugin_id.clone(), |id| store.entity_type(id));

            // The dispatch allocated the id the edit and the stream table
            // both key on, and resolved the entity set the op operates on.
            // Pull both from the successful outcome; the edit opens over the
            // whole resolved set (a whole-pose op moves every entity, so a
            // single-entity edit would drop every other entity's result and
            // commit a geometrically inconsistent pose). Filter to entities
            // with a committed lane - `begin_action` forks each lane from its
            // current head, and transient stubs (ambient / zero-residue) have
            // none - mirroring the post-Init adoption path.
            let lanes: Option<Vec<EntityId>> = match &dispatch_outcome {
                Ok(OpOutcome::Stream { scope, .. } | OpOutcome::Invoke { scope, .. }) => {
                    Some(self.lanes_for_scope(scope))
                }
                Err(_) => None,
            };
            let dispatch_id = match &dispatch_outcome {
                Ok(OpOutcome::Stream { request_id, .. } | OpOutcome::Invoke { request_id, .. }) => {
                    Some(*request_id)
                }
                Err(_) => None,
            };

            // An op that creates entities does NOT edit an existing lane: its
            // terminal assembly is adopted as new entities at commit. Skipping
            // `begin_action` leaves the focused target untouched (streaming
            // frames then no-op for want of an open edit under their token).
            let creates_entities = self.runner_client.op_creates_entities(&op.op_id);
            // A preview-style op opens its in-place edit normally but renders
            // its stream as a discardable ghost; the real lane advances only
            // on a commit (a non-terminal checkpoint commits an accepted
            // segment and re-opens; the terminal commits the last). It is NOT
            // a create, so it is not in the `begin_action`-skip condition
            // below.
            let preview = self.runner_client.op_preview(&plugin_id, &op.op_id);

            // Open the edit under the dispatch id over the resolved lane set.
            // Skipped on dispatch failure (any open tentative belongs to a
            // prior op), when the resolved set has no editable lane, or for a
            // creates-entities op (handled via adoption at commit). For a
            // preview op the edit opens as usual; `edit_token` also carries the
            // first resolved lane so the ghost can clone its geometry.
            let edit_token = dispatch_id.zip(lanes).and_then(|(request_id, lanes)| {
                if creates_entities || lanes.is_empty() {
                    return None;
                }
                let seed_lane = lanes.first().copied();
                let kind = CheckpointKind::PluginOp {
                    plugin_id: plugin_id.clone(),
                    op_id: op.op_id.clone(),
                    display: display.clone(),
                };
                // A preview op re-opens this edit on each checkpoint, so
                // retain the begin args. Cloned because `begin_action` below
                // consumes both lanes and kind, and the re-open needs them.
                let stash = preview.then(|| (lanes.clone(), kind.clone(), display.clone()));
                match self.store.begin_action(lanes, kind, display.clone(), request_id) {
                    Ok(()) => {
                        if let Some(s) = stash {
                            let _ = self.inplace_edits.insert(request_id, s);
                        }
                        Some((request_id, seed_lane))
                    }
                    Err(e) => {
                        log::trace!(
                            "handle_dispatch_op({:?}): begin_action skipped: {e}",
                            op.op_id
                        );
                        None
                    }
                }
            });

            // Seed the discardable ghost for a preview op once its edit is
            // open: the stream's frames update this ghost, not the frozen
            // lane, and it is removed at the terminal.
            if preview {
                if let Some((token, Some(lane_id))) = edit_token {
                    self.seed_inplace_preview(token, lane_id, display);
                }
            }
            let edit_token = edit_token.map(|(request_id, _)| request_id);

            match dispatch_outcome {
                Ok(OpOutcome::Stream { .. }) => {
                    // The stream table entry (inserted by
                    // `RunnerClient::dispatch_op`) and the edit are keyed
                    // by the same dispatch id; the terminal arm commits /
                    // aborts via that id. Nothing to reconcile here.
                }
                Ok(OpOutcome::Invoke { bytes, .. }) => {
                    self.apply_invoke_result(&bytes, edit_token);
                }
                Err(DispatchError::EntityLocked { entity }) => {
                    // Advisory refusal: the target entity is busy with
                    // another op. No edit was begun (gated on `is_ok`), so
                    // there is nothing to open or roll back.
                    log::warn!(
                        "handle_dispatch_op({:?}): dispatch refused, entity {entity} locked",
                        op.op_id
                    );
                }
                Err(DispatchError::BackendBusy { plugin_id }) => {
                    // Advisory refusal: the plugin's backend worker is
                    // already running an op. No edit was begun (gated on
                    // `is_ok`), so there is nothing to open or roll back.
                    log::info!("dispatch refused: backend {plugin_id} busy");
                }
                Err(DispatchError::Failed(s)) => {
                    log::error!("handle_dispatch_op({:?}): dispatch failed: {s}", op.op_id);
                }
            }
            // GUI dirty is derived from the batch: an Invoke commits
            // (HeadMoved → SCENE | SCORE | ACTIONS) and a Stream's frames
            // emit tentative Edits, then a HeadMoved at commit. A dispatch
            // changes neither focus nor selection, so the action catalog
            // (which depends only on those) is unchanged; no ACTIONS push
            // is owed at dispatch time.
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = op;
        }
    }

    /// Per-entity set of residues the loaded puzzle permits redesign at, read
    /// off the session's design mask over the live head entities. Empty when
    /// no design gating is active (free-form session, fold puzzle). Carried on
    /// the [`DispatchIntent`] so the plugin can gate identity changes; the
    /// engine intersects it with the resolved selection, so computing it over
    /// every live protein entity is sufficient.
    #[cfg(not(target_arch = "wasm32"))]
    fn designable_residues(
        &self,
    ) -> std::collections::BTreeMap<EntityId, std::collections::BTreeSet<u32>> {
        use std::collections::{BTreeMap, BTreeSet};
        let mut designable: BTreeMap<EntityId, BTreeSet<u32>> = BTreeMap::new();
        if !self.store.design_gating_active() {
            return designable;
        }
        for entity in self.store.head_assembly().entities() {
            let eid = entity.id();
            let count = u32::try_from(entity.residue_count()).unwrap_or(u32::MAX);
            let residues: BTreeSet<u32> =
                (0..count).filter(|&res| self.store.is_designable(eid, res)).collect();
            if !residues.is_empty() {
                let _ = designable.insert(eid, residues);
            }
        }
        designable
    }

    /// Resolve a dispatch's [`EditScope`] into the concrete set of lanes the
    /// edit opens over. A whole-pose op (`AllEntities`) spans every committed
    /// entity; an entity-scoped op spans its resolved set. Either way the
    /// result is filtered to entities that hold a committed lane - the only
    /// ones `begin_action` can fork a tentative from - matching the post-Init
    /// adoption path's lane filter. Transient stubs (ambient /
    /// zero-residue entities) drop out silently.
    #[cfg(not(target_arch = "wasm32"))]
    pub(in crate::app) fn lanes_for_scope(&self, scope: &EditScope) -> Vec<EntityId> {
        let has_lane = |id: &EntityId| self.store.history().lane(*id).is_some();
        match scope {
            EditScope::AllEntities => self.store.ids().filter(has_lane).collect(),
            EditScope::Entities(set) => set.iter().copied().filter(has_lane).collect(),
        }
    }

    /// Apply the assembly bytes returned by a one-shot `dispatch_invoke`
    /// to the ongoing tentative and commit it. Mirrors the Stream-side
    /// `Final` path; called from `handle_dispatch_op` for `OpKind::Invoke`.
    /// The transition is inferred from the prior-vs-result structural
    /// delta and queued on the locked entity so the next tick's
    /// render-projector publish eases the result in rather than snapping.
    #[cfg(not(target_arch = "wasm32"))]
    fn apply_invoke_result(&mut self, bytes: &[u8], edit_token: Option<u64>) {
        let Some(token) = edit_token else {
            // No edit was begun for this invoke (begin skipped), so there
            // is nothing to apply into or commit.
            return;
        };
        let assembly = match molex::Assembly::from_bytes(bytes) {
            Ok(a) => a,
            Err(e) => {
                log::warn!("dispatch_invoke: decode failed: {e:?}");
                if self.store.is_pending(token) {
                    let _ = self.store.commit_action(token);
                }
                return;
            }
        };
        let applied = self.store.apply_streaming_assembly(&assembly, None, token);
        if applied {
            match self.store.commit_action(token) {
                Ok(ckpt) => self.score_committed_checkpoint(ckpt),
                Err(e) => log::warn!("dispatch_invoke: commit_action failed: {e}"),
            }
            // `commit_action` emits `HeadMoved`, from which the GUI consumer
            // derives SCENE (+ SCORE + ACTIONS); no explicit raise is owed.
        } else if self.store.is_pending(token) {
            // Nothing matched (e.g. plugin returned an empty / unrelated
            // assembly): drop the tentative.
            let _ = self.store.commit_action(token);
        }
        // The edit's correlation id is spent; drop any lingering target.
        let _ = self.score_targets.remove(&token);
    }

    /// Common tail for undo / redo / `jump_checkpoint`: clear cached
    /// per-residue scores (they were computed against the *previous* head
    /// and become meaningless on a head move). The GUI projection reads the
    /// score off the new head checkpoint on the next GUI-consumer pass. The
    /// `HeadMoved` emitted by undo/redo/jump rides
    /// the batch, from which the render projector republishes (picking
    /// `replace_assembly` / `set_assembly`) and the GUI consumer derives
    /// SCENE | SCORE | ACTIONS dirty.
    fn after_head_move(&mut self) {
        if let Some(engine) = self.engine.as_mut() {
            let ids: Vec<EntityId> = self.store.ids().collect();
            for eid in ids {
                engine.set_per_residue_scores(eid.raw(), None);
            }
        }
    }

    /// Dispatch a [`HistoryCommand`] from the GUI to the matching
    /// `Session` method. Refusals are logged; the GUI surface
    /// shows the result by virtue of the head not moving (no separate
    /// toast / error channel - `HistoryError::EntityLocked` only
    /// fires while the user's own action is still running, where the
    /// running indicator is the natural feedback). The match is
    /// exhaustive: adding a variant without a handler is a
    /// compile error.
    pub(in crate::app) fn run_history_command(&mut self, cmd: &HistoryCommand) {
        if self.engine.is_none() {
            return;
        }
        let result: Result<HistoryOutcome, SessionError> = match *cmd {
            HistoryCommand::JumpCheckpoint { id } => self
                .store
                .jump_checkpoint(id.into_inner())
                .map(|_| HistoryOutcome::HeadMoved),
            HistoryCommand::Undo => self.store.undo().map(|opt| {
                if opt.is_some() {
                    HistoryOutcome::HeadMoved
                } else {
                    log::info!("Undo: already at root");
                    HistoryOutcome::Noop
                }
            }),
            HistoryCommand::Redo { branch } => self
                .store
                .redo(branch.map(foldit_gui::WireId::into_inner))
                .map(|opt| {
                    if opt.is_some() {
                        HistoryOutcome::HeadMoved
                    } else {
                        log::info!("Redo: nowhere forward to go");
                        HistoryOutcome::Noop
                    }
                }),
            HistoryCommand::LaneUndo { entity, target } => self
                .store
                .lane_undo(entity, target.into_inner())
                .map(|_| HistoryOutcome::HeadMoved),
            HistoryCommand::LaneRedo { entity, branch } => self
                .store
                .lane_redo(entity, branch.map(foldit_gui::WireId::into_inner))
                .map(|_| HistoryOutcome::HeadMoved),
            HistoryCommand::PinCheckpoint { id } => self
                .store
                .pin_checkpoint(id.into_inner())
                .map(|()| HistoryOutcome::Curated),
            HistoryCommand::UnpinCheckpoint { id } => self
                .store
                .unpin_checkpoint(id.into_inner())
                .map(|()| HistoryOutcome::Curated),
            HistoryCommand::SetExcludeFromBest { id, exclude } => self
                .store
                .set_exclude_from_best(id.into_inner(), exclude)
                .map(|()| HistoryOutcome::Curated),
            HistoryCommand::AbortAction => {
                // "Discard the running action." Targeting a single edit
                // no-ops once two edits run concurrently, so discard every
                // open edit instead of silently doing nothing.
                let rids: Vec<u64> = self.store.pending_request_ids().collect();
                if rids.is_empty() {
                    Ok(HistoryOutcome::Noop)
                } else {
                    for rid in rids {
                        if let Err(e) = self.store.abort_action(rid) {
                            log::warn!("abort_action({rid}) failed: {e}");
                        }
                        #[cfg(not(target_arch = "wasm32"))]
                        {
                            let _ = self.score_targets.remove(&rid);
                        }
                    }
                    Ok(HistoryOutcome::HeadMoved)
                }
            }
        };

        match result {
            Ok(HistoryOutcome::HeadMoved) => self.after_head_move(),
            Ok(HistoryOutcome::Curated) => {
                // Pin / unpin / exclude mutate the history DAG's curation
                // metadata without moving the head or bumping
                // `topology_version`, so the GUI consumer's cursor-driven
                // history push never re-fires. Push the refreshed history
                // section at-site so the panel reflects the change.
                self.frontend.set_history(project_history(&self.store));
            }
            Ok(HistoryOutcome::Noop) => {}
            Err(e) => log::warn!("history command refused: {e}"),
        }
    }
}
