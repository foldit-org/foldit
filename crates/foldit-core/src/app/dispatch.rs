use molex::entity::molecule::id::EntityId;

use crate::app::App;
#[cfg(not(target_arch = "wasm32"))]
use crate::history::CheckpointKind;
#[cfg(not(target_arch = "wasm32"))]
use crate::runner_client::{DispatchError, DispatchIntent, EditScope, OpEvent, OpOutcome};
#[cfg(not(target_arch = "wasm32"))]
use viso::Focus;

impl App {
    pub fn apply_backend_updates(&mut self) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let events = self.runner_client.drain_op_events();
            if events.is_empty() {
                return;
            }

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
                            self.store.stream_preview_frame(token, &assembly);
                        } else if preview {
                            // Preview-style op: the frame animates the
                            // discardable ghost, never the frozen lane.
                            self.store.stream_inplace_preview_frame(token, &assembly);
                        } else {
                            let applied =
                                self.store.apply_streaming_assembly(&assembly, None, token);
                            if applied {
                                if let Some(report) = score {
                                    let (raw, game, breakdown) =
                                        self.scores.prepare_score_stamp(report);
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
                    } => {
                        self.apply_commit_event(token, &assembly, score, creates_entities, preview);
                    }
                    OpEvent::Abort { token, reason } => {
                        if let Some(token) = token {
                            // Discard any in-progress creates-entities preview
                            // or in-place ghost this stream was animating.
                            self.store.discard_created_preview(token);
                            self.store.discard_inplace_ghost(token);
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

    /// Dispatch a plugin op by op-id.
    pub fn handle_dispatch_op(&mut self, op: foldit_gui::OpDispatch) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let focused_entity_id: Option<molex::EntityId> = match self.store.focus() {
                Focus::Entity(eid) => Some(eid),
                Focus::All => None,
            };

            let Some(plugin_id) = self.runner_client.resolve_op_plugin_id(&op.op_id) else {
                log::warn!(
                    "handle_dispatch_op({:?}): op not resolvable (no orchestrator or op-id not in registry)",
                    op.op_id
                );
                return;
            };

            let display = self
                .runner_client
                .op_display(&plugin_id, &op.op_id)
                .unwrap_or_else(|| op.op_id.clone());
            let intent = DispatchIntent {
                selection: self.store.selection().clone(),
                designable: self.store.designable_residues(),
                focused_entity_id,
                op_id: op.op_id.clone(),
                params: op.params,
            };
            let store = &self.store;
            let dispatch_outcome =
                self.runner_client
                    .dispatch_op(intent, plugin_id.clone(), |id| store.entity_type(id));

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

            let creates_entities = self.runner_client.op_creates_entities(&op.op_id);
            let preview = self.runner_client.op_preview(&plugin_id, &op.op_id);

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
                match self
                    .store
                    .begin_action(lanes, kind, display.clone(), request_id)
                {
                    Ok(()) => Some((request_id, seed_lane)),
                    Err(e) => {
                        log::trace!(
                            "handle_dispatch_op({:?}): begin_action skipped: {e}",
                            op.op_id
                        );
                        None
                    }
                }
            });

            if preview {
                if let Some((token, Some(lane_id))) = edit_token {
                    self.store.seed_inplace_preview(token, lane_id, display);
                }
            }
            let edit_token = edit_token.map(|(request_id, _)| request_id);

            match dispatch_outcome {
                Ok(OpOutcome::Stream { .. }) => {
                }
                Ok(OpOutcome::Invoke { bytes, .. }) => {
                    self.apply_invoke_result(&bytes, edit_token);
                }
                Err(DispatchError::EntityLocked { entity }) => {
                    log::warn!(
                        "handle_dispatch_op({:?}): dispatch refused, entity {entity} locked",
                        op.op_id
                    );
                }
                Err(DispatchError::BackendBusy { plugin_id }) => {
                    log::info!("dispatch refused: backend {plugin_id} busy");
                }
                Err(DispatchError::Failed(s)) => {
                    log::error!("handle_dispatch_op({:?}): dispatch failed: {s}", op.op_id);
                }
            }
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = op;
        }
    }

    /// Resolve a dispatch's [`EditScope`] into the concrete set of lanes the
    /// edit opens over.
    #[cfg(not(target_arch = "wasm32"))]
    pub(in crate::app) fn lanes_for_scope(&self, scope: &EditScope) -> Vec<EntityId> {
        let has_lane = |id: &EntityId| self.store.history().lane(*id).is_some();
        match scope {
            EditScope::AllEntities => self.store.ids().filter(has_lane).collect(),
            EditScope::Entities(set) => set.iter().copied().filter(has_lane).collect(),
        }
    }

    /// Apply the assembly bytes returned by a one-shot `dispatch_invoke`
    /// to the ongoing tentative and commit it.
    #[cfg(not(target_arch = "wasm32"))]
    fn apply_invoke_result(&mut self, bytes: &[u8], edit_token: Option<u64>) {
        let Some(token) = edit_token else {
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
                Ok(ckpt) => {
                    self.scores.score_committed_checkpoint(
                        &mut self.runner_client,
                        &self.store,
                        ckpt,
                    );
                }
                Err(e) => log::warn!("dispatch_invoke: commit_action failed: {e}"),
            }
        } else if self.store.is_pending(token) {
            let _ = self.store.commit_action(token);
        }
        let _ = self.scores.remove_target(token);
    }
}
