//! Op / query / stream dispatch, plus the synchronous worker call
//! helper and STALE_GEN recovery retry.

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{mpsc, Arc};

use super::OpDispatchError;
use crate::error::RunnerError;
use crate::orchestrator::client::BroadcastPayload;
use crate::orchestrator::core::Orchestrator;
use crate::orchestrator::lock_check::{DispatchError, DispatchHandle};
use crate::orchestrator::pump::PluginTask;
use crate::orchestrator::types::{
    CachedPluginOp, DispatchContext, LockTargets, ParamValue,
};

/// One completed async-invoke result yielded by
/// [`Orchestrator::poll_invokes`]: the owning plugin id paired with either
/// the synchronous-invoke success tuple `(request_id, bytes,
/// LockTargets)` or the dispatch error.
type InvokePollResult =
    (String, Result<(u64, Vec<u8>, LockTargets), OpDispatchError>);

/// True if the plugin returned the protocol-level `STALE_GEN` error.
/// The orchestrator catches this on the first dispatch, re-sends the
/// cached full Assembly, and retries the original call once.
pub(in crate::orchestrator) fn is_stale_gen(err: &RunnerError) -> bool {
    matches!(err, RunnerError::PluginError { code, .. } if code == "STALE_GEN")
}

impl Orchestrator {
    /// Dispatch a single-shot op. Synchronous: blocks the caller until
    /// the plugin returns. On success, fans the resulting Assembly bytes
    /// out to other plugins via `UpdateAssembly` (best-effort) and
    /// returns the allocated dispatch `request_id` alongside the bytes
    /// and the resolved [`LockTargets`] (the entity set, or `Global`, the
    /// op actually locked); the caller keys its edit on that id and opens
    /// the edit over that target set.
    ///
    /// `entity_type_of` lets the lock-resolution code map entity ids to
    /// types; pass a closure backed by the caller's EntityStore. For
    /// global ops (empty `compatible_focus_types`) the closure is
    /// never called.
    ///
    /// # Errors
    ///
    /// Returns an [`OpDispatchError`] if the op is unknown, no session
    /// exists, the lock check refuses dispatch, the worker channel is
    /// dropped, or the plugin returns an op-level error.
    // Single linear dispatch sequence (resolve op, lock gate, send to worker,
    // map the reply); splitting it would scatter the lock/send ordering that
    // must stay in one place.
    #[allow(clippy::too_many_lines)]
    pub fn dispatch_invoke<F>(
        &mut self,
        op_id: &str,
        ctx: DispatchContext,
        params: HashMap<String, ParamValue>,
        entity_type_of: F,
    ) -> Result<(u64, Vec<u8>, LockTargets), OpDispatchError>
    where
        F: Fn(molex::EntityId) -> Option<molex::EntityKind>,
    {
        let cached = self
            .plugin_registry
            .get_op(op_id)
            .ok_or_else(|| OpDispatchError::UnknownOp(String::from(op_id)))?
            .clone();
        let session = self
            .plugin_sessions
            .get(&cached.plugin_id)
            .copied()
            .ok_or_else(|| {
                OpDispatchError::NoSession(cached.plugin_id.clone())
            })?;

        if !self.locks.try_lock_backend(&cached.plugin_id) {
            return Err(OpDispatchError::LockRefused(
                DispatchError::BackendBusy {
                    plugin_id: cached.plugin_id.clone(),
                },
            ));
        }
        let mut lock = match self.locks.dispatch_lock_check(
            &cached.lock_meta,
            &ctx,
            &cached.display_name,
            &entity_type_of,
        ) {
            Ok(h) => h,
            Err(e) => {
                self.locks.unlock_backend(&cached.plugin_id);
                return Err(OpDispatchError::LockRefused(e));
            }
        };
        lock.backend_lock = Some(cached.plugin_id.clone());

        // STALE_GEN recovery: if the plugin reports it dropped a
        // broadcast (or is post-spawn cold), resync via the cached
        // full Assembly and retry exactly once.
        let mut result =
            self.call_worker(&cached.plugin_id, |reply| PluginTask::Invoke {
                session,
                op: String::from(op_id),
                ctx: ctx.clone(),
                params: params.clone(),
                reply,
            });
        if let Err(OpDispatchError::Plugin(e)) = &result {
            if is_stale_gen(e) {
                if let Err(resync_err) =
                    self.try_stale_gen_resync(&cached.plugin_id)
                {
                    self.locks.release_dispatch_locks(lock);
                    return Err(resync_err);
                }
                result = self.call_worker(&cached.plugin_id, |reply| {
                    PluginTask::Invoke {
                        session,
                        op: String::from(op_id),
                        ctx,
                        params,
                        reply,
                    }
                });
            }
        }

        // Capture the resolved lock target from the handle before it is
        // released, so the host can open its edit over the same set the op
        // actually locked (whole-pose ops carry `global_held`, so the edit
        // must reach every entity, not the host's single-entity guess).
        let resolved = if lock.global_held {
            LockTargets::Global
        } else {
            LockTargets::Entities(lock.entities.clone())
        };

        // Always release the lock, even on failure.
        self.locks.release_dispatch_locks(lock);

        let bytes = result?;
        // Fan out the post-op Assembly to peer plugins (best-effort).
        // Post-op fan-out always carries the full assembly the
        // originating plugin returned.
        self.fan_out_assembly(
            &cached.plugin_id,
            &BroadcastPayload::Full(bytes.clone()),
        );
        // Allocate the dispatch id the caller keys its edit on. Invoke is
        // host-side only: the plugin never sees this id.
        let request_id = self.alloc_request_id();
        Ok((request_id, bytes, resolved))
    }

    /// Submit a single-shot op WITHOUT blocking on the worker reply. The
    /// non-blocking FRONT half of [`Self::dispatch_invoke`]: resolves the
    /// op + session, acquires the backend + dispatch lock, submits the
    /// `Invoke`, and stashes the reply receiver + held lock handle in
    /// `pending_invokes` for [`Self::poll_invokes`] to finish. Returns
    /// immediately; it does NOT `recv`.
    ///
    /// The acquired dispatch lock is HELD in `pending_invokes` across
    /// frames and released only when `poll_invokes` drains the reply (see
    /// the `PendingInvoke` field docs). This is the one structural
    /// difference from the streaming twin; it is safe for the intended
    /// startup use, where the async invoke is the sole dispatcher.
    ///
    /// Idempotent: a no-op (returns `Ok`) if an invoke for this plugin is
    /// already in flight (the in-flight invoke coalesces; a second kick
    /// does not re-submit), mirroring [`Self::kick_init_session`].
    ///
    /// STALE_GEN is NOT auto-retried for this twin. `dispatch_invoke`
    /// re-sends the cached full Assembly and retries once on a
    /// stale-generation plugin error; re-submitting cleanly across the
    /// kick/poll split would require re-kicking from the poll half, which
    /// the split makes awkward. The async invoke is used at startup, right
    /// after Init, where a stale broadcast is unlikely, so a STALE_GEN
    /// reply is surfaced as an error by `poll_invokes` rather than retried.
    ///
    /// # Errors
    ///
    /// Returns an [`OpDispatchError`] if the op is unknown, no session
    /// exists, the lock check refuses dispatch, or the submit to the
    /// worker thread fails (worker channel closed).
    // Mirrors `dispatch_invoke`'s single linear lock/submit sequence;
    // splitting it would scatter the lock ordering that must stay together.
    #[allow(clippy::too_many_lines)]
    pub fn kick_invoke<F>(
        &mut self,
        op_id: &str,
        ctx: DispatchContext,
        params: HashMap<String, ParamValue>,
        entity_type_of: F,
    ) -> Result<(), OpDispatchError>
    where
        F: Fn(molex::EntityId) -> Option<molex::EntityKind>,
    {
        let cached = self
            .plugin_registry
            .get_op(op_id)
            .ok_or_else(|| OpDispatchError::UnknownOp(String::from(op_id)))?
            .clone();
        if self.pending_invokes.contains_key(&cached.plugin_id) {
            return Ok(()); // invoke already in flight for this plugin
        }
        let session = self
            .plugin_sessions
            .get(&cached.plugin_id)
            .copied()
            .ok_or_else(|| {
                OpDispatchError::NoSession(cached.plugin_id.clone())
            })?;

        if !self.locks.try_lock_backend(&cached.plugin_id) {
            return Err(OpDispatchError::LockRefused(
                DispatchError::BackendBusy {
                    plugin_id: cached.plugin_id.clone(),
                },
            ));
        }
        let mut lock = match self.locks.dispatch_lock_check(
            &cached.lock_meta,
            &ctx,
            &cached.display_name,
            &entity_type_of,
        ) {
            Ok(h) => h,
            Err(e) => {
                self.locks.unlock_backend(&cached.plugin_id);
                return Err(OpDispatchError::LockRefused(e));
            }
        };
        lock.backend_lock = Some(cached.plugin_id.clone());

        // Submit without waiting; the reply lands in `pending_invokes` and
        // `poll_invokes` finishes the round-trip. On a submit failure the
        // lock is released here so a dropped worker doesn't strand it.
        let (reply_tx, reply_rx) = mpsc::channel();
        let task = PluginTask::Invoke {
            session,
            op: String::from(op_id),
            ctx,
            params,
            reply: reply_tx,
        };
        let Some(handle) = self.plugin_workers.get(&cached.plugin_id) else {
            self.locks.release_dispatch_locks(lock);
            return Err(OpDispatchError::WorkerGone(cached.plugin_id.clone()));
        };
        if handle.submit(task).is_err() {
            self.locks.release_dispatch_locks(lock);
            return Err(OpDispatchError::WorkerGone(cached.plugin_id.clone()));
        }

        let _ = self.pending_invokes.insert(
            cached.plugin_id.clone(),
            crate::orchestrator::core::PendingInvoke {
                reply: reply_rx,
                lock,
                plugin_id: cached.plugin_id,
            },
        );
        Ok(())
    }

    /// Drain whatever async-invoke replies have arrived since the last
    /// call. The non-blocking BACK half of [`Self::dispatch_invoke`]:
    /// `try_recv`s each pending entry, and for each that replied runs the
    /// post-`call_worker` tail of `dispatch_invoke` (captures the resolved
    /// [`LockTargets`] from the held handle, releases the dispatch lock,
    /// fans the post-op assembly out to peers, allocates the dispatch id),
    /// yielding `(plugin_id, Ok((request_id, bytes, LockTargets)))`. A
    /// plugin whose invoke has not replied stays pending. A plugin-level
    /// error (including STALE_GEN, which this twin does not retry) or a
    /// dropped worker releases the lock and yields `(plugin_id, Err(..))`.
    /// The pending slot clears on any outcome.
    #[must_use]
    pub fn poll_invokes(&mut self) -> Vec<InvokePollResult> {
        // Collect the ready entries first; the success tail mutates `self`
        // (release_dispatch_locks + fan_out_assembly + alloc_request_id)
        // so it can't run while the pending-map iterator borrows `self`.
        let mut ready: Vec<(String, Result<Vec<u8>, OpDispatchError>)> =
            Vec::new();
        for (plugin_id, pending) in &self.pending_invokes {
            match pending.reply.try_recv() {
                Ok(reply) => ready.push((
                    plugin_id.clone(),
                    reply.map_err(OpDispatchError::Plugin),
                )),
                Err(mpsc::TryRecvError::Empty) => {} // still in flight
                Err(mpsc::TryRecvError::Disconnected) => ready.push((
                    plugin_id.clone(),
                    Err(OpDispatchError::WorkerGone(plugin_id.clone())),
                )),
            }
        }

        let mut out = Vec::with_capacity(ready.len());
        for (plugin_id, result) in ready {
            let Some(pending) = self.pending_invokes.remove(&plugin_id) else {
                continue;
            };
            // Capture the resolved lock target from the held handle before
            // it is released (whole-pose ops carry `global_held`, so the
            // edit must reach every entity, not a single-entity guess).
            let resolved = if pending.lock.global_held {
                LockTargets::Global
            } else {
                LockTargets::Entities(pending.lock.entities.clone())
            };
            // Always release the lock, even on failure.
            self.locks.release_dispatch_locks(pending.lock);

            match result {
                Ok(bytes) => {
                    // Fan the post-op Assembly out to peer plugins
                    // (best-effort), as the synchronous invoke does.
                    self.fan_out_assembly(
                        &pending.plugin_id,
                        &BroadcastPayload::Full(bytes.clone()),
                    );
                    let request_id = self.alloc_request_id();
                    out.push((plugin_id, Ok((request_id, bytes, resolved))));
                }
                Err(e) => out.push((plugin_id, Err(e))),
            }
        }
        out
    }

    /// Dispatch a read query. Synchronous; queries don't take entity
    /// locks (per protocol §"Concurrency and locking"). Takes
    /// `&mut self` only so it can run STALE_GEN recovery on the
    /// originating plugin.
    ///
    /// # Errors
    ///
    /// Returns an [`OpDispatchError`] if the query is unknown, no
    /// session exists, the worker channel is dropped, or the plugin
    /// returns a query-level error.
    pub fn dispatch_query(
        &mut self,
        query_id: &str,
        ctx: DispatchContext,
        params: HashMap<String, ParamValue>,
    ) -> Result<Vec<u8>, OpDispatchError> {
        let cached = self
            .plugin_registry
            .get_query(query_id)
            .ok_or_else(|| {
                OpDispatchError::UnknownQuery(String::from(query_id))
            })?
            .clone();
        let session = self
            .plugin_sessions
            .get(&cached.plugin_id)
            .copied()
            .ok_or_else(|| {
                OpDispatchError::NoSession(cached.plugin_id.clone())
            })?;

        let mut result =
            self.call_worker(&cached.plugin_id, |reply| PluginTask::Query {
                session,
                query: String::from(query_id),
                ctx: ctx.clone(),
                params: params.clone(),
                assembly: Vec::new(),
                reply,
            });
        if let Err(OpDispatchError::Plugin(e)) = &result {
            if is_stale_gen(e) {
                self.try_stale_gen_resync(&cached.plugin_id)?;
                result = self.call_worker(&cached.plugin_id, |reply| {
                    PluginTask::Query {
                        session,
                        query: String::from(query_id),
                        ctx,
                        params,
                        assembly: Vec::new(),
                        reply,
                    }
                });
            }
        }
        result
    }

    /// Begin a streaming op. Acquires the lock and returns the
    /// host-assigned `request_id`; subsequent
    /// `PluginUpdate::{Pending,Cancelled,Final,Error}` events arrive on
    /// the orchestrator's plugin update channel, drained by
    /// `pump_updates`.
    ///
    /// **Lock release**: the caller is responsible for releasing the
    /// returned [`DispatchHandle`] via `release_dispatch_locks` once the
    /// stream terminates (Final, Error, or Cancel). This intentionally
    /// keeps lock ownership tied to the streaming session.
    ///
    /// # Errors
    ///
    /// Returns an [`OpDispatchError`] if the op is unknown, no session
    /// exists, the lock check refuses dispatch, or the plugin's
    /// `StartStream` reply errors out.
    pub fn dispatch_start_stream<F>(
        &mut self,
        op_id: &str,
        ctx: DispatchContext,
        params: HashMap<String, ParamValue>,
        entity_type_of: F,
    ) -> Result<(u64, DispatchHandle), OpDispatchError>
    where
        F: Fn(molex::EntityId) -> Option<molex::EntityKind>,
    {
        let cached = self
            .plugin_registry
            .get_op(op_id)
            .ok_or_else(|| OpDispatchError::UnknownOp(String::from(op_id)))?
            .clone();
        self.start_stream_on(
            &cached.plugin_id,
            &cached,
            ctx,
            params,
            entity_type_of,
            false,
        )
    }

    /// Start a stream on an explicitly named plugin WITHOUT acquiring any
    /// entity/global lock (only the per-plugin backend lock). For host
    /// stream ops that provision assets without touching geometry (weights
    /// download), so an in-flight run does not disable every other action.
    ///
    /// # Errors
    ///
    /// Returns an [`OpDispatchError`] if the op id is unknown, the target
    /// plugin has no session or its worker is gone, or the plugin's
    /// `StartStream` reply errors out.
    // The explicit target plus the shared stream inputs (op id, ctx,
    // params, entity-type resolver) put this one over the arg cap.
    #[allow(clippy::too_many_arguments)]
    pub fn dispatch_start_stream_on_plugin_lockless<F>(
        &mut self,
        plugin_id: &str,
        op_id: &str,
        ctx: DispatchContext,
        params: HashMap<String, ParamValue>,
        entity_type_of: F,
    ) -> Result<(u64, DispatchHandle), OpDispatchError>
    where
        F: Fn(molex::EntityId) -> Option<molex::EntityKind>,
    {
        let cached = self
            .plugin_registry
            .get_op(op_id)
            .ok_or_else(|| OpDispatchError::UnknownOp(String::from(op_id)))?
            .clone();
        self.start_stream_on(
            plugin_id,
            &cached,
            ctx,
            params,
            entity_type_of,
            true,
        )
    }

    /// Shared body for [`Self::dispatch_start_stream`] and
    /// [`Self::dispatch_start_stream_on_plugin_lockless`]: acquire the lock, submit
    /// `StartStream` to `target_plugin`, and record the stream. Op
    /// metadata (`lock_meta`, `display_name`, `op_id`) comes from `cached`;
    /// every routing key (session, backend lock, worker) is `target_plugin`.
    // STALE_GEN retry + stream-id bookkeeping puts this just over the
    // 60-line bar; collapsing the retry into a helper would cost a
    // worse signature than the marginal length save.
    #[allow(clippy::too_many_lines, clippy::too_many_arguments)]
    fn start_stream_on<F>(
        &mut self,
        target_plugin: &str,
        cached: &CachedPluginOp,
        ctx: DispatchContext,
        params: HashMap<String, ParamValue>,
        entity_type_of: F,
        lockless: bool,
    ) -> Result<(u64, DispatchHandle), OpDispatchError>
    where
        F: Fn(molex::EntityId) -> Option<molex::EntityKind>,
    {
        let session = self
            .plugin_sessions
            .get(target_plugin)
            .copied()
            .ok_or_else(|| {
                OpDispatchError::NoSession(String::from(target_plugin))
            })?;
        if !self.locks.try_lock_backend(target_plugin) {
            return Err(OpDispatchError::LockRefused(
                DispatchError::BackendBusy {
                    plugin_id: String::from(target_plugin),
                },
            ));
        }
        let mut handle_lock = if lockless {
            // A weights download provisions assets and touches no geometry, so
            // it acquires no entity/global lock; only the backend lock below
            // serializes the worker. Holding no entity/global lock keeps every
            // other action enabled while the download runs.
            DispatchHandle {
                entities: Vec::new(),
                global_held: false,
                create_barrier_held: false,
                cancel_flag: Arc::new(AtomicBool::new(false)),
                backend_lock: None,
            }
        } else {
            match self.locks.dispatch_lock_check(
                &cached.lock_meta,
                &ctx,
                &cached.display_name,
                &entity_type_of,
            ) {
                Ok(h) => h,
                Err(e) => {
                    self.locks.unlock_backend(target_plugin);
                    return Err(OpDispatchError::LockRefused(e));
                }
            }
        };
        handle_lock.backend_lock = Some(String::from(target_plugin));

        // Allocate the dispatch id up front and flow it DOWN to the
        // plugin: the plugin obeys it as the stream id rather than
        // choosing its own.
        let request_id = self.alloc_request_id();
        // op_id owned into the task; cached.op_id equals the caller's op
        // id since the registry keys ops by that field.
        let mut result =
            self.call_worker(target_plugin, |reply| PluginTask::StartStream {
                session,
                op: cached.op_id.clone(),
                ctx: ctx.clone(),
                params: params.clone(),
                request_id,
                reply,
            });
        if let Err(OpDispatchError::Plugin(e)) = &result {
            if is_stale_gen(e) {
                if let Err(resync_err) =
                    self.try_stale_gen_resync(target_plugin)
                {
                    self.locks.release_dispatch_locks(handle_lock);
                    return Err(resync_err);
                }
                result = self.call_worker(target_plugin, |reply| {
                    PluginTask::StartStream {
                        session,
                        op: cached.op_id.clone(),
                        ctx,
                        params,
                        request_id,
                        reply,
                    }
                });
            }
        }
        match result {
            Ok(()) => {
                let _ = self
                    .stream_plugins
                    .insert(request_id, String::from(target_plugin));
                Ok((request_id, handle_lock))
            }
            Err(e) => {
                self.locks.release_dispatch_locks(handle_lock);
                Err(e)
            }
        }
    }

    /// Push new params to a running stream (e.g. pull-target updates).
    ///
    /// # Errors
    ///
    /// Returns an [`OpDispatchError`] if the plugin worker is gone, the
    /// reply channel is dropped, or the plugin returns an
    /// `UpdateStream` error.
    pub fn dispatch_update_stream(
        &self,
        plugin_id: &str,
        request_id: u64,
        params: HashMap<String, ParamValue>,
    ) -> Result<(), OpDispatchError> {
        let handle = self.plugin_workers.get(plugin_id).ok_or_else(|| {
            OpDispatchError::WorkerGone(String::from(plugin_id))
        })?;
        let (reply_tx, reply_rx) = mpsc::channel();
        handle
            .submit(PluginTask::UpdateStream {
                request_id,
                params,
                reply: reply_tx,
            })
            .map_err(|_| {
                OpDispatchError::WorkerGone(String::from(plugin_id))
            })?;
        Ok(reply_rx.recv().map_err(|_| {
            OpDispatchError::WorkerGone(String::from(plugin_id))
        })??)
    }

    /// Cancel a running stream. Idempotent.
    ///
    /// # Errors
    ///
    /// Returns an [`OpDispatchError`] if the plugin worker is gone, the
    /// reply channel is dropped, or the plugin returns a
    /// `CancelStream` error.
    pub fn dispatch_cancel_stream(
        &self,
        plugin_id: &str,
        request_id: u64,
    ) -> Result<(), OpDispatchError> {
        let handle = self.plugin_workers.get(plugin_id).ok_or_else(|| {
            OpDispatchError::WorkerGone(String::from(plugin_id))
        })?;
        let (reply_tx, reply_rx) = mpsc::channel();
        handle
            .submit(PluginTask::CancelStream {
                request_id,
                reply: reply_tx,
            })
            .map_err(|_| {
                OpDispatchError::WorkerGone(String::from(plugin_id))
            })?;
        Ok(reply_rx.recv().map_err(|_| {
            OpDispatchError::WorkerGone(String::from(plugin_id))
        })??)
    }

    // Internals

    pub(in crate::orchestrator) fn call_worker<R, F>(
        &self,
        plugin_id: &str,
        make_task: F,
    ) -> Result<R, OpDispatchError>
    where
        F: FnOnce(mpsc::Sender<Result<R, RunnerError>>) -> PluginTask,
    {
        let worker_gone =
            || OpDispatchError::WorkerGone(String::from(plugin_id));
        let handle =
            self.plugin_workers.get(plugin_id).ok_or_else(worker_gone)?;
        let (reply_tx, reply_rx) = mpsc::channel();
        handle
            .submit(make_task(reply_tx))
            .map_err(|_| worker_gone())?;
        Ok(reply_rx.recv().map_err(|_| worker_gone())??)
    }

    /// Resync a single plugin from `last_full_broadcast` after it
    /// reported `STALE_GEN`. Errors if no cached payload exists (the
    /// plugin reported STALE_GEN before any host broadcast — invariant
    /// violation, surface as a plugin error rather than retry blindly).
    pub(in crate::orchestrator) fn try_stale_gen_resync(
        &mut self,
        plugin_id: &str,
    ) -> Result<(), OpDispatchError> {
        let cached = self.last_full_broadcast.clone().ok_or_else(|| {
            OpDispatchError::Plugin(RunnerError::Generic(format!(
                "plugin {plugin_id} reported STALE_GEN but no cached full \
                 Assembly is available"
            )))
        })?;
        self.resync_one_plugin(plugin_id, BroadcastPayload::Full(cached))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::RunnerError;

    #[test]
    fn is_stale_gen_matches_only_stale_gen_code() {
        assert!(is_stale_gen(&RunnerError::PluginError {
            code: String::from("STALE_GEN"),
            message: String::from("drift"),
        }));
        assert!(!is_stale_gen(&RunnerError::PluginError {
            code: String::from("INTERNAL"),
            message: String::from("boom"),
        }));
        assert!(!is_stale_gen(&RunnerError::Generic(String::from(
            "STALE_GEN"
        ))));
        assert!(!is_stale_gen(&RunnerError::Unsupported));
    }
}
