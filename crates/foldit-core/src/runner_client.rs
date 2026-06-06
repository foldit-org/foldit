//! Runner client: owns the orchestrator handle and the native stream
//! bookkeeping that drives plugin operations.
//!
//! `RunnerClient` holds the `Orchestrator` and (on native builds) the
//! in-flight `StreamHost` state, plus the orchestrator-lifecycle handlers
//! that touch only the orchestrator (`reset_for_new_structure`,
//! `shutdown`).
//! Inbound plugin traffic is drained here too: [`RunnerClient::drain_op_events`]
//! consumes the orchestrator's raw `PluginUpdate`s and the stream table,
//! resolving each into a core-side [`OpEvent`] keyed by the edit token so
//! `App` applies them without naming orchestrator types or touching the
//! stream bookkeeping.
//!
//! `App::handle_dispatch_op` still interleaves orchestrator I/O with store
//! mutations (it begins the edit only after the dispatch succeeds), so it
//! stays on `App` and reaches into `self.runner_client` for the orchestrator
//! and stream state it needs.

use molex::Assembly;

/// Owns the orchestrator handle and the native-only stream bookkeeping.
/// `App` holds one of these and reaches into its fields by direct path so
/// the orchestrator and stream state can be borrowed disjointly (the
/// dispatch methods on `App` rely on this). The `SessionUpdate` stream's
/// plugin projection lives separately in `RunnerProjector`, a peer `App`
/// field, so the two can be borrowed disjointly across the tick seam.
pub struct RunnerClient {
    pub(crate) orchestrator: Option<foldit_runner::Orchestrator>,
    #[cfg(not(target_arch = "wasm32"))]
    pub stream_host: StreamHost,
}

impl RunnerClient {
    pub fn new() -> Self {
        Self {
            orchestrator: None,
            #[cfg(not(target_arch = "wasm32"))]
            stream_host: StreamHost {
                active_streams: std::collections::HashMap::new(),
                pull_drag: None,
            },
        }
    }

    /// Construct and install a fresh orchestrator handle. Called on
    /// structure load (and again on the load-error path) to replace any
    /// prior handle before plugin discovery runs.
    pub(crate) fn init_orchestrator(&mut self) {
        self.orchestrator = Some(foldit_runner::Orchestrator::new());
    }

    /// Mutable access to the orchestrator handle, so the tick seam can
    /// borrow it disjointly from the peer `RunnerProjector` field on `App`.
    pub(crate) fn orchestrator_mut(&mut self) -> Option<&mut foldit_runner::Orchestrator> {
        self.orchestrator.as_mut()
    }

    /// Release any lock state when puzzle topology changes.
    pub fn reset_for_new_structure(&mut self) {
        if let Some(ref mut orch) = self.orchestrator {
            for eid in orch.locked_entities() {
                orch.unlock(eid);
            }
        }
    }

    /// Shut down the orchestrator (and, through it, plugin workers).
    pub fn shutdown(&self) {
        if let Some(ref orch) = self.orchestrator {
            orch.shutdown();
        }
    }
}

// ── Native-only dispatch mechanism ──────────────────────────────────────
//
// These methods own the plugin-side bookkeeping of dispatch (orchestrator
// I/O + `StreamHost` table maintenance) and never touch `Session` or
// `VisoEngine` — the coordination boundary keeps those on `App`.

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

    // ── Action catalog ──────────────────────────────────────────────────

    /// Project the orchestrator's op catalog into the GUI's [`ActionInfo`]
    /// shape, resolving each entry's `enabled` flag against the current
    /// lock state plus the supplied focus/selection. Read-only: no lock is
    /// taken. Empty when no orchestrator is wired up.
    ///
    /// The selection flatten mirrors [`Self::dispatch_op`]'s, so the
    /// availability reasoning sees the exact target set a real dispatch
    /// would. The `CatalogEntry -> ActionInfo` forward lives here so `App`
    /// names no runner catalog type.
    ///
    /// [`ActionInfo`]: foldit_gui::state::ActionInfo
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn actions_catalog<F>(
        &self,
        focus: Option<molex::EntityId>,
        selection: &std::collections::BTreeMap<
            molex::EntityId,
            std::collections::BTreeSet<u32>,
        >,
        entity_type_of: F,
    ) -> Vec<foldit_gui::state::ActionInfo>
    where
        F: Fn(molex::EntityId) -> Option<molex::EntityKind>,
    {
        use foldit_gui::state::ActionInfo;
        use foldit_runner::orchestrator::{DispatchContext, ResidueRef};

        let Some(orch) = self.orchestrator.as_ref() else {
            return vec![];
        };

        // Same flatten dispatch_op does: molex ids -> wire-shape refs.
        let flat: Vec<ResidueRef> = selection
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
            focused_entity_id: focus,
            selection: flat,
        };

        orch.actions_catalog(&ctx, entity_type_of)
            .into_iter()
            .map(|entry| ActionInfo {
                op_id: entry.op_id,
                plugin_id: entry.plugin_id,
                display: entry.display,
                icon_path: entry.icon_path.to_string_lossy().into_owned(),
                enabled: entry.enabled,
                active: false,
                hotkey: entry.hotkey,
                tooltip: entry.tooltip,
                params: entry
                    .params
                    .into_iter()
                    .map(crate::wire_params::param_spec_to_wire)
                    .collect(),
            })
            .collect()
    }

    /// Resolve a manifest hotkey string to its `(plugin_id, op_id)` via the
    /// static op catalog. `None` when no catalog button binds the key.
    /// Static identity only: no focus/selection/lock state involved.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn hotkey_to_op(&self, key: &str) -> Option<(String, String)> {
        let orch = self.orchestrator.as_ref()?;
        orch.ops_catalog()
            .into_iter()
            .find(|e| e.hotkey.as_deref() == Some(key))
            .map(|e| (e.plugin_id, e.op_id))
    }

    /// Static display label for `(plugin_id, op_id)` from the op catalog.
    /// `None` when the op isn't surfaced as a manifest button (the caller
    /// falls back to the op id). Static identity only.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn op_display(&self, plugin_id: &str, op_id: &str) -> Option<String> {
        let orch = self.orchestrator.as_ref()?;
        orch.ops_catalog()
            .into_iter()
            .find(|e| e.plugin_id == plugin_id && e.op_id == op_id)
            .map(|e| e.display)
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

    // ── Score paths ─────────────────────────────────────────────────────
    //
    // Forward the well-known `score` query to the orchestrator, building
    // the default dispatch context internally so the score query covers
    // the whole assembly. App owns merging the returned reports into the
    // head checkpoint and pushing per-residue colors. Reports cross the
    // facade as the core-owned `crate::scores::ScoreReport`; the proto type
    // is named only inside this module's `From` conversion below.

    /// Blocking score round-trip: fan the `score` query across every
    /// provider and return one report per provider that replied. Used
    /// until the first score lands, where a synchronous result keeps the
    /// load gate deterministic. Empty map when no orchestrator is wired up.
    pub(crate) fn collect_scores_blocking(
        &mut self,
    ) -> std::collections::HashMap<String, crate::scores::ScoreReport> {
        use foldit_runner::orchestrator::DispatchContext;
        self.orchestrator
            .as_mut()
            .map(|orch| {
                orch.collect_scores(&DispatchContext::default())
                    .into_iter()
                    .map(|(id, report)| (id, report.into()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Fire a non-blocking `score` query at every provider with none
    /// already in flight. Replies land on stored receivers drained by
    /// [`Self::poll_score_results`]. No-op when no orchestrator exists.
    pub(crate) fn request_scores(&mut self) {
        use foldit_runner::orchestrator::DispatchContext;
        if let Some(orch) = self.orchestrator.as_mut() {
            orch.request_scores(&DispatchContext::default());
        }
    }

    /// Drain whatever async `score` replies have arrived. Non-blocking;
    /// empty map when nothing is ready or no orchestrator exists.
    pub(crate) fn poll_score_results(
        &mut self,
    ) -> std::collections::HashMap<String, crate::scores::ScoreReport> {
        self.orchestrator
            .as_mut()
            .map(|orch| {
                orch.poll_score_results()
                    .into_iter()
                    .map(|(id, report)| (id, report.into()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Fire a composition-score request for `request_id`, carrying the
    /// ASSEM02 bytes of the composition to score (one open edit's lanes
    /// over its peers' committed heads, or a committed checkpoint's union).
    /// Replies land on receivers drained by
    /// [`Self::poll_composition_scores`]. No-op when no orchestrator exists.
    pub(crate) fn score_composition(&mut self, assembly: Vec<u8>, request_id: u64) {
        if let Some(orch) = self.orchestrator.as_mut() {
            orch.score_composition(assembly, request_id);
        }
    }

    /// Drain whatever composition-score replies have arrived, each tagged
    /// with the `request_id` the host correlated it under. Non-blocking;
    /// empty when nothing is ready or no orchestrator exists.
    pub(crate) fn poll_composition_scores(&mut self) -> Vec<(u64, crate::scores::ScoreReport)> {
        self.orchestrator
            .as_mut()
            .map(|orch| {
                orch.poll_composition_scores()
                    .into_iter()
                    .map(|(rid, report)| (rid, report.into()))
                    .collect()
            })
            .unwrap_or_default()
    }

    // ── Bootstrap discover + register ───────────────────────────────────

    /// Discover plugins under `root` and register each against the given
    /// initial assembly, bringing up its worker session. Returns the
    /// `(plugin_id, post_init_bytes)` pair for every plugin that
    /// registered successfully; the post-Init bytes carry each plugin's
    /// normalized assembly for the caller to apply. Empty `Vec` when no
    /// orchestrator is wired up or discovery fails — both degrade the app
    /// to viewer-only rather than erroring.
    pub(crate) fn bootstrap_runner(
        &mut self,
        root: &std::path::Path,
        initial_assembly: Vec<u8>,
    ) -> Vec<(String, Vec<u8>)> {
        let Some(orch) = self.orchestrator.as_mut() else {
            return Vec::new();
        };
        let discovered = match orch.discover_plugins(root) {
            Ok(ids) => ids,
            Err(e) => {
                log::warn!(
                    "[RunnerClient] discover_plugins({}) failed: {e}; plugins disabled",
                    root.display()
                );
                return Vec::new();
            }
        };
        log::info!("[RunnerClient] discovered plugins: {discovered:?}");
        let mut registered = Vec::with_capacity(discovered.len());
        for plugin_id in &discovered {
            match orch.ensure_plugin_registered(plugin_id, initial_assembly.clone())
            {
                Ok(bytes) => {
                    log::info!("[RunnerClient] {plugin_id} plugin ready");
                    registered.push((plugin_id.clone(), bytes));
                }
                Err(e) => {
                    log::warn!(
                        "[RunnerClient] ensure_plugin_registered('{plugin_id}') \
                         failed: {e}; {plugin_id} plugin disabled"
                    );
                }
            }
        }
        registered
    }
}

/// Convert the runner's proto score report into the core-owned twin at the
/// facade boundary. Structural copy: `total` and `terms` move as-is; the
/// per-residue list is rebuilt, dropping any entry with no residue ref
/// (the proto field is optional). That skip preserves the historical
/// `residue.as_ref() else continue` behavior, relocated from the consumer.
impl From<foldit_runner::proto::plugin::ScoreReport> for crate::scores::ScoreReport {
    fn from(report: foldit_runner::proto::plugin::ScoreReport) -> Self {
        Self {
            total: report.total,
            terms: report.terms,
            per_residue: report
                .per_residue
                .into_iter()
                .filter_map(|rs| {
                    rs.residue.map(|rref| crate::scores::ResidueScore {
                        entity_id: rref.entity_id,
                        residue_index: rref.residue_index,
                        score: rs.score,
                    })
                })
                .collect(),
        }
    }
}

/// Core-shaped dispatch request handed to [`RunnerClient::dispatch_op`].
/// Carries only molex / gui-wire types so `App` never builds the
/// orchestrator's `DispatchContext` / `ResidueRef` / `ParamValue` shapes;
/// the flatten and conversion happen inside `dispatch_op`.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) struct DispatchIntent {
    /// The authoritative in-core selection (molex `EntityId`, same type as
    /// `App.selection`), flattened to per-residue refs at dispatch time.
    pub selection: std::collections::BTreeMap<
        molex::entity::molecule::id::EntityId,
        std::collections::BTreeSet<u32>,
    >,
    /// The GUI-provided focus, a raw gui-wire entity id (not a runner
    /// `EntityId`); wrapped into the runner id inside `dispatch_op`.
    pub focused_entity_id: Option<u64>,
    /// The op to dispatch; resolved against the registry for Invoke vs Stream.
    pub op_id: String,
    /// Op params in gui-wire form, converted to the orchestrator's native
    /// `ParamValue` inside `dispatch_op`.
    pub params: std::collections::HashMap<String, foldit_gui::state::ParamValue>,
}

/// Core-shaped pull-drag start request handed to
/// [`RunnerClient::start_stream`]. Carries only molex / core-native
/// types; the `DispatchContext` / `ResidueRef` / `ParamValue` build all
/// happen inside `start_stream`, so `App`'s pull-drag path names no
/// orchestrator type. Pull-drag is always a stream (no Invoke branch).
#[cfg(not(target_arch = "wasm32"))]
pub(crate) struct StreamStartIntent {
    /// The pull op-id (one of `pull_drag::OP_PULL_*`); resolved against
    /// the registry inside `start_stream` for the plugin id + dispatch.
    pub op_id: &'static str,
    /// The picked entity (already a molex id — no runner-id wrapping).
    /// Becomes both the `DispatchContext` focus and the single
    /// `ResidueRef`'s entity.
    pub focused_entity: molex::EntityId,
    /// 0-based residue index within the entity; the single selection ref
    /// and the start-param 1-indexing both derive from it.
    pub residue_in_entity: u32,
    /// PDB atom name the user picked; only the sidechain op consumes it
    /// (backbone is residue-anchored), inside `build_start_params`.
    pub atom_name: String,
}

/// Core-side reason a dispatch was refused or failed, produced by
/// [`RunnerClient::dispatch_op`]. Deliberately carries no orchestrator
/// type: the lock refusal is reshaped into a raw entity id so `App`
/// distinguishes a busy-entity refusal (advisory, no error log) from a
/// genuine failure without naming any runner error.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug)]
pub(crate) enum DispatchError {
    /// A required entity was already locked by another op; carries the raw
    /// id of the locked entity.
    EntityLocked { entity: u64 },
    /// The plugin's backend worker is already running an op; only one op
    /// per backend at a time.
    BackendBusy {
        /// The plugin whose backend is busy.
        plugin_id: String,
    },
    /// Any other dispatch failure, rendered to a string.
    Failed(String),
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

/// Discriminated result of a dispatch — wraps the two return shapes
/// `dispatch_invoke` and `dispatch_start_stream` produce so
/// `App::handle_dispatch_op` can post-process either uniformly. Lives
/// here (rather than in `app.rs`) because [`RunnerClient::dispatch_op`]
/// is the producer and `App` is just one of two consumers.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) enum OpOutcome {
    /// Synchronous invoke completed. `request_id` is the dispatch id the
    /// caller keys its edit on; `bytes` is the plugin's reply, fed into
    /// `apply_invoke_result`; `scope` is the entity set the op locked, so
    /// the caller opens its edit over every targeted entity.
    Invoke {
        request_id: u64,
        bytes: Vec<u8>,
        scope: EditScope,
    },
    /// Stream dispatch succeeded; the `DispatchHandle` is already stored
    /// in `StreamHost::active_streams` under `request_id` — the same id
    /// the caller opens its edit under, so there is nothing left to
    /// reconcile here. The matching terminal arm in
    /// `apply_backend_updates` performs the cleanup. `scope` is the entity
    /// set the op locked, so the caller opens its edit over every target.
    Stream { request_id: u64, scope: EditScope },
}

/// The entity set a dispatched op resolved to, threaded from the runner's
/// resolved lock target back to `App` so the edit opens over every entity
/// the op operates on (not the host's single-entity fallback guess). A
/// neutral core-owned scope: it names only `molex::EntityId`, so `App`
/// never sees the runner's `LockTargets` / `DispatchHandle`.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) enum EditScope {
    /// A whole-pose / global op: the edit opens over the whole document.
    AllEntities,
    /// The op resolved to this specific entity set (focus / selection,
    /// type-filtered by the runner's lock resolution).
    Entities(Vec<molex::EntityId>),
}

/// Map a runner `DispatchHandle`'s resolved target onto the neutral
/// [`EditScope`]: a `global_held` handle is whole-pose, otherwise the
/// handle's locked entity set.
#[cfg(not(target_arch = "wasm32"))]
fn edit_scope_from_handle(
    handle: &foldit_runner::orchestrator::DispatchHandle,
) -> EditScope {
    if handle.global_held {
        EditScope::AllEntities
    } else {
        EditScope::Entities(handle.entities.clone())
    }
}

/// Map the runner's resolved [`LockTargets`] (returned by `dispatch_invoke`)
/// onto the neutral [`EditScope`].
#[cfg(not(target_arch = "wasm32"))]
fn edit_scope_from_targets(
    targets: foldit_runner::orchestrator::LockTargets,
) -> EditScope {
    use foldit_runner::orchestrator::LockTargets;
    match targets {
        LockTargets::Global => EditScope::AllEntities,
        LockTargets::Entities(set) => EditScope::Entities(set),
    }
}

/// Core-side projection of inbound plugin traffic, produced by
/// [`RunnerClient::drain_op_events`]. Each variant enumerates one of
/// core's edit-lifecycle verbs keyed by the dispatch `request_id` (the
/// single id `App` opened the edit under), and owns its `Assembly` so the
/// returned batch outlives the driver borrow that produced it. `App`
/// applies these without naming any orchestrator type.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) enum OpEvent {
    /// Mid-stream tentative frame, keyed by the dispatch `request_id`.
    /// `App` applies it into the edit open under that id, or no-ops when
    /// none is open.
    Update { token: u64, assembly: Assembly },
    /// Terminal success. The runner's distinct `Final` and `Cancelled`
    /// terminals collapse here because core commits either identically.
    /// `token` is the dispatch `request_id`; `App` commits the edit open
    /// under it, or accounts for the terminal with nothing to commit when
    /// `is_pending` reports none is open.
    Commit {
        token: Option<u64>,
        assembly: Assembly,
    },
    /// Terminal failure. `token` is the dispatch `request_id`; `App`
    /// aborts the edit open under it (gated on `is_pending`), or accounts
    /// for the terminal with nothing to abort.
    Abort { token: Option<u64>, reason: String },
}

/// Owns the in-flight stream bookkeeping that only exists on native
/// builds: the plugin stream handle table plus the live pull-drag
/// state. Grouped so App's stream lifecycle touches one field.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) struct StreamHost {
    /// In-flight stream handles keyed by request_id. Populated by
    /// `handle_dispatch_op` on `StartStream`; the matching
    /// `release_dispatch_locks` runs in `drain_op_events` when the
    /// stream's terminal `PluginUpdate` arrives. The stored
    /// `plugin_id` is the dispatch target for `dispatch_cancel_stream`
    /// when the user hits ESC.
    pub(crate) active_streams: std::collections::HashMap<
        u64,
        ActiveStreamEntry,
    >,
    /// Live pull-drag state. `Some(...)` between pointer-down on an
    /// atom and pointer-up / stream-terminal / ESC cancel. The drag's
    /// stream id also lives in `active_streams` so Final/Error
    /// handling flows through the unified stream-cleanup path; this
    /// field carries the extra viso-side bookkeeping needed for
    /// pointer-move (PullInfo + op id).
    pub(crate) pull_drag: Option<crate::pull_drag::PullDrag>,
}

/// Bundle stored per running stream so `drain_op_events` /
/// `cancel_operations` can release locks and dispatch cancel against
/// the right plugin worker without re-querying the orchestrator.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) struct ActiveStreamEntry {
    pub(crate) handle: foldit_runner::orchestrator::DispatchHandle,
    pub(crate) plugin_id: String,
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
