//! Plugin update drain + Assembly fan-out to peer plugins.

use std::sync::mpsc;

use super::OpDispatchError;
use crate::orchestrator::client::BroadcastPayload;
use crate::orchestrator::core::Orchestrator;
use crate::orchestrator::pump::PluginTask;
use crate::orchestrator::types::PluginUpdate;

impl Orchestrator {
    /// Drain the plugin update channel. Returns all queued updates and
    /// fans out each `Final`'s assembly to peer plugins via the same
    /// `UpdateAssembly` broadcast channel `dispatch_invoke` uses on
    /// completion -- mirroring the invoke-side fan-out so streaming
    /// ops keep peer plugins (the always-on Rosetta mirror, ML
    /// scorers) in sync. Self-broadcast is skipped: the originating
    /// plugin already holds the post-stream pose. Cleans up the
    /// `stream_plugins` map for any terminal `Final` / `Error`
    /// observed.
    pub fn drain_plugin_updates(&mut self) -> Vec<PluginUpdate> {
        let mut updates = Vec::new();
        while let Ok(u) = self.plugin_update_rx.try_recv() {
            updates.push(u);
        }

        for update in &updates {
            match update {
                PluginUpdate::Cancelled {
                    request_id,
                    assembly,
                    ..
                } => {
                    // Host-initiated cancel that returned a working
                    // pose: treat the same as `Final` for peer
                    // synchronization (the host is committing this
                    // assembly to canonical state, so other plugins
                    // need to see it via `UpdateAssembly`).
                    self.fan_out_terminal_assembly(
                        *request_id,
                        assembly,
                        "Cancelled",
                    );
                }
                PluginUpdate::Final {
                    request_id,
                    assembly,
                    ..
                } => {
                    self.fan_out_terminal_assembly(
                        *request_id,
                        assembly,
                        "Final",
                    );
                }
                PluginUpdate::Error { request_id, .. } => {
                    let _ = self.stream_plugins.remove(request_id);
                }
                PluginUpdate::Pending { .. }
                | PluginUpdate::Checkpoint { .. } => {
                    // Both are non-terminal: the originating plugin keeps
                    // streaming, so we don't tear down its `stream_plugins`
                    // entry. A pending is a discardable preview; a
                    // checkpoint is committed host-side, but that commit
                    // and any peer fan-out is driven off the drained
                    // update, not from here.
                }
            }
        }

        updates
    }

    /// Helper for [`Self::drain_plugin_updates`]: pop the originator
    /// from `stream_plugins`, re-serialize the decoded assembly, and
    /// fan it out to peer plugins. `kind` is `"Cancelled"` or
    /// `"Final"`, used only in the failure log line.
    ///
    /// Serializer regressions are logged and swallowed rather than
    /// blocking the drain.
    fn fan_out_terminal_assembly(
        &mut self,
        request_id: u64,
        assembly: &molex::Assembly,
        kind: &str,
    ) {
        let originator =
            self.stream_plugins.remove(&request_id).unwrap_or_default();
        match assembly.to_bytes() {
            Ok(bytes) => {
                self.fan_out_assembly(
                    &originator,
                    &BroadcastPayload::Full(bytes),
                );
            }
            Err(e) => {
                log::warn!(
                    "drain_plugin_updates: serialize {kind} assembly for \
                     fan-out failed (rid {request_id}): {e:?}"
                );
            }
        }
    }

    /// Push an Assembly broadcast to every other registered plugin so
    /// they can update their working state (per protocol §"Source of
    /// truth"). Bumps `broadcast_gen` once per call; stamps each
    /// outgoing `UpdateAssembly` with `from_gen = pre`, `to_gen = post`.
    /// Best-effort; failures are logged and ignored.
    /// Broadcast a host-originated Assembly change to every loaded
    /// plugin except the one named by `exclude_plugin_id`. Bumps the
    /// host's broadcast generation, stamps it onto each outgoing
    /// `UpdateAssembly`, and caches full payloads for `STALE_GEN`
    /// recovery.
    ///
    /// `exclude_plugin_id` is the plugin that sourced the edit being
    /// broadcast; it already holds this assembly, so re-sending it would
    /// land as a destructive self-delta. Pass `""` to broadcast to all
    /// (a host-internal edit with no plugin source).
    pub fn broadcast_to_plugins(
        &mut self,
        payload: &BroadcastPayload,
        exclude_plugin_id: &str,
    ) {
        self.fan_out_assembly(exclude_plugin_id, payload);
    }

    pub(super) fn fan_out_assembly(
        &mut self,
        originating_plugin_id: &str,
        payload: &BroadcastPayload,
    ) {
        let from_gen = self.broadcast_gen;
        let to_gen = from_gen.saturating_add(1);
        self.broadcast_gen = to_gen;
        // Cache full broadcasts so STALE_GEN recovery has something to
        // resend. Delta broadcasts intentionally don't overwrite —
        // see `Orchestrator::last_full_broadcast` docs.
        if let BroadcastPayload::Full(bytes) = payload {
            self.last_full_broadcast = Some(bytes.clone());
        }
        // A worker running a stream owns its pose for the action's whole
        // life: it pushes frames out and must never get an assembly pushed
        // in. A host update would clobber the in-flight sampler and cancel
        // the stream ("unknown request id"). Collect the streaming plugin
        // ids once (a cheap per-broadcast local) so the worker loop below
        // can skip them without re-borrowing `self`.
        let streaming: std::collections::HashSet<&str> =
            self.stream_plugins.values().map(String::as_str).collect();
        for (plugin_id, handle) in &self.plugin_workers {
            if plugin_id == originating_plugin_id
                || streaming.contains(plugin_id.as_str())
            {
                continue;
            }
            let session = match self.plugin_sessions.get(plugin_id) {
                Some(s) => *s,
                None => continue,
            };
            let (reply_tx, _reply_rx) = mpsc::channel();
            if let Err(e) = handle.submit(PluginTask::UpdateAssembly {
                session,
                payload: payload.clone(),
                from_gen,
                to_gen,
                reply: reply_tx,
            }) {
                log::warn!("fan-out UpdateAssembly to {plugin_id} failed: {e}");
            }
        }
    }

    /// Re-send `payload` to a single plugin so it can recover from a
    /// dropped broadcast. Used by `STALE_GEN` retry.
    pub(super) fn resync_one_plugin(
        &mut self,
        plugin_id: &str,
        payload: BroadcastPayload,
    ) -> Result<(), OpDispatchError> {
        // A worker with a live stream owns its pose; a STALE_GEN resend
        // would push a Full into the running sampler and cancel its stream,
        // so streaming workers are skipped (they resync after the stream
        // ends, via the post-terminal fan-out).
        if self.stream_plugins.values().any(|p| p == plugin_id) {
            return Ok(());
        }
        let from_gen = self.broadcast_gen;
        let to_gen = from_gen.saturating_add(1);
        self.broadcast_gen = to_gen;
        let session =
            self.plugin_sessions
                .get(plugin_id)
                .copied()
                .ok_or_else(|| {
                    OpDispatchError::NoSession(String::from(plugin_id))
                })?;
        let handle = self.plugin_workers.get(plugin_id).ok_or_else(|| {
            OpDispatchError::WorkerGone(String::from(plugin_id))
        })?;
        let (reply_tx, reply_rx) = mpsc::channel();
        handle
            .submit(PluginTask::UpdateAssembly {
                session,
                payload,
                from_gen,
                to_gen,
                reply: reply_tx,
            })
            .map_err(|_| {
                OpDispatchError::WorkerGone(String::from(plugin_id))
            })?;
        reply_rx
            .recv()
            .map_err(|_| OpDispatchError::WorkerGone(String::from(plugin_id)))?
            .map_err(OpDispatchError::Plugin)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broadcast_to_plugins_bumps_gen_and_caches_full() {
        let mut orch = Orchestrator::new();
        assert_eq!(orch.broadcast_gen(), 0);
        assert!(orch.last_full_broadcast.is_none());

        // No plugins registered → broadcast does nothing externally
        // but still bumps the gen counter and caches the bytes.
        orch.broadcast_to_plugins(&BroadcastPayload::Full(vec![1, 2, 3]), "");
        assert_eq!(orch.broadcast_gen(), 1);
        assert_eq!(orch.last_full_broadcast.as_deref(), Some(&[1u8, 2, 3][..]));

        // Delta does not overwrite the cached full snapshot but does
        // advance the gen counter.
        orch.broadcast_to_plugins(&BroadcastPayload::Delta(vec![9]), "");
        assert_eq!(orch.broadcast_gen(), 2);
        assert_eq!(orch.last_full_broadcast.as_deref(), Some(&[1u8, 2, 3][..]));

        // A second full payload replaces the cache.
        orch.broadcast_to_plugins(&BroadcastPayload::Full(vec![4, 5]), "");
        assert_eq!(orch.broadcast_gen(), 3);
        assert_eq!(orch.last_full_broadcast.as_deref(), Some(&[4u8, 5][..]));
    }
}
