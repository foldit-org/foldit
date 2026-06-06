//! Native-only dispatch mechanism.
//!
//! These methods own the plugin-side bookkeeping of dispatch (orchestrator
//! I/O + `StreamHost` table maintenance) and never touch `Session` or
//! `VisoEngine` — the coordination boundary keeps those on `App`. The
//! pull-drag dispatch group lives here too, alongside the inbound-event
//! drain and the runner-error reshaping.

#[cfg(not(target_arch = "wasm32"))]
use super::types::{
    edit_scope_from_handle, edit_scope_from_targets, ActiveStreamEntry, DispatchError,
    DispatchIntent, OpEvent, OpOutcome, StreamStartIntent,
};
#[cfg(not(target_arch = "wasm32"))]
use super::RunnerClient;

#[cfg(not(target_arch = "wasm32"))]
impl RunnerClient {
    /// Drain the orchestrator's queued plugin traffic and resolve each
    /// raw `PluginUpdate` into a core-side [`OpEvent`] keyed by the
    /// dispatch `request_id`, performing the terminal stream cleanup as it
    /// goes. Returns an empty `Vec` when no orchestrator is wired up.
    ///
    /// The runner's two success terminals (`Final` and `Cancelled`)
    /// collapse into one [`OpEvent::Commit`]: core commits either
    /// identically. The `request_id` is the same id `App` opened the edit
    /// under, so events carry it directly; whether an edit is actually
    /// open under it is `App`'s call (via `is_pending` / a no-op apply),
    /// which keeps the terminal cleanup here independent of edit state.
    pub(crate) fn drain_op_events(&mut self) -> Vec<OpEvent> {
        let updates = self
            .orchestrator
            .as_mut()
            .map(|orch| orch.drain_plugin_updates())
            .unwrap_or_default();
        let mut events = Vec::with_capacity(updates.len());
        for update in updates {
            use foldit_runner::orchestrator::PluginUpdate;
            match update {
                PluginUpdate::Pending {
                    request_id,
                    latest_assembly,
                    progress,
                    stage,
                } => {
                    let Some(assembly) = latest_assembly else {
                        log::trace!(
                            "plugin update Pending rid={request_id} \
                             progress={progress:?} stage={stage:?} \
                             (skipped: no assembly)"
                        );
                        continue;
                    };
                    // The dispatch id is the edit token. `App` no-ops the
                    // frame if no edit is open under it.
                    events.push(OpEvent::Update {
                        token: request_id,
                        assembly,
                    });
                }
                PluginUpdate::Cancelled {
                    request_id,
                    assembly,
                } => {
                    let entities = assembly.entities().len();
                    events.push(OpEvent::Commit {
                        token: Some(request_id),
                        assembly,
                    });
                    // Free the table entry / dispatch lock / pull-drag
                    // regardless of whether an edit was open.
                    let _ = self.release_terminal_stream(request_id);
                    log::info!(
                        "plugin update Cancelled rid={request_id} entities={entities}"
                    );
                }
                PluginUpdate::Final {
                    request_id,
                    assembly,
                    ..
                } => {
                    let entities = assembly.entities().len();
                    events.push(OpEvent::Commit {
                        token: Some(request_id),
                        assembly,
                    });
                    let _ = self.release_terminal_stream(request_id);
                    log::info!(
                        "plugin update Final rid={request_id} entities={entities}"
                    );
                }
                PluginUpdate::Error {
                    request_id,
                    message,
                } => {
                    events.push(OpEvent::Abort {
                        token: Some(request_id),
                        reason: message.clone(),
                    });
                    let _ = self.release_terminal_stream(request_id);
                    log::warn!(
                        "plugin update Error rid={request_id} message={message}"
                    );
                }
            }
        }
        events
    }

    /// Terminal stream cleanup (Cancelled / Final / Error): remove the
    /// entry from the active-streams table, release its dispatch locks
    /// on the orchestrator, and clear `pull_drag` if it pointed at this
    /// stream. Returns the entry's `plugin_id` so callers can log
    /// without re-querying.
    pub(crate) fn release_terminal_stream(&mut self, rid: u64) -> Option<String> {
        let entry = self.stream_host.active_streams.remove(&rid)?;
        let ActiveStreamEntry {
            handle, plugin_id, ..
        } = entry;
        if let Some(orch) = self.orchestrator.as_mut() {
            orch.release_dispatch_locks(handle);
        }
        if matches!(&self.stream_host.pull_drag, Some(d) if d.request_id == rid) {
            self.stream_host.pull_drag = None;
        }
        Some(plugin_id)
    }

    /// Send a cancel to every in-flight stream's plugin. Used by the
    /// ESC / `VisoCommand::ClearSelection` paths. Doesn't touch
    /// `active_streams`: the terminal cleanup runs when the plugin's
    /// `Cancelled` reply lands in the next drain.
    pub(crate) fn cancel_all_active_streams(&mut self) {
        let Some(orch) = self.orchestrator.as_mut() else {
            return;
        };
        for (rid, entry) in &self.stream_host.active_streams {
            if let Err(e) = orch.dispatch_cancel_stream(&entry.plugin_id, *rid) {
                log::warn!(
                    "dispatch_cancel_stream plugin={} rid={rid} failed: {e}",
                    entry.plugin_id,
                );
            }
        }
    }

    /// One-call dispatch: take the core-shaped [`DispatchIntent`], resolve
    /// the op kind off the registry, flatten the selection / convert params
    /// into the orchestrator's wire shapes, branch on Invoke vs Stream, and
    /// for streams insert the `ActiveStreamEntry` so the matching terminal
    /// arm can find it. `App` still owns the catalog lookup that produces
    /// `plugin_id` (passed in, since `App` needs it for `begin_action`) and
    /// the post-processing (`begin_action`, `apply_invoke_result`,
    /// projector pump, score poll). Returns a core-shaped
    /// [`DispatchError`] that names no orchestrator type.
    pub(crate) fn dispatch_op(
        &mut self,
        intent: DispatchIntent,
        plugin_id: String,
        entity_type_of: impl Fn(molex::EntityId) -> Option<molex::EntityKind>,
    ) -> Result<OpOutcome, DispatchError> {
        use foldit_runner::orchestrator::{
            DispatchContext, OpKind, ResidueRef,
        };
        let Some(orch) = self.orchestrator.as_mut() else {
            return Err(DispatchError::Failed(String::from(
                "orchestrator not initialized",
            )));
        };

        // Resolve Invoke vs Stream off the op registry. An op-id the
        // registry can't surface is dropped as a failed dispatch (no
        // destructive side effect), matching the prior drop-and-warn.
        let Some(cached) = orch.plugin_registry().get_op(&intent.op_id).cloned()
        else {
            return Err(DispatchError::Failed(format!(
                "op-id {:?} not in registry",
                intent.op_id
            )));
        };
        let kind = cached.kind;

        // Flatten the authoritative selection (molex ids) into the
        // wire-shape `ResidueRef` list the orchestrator's context expects.
        let selection: Vec<ResidueRef> = intent
            .selection
            .iter()
            .flat_map(|(entity, residues)| {
                let id = *entity;
                residues.iter().map(move |&residue_index| ResidueRef {
                    entity_id: id,
                    residue_index,
                })
            })
            .collect();

        let ctx = DispatchContext {
            focused_entity_id: intent
                .focused_entity_id
                .map(|raw| molex::EntityId::from_raw(raw as u32)),
            selection,
        };
        let params: std::collections::HashMap<
            String,
            foldit_runner::orchestrator::ParamValue,
        > = intent
            .params
            .into_iter()
            .map(|(k, v)| (k, crate::wire_params::param_value_from_wire(v)))
            .collect();

        match kind {
            OpKind::Invoke => orch
                .dispatch_invoke(&intent.op_id, ctx, params, entity_type_of)
                .map(|(request_id, bytes, targets)| OpOutcome::Invoke {
                    request_id,
                    bytes,
                    scope: edit_scope_from_targets(targets),
                })
                .map_err(map_dispatch_error),
            OpKind::Stream => {
                let (rid, handle) = orch
                    .dispatch_start_stream(&intent.op_id, ctx, params, entity_type_of)
                    .map_err(map_dispatch_error)?;
                // Derive the edit scope from the handle (the set the op
                // actually locked) before it is consumed into the table.
                let scope = edit_scope_from_handle(&handle);
                let _ = self.stream_host.active_streams.insert(
                    rid,
                    ActiveStreamEntry { handle, plugin_id },
                );
                Ok(OpOutcome::Stream {
                    request_id: rid,
                    scope,
                })
            }
        }
    }

    // ── Pull-drag dispatch ──────────────────────────────────────────────

    /// Pull-drag dispatch: take the core-shaped [`StreamStartIntent`],
    /// resolve the plugin id off the registry, build the `DispatchContext`
    /// + start params internally, call `dispatch_start_stream`, insert the
    /// `ActiveStreamEntry`, and return the dispatch `request_id` plus the
    /// resolved plugin id. Pull-drag is always a stream, so there is no
    /// Invoke branch. `App` keeps the `begin_action` history side-effect
    /// (it needs the returned `plugin_id`, and opens the edit under the
    /// returned `request_id`) and the `PullDrag` state. Returns a
    /// core-shaped [`DispatchError`] that names no orchestrator type.
    pub(crate) fn start_stream(
        &mut self,
        intent: StreamStartIntent,
        entity_type_of: impl Fn(molex::EntityId) -> Option<molex::EntityKind>,
    ) -> Result<(u64, String), DispatchError> {
        use foldit_runner::orchestrator::{DispatchContext, ResidueRef};
        let Some(orch) = self.orchestrator.as_mut() else {
            return Err(DispatchError::Failed(String::from(
                "orchestrator not initialized",
            )));
        };
        let Some(cached) = orch.plugin_registry().get_op(intent.op_id).cloned() else {
            return Err(DispatchError::Failed(format!(
                "op-id {:?} not in registry",
                intent.op_id
            )));
        };
        let plugin_id = cached.plugin_id.clone();

        let ctx = DispatchContext {
            focused_entity_id: Some(intent.focused_entity),
            selection: vec![ResidueRef {
                entity_id: intent.focused_entity,
                residue_index: intent.residue_in_entity,
            }],
        };
        let params = crate::pull_drag::build_start_params(
            intent.op_id,
            intent.residue_in_entity,
            &intent.atom_name,
        );

        let (rid, handle) = orch
            .dispatch_start_stream(intent.op_id, ctx, params, entity_type_of)
            .map_err(map_dispatch_error)?;
        let _ = self.stream_host.active_streams.insert(
            rid,
            ActiveStreamEntry {
                handle,
                plugin_id: plugin_id.clone(),
            },
        );
        Ok((rid, plugin_id))
    }

    /// Push a single-key `endpoint` `Vec3` update to a running pull stream.
    /// The `"endpoint"` param key is a bridge-protocol detail and lives
    /// behind this barrier, not in `App`. No-op (logged at trace) when no
    /// orchestrator is wired up or the dispatch is rejected.
    pub(crate) fn update_stream(&self, rid: u64, plugin_id: &str, endpoint: glam::Vec3) {
        use foldit_runner::orchestrator::ParamValue;
        let Some(orch) = self.orchestrator.as_ref() else {
            return;
        };
        let mut params = std::collections::HashMap::new();
        let _ = params.insert(
            String::from("endpoint"),
            ParamValue::Vec3([endpoint.x, endpoint.y, endpoint.z]),
        );
        if let Err(e) = orch.dispatch_update_stream(plugin_id, rid, params) {
            log::trace!("update_stream: dispatch_update_stream rid={rid} failed: {e}");
        }
    }

    /// Thin pass-through that asks the orchestrator to cancel a running
    /// pull stream. The terminal commit still flows through
    /// `drain_op_events` on the plugin's `Cancelled` reply — this only
    /// sends the cancel. No-op (logged) when no orchestrator exists.
    pub(crate) fn end_stream(&self, rid: u64, plugin_id: &str) {
        let Some(orch) = self.orchestrator.as_ref() else {
            return;
        };
        if let Err(e) = orch.dispatch_cancel_stream(plugin_id, rid) {
            log::trace!("end_stream: dispatch_cancel_stream rid={rid} failed: {e}");
        }
    }

    /// Allocate a dispatch `request_id` from the orchestrator (the single
    /// id authority) for a host-internal action that opens an edit without
    /// going through dispatch — e.g. seeding a post-Init normalized
    /// assembly. `None` when no orchestrator is wired up.
    pub(crate) fn alloc_request_id(&mut self) -> Option<u64> {
        self.orchestrator.as_mut().map(|orch| orch.alloc_request_id())
    }

    /// Whether a pull-drag is currently live (the three input guards).
    pub(crate) fn has_active_pull_drag(&self) -> bool {
        self.stream_host.pull_drag.is_some()
    }

    /// Snapshot the live drag's viso `PullInfo` for the visualization
    /// passes (cloned so the engine borrow doesn't overlap the field).
    pub(crate) fn pull_drag_pull_info(&self) -> Option<viso::PullInfo> {
        self.stream_host
            .pull_drag
            .as_ref()
            .map(|d| d.pull_info.clone())
    }

    /// Mutable handle to the live drag (pointer-move updates its
    /// `screen_target` and reads its rid / plugin id).
    pub(crate) fn pull_drag_mut(&mut self) -> Option<&mut crate::pull_drag::PullDrag> {
        self.stream_host.pull_drag.as_mut()
    }

    /// Install the live drag state on stream start.
    pub(crate) fn set_pull_drag(&mut self, drag: crate::pull_drag::PullDrag) {
        self.stream_host.pull_drag = Some(drag);
    }

    /// Take + clear the live drag state on pointer-up / cancel.
    pub(crate) fn take_pull_drag(&mut self) -> Option<crate::pull_drag::PullDrag> {
        self.stream_host.pull_drag.take()
    }
}

/// Reshape a runner `OpDispatchError` into the core-side [`DispatchError`].
/// The lock-refusal arm is unwrapped to the bare entity id so no runner
/// type crosses the boundary; everything else collapses to `Failed`.
#[cfg(not(target_arch = "wasm32"))]
fn map_dispatch_error(
    e: foldit_runner::orchestrator::OpDispatchError,
) -> DispatchError {
    use foldit_runner::orchestrator::{DispatchError as RunnerDispatchError, OpDispatchError};
    match e {
        OpDispatchError::LockRefused(RunnerDispatchError::EntityLocked {
            entity,
            ..
        }) => DispatchError::EntityLocked {
            entity: u64::from(entity.raw()),
        },
        OpDispatchError::LockRefused(RunnerDispatchError::BackendBusy {
            plugin_id,
        }) => DispatchError::BackendBusy { plugin_id },
        other => DispatchError::Failed(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── map_dispatch_error: runner refusal → core shape ──

    /// A runner lock-refusal must surface as the core `EntityLocked`
    /// variant carrying the bare entity id, so `App` can treat a busy
    /// entity as advisory without ever naming a runner type.
    #[test]
    fn lock_refusal_maps_to_entity_locked() {
        use foldit_runner::orchestrator::{
            DispatchError as RunnerDispatchError, OpDispatchError,
        };
        let runner_err = OpDispatchError::LockRefused(
            RunnerDispatchError::EntityLocked {
                entity: molex::EntityId::from_raw(7),
                current_op: None,
            },
        );
        match map_dispatch_error(runner_err) {
            DispatchError::EntityLocked { entity } => assert_eq!(entity, 7),
            DispatchError::BackendBusy { plugin_id } => {
                panic!("expected EntityLocked, got BackendBusy({plugin_id})")
            }
            DispatchError::Failed(s) => panic!("expected EntityLocked, got Failed({s})"),
        }
    }

    /// A runner backend-busy refusal must surface as the core
    /// `BackendBusy` variant (advisory), not `Failed`.
    #[test]
    fn backend_busy_maps_to_backend_busy() {
        use foldit_runner::orchestrator::{
            DispatchError as RunnerDispatchError, OpDispatchError,
        };
        let runner_err = OpDispatchError::LockRefused(
            RunnerDispatchError::BackendBusy {
                plugin_id: String::from("rosetta"),
            },
        );
        match map_dispatch_error(runner_err) {
            DispatchError::BackendBusy { plugin_id } => {
                assert_eq!(plugin_id, "rosetta");
            }
            DispatchError::EntityLocked { entity } => {
                panic!("expected BackendBusy, got EntityLocked({entity})")
            }
            DispatchError::Failed(s) => {
                panic!("expected BackendBusy, got Failed({s})")
            }
        }
    }

    /// Any non-lock runner error collapses to `Failed`.
    #[test]
    fn other_dispatch_error_maps_to_failed() {
        use foldit_runner::orchestrator::OpDispatchError;
        let runner_err = OpDispatchError::UnknownOp("nope".to_string());
        assert!(matches!(
            map_dispatch_error(runner_err),
            DispatchError::Failed(_)
        ));
    }
}
