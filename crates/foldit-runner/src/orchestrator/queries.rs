//! Generic async query path.
//!
//! Generalizes the async `score` path ([`super::scores`]) to an arbitrary
//! query id: fire a non-blocking query at the single provider that
//! registered the id, stash the reply receiver, and drain it later off the
//! caller's thread. Unlike the score pollers, replies are returned as
//! opaque bytes; the caller decodes the proto for the specific query.
//!
//! Lives outside `ops.rs` so that file stays under the per-file line
//! budget.

use std::collections::HashMap;
use std::sync::mpsc;

use super::core::{Orchestrator, ScoreReplyRx};
use super::pump::PluginTask;
use super::types::DispatchContext;

impl Orchestrator {
    /// Fire a non-blocking query at the single provider that registered
    /// `id`. The reply lands on a one-shot receiver stored in
    /// `pending_queries` (keyed by query id) and drained by
    /// [`Self::poll_query_results`], so the caller's thread never blocks on
    /// the worker round-trip. One query per id is coalesced: if a query for
    /// `id` is already outstanding, this is a no-op. No-op as well when no
    /// provider registered `id`, when its session is absent, or when its
    /// worker handle is gone. Reuses the existing `PluginTask::Query` reply
    /// mechanism, so no worker/proto change is needed.
    ///
    /// The reply is opaque bytes; the caller decodes the proto for the
    /// query (matching the sync [`Self::dispatch_query`], which also
    /// returns raw bytes). The query runs against the live session pose
    /// (empty assembly, default params).
    #[cfg(not(target_arch = "wasm32"))]
    pub fn request_query(&mut self, id: &str, ctx: &DispatchContext) {
        if self.pending_queries.contains_key(id) {
            return; // a query is already outstanding for this id
        }
        let Some(cached) = self.plugin_registry.get_query(id) else {
            return;
        };
        let plugin_id = cached.plugin_id.clone();
        let Some(&session) = self.plugin_sessions.get(&plugin_id) else {
            return;
        };
        let Some(handle) = self.plugin_workers.get(&plugin_id) else {
            return;
        };
        let (reply_tx, reply_rx) = mpsc::channel();
        let task = PluginTask::Query {
            session,
            query: String::from(id),
            ctx: ctx.clone(),
            params: HashMap::new(),
            assembly: Vec::new(), // session pose, not a composition
            reply: reply_tx,
        };
        if handle.submit(task).is_ok() {
            let _ = self.pending_queries.insert(String::from(id), reply_rx);
        }
    }

    /// Whether a query for `id` is currently in flight (an outstanding reply
    /// slot in `pending_queries`).
    #[cfg(not(target_arch = "wasm32"))]
    #[must_use]
    pub fn has_pending_query(&self, id: &str) -> bool {
        self.pending_queries.contains_key(id)
    }

    /// Drain whatever async query replies have arrived since the last call.
    /// Non-blocking `try_recv`; a query whose reply is not yet ready stays
    /// pending. Returns one `(query_id, bytes)` pair per query that replied
    /// this call, with the reply as opaque bytes the caller decodes. Errors
    /// and disconnects are logged and dropped; the query's pending slot
    /// clears either way, so the next [`Self::request_query`] for that id
    /// re-fires it.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn poll_query_results(&mut self) -> Vec<(String, Vec<u8>)> {
        drain_opaque_replies(&mut self.pending_queries, "poll_query_results")
    }
}

/// Drain the arrived one-shot replies from a per-key pending map without
/// blocking. Shared by every async path that stashes opaque reply receivers
/// keyed by a `String` (the generic query drain keyed by query id, the
/// weights-status drain keyed by plugin id): each fired a `PluginTask::Query`
/// and stored its reply receiver, and this collects whatever `try_recv`
/// yields. Returns one `(key, bytes)` pair per key that delivered a reply this
/// call; a key whose reply has not arrived stays pending, and any key that
/// delivered, failed, or disconnected is removed so the next request for it
/// re-fires. `label` names the calling path in the failure log.
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn drain_opaque_replies(
    pending: &mut HashMap<String, ScoreReplyRx>,
    label: &str,
) -> Vec<(String, Vec<u8>)> {
    let mut out: Vec<(String, Vec<u8>)> = Vec::new();
    let mut done: Vec<String> = Vec::new();
    for (key, rx) in &*pending {
        match rx.try_recv() {
            Ok(Ok(bytes)) => {
                out.push((key.clone(), bytes));
                done.push(key.clone());
            }
            Ok(Err(e)) => {
                log::warn!("[Orchestrator] {label}: query {key} failed: {e}");
                done.push(key.clone());
            }
            Err(mpsc::TryRecvError::Empty) => {} // still in flight
            Err(mpsc::TryRecvError::Disconnected) => {
                done.push(key.clone()); // worker gone; clear slot
            }
        }
    }
    for key in done {
        let _ = pending.remove(&key);
    }
    out
}
