//! Plugin lifecycle: spawn + Init + catalog cache, and teardown.

use std::sync::mpsc;

use crate::error::RunnerError;
use crate::orchestrator::core::Orchestrator;
use crate::orchestrator::pump::{spawn_pump, PluginTask, PluginWorkerHandle};
use crate::orchestrator::spawn::{
    bind_and_spawn_worker, spawn_plugin_worker, AcceptOutcome,
    PluginSpawnDescriptor,
};
use crate::orchestrator::types::{
    CachedPluginOp, CachedPluginQuery, InitPayload, OpKind, OpLockMeta,
    ParamConstraint, ParamSpec, ParamType, ParamValue,
};
use crate::proto::plugin as proto;

impl Orchestrator {
    /// Spawn a plugin worker and start its pump, bringing the plugin to
    /// the WARM state: its worker process is alive and its backend /
    /// database / scoring are loaded (the worker's boot path runs the
    /// plugin's `create` slot), but NO session has been created yet
    /// (no `Init`, no pose). This is the app-lifecycle warm-up, done once
    /// at startup independent of any structure.
    ///
    /// Idempotent on `plugin_workers`: a no-op if the plugin is already
    /// warm. Pair with [`Self::init_plugin_session`] at file-load to
    /// create the session against the just-loaded assembly.
    ///
    /// # Errors
    ///
    /// Returns an error if the worker binary is missing or spawn fails.
    pub fn warm_plugin(
        &mut self,
        descriptor: &PluginSpawnDescriptor,
    ) -> Result<(), RunnerError> {
        let plugin_id = String::from(descriptor.id());
        if self.plugin_workers.contains_key(&plugin_id) {
            // Already warm; spawning a second worker would leak the first.
            return Ok(());
        }
        let worker_binary = self
            .worker_binary
            .as_ref()
            .ok_or_else(|| {
                RunnerError::Generic(
                    "worker binary not found; build foldit-worker first".into(),
                )
            })?
            .clone();

        let (process, client) = spawn_plugin_worker(&worker_binary, descriptor)
            .map_err(|e| RunnerError::Generic(format!("spawn failed: {e}")))?;

        let (task_tx, join) = spawn_pump(client, self.plugin_update_tx.clone());
        let handle =
            PluginWorkerHandle::new(plugin_id.clone(), process, task_tx, join);
        let _ = self.plugin_workers.insert(plugin_id, handle);
        Ok(())
    }

    /// Begin warming a plugin worker WITHOUT blocking on its connect.
    /// Binds the IPC listener and spawns the worker child, then stashes
    /// the un-accepted worker in `pending_warms` for
    /// [`Self::poll_warm_plugins`] to finish. The non-blocking twin of
    /// [`Self::warm_plugin`].
    ///
    /// Idempotent: a no-op (returns `Ok`) if the plugin is already warm
    /// (`plugin_workers`) or already warming (`pending_warms`).
    ///
    /// # Errors
    ///
    /// Returns an error if the worker binary is missing or the bind /
    /// spawn fails.
    pub fn kick_warm_plugin(
        &mut self,
        descriptor: &PluginSpawnDescriptor,
    ) -> Result<(), RunnerError> {
        let plugin_id = String::from(descriptor.id());
        if self.plugin_workers.contains_key(&plugin_id)
            || self.pending_warms.contains_key(&plugin_id)
        {
            // Already warm or warming; a second spawn would leak a worker.
            return Ok(());
        }
        let worker_binary = self
            .worker_binary
            .as_ref()
            .ok_or_else(|| {
                RunnerError::Generic(
                    "worker binary not found; build foldit-worker first".into(),
                )
            })?
            .clone();

        let pending = bind_and_spawn_worker(&worker_binary, descriptor)
            .map_err(|e| RunnerError::Generic(format!("spawn failed: {e}")))?;
        // The accept is deferred to per-frame polling, so the listener
        // must not block on accept.
        pending
            .set_accept_nonblocking()
            .map_err(|e| RunnerError::Generic(format!("spawn failed: {e}")))?;
        let _ = self.pending_warms.insert(plugin_id, pending);
        Ok(())
    }

    /// Finish warming any plugin whose worker has connected since the
    /// last call. Non-blocking: a worker that has not connected yet stays
    /// pending. For each plugin that connected this call, starts its pump,
    /// inserts the [`PluginWorkerHandle`] into `plugin_workers` (the same
    /// promotion [`Self::warm_plugin`] does), and yields `(plugin_id,
    /// Ok(()))`; an accept failure clears the slot and yields `(plugin_id,
    /// Err(..))`. The pending slot clears on either outcome.
    #[must_use]
    pub fn poll_warm_plugins(
        &mut self,
    ) -> Vec<(String, Result<(), RunnerError>)> {
        let mut out: Vec<(String, Result<(), RunnerError>)> = Vec::new();
        // Drain the pending entries that have a connection ready, building
        // their handles, then re-insert the still-pending ones. Owning the
        // `PendingWorker` by value (try_accept consumes it) keeps the
        // listener alive across frames for the not-yet-connected case.
        let pending_ids: Vec<String> =
            self.pending_warms.keys().cloned().collect();
        for plugin_id in pending_ids {
            let Some(pending) = self.pending_warms.remove(&plugin_id) else {
                continue;
            };
            match pending.try_accept() {
                AcceptOutcome::Connected(process, client) => {
                    let (task_tx, join) =
                        spawn_pump(client, self.plugin_update_tx.clone());
                    let handle = PluginWorkerHandle::new(
                        plugin_id.clone(),
                        Some(process),
                        task_tx,
                        join,
                    );
                    let _ =
                        self.plugin_workers.insert(plugin_id.clone(), handle);
                    out.push((plugin_id, Ok(())));
                }
                AcceptOutcome::Pending(pending) => {
                    // Worker has not connected yet; keep warming.
                    let _ = self.pending_warms.insert(plugin_id, pending);
                }
                AcceptOutcome::Failed(e) => {
                    // Genuine accept failure; the pending worker was
                    // dropped (its listener + child go with it). Report it.
                    out.push((
                        plugin_id,
                        Err(RunnerError::Generic(format!(
                            "accept failed: {e}"
                        ))),
                    ));
                }
            }
        }
        out
    }

    /// Create the plugin's session: submit `Init` against the
    /// ALREADY-WARM worker (the structure is built here, against
    /// `init_assembly`), cache the resulting op + query catalog, and
    /// store the session id. Returns the plugin's post-Init normalized
    /// assembly bytes.
    ///
    /// Idempotent on `plugin_sessions` (NOT on `plugin_workers`): a
    /// plugin whose session already exists returns empty bytes without
    /// re-`Init`-ing. The worker must already be warm (via
    /// [`Self::warm_plugin`]); this errors clearly if it isn't.
    ///
    /// # Errors
    ///
    /// Returns an error if the worker isn't warm yet, or if the plugin's
    /// `Init` reply errors out.
    pub fn init_plugin_session(
        &mut self,
        plugin_id: &str,
        init_assembly: Vec<u8>,
    ) -> Result<Vec<u8>, RunnerError> {
        if self.plugin_sessions.contains_key(plugin_id) {
            return Ok(Vec::new());
        }
        let handle = self.plugin_workers.get(plugin_id).ok_or_else(|| {
            RunnerError::Generic(format!(
                "init_plugin_session: plugin {plugin_id} is not warm; call \
                 warm_plugin() first"
            ))
        })?;

        // Synchronous Init. The sync path (round-trip tests, register_plugin)
        // carries no puzzle payload; the host's session-init runs through the
        // non-blocking kick_init_session below.
        let (reply_tx, reply_rx) = mpsc::channel();
        handle
            .submit(PluginTask::Init {
                assembly: init_assembly,
                payload: InitPayload::default(),
                reply: reply_tx,
            })
            .map_err(|_| {
                RunnerError::Generic(format!(
                    "worker thread for {plugin_id} closed"
                ))
            })?;
        let (session, registration, initial_assembly) =
            reply_rx.recv().map_err(|_| {
                RunnerError::Generic(format!(
                    "Init reply for {plugin_id} dropped"
                ))
            })??;

        self.cache_registration(plugin_id, &registration);
        let _ = self
            .plugin_sessions
            .insert(String::from(plugin_id), session);
        Ok(initial_assembly)
    }

    /// Submit the plugin's `Init` WITHOUT blocking on the reply. Same
    /// guards and submit as [`Self::init_plugin_session`], but instead of
    /// waiting for the reply it stashes the reply receiver in
    /// `pending_inits`, keyed by plugin id, for [`Self::poll_init_sessions`]
    /// to drain. The worker must already be warm.
    ///
    /// Idempotent: a no-op (returns `Ok`) if the plugin already has a
    /// session, or if an Init for it is already in flight (the in-flight
    /// Init coalesces; a second kick does not re-submit).
    ///
    /// # Errors
    ///
    /// Returns an error if the worker isn't warm yet, or if the submit to
    /// the worker thread fails (worker channel closed).
    #[allow(clippy::needless_pass_by_value)]
    pub fn kick_init_session(
        &mut self,
        plugin_id: &str,
        init_assembly: Vec<u8>,
        payload: InitPayload,
    ) -> Result<(), RunnerError> {
        if self.plugin_sessions.contains_key(plugin_id) {
            return Ok(());
        }
        if self.pending_inits.contains_key(plugin_id) {
            return Ok(()); // Init already in flight for this plugin
        }
        let handle = self.plugin_workers.get(plugin_id).ok_or_else(|| {
            RunnerError::Generic(format!(
                "kick_init_session: plugin {plugin_id} is not warm; call \
                 warm_plugin() / kick_warm_plugin() first"
            ))
        })?;

        let (reply_tx, reply_rx) = mpsc::channel();
        handle
            .submit(PluginTask::Init {
                assembly: init_assembly,
                payload,
                reply: reply_tx,
            })
            .map_err(|_| {
                RunnerError::Generic(format!(
                    "worker thread for {plugin_id} closed"
                ))
            })?;
        let _ = self.pending_inits.insert(String::from(plugin_id), reply_rx);
        Ok(())
    }

    /// Drain whatever `Init` replies have arrived since the last call.
    /// Non-blocking `try_recv`; a plugin whose Init has not replied stays
    /// pending. For each plugin that replied this call, on success caches
    /// the registration and stores the session (the same post-processing
    /// [`Self::init_plugin_session`] does) and yields `(plugin_id,
    /// Ok(initial_assembly))`; on a reply error or a dropped worker yields
    /// `(plugin_id, Err(..))`. The pending slot clears either way.
    #[must_use]
    pub fn poll_init_sessions(
        &mut self,
    ) -> Vec<(String, Result<Vec<u8>, RunnerError>)> {
        let mut out: Vec<(String, Result<Vec<u8>, RunnerError>)> = Vec::new();
        let mut done: Vec<String> = Vec::new();
        // Collect the decoded outcomes first; the success path mutates
        // `self` (cache_registration + plugin_sessions) so it can't run
        // while the pending-map iterator borrows `self`.
        let mut ready: Vec<(
            String,
            Result<crate::orchestrator::core::InitReplyPayload, RunnerError>,
        )> = Vec::new();
        for (plugin_id, rx) in &self.pending_inits {
            match rx.try_recv() {
                Ok(reply) => {
                    ready.push((plugin_id.clone(), reply));
                    done.push(plugin_id.clone());
                }
                Err(mpsc::TryRecvError::Empty) => {} // Init still in flight
                Err(mpsc::TryRecvError::Disconnected) => {
                    ready.push((
                        plugin_id.clone(),
                        Err(RunnerError::Generic(format!(
                            "Init reply for {plugin_id} dropped"
                        ))),
                    ));
                    done.push(plugin_id.clone());
                }
            }
        }
        for id in done {
            let _ = self.pending_inits.remove(&id);
        }
        for (plugin_id, reply) in ready {
            match reply {
                Ok((session, registration, initial_assembly)) => {
                    self.cache_registration(&plugin_id, &registration);
                    let _ =
                        self.plugin_sessions.insert(plugin_id.clone(), session);
                    out.push((plugin_id, Ok(initial_assembly)));
                }
                Err(e) => out.push((plugin_id, Err(e))),
            }
        }
        out
    }

    /// Spawn a plugin worker, run Init against it, and cache the
    /// resulting op + query catalog. Convenience wrapper that fuses
    /// [`Self::warm_plugin`] + [`Self::init_plugin_session`] for callers
    /// that don't split the two phases (the round-trip tests). Idempotent
    /// failure on duplicate plugin id.
    ///
    /// # Errors
    ///
    /// Returns an error if the plugin id is already registered, the
    /// worker binary is missing, spawn fails, or the plugin's `Init`
    /// reply errors out.
    pub fn register_plugin(
        &mut self,
        descriptor: &PluginSpawnDescriptor,
        init_assembly: Vec<u8>,
    ) -> Result<Vec<u8>, RunnerError> {
        let plugin_id = String::from(descriptor.id());
        if self.plugin_sessions.contains_key(&plugin_id) {
            return Err(RunnerError::Generic(format!(
                "plugin {plugin_id} already registered"
            )));
        }
        self.warm_plugin(descriptor)?;
        self.init_plugin_session(&plugin_id, init_assembly)
    }

    /// Lazy-register: if `plugin_id` doesn't have a session yet, look up
    /// its discovered descriptor, warm its worker (a no-op if already
    /// warm), and create its session. No-op if already inited.
    ///
    /// The idempotency check is `plugin_sessions`, not `plugin_workers`:
    /// the warm/session split means a worker can be warm (spawned at
    /// startup) without yet holding a session, and this lazy path must
    /// still run `Init` against such a worker.
    ///
    /// # Errors
    ///
    /// Returns an error if the plugin id wasn't seen by discovery, or
    /// if warming / session-init errors out.
    /// Returns the plugin's post-Init normalized assembly bytes (see
    /// [`Self::init_plugin_session`]). Empty `Vec<u8>` either when the
    /// plugin already had a session (no fresh Init was performed) or
    /// when the plugin's Init did not mutate the input assembly.
    pub fn ensure_plugin_registered(
        &mut self,
        plugin_id: &str,
        init_assembly: Vec<u8>,
    ) -> Result<Vec<u8>, RunnerError> {
        if self.plugin_sessions.contains_key(plugin_id) {
            return Ok(Vec::new());
        }
        let descriptor = self
            .plugin_descriptors
            .get(plugin_id)
            .cloned()
            .ok_or_else(|| {
                RunnerError::Generic(format!(
                    "ensure_plugin_registered: no discovered descriptor for \
                     plugin id `{plugin_id}`. Did you call discover_plugins()?"
                ))
            })?;
        self.warm_plugin(&descriptor)?;
        self.init_plugin_session(plugin_id, init_assembly)
    }

    /// Tear down a plugin: drop session, terminate worker, evict from
    /// registry. Idempotent.
    pub fn unregister_plugin(&mut self, plugin_id: &str) {
        if let (Some(handle), Some(session)) = (
            self.plugin_workers.get(plugin_id),
            self.plugin_sessions.get(plugin_id).copied(),
        ) {
            // Best-effort Drop request before terminating.
            let (reply_tx, _reply_rx) = mpsc::channel();
            let _ = handle.submit(PluginTask::Drop {
                session,
                reply: reply_tx,
            });
        }
        let _ = self.plugin_workers.remove(plugin_id);
        let _ = self.plugin_sessions.remove(plugin_id);
        self.plugin_registry.drop_plugin(plugin_id);
    }

    /// Drop every plugin's session WITHOUT terminating its (warm) worker.
    /// Sends a best-effort `Drop` to each worker and clears `plugin_sessions`
    /// so the next [`Self::init_plugin_session`] re-`Init`s against the new
    /// structure. Workers stay warm and the op registry is left intact (a
    /// re-`Init` overwrites each entry, so the action catalog does not blink
    /// empty across a reload). Used by the in-session structure/puzzle reload
    /// paths, where the topology changes but re-warming every worker would be
    /// wasteful. Idempotent: a no-op when no plugin holds a session.
    pub fn drop_all_sessions(&mut self) {
        let sessions: Vec<(String, u64)> = self
            .plugin_sessions
            .iter()
            .map(|(id, &session)| (id.clone(), session))
            .collect();
        for (plugin_id, session) in sessions {
            if let Some(handle) = self.plugin_workers.get(&plugin_id) {
                // Best-effort Drop; the worker stays alive for the re-Init.
                let (reply_tx, _reply_rx) = mpsc::channel();
                let _ = handle.submit(PluginTask::Drop {
                    session,
                    reply: reply_tx,
                });
            }
        }
        self.plugin_sessions.clear();
    }

    fn cache_registration(
        &mut self,
        plugin_id: &str,
        reg: &proto::PluginRegistration,
    ) {
        for op in &reg.operations {
            let lock_meta = OpLockMeta {
                compatible_focus_types: op
                    .compatible_focus_types
                    .iter()
                    .filter_map(|t| entity_type_from_proto(*t))
                    .collect(),
                creates_entities: op.creates_entities,
                requires_focus: op.requires_focus,
            };
            let kind = match proto::OpKind::try_from(op.kind)
                .unwrap_or(proto::OpKind::Unspecified)
            {
                proto::OpKind::Stream => OpKind::Stream,
                _ => OpKind::Invoke,
            };
            self.plugin_registry.register_op(CachedPluginOp {
                plugin_id: String::from(plugin_id),
                op_id: op.id.clone(),
                display_name: op.display_name.clone(),
                kind,
                lock_meta,
                params: op
                    .params
                    .iter()
                    .filter_map(param_spec_from_proto)
                    .collect(),
            });
        }
        for q in &reg.queries {
            self.plugin_registry.register_query(CachedPluginQuery {
                plugin_id: String::from(plugin_id),
                query_id: q.id.clone(),
                display_name: q.display_name.clone(),
                params: q
                    .params
                    .iter()
                    .filter_map(param_spec_from_proto)
                    .collect(),
            });
        }
    }
}

fn entity_type_from_proto(t: i32) -> Option<molex::EntityKind> {
    match proto::EntityType::try_from(t).ok()? {
        proto::EntityType::Protein => Some(molex::EntityKind::Protein),
        proto::EntityType::NucleicAcid => Some(molex::EntityKind::NucleicAcid),
        proto::EntityType::SmallMolecule => {
            Some(molex::EntityKind::SmallMolecule)
        }
        proto::EntityType::Bulk => Some(molex::EntityKind::Bulk),
        proto::EntityType::Unspecified => None,
    }
}

fn param_type_from_proto(t: i32) -> Option<ParamType> {
    match proto::ParamType::try_from(t).ok()? {
        proto::ParamType::Int => Some(ParamType::Int),
        proto::ParamType::Float => Some(ParamType::Float),
        proto::ParamType::Bool => Some(ParamType::Bool),
        proto::ParamType::String => Some(ParamType::String),
        proto::ParamType::Enum => Some(ParamType::Enum),
        proto::ParamType::Vec3 => Some(ParamType::Vec3),
        proto::ParamType::Unspecified => None,
    }
}

fn param_value_from_proto(v: &proto::ParamValue) -> Option<ParamValue> {
    use proto::param_value::Value;
    let value = v.value.as_ref()?;
    Some(match value {
        Value::IntValue(i) => ParamValue::Int(*i),
        Value::FloatValue(f) => ParamValue::Float(*f),
        Value::BoolValue(b) => ParamValue::Bool(*b),
        Value::StringValue(s) => ParamValue::String(s.clone()),
        Value::Vec3Value(v3) => ParamValue::Vec3([v3.x, v3.y, v3.z]),
    })
}

fn param_constraint_from_proto(
    c: &proto::ParamConstraints,
) -> Option<ParamConstraint> {
    use proto::param_constraints::Constraint;
    let constraint = c.constraint.as_ref()?;
    Some(match constraint {
        Constraint::IntRange(r) => ParamConstraint::IntRange {
            min: r.min,
            max: r.max,
        },
        Constraint::FloatRange(r) => ParamConstraint::FloatRange {
            min: r.min,
            max: r.max,
        },
        Constraint::EnumValues(e) => {
            ParamConstraint::EnumValues(e.values.clone())
        }
        Constraint::StringPattern(p) => {
            ParamConstraint::StringPattern(p.pattern.clone())
        }
    })
}

/// Convert a proto `ParamSpec` into the native form. Returns `None` if
/// the param type tag is `Unspecified` (malformed registration); the
/// caller filters these out so the registry never holds a spec with no
/// type.
fn param_spec_from_proto(spec: &proto::ParamSpec) -> Option<ParamSpec> {
    Some(ParamSpec {
        name: spec.name.clone(),
        display_name: spec.display_name.clone(),
        description: spec.description.clone(),
        param_type: param_type_from_proto(spec.r#type)?,
        default: spec.default.as_ref().and_then(param_value_from_proto),
        constraints: spec
            .constraints
            .as_ref()
            .and_then(param_constraint_from_proto),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestrator::types::{DispatchContext, LockTargets};

    /// Build a fake registration carrying two params with distinct
    /// type + constraint shapes (int + IntRange with default, string +
    /// StringPattern without default). Used by
    /// [`cache_registration_preserves_param_spec_array`].
    fn fake_two_param_registration() -> proto::PluginRegistration {
        use proto::param_constraints::Constraint as PC;
        use proto::param_value::Value as PV;

        let iters = proto::ParamSpec {
            name: String::from("iters"),
            display_name: String::from("Iterations"),
            description: String::new(),
            r#type: proto::ParamType::Int as i32,
            default: Some(proto::ParamValue {
                value: Some(PV::IntValue(10)),
            }),
            constraints: Some(proto::ParamConstraints {
                constraint: Some(PC::IntRange(proto::IntRange {
                    min: 1,
                    max: 100,
                })),
            }),
        };
        let sequence = proto::ParamSpec {
            name: String::from("sequence"),
            display_name: String::from("Sequence"),
            description: String::new(),
            r#type: proto::ParamType::String as i32,
            default: None,
            constraints: Some(proto::ParamConstraints {
                constraint: Some(PC::StringPattern(proto::StringPattern {
                    pattern: String::from("^[A-Z]+$"),
                })),
            }),
        };
        proto::PluginRegistration {
            id: String::from("test_plugin"),
            version: String::from("0.0.1"),
            operations: vec![proto::PluginOp {
                id: String::from("test_op"),
                display_name: String::from("Test Op"),
                description: String::new(),
                kind: proto::OpKind::Invoke as i32,
                params: vec![iters, sequence],
                compatible_focus_types: vec![],
                creates_entities: false,
                requires_focus: false,
                ui: None,
            }],
            queries: vec![],
        }
    }

    /// Build a registration carrying a single op with the given lock-shape
    /// fields, exercising the proto fields `cache_registration` reads into
    /// `OpLockMeta`. `op_id` lets a test cache two ops without collision.
    fn fake_lock_meta_registration(
        op_id: &str,
        requires_focus: bool,
        compatible_focus_types: Vec<proto::EntityType>,
        creates_entities: bool,
    ) -> proto::PluginRegistration {
        proto::PluginRegistration {
            id: String::from("lock_plugin"),
            version: String::from("0.0.1"),
            operations: vec![proto::PluginOp {
                id: String::from(op_id),
                display_name: String::from("Lock Op"),
                description: String::new(),
                kind: proto::OpKind::Invoke as i32,
                params: vec![],
                compatible_focus_types: compatible_focus_types
                    .into_iter()
                    .map(|t| t as i32)
                    .collect(),
                creates_entities,
                requires_focus,
                ui: None,
            }],
            queries: vec![],
        }
    }

    /// Drive the real proto -> `cache_registration` -> `OpLockMeta` path and
    /// confirm `requires_focus` survives the read (tests 1-2), then feed the
    /// cached meta into `LockTargets::resolve` to confirm the gate keys off it
    /// (tests 3-4). The prior tests built `OpLockMeta` directly, skipping the
    /// proto read where the field could be dropped/defaulted.
    #[test]
    fn requires_focus_flows_from_proto_through_cache_into_resolve() {
        let mut orch = Orchestrator::new();

        // rfd3 shape: focus-required, multi-type, entity-creating.
        orch.cache_registration(
            "lock_plugin",
            &fake_lock_meta_registration(
                "rfd3",
                true,
                vec![
                    proto::EntityType::Protein,
                    proto::EntityType::SmallMolecule,
                ],
                true,
            ),
        );
        // Shake shape: type-restricted but focus-optional.
        orch.cache_registration(
            "lock_plugin",
            &fake_lock_meta_registration(
                "shake",
                false,
                vec![proto::EntityType::Protein],
                false,
            ),
        );

        let rfd3_meta = &orch
            .plugin_registry
            .get_op("rfd3")
            .expect("rfd3 should be registered")
            .lock_meta;
        let shake_meta = &orch
            .plugin_registry
            .get_op("shake")
            .expect("shake should be registered")
            .lock_meta;

        // 1. requires_focus: true survives the proto read.
        assert!(
            rfd3_meta.requires_focus,
            "requires_focus=true must flow from proto into OpLockMeta"
        );
        // 2. requires_focus: false survives the proto read.
        assert!(
            !shake_meta.requires_focus,
            "requires_focus=false must flow from proto into OpLockMeta"
        );

        // Unfocused, unselected dispatch: the gate splits on requires_focus.
        let ctx = DispatchContext::default();
        let no_types =
            |_: molex::EntityId| -> Option<molex::EntityKind> { None };

        // 3. focus-required + nothing in scope -> gated to an empty entity set.
        assert_eq!(
            LockTargets::resolve(rfd3_meta, &ctx, no_types),
            LockTargets::Entities(Vec::new()),
            "focus-required op with no focus/selection must gate"
        );
        // 4. focus-optional -> falls back to a global run.
        assert_eq!(
            LockTargets::resolve(shake_meta, &ctx, no_types),
            LockTargets::Global,
            "focus-optional op must stay global when unfocused"
        );
    }

    #[test]
    fn cache_registration_preserves_param_spec_array() {
        let reg = fake_two_param_registration();

        let mut orch = Orchestrator::new();
        orch.cache_registration("test_plugin", &reg);

        let op = orch
            .plugin_registry
            .get_op("test_op")
            .expect("test_op should be registered");
        assert_eq!(op.params.len(), 2);

        // First param: int with default + IntRange constraint.
        let iters = &op.params[0];
        assert_eq!(iters.name, "iters");
        assert_eq!(iters.display_name, "Iterations");
        assert_eq!(iters.param_type, ParamType::Int);
        assert_eq!(iters.default, Some(ParamValue::Int(10)));
        assert_eq!(
            iters.constraints,
            Some(ParamConstraint::IntRange { min: 1, max: 100 })
        );

        // Second param: string with no default + StringPattern constraint.
        let seq = &op.params[1];
        assert_eq!(seq.name, "sequence");
        assert_eq!(seq.param_type, ParamType::String);
        assert!(seq.default.is_none());
        assert_eq!(
            seq.constraints,
            Some(ParamConstraint::StringPattern(String::from("^[A-Z]+$")))
        );
    }
}
