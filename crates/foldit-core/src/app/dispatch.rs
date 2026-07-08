use molex::entity::molecule::id::EntityId;

use crate::app::App;
#[cfg(not(target_arch = "wasm32"))]
use crate::app::refine::RefineEvent;
#[cfg(not(target_arch = "wasm32"))]
use crate::history::CheckpointKind;
#[cfg(not(target_arch = "wasm32"))]
use crate::runner_client::{DispatchError, DispatchIntent, EditScope, OpEvent, OpOutcome};
#[cfg(not(target_arch = "wasm32"))]
use viso::Focus;

impl App {
    #[allow(
        clippy::too_many_lines,
        reason = "flat dispatch over the OpEvent enum; splitting the arms would scatter the stream-event handling that reads best in one place"
    )]
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
                    OpEvent::Progress {
                        token,
                        progress,
                        stage,
                    } => {
                        if self
                            .runner_client
                            .update_weights_progress(token, progress, stage)
                        {
                            self.mark_dirty(foldit_gui::DirtyFlags::ACTIONS);
                        }
                    }
                    OpEvent::Commit {
                        token,
                        assembly,
                        score,
                        creates_entities,
                        preview,
                    } => {
                        if let Some(plugin_id) =
                            token.and_then(|rid| self.runner_client.is_downloading_rid(rid))
                        {
                            self.on_weights_download_committed(&plugin_id);
                        } else {
                            self.apply_commit_event(
                                token,
                                &assembly,
                                score,
                                creates_entities,
                                preview,
                            );
                        }
                    }
                    OpEvent::Abort { token, reason } => {
                        if let Some(token) = token {
                            // A failed weight download flips the plugin to
                            // Failed (its download button stays as a retry); it
                            // opened no history edit, so skip the preview / ghost
                            // / abort-action cleanup a real op stream needs.
                            if self.runner_client.set_weights_failed(token, reason.clone()) {
                                self.gui.push_notification(
                                    foldit_gui::NotificationLevel::Error,
                                    reason.clone(),
                                );
                                self.mark_dirty(foldit_gui::DirtyFlags::ACTIONS);
                            } else {
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
                        }
                        log::warn!("plugin op aborted: {reason}");
                    }
                }
            }
        }
    }

    /// Handle a weight-download stream's Commit terminal. A download terminal
    /// carries no real geometry, so re-query `weights_status` rather than
    /// assuming Ready: the fresh reply is authoritative about whether the
    /// download produced usable weights (-> Ready, normal buttons return) or
    /// not (-> back to Missing, download button stays). The Info toast is the
    /// neutral success counterpart to the Error toast a failed download raises
    /// on Abort.
    #[cfg(not(target_arch = "wasm32"))]
    fn on_weights_download_committed(&mut self, plugin_id: &str) {
        self.runner_client.request_weights_status();
        self.gui.push_notification(
            foldit_gui::NotificationLevel::Info,
            format!("{plugin_id} weights downloaded"),
        );
        self.mark_dirty(foldit_gui::DirtyFlags::ACTIONS);
    }

    /// Dispatch a plugin op by op-id (fire-and-forget; any stream rid is
    /// discarded). The panel stream-control path shares the same body via
    /// [`Self::dispatch_op_inner`] but keeps the rid.
    pub fn handle_dispatch_op(&mut self, op: foldit_gui::OpDispatch) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let _ = self.dispatch_op_inner(op);
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = op;
        }
    }

    /// Single source for plugin op dispatch: resolve the op off the
    /// registry, open the history edit, and run the resulting outcome.
    /// Returns the stream `request_id` when the op dispatched as a stream
    /// (so a panel can drive update / cancel against it); `None` for a
    /// one-shot invoke or any dispatch failure.
    #[cfg(not(target_arch = "wasm32"))]
    pub(in crate::app) fn dispatch_op_inner(&mut self, op: foldit_gui::OpDispatch) -> Option<u64> {
        let focused_entity_id: Option<molex::EntityId> = match self.store.focus() {
            Focus::Entity(eid) => Some(eid),
            Focus::All => None,
        };

        // A handful of ops are performed in-process by the host with no plugin
        // round-trip. They carry no plugin owner, so they must be intercepted
        // here: `resolve_op_plugin_id` below drops any op-id the plugin
        // registry doesn't know, which would silently discard a native op.
        if op.op_id == "mutate_residue" {
            return self.dispatch_native_mutate(&op);
        }
        // Every ML plugin registers `download_weights` under the same op-id,
        // so `resolve_op_plugin_id` collapses them to one arbitrary
        // last-writer owner and the download would hit the wrong plugin.
        // Route it to the plugin the button belongs to instead, read off
        // the `plugin_id` param the action rides in on.
        if op.op_id == "download_weights" {
            return self.dispatch_native_download(&op, focused_entity_id);
        }
        // Off-thread crystallographic B-factor refine: molex runs on a
        // background thread and the result applies on completion in `tick`, so
        // this intercept only kicks the thread and returns.
        if op.op_id == "refine_b" {
            return self.dispatch_native_refine();
        }

        let Some(plugin_id) = self.runner_client.resolve_op_plugin_id(&op.op_id) else {
            log::warn!(
                "dispatch_op_inner({:?}): op not resolvable (no orchestrator or op-id not in registry)",
                op.op_id
            );
            return None;
        };

        let display = self
            .runner_client
            .op_display(&plugin_id, &op.op_id)
            .unwrap_or_else(|| op.op_id.clone());
        let intent = self.dispatch_intent_from_op(op.op_id.clone(), op.params, focused_entity_id);
        let store = &self.store;
        let dispatch_outcome = self
            .runner_client
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
                        "dispatch_op_inner({:?}): begin_action skipped: {e}",
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
            Ok(OpOutcome::Stream { request_id, .. }) => Some(request_id),
            Ok(OpOutcome::Invoke { bytes, .. }) => {
                self.apply_invoke_result(&bytes, edit_token);
                None
            }
            Err(DispatchError::EntityLocked { entity }) => {
                log::warn!(
                    "dispatch_op_inner({:?}): dispatch refused, entity {entity} locked",
                    op.op_id
                );
                None
            }
            Err(DispatchError::BackendBusy { plugin_id }) => {
                log::info!("dispatch refused: backend {plugin_id} busy");
                None
            }
            Err(DispatchError::Failed(s)) => {
                log::error!("dispatch_op_inner({:?}): dispatch failed: {s}", op.op_id);
                None
            }
        }
    }

    /// Build the [`DispatchIntent`] for an op from the current selection and
    /// designable set, tagged with the already-resolved `focused_entity_id`.
    /// Both dispatch paths (registry-routed and the native download
    /// intercept) assemble the intent identically through here, so the focus
    /// lookup lives in exactly one place. Takes `op_id` and `params` by value
    /// so the registry path can hand its owned params straight through; the
    /// download intercept, which only borrows its op, clones at the call.
    #[cfg(not(target_arch = "wasm32"))]
    fn dispatch_intent_from_op(
        &self,
        op_id: String,
        params: std::collections::HashMap<String, foldit_gui::state::ParamValue>,
        focused_entity_id: Option<molex::EntityId>,
    ) -> DispatchIntent {
        DispatchIntent {
            selection: self.store.selection().clone(),
            designable: self.store.designable_residues(),
            focused_entity_id,
            op_id,
            params,
        }
    }

    /// Mutate the single selected residue entirely in the host: swap its
    /// amino acid via molex, apply the rebuilt protein into the session,
    /// commit, and fire a rosetta rescore on the committed checkpoint. No
    /// plugin is involved in the edit; rosetta runs only for the score.
    ///
    /// Every unmet precondition is a refused no-op (`None`): a held plugin
    /// lock, a selection that isn't exactly one designable residue, a
    /// missing / unparseable amino-acid param, a non-protein target, or a
    /// molex placement failure. None of these panic or leave a dangling
    /// tentative. Returns the edit's `request_id` on success.
    #[cfg(not(target_arch = "wasm32"))]
    fn dispatch_native_mutate(&mut self, op: &foldit_gui::OpDispatch) -> Option<u64> {
        // Refuse while a plugin operation holds a lock: a concurrent native
        // mutation would race the in-flight edit. Read the lock state
        // immutably, never taking the orchestrator's mutable handle.
        if self.runner_client.any_lock_held() {
            return None;
        }

        // Exactly one entity, exactly one selected residue, and that
        // selection must be designable (the gate the action button shows).
        if !self.store.selection_is_designable() {
            return None;
        }
        let (eid, res_idx) = {
            let selection = self.store.selection();
            if selection.len() != 1 {
                return None;
            }
            let (eid, residues) = selection.iter().next()?;
            if residues.len() != 1 {
                return None;
            }
            (*eid, *residues.iter().next()?)
        };

        // The 3-letter amino-acid code rides on the "aa" param; there is no
        // 1-letter residue constructor.
        let Some(foldit_gui::state::ParamValue::String(code)) = op.params.get("aa") else {
            return None;
        };
        let code_bytes: [u8; 3] = code.as_bytes().try_into().ok()?;
        let aa = molex::chemistry::AminoAcid::from_code(code_bytes)?;

        // The selected residue's u32 is the same 0-based positional index
        // molex `mutate_residue` takes. `mutate_residue` preserves the entity
        // id, so the rebuilt protein re-applies onto the same lane.
        let new_protein = self
            .store
            .entity(eid)?
            .as_protein()?
            .mutate_residue(res_idx as usize, aa)
            .ok()?;
        let assembly = molex::Assembly::new(vec![molex::MoleculeEntity::Protein(new_protein)]);

        // Host-internal action: no dispatch happened, so the edit's
        // request_id is drawn straight from the orchestrator (the single id
        // authority), then applied through the same begin/apply/commit
        // sequence a plugin invoke uses.
        let rid = self.runner_client.alloc_request_id()?;
        let display = String::from("Mutate");
        let kind = CheckpointKind::NativeEdit {
            op_id: op.op_id.clone(),
            display: display.clone(),
        };
        if let Err(e) = self.store.begin_action([eid], kind, display, rid) {
            log::trace!("dispatch_native_mutate: begin_action skipped: {e}");
            return None;
        }
        let applied = self.store.apply_streaming_assembly(&assembly, None, rid);
        if !applied {
            // The incoming assembly did not match the open lane, so no edit
            // landed. Discard the tentative rather than committing it: a missed
            // apply must be a true no-op, not a phantom checkpoint that moves
            // the session head.
            let _ = self.store.abort_action(rid);
            return None;
        }
        match self.store.commit_action(rid) {
            Ok(ckpt) => {
                self.scores
                    .score_committed_checkpoint(&mut self.runner_client, &self.store, ckpt);
                self.spawn_rfree_compute(ckpt);
                Some(rid)
            }
            Err(e) => {
                log::warn!("dispatch_native_mutate: commit_action failed: {e}");
                None
            }
        }
    }

    /// Kick a crystallographic B-factor refine on a background thread. Every
    /// unmet precondition is a refused no-op with a user-facing error toast: a
    /// held plugin lock, a refine already running, no shared GPU device, no
    /// loaded density, or an empty committed head. Snapshots the committed head
    /// (the refine input and the apply-time race fingerprint), then spawns a
    /// thread that runs molex only and streams progress / completion back over
    /// the channel; `tick`'s drain applies the result on the main thread.
    ///
    /// Opens no history edit here and no stream, so returns `None` always.
    #[cfg(not(target_arch = "wasm32"))]
    fn dispatch_native_refine(&mut self) -> Option<u64> {
        const N_MACRO_CYCLES: usize = 5;

        // Refuse while a plugin operation holds a lock: applying the refined B
        // on completion would race the in-flight edit.
        if self.runner_client.any_lock_held() {
            self.gui.push_notification(
                foldit_gui::NotificationLevel::Error,
                "Cannot refine while another operation is running".to_owned(),
            );
            return None;
        }
        if self.refine_in_flight {
            self.gui.push_notification(
                foldit_gui::NotificationLevel::Error,
                "A B-factor refine is already running".to_owned(),
            );
            return None;
        }
        if self.experimental_data.is_none() {
            self.gui.push_notification(
                foldit_gui::NotificationLevel::Error,
                "B-factor refine needs a loaded density".to_owned(),
            );
            return None;
        }
        if self.shared_device.is_none() {
            self.gui.push_notification(
                foldit_gui::NotificationLevel::Error,
                "B-factor refine needs a GPU device".to_owned(),
            );
            return None;
        }

        // The density and device were just confirmed present, so an empty
        // committed head is the only `None` left here. The snapshot is the
        // refine input and the source of the `(entity, atom count)` fingerprint
        // the apply step re-checks, so a mid-refine edit discards the result
        // rather than scattering stale B onto changed geometry.
        let Some((data, dev, snapshot, table)) = self.xtal_job_inputs() else {
            self.gui.push_notification(
                foldit_gui::NotificationLevel::Error,
                "No structure to refine".to_owned(),
            );
            return None;
        };
        self.refine_fingerprint = snapshot
            .iter()
            .map(|e| (e.id().raw(), e.atom_count()))
            .collect();

        self.refine_in_flight = true;
        self.gui.actions.refine_progress = Some(foldit_gui::state::RefineProgress {
            fraction: 0.0,
            label: "Refining B-factors...".to_owned(),
        });
        self.mark_dirty(foldit_gui::DirtyFlags::ACTIONS);

        let tx = self.refine_tx.clone();
        std::thread::spawn(move || {
            let progress_tx = tx.clone();
            let result = molex::xtal::refine_b_from_atom_table_gpu(
                &data,
                &table,
                N_MACRO_CYCLES,
                &dev,
                move |macro_cycle, inner_iter, inner_total| {
                    let _ = progress_tx.send(RefineEvent::Progress {
                        macro_cycle,
                        inner_iter,
                        inner_total,
                    });
                },
            );
            let event = match result {
                Some((full_b, r_work, r_free)) => RefineEvent::Done {
                    full_b,
                    r_work,
                    r_free,
                },
                None => RefineEvent::Failed("B-factor refinement failed".to_owned()),
            };
            let _ = tx.send(event);
        });

        None
    }

    /// Route a weight download to the specific plugin named in the op's
    /// `plugin_id` param, bypassing the flat op-registry resolution. The
    /// download stream is registered like any other stream (so its
    /// terminal releases the dispatch locks), but it opens no history
    /// edit: a download changes no geometry. Returns the stream
    /// `request_id`, or `None` when the `plugin_id` param is absent / not a
    /// string, or the dispatch fails.
    #[cfg(not(target_arch = "wasm32"))]
    fn dispatch_native_download(
        &mut self,
        op: &foldit_gui::OpDispatch,
        focused_entity_id: Option<molex::EntityId>,
    ) -> Option<u64> {
        let Some(foldit_gui::state::ParamValue::String(plugin_id)) = op.params.get("plugin_id")
        else {
            log::warn!("download_weights dispatch missing string plugin_id param; refused");
            return None;
        };
        let plugin_id = plugin_id.clone();

        // Refuse a second download for a plugin whose download is already in
        // flight: the button is disabled while Downloading, but a queued or
        // racing click could still reach here and start a duplicate stream.
        if self.runner_client.is_plugin_downloading(&plugin_id) {
            log::warn!("download_weights refused: {plugin_id} weights already downloading");
            return None;
        }

        let intent =
            self.dispatch_intent_from_op(op.op_id.clone(), op.params.clone(), focused_entity_id);
        let rid = {
            let store = &self.store;
            self.runner_client
                .dispatch_stream_on_plugin_lockless(&plugin_id, intent, |id| store.entity_type(id))
        };
        if let Some(rid) = rid {
            // Stamp Downloading at dispatch, not on the first progress frame:
            // a download that terminates before any frame is still matched by
            // rid at its terminal.
            self.runner_client.set_weights_downloading(&plugin_id, rid);
            self.mark_dirty(foldit_gui::DirtyFlags::ACTIONS);
        }
        rid
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
                    self.spawn_rfree_compute(ckpt);
                }
                Err(e) => log::warn!("dispatch_invoke: commit_action failed: {e}"),
            }
        } else if self.store.is_pending(token) {
            let _ = self.store.commit_action(token);
        }
        let _ = self.scores.remove_target(token);
    }
}
