//! Per-plugin IPC pump thread.
//!
//! The orchestrator spawns one OS thread per registered plugin. The
//! thread owns the [`PluginClient`] (a wrapper around the plugin's IPC
//! socket — single-threaded by design) and serializes synchronous
//! round-trips to the plugin's worker subprocess. It is not a
//! computational worker; the actual computation lives in that
//! subprocess.
//!
//! The thread receives [`PluginTask`]s from the orchestrator over an
//! `mpsc` channel and either:
//!
//! 1. Forwards the call to the plugin subprocess over the wire, or
//! 2. Tracks a new stream id and starts polling it on a 50 ms cadence.
//!
//! Stream poll results (`Pending` / `Final` / `Error`) flow back to the
//! orchestrator via a second `mpsc` channel as [`PluginUpdate`]s,
//! drained per frame.

use std::collections::HashMap;
use std::process::Child;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::client::{BroadcastPayload, PluginClient};
use super::types::{
    DispatchContext, InitPayload, ParamValue, PluginUpdate, PollOutcome,
};
use crate::error::RunnerError;
use crate::proto::plugin as proto;

/// How often the pump thread polls each active stream when idle.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Init reply payload: `(session_id, registration, initial_assembly)`.
/// `initial_assembly` is assembly bytes of the plugin's post-Init
/// normalized assembly (e.g. Rosetta's full-atom pose), or empty if
/// the plugin made no changes (Python plugins, etc.).
type InitReply = (u64, proto::PluginRegistration, Vec<u8>);

// PluginTask -- work items sent to a pump thread

/// Task submitted from the orchestrator to a pump thread.
///
/// Each variant carries a `mpsc::Sender` reply channel for the thread to
/// deliver the result on (synchronous round-trip from the caller's
/// perspective).
///
/// Streaming is the exception: `StartStream` returns the request_id
/// synchronously via `reply`, after which the thread takes over driving
/// the stream and pushes `Pending` / `Final` / `Error` updates onto the
/// orchestrator's [`PluginUpdate`] queue.
pub enum PluginTask {
    /// Open a plugin session with the initial assembly bytes and any
    /// puzzle-specific payload (ligand assets + catalytic constraints).
    Init {
        /// Initial assembly bytes handed to the plugin.
        assembly: Vec<u8>,
        /// Puzzle-specific session payload (empty for protein-only /
        /// free-form loads).
        payload: InitPayload,
        /// Reply channel for the `InitReply` payload.
        reply: mpsc::Sender<Result<InitReply, RunnerError>>,
    },
    /// Broadcast an assembly update to the plugin.
    UpdateAssembly {
        /// Target session id.
        session: u64,
        /// Either a full assembly snapshot or a delta patch.
        payload: BroadcastPayload,
        /// Generation the plugin should consider its starting view.
        from_gen: u64,
        /// Generation the broadcast lands the plugin on.
        to_gen: u64,
        /// Reply channel for completion / error.
        reply: mpsc::Sender<Result<(), RunnerError>>,
    },
    /// Close a plugin session.
    Drop {
        /// Target session id.
        session: u64,
        /// Reply channel for completion / error.
        reply: mpsc::Sender<Result<(), RunnerError>>,
    },
    /// Run a one-shot op against the plugin.
    Invoke {
        /// Target session id.
        session: u64,
        /// Op id declared by the plugin.
        op: String,
        /// Selection / focused-entity context for the op.
        ctx: DispatchContext,
        /// Op parameters keyed by parameter name.
        params: HashMap<String, ParamValue>,
        /// Reply channel for the resulting assembly bytes.
        reply: mpsc::Sender<Result<Vec<u8>, RunnerError>>,
    },
    /// Start a streaming op under the host-assigned `request_id`. The
    /// pump thread takes over driving the stream once the start has been
    /// acknowledged on `reply`.
    StartStream {
        /// Target session id.
        session: u64,
        /// Op id declared by the plugin.
        op: String,
        /// Selection / focused-entity context for the op.
        ctx: DispatchContext,
        /// Op parameters keyed by parameter name.
        params: HashMap<String, ParamValue>,
        /// Host-assigned stream id the plugin obeys.
        request_id: u64,
        /// Reply channel for the start acknowledgement.
        reply: mpsc::Sender<Result<(), RunnerError>>,
    },
    /// Apply a live parameter update to an active stream.
    UpdateStream {
        /// Stream id returned by an earlier `StartStream`.
        request_id: u64,
        /// Updated parameter values keyed by parameter name.
        params: HashMap<String, ParamValue>,
        /// Reply channel for completion / error.
        reply: mpsc::Sender<Result<(), RunnerError>>,
    },
    /// Cancel an active stream.
    CancelStream {
        /// Stream id to cancel.
        request_id: u64,
        /// Reply channel for completion / error.
        reply: mpsc::Sender<Result<(), RunnerError>>,
    },
    /// Run a read-only query against the plugin.
    Query {
        /// Target session id.
        session: u64,
        /// Query id declared by the plugin.
        query: String,
        /// Selection / focused-entity context for the query.
        ctx: DispatchContext,
        /// Query parameters keyed by parameter name.
        params: HashMap<String, ParamValue>,
        /// Composition bytes naming a specific assembly to read/score, or
        /// empty to run the query against the live session pose.
        assembly: Vec<u8>,
        /// Reply channel for the resulting bytes.
        reply: mpsc::Sender<Result<Vec<u8>, RunnerError>>,
    },
}

// Pump thread loop

/// Run the per-plugin pump thread. Owns the [`PluginClient`]; processes
/// tasks from `task_rx`; drives polling for any active streams.
///
/// Returns when `task_rx` is disconnected (the orchestrator dropped the
/// last sender) and there are no active streams left to drain.
pub fn pump_loop(
    mut client: Box<dyn PluginClient>,
    task_rx: &mpsc::Receiver<PluginTask>,
    update_tx: &mpsc::Sender<PluginUpdate>,
) {
    let mut active_streams: HashMap<u64, Instant> = HashMap::new();
    let plugin_id = String::from(client.plugin_id());

    loop {
        // When no streams are active, block indefinitely waiting for the
        // next task. When streams ARE active, time out after the poll
        // interval so we can poll them.
        let task_result = if active_streams.is_empty() {
            task_rx.recv().map_err(|_| TaskWaitError::Disconnected)
        } else {
            task_rx.recv_timeout(POLL_INTERVAL).map_err(|e| match e {
                mpsc::RecvTimeoutError::Timeout => TaskWaitError::Timeout,
                mpsc::RecvTimeoutError::Disconnected => {
                    TaskWaitError::Disconnected
                }
            })
        };

        match task_result {
            Ok(task) => handle_task(task, client.as_mut(), &mut active_streams),
            Err(TaskWaitError::Timeout) => {}
            Err(TaskWaitError::Disconnected) => {
                if active_streams.is_empty() {
                    log::info!(
                        "pump thread for plugin {plugin_id} exiting (channel \
                         closed)"
                    );
                    return;
                }
                // Channel closed but streams still in flight — drain them
                // before exiting, then return on the next disconnected loop.
            }
        }

        poll_active_streams(client.as_mut(), &mut active_streams, update_tx);
    }
}

enum TaskWaitError {
    Timeout,
    Disconnected,
}

fn handle_task(
    task: PluginTask,
    client: &mut dyn PluginClient,
    active_streams: &mut HashMap<u64, Instant>,
) {
    match task {
        PluginTask::Init {
            assembly,
            payload,
            reply,
        } => {
            let _ = reply.send(client.init(assembly, payload));
        }
        PluginTask::UpdateAssembly {
            session,
            payload,
            from_gen,
            to_gen,
            reply,
        } => {
            let _ = reply.send(
                client.update_assembly(session, payload, from_gen, to_gen),
            );
        }
        PluginTask::Drop { session, reply } => {
            let _ = reply.send(client.drop_session(session));
        }
        PluginTask::Invoke {
            session,
            op,
            ctx,
            params,
            reply,
        } => {
            let _ = reply.send(client.invoke(session, &op, &ctx, params));
        }
        PluginTask::Query {
            session,
            query,
            ctx,
            params,
            assembly,
            reply,
        } => {
            let _ = reply
                .send(client.query(session, &query, &ctx, params, assembly));
        }
        stream_task @ (PluginTask::StartStream { .. }
        | PluginTask::UpdateStream { .. }
        | PluginTask::CancelStream { .. }) => {
            handle_stream_task(stream_task, client, active_streams);
        }
    }
}

fn handle_stream_task(
    task: PluginTask,
    client: &mut dyn PluginClient,
    active_streams: &mut HashMap<u64, Instant>,
) {
    match task {
        PluginTask::StartStream {
            session,
            op,
            ctx,
            params,
            request_id,
            reply,
        } => {
            let result =
                client.start_stream(session, &op, &ctx, params, request_id);
            if result.is_ok() {
                let now = Instant::now();
                let _ = active_streams.insert(
                    request_id,
                    now.checked_sub(POLL_INTERVAL).unwrap_or(now),
                );
            }
            let _ = reply.send(result);
        }
        PluginTask::UpdateStream {
            request_id,
            params,
            reply,
        } => {
            let _ = reply.send(client.update_stream(request_id, params));
        }
        PluginTask::CancelStream { request_id, reply } => {
            let result = client.cancel_stream(request_id);
            // Keep the stream in `active_streams` so the next poll
            // captures the plugin's terminal `STREAM_CANCELLED` (or
            // whatever final state the plugin emits). That terminal
            // travels through `update_tx` to the host, which is where
            // `release_dispatch_locks` runs -- early-removing here
            // would silently strand the lock until next session
            // teardown. If cancel itself errors (worker gone /
            // transport dead), the next `poll_stream` call hits the
            // same transport failure, `handle_poll_outcome` emits
            // `PluginUpdate::Error`, and the stream is removed via
            // the normal terminal path. Idempotent at every layer.
            let _ = reply.send(result);
        }
        _ => unreachable!("handle_stream_task called with non-stream task"),
    }
}

fn poll_active_streams(
    client: &mut dyn PluginClient,
    active_streams: &mut HashMap<u64, Instant>,
    update_tx: &mpsc::Sender<PluginUpdate>,
) {
    if active_streams.is_empty() {
        return;
    }
    let now = Instant::now();
    let mut to_remove: Vec<u64> = Vec::new();

    let due: Vec<u64> = active_streams
        .iter()
        .filter(|(_, last)| now.duration_since(**last) >= POLL_INTERVAL)
        .map(|(rid, _)| *rid)
        .collect();

    for rid in due {
        let _ = active_streams.insert(rid, now);
        let outcome = client.poll_stream(rid);
        if handle_poll_outcome(rid, outcome, update_tx) {
            to_remove.push(rid);
        }
    }

    for rid in to_remove {
        let _ = active_streams.remove(&rid);
    }
}

/// Forward a single `poll_stream` outcome to `update_tx`. Returns `true`
/// if the stream is finished (final / error / transport error) and the
/// caller should stop polling it.
///
/// `clippy::too_many_lines` allow: this is a fan-out switch with one
/// arm per `PollOutcome` variant; the body is mostly per-variant
/// serialization and emit, not duplication that splits cleanly.
#[allow(clippy::too_many_lines)]
fn handle_poll_outcome(
    rid: u64,
    outcome: Result<PollOutcome, RunnerError>,
    update_tx: &mpsc::Sender<PluginUpdate>,
) -> bool {
    match outcome {
        Ok(PollOutcome::Pending {
            latest_assembly,
            progress,
            stage,
            score,
        }) => {
            let assembly = latest_assembly.and_then(|bytes| {
                molex::Assembly::from_bytes(&bytes)
                    .map_err(|e| {
                        log::warn!(
                            "failed to decode pending assembly bytes: {e:?}"
                        );
                        e
                    })
                    .ok()
            });
            let _ = update_tx.send(PluginUpdate::Pending {
                request_id: rid,
                latest_assembly: assembly,
                progress,
                stage,
                score,
            });
            false
        }
        Ok(PollOutcome::Checkpoint {
            latest_assembly,
            progress,
            stage,
            score,
        }) => {
            // A checkpoint commits an intermediate state and the stream
            // continues; unlike a terminal it does not end the op, so we
            // return `false` to keep polling.
            let assembly = latest_assembly.and_then(|bytes| {
                molex::Assembly::from_bytes(&bytes)
                    .map_err(|e| {
                        log::warn!(
                            "failed to decode checkpoint assembly bytes: {e:?}"
                        );
                        e
                    })
                    .ok()
            });
            let _ = update_tx.send(PluginUpdate::Checkpoint {
                request_id: rid,
                latest_assembly: assembly,
                progress,
                stage,
                score,
            });
            false
        }
        Ok(PollOutcome::Cancelled { assembly, score }) => {
            // Same downstream handling as `Final`: deserialize the
            // working assembly and emit a terminal update. Decode
            // failure degrades to `Error` (the host can't commit
            // garbage bytes) the same way `Final`'s decode-failure
            // path does.
            match molex::Assembly::from_bytes(&assembly) {
                Ok(assembly) => {
                    let _ = update_tx.send(PluginUpdate::Cancelled {
                        request_id: rid,
                        assembly,
                        score,
                    });
                }
                Err(e) => {
                    let _ = update_tx.send(PluginUpdate::Error {
                        request_id: rid,
                        message: format!(
                            "failed to decode cancelled assembly bytes: {e:?}"
                        ),
                    });
                }
            }
            true
        }
        Ok(PollOutcome::Final { assembly, score }) => {
            match molex::Assembly::from_bytes(&assembly) {
                Ok(assembly) => {
                    let _ = update_tx.send(PluginUpdate::Final {
                        request_id: rid,
                        assembly,
                        result: None,
                        score,
                    });
                }
                Err(e) => {
                    let _ = update_tx.send(PluginUpdate::Error {
                        request_id: rid,
                        message: format!(
                            "failed to decode final assembly bytes: {e:?}"
                        ),
                    });
                }
            }
            true
        }
        Ok(PollOutcome::Error { code, message, .. }) => {
            let _ = update_tx.send(PluginUpdate::Error {
                request_id: rid,
                message: format!("[{code}] {message}"),
            });
            true
        }
        Err(e) => {
            let _ = update_tx.send(PluginUpdate::Error {
                request_id: rid,
                message: format!("poll_stream transport error: {e}"),
            });
            true
        }
    }
}

// PluginWorkerHandle — orchestrator-side handle to a running plugin worker

/// Orchestrator-side handle to a running plugin worker.
///
/// Holds the running worker subprocess, the task channel sender, and
/// the join handle for the pump thread. Drop terminates the worker
/// (kills the process group, joins the thread).
pub struct PluginWorkerHandle {
    plugin_id: String,
    /// `None` for in-process plugin runtimes (none today; left for the
    /// future wasm host that lives in the same address space as the
    /// orchestrator). For subprocess-hosted plugins (Python via
    /// foldit-worker; native dylibs hosted by the same worker
    /// binary), this is `Some` and the process group is killed on
    /// terminate.
    pid: Option<u32>,
    process: Option<Child>,
    /// `Option` so [`PluginWorkerHandle::terminate`] can drop the sender,
    /// causing the pump thread to exit cleanly when the channel
    /// disconnects.
    task_tx: Option<mpsc::Sender<PluginTask>>,
    thread: Option<JoinHandle<()>>,
}

impl PluginWorkerHandle {
    /// Assemble a handle around an already-spawned process and pump
    /// thread. The pid is captured eagerly so `terminate` still has it
    /// after the `Child` is taken.
    pub fn new(
        plugin_id: impl Into<String>,
        process: Option<Child>,
        task_tx: mpsc::Sender<PluginTask>,
        thread: JoinHandle<()>,
    ) -> Self {
        let pid = process.as_ref().map(Child::id);
        Self {
            plugin_id: plugin_id.into(),
            pid,
            process,
            task_tx: Some(task_tx),
            thread: Some(thread),
        }
    }

    /// Stable plugin id this worker hosts.
    #[must_use]
    pub fn plugin_id(&self) -> &str {
        &self.plugin_id
    }

    /// Submit a task to the pump thread.
    ///
    /// # Errors
    ///
    /// Returns the unsent task wrapped in `SendError` if the channel is
    /// closed (worker terminated).
    pub fn submit(
        &self,
        task: PluginTask,
    ) -> Result<(), Box<mpsc::SendError<PluginTask>>> {
        match self.task_tx.as_ref() {
            Some(tx) => tx.send(task).map_err(Box::new),
            None => Err(Box::new(mpsc::SendError(task))),
        }
    }

    /// Whether the worker process is still alive. In-process plugins
    /// (no `Child`) report `true` until terminated.
    pub fn is_alive(&mut self) -> bool {
        match self.process.as_mut() {
            Some(p) => matches!(p.try_wait(), Ok(None)),
            None => self.task_tx.is_some(),
        }
    }

    /// Terminate the worker: drop the task channel, kill the process group
    /// (if there is one), wait for the thread. Idempotent.
    pub fn terminate(&mut self) {
        // Drop the task sender — pump thread will see Disconnected.
        self.task_tx = None;

        if let (Some(mut process), Some(pid)) = (self.process.take(), self.pid)
        {
            #[cfg(unix)]
            {
                let _ = std::process::Command::new("kill")
                    .args(["-9", "--", &format!("-{pid}")])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
            }
            #[cfg(windows)]
            {
                let _ = std::process::Command::new("taskkill")
                    .args(["/F", "/T", "/PID", &pid.to_string()])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
            }
            let _ = process.wait();
            super::cleanup::unregister_worker_pgid(pid);
        }

        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for PluginWorkerHandle {
    fn drop(&mut self) {
        self.terminate();
    }
}

// spawn_pump — pair a PluginClient with a pump thread

/// Spawn the pump thread that owns `client` and processes
/// [`PluginTask`]s.
///
/// Returns the task channel sender plus the join handle; the caller
/// (typically `Orchestrator::register_plugin`) wraps these into a
/// [`PluginWorkerHandle`].
///
/// # Panics
///
/// Panics if the OS refuses to spawn the pump thread.
#[must_use]
#[allow(clippy::expect_used)]
pub fn spawn_pump(
    client: Box<dyn PluginClient>,
    update_tx: mpsc::Sender<PluginUpdate>,
) -> (mpsc::Sender<PluginTask>, JoinHandle<()>) {
    let (task_tx, task_rx) = mpsc::channel::<PluginTask>();
    let plugin_id = String::from(client.plugin_id());
    let join = thread::Builder::new()
        .name(format!("plugin-pump-{plugin_id}"))
        .spawn(move || pump_loop(client, &task_rx, &update_tx))
        .expect("failed to spawn plugin pump thread");
    (task_tx, join)
}
