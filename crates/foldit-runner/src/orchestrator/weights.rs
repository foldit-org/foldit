//! Multi-plugin `weights_status` fan-out.
//!
//! Every ML plugin registers a `weights_status` query under the same
//! well-known id, reporting whether its model weights are present on disk.
//! The generic query path ([`super::queries`]) resolves only the first
//! provider of an id, so it can't reach every plugin behind a shared id.
//! This fans the query out to every provider and drains the per-plugin
//! replies, returning them keyed by plugin id.
//!
//! Replies are opaque `{ready,present,missing}` JSON bytes; decoding lives
//! with the caller, not here (unlike the typed score pollers in
//! [`super::scores`]).
//!
//! Lives outside `ops.rs` so that file stays under the per-file line
//! budget.

use super::core::Orchestrator;
use super::queries::drain_opaque_replies;
use super::types::DispatchContext;

impl Orchestrator {
    /// Fire a non-blocking `weights_status` query at every provider that has
    /// no query already in flight. The reply lands on a one-shot receiver
    /// stored in `pending_weights_queries` (keyed by plugin id) and drained
    /// by [`Self::poll_weights_status`], so the caller's thread never blocks
    /// on the worker round-trip. A provider with an outstanding query is
    /// skipped (coalesced). The query runs against the live session pose
    /// (empty assembly, default context/params); weights presence does not
    /// depend on the pose. Reuses the existing `PluginTask::Query` reply
    /// mechanism, so no worker/proto change is needed.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn request_weights_status(&mut self) {
        let pairs = self.fan_out_session_query(
            "weights_status",
            &DispatchContext::default(),
            &self.pending_weights_queries,
        );
        for (plugin_id, reply_rx) in pairs {
            let _ = self.pending_weights_queries.insert(plugin_id, reply_rx);
        }
    }

    /// Drain whatever async `weights_status` replies have arrived since the
    /// last call. Non-blocking `try_recv`; a provider whose reply is not yet
    /// ready stays pending. Returns one `(plugin_id, bytes)` pair per
    /// provider that replied this call, with the reply as opaque
    /// `{ready,present,missing}` JSON bytes the caller decodes. Errors and
    /// disconnects are logged and dropped; the provider's pending slot clears
    /// either way, so the next [`Self::request_weights_status`] re-fires it.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn poll_weights_status(&mut self) -> Vec<(String, Vec<u8>)> {
        drain_opaque_replies(
            &mut self.pending_weights_queries,
            "poll_weights_status",
        )
    }
}
