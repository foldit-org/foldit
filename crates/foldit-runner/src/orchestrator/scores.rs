//! Multi-plugin score aggregation.
//!
//! The unified plugin protocol exposes scoring as a generic Query: any
//! plugin that registers the well-known `score` query contributes a
//! [`proto::ScoreReport`] (total + per-term dictionary + per-residue
//! breakdown). The host fans out across every provider and merges the
//! reports into the app-wide score view.
//!
//! Today only Rosetta returns a non-trivial total, but the plumbing is
//! N-plugin from the start so other scorers (ML predictors with
//! confidence terms, geometric validators, etc.) can drop in alongside
//! without host-side wiring changes.
//!
//! Lives outside `ops.rs` so that file stays under the per-file line
//! budget.

use std::collections::HashMap;
use std::sync::mpsc;

use super::core::{Orchestrator, ScoreReplyRx};
use super::pump::PluginTask;
use super::types::DispatchContext;
use crate::proto::plugin as proto;

impl Orchestrator {
    /// Fire a non-blocking `score` query at every provider that has no
    /// query already in flight. The reply lands on a one-shot receiver
    /// stored in `pending_score_queries` and drained by
    /// [`Self::poll_score_results`], so the caller's thread (the host
    /// render thread) never blocks on the worker. A provider with an
    /// outstanding query is skipped, which coalesces a fast pose stream
    /// against a slow scorer. Reuses the existing `PluginTask::Query`
    /// reply mechanism, so no worker/proto change is needed.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn request_scores(&mut self, ctx: &DispatchContext) {
        let pairs = self.fan_out_session_query(
            "score",
            ctx,
            &self.pending_score_queries,
        );
        for (plugin_id, reply_rx) in pairs {
            let _ = self.pending_score_queries.insert(plugin_id, reply_rx);
        }
    }

    /// Fan a non-blocking session-pose query out to every provider of `query`
    /// that has no reply already in flight, returning the reply receivers
    /// keyed by plugin id for the caller to stash in its own pending map.
    /// Shared by the whole-assembly score fan-out and the weights-status
    /// fan-out: both query every provider of a well-known id against the live
    /// session pose (empty assembly, default params) and coalesce against a
    /// per-path pending map. A provider already present in `pending`, or
    /// missing its session or worker handle, is skipped. Reuses the
    /// `PluginTask::Query` reply mechanism, so no worker/proto change is
    /// needed. The caller owns insertion so each path keeps its own map.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn fan_out_session_query(
        &self,
        query: &str,
        ctx: &DispatchContext,
        pending: &HashMap<String, ScoreReplyRx>,
    ) -> Vec<(String, ScoreReplyRx)> {
        let providers: Vec<String> = self
            .plugin_registry
            .query_providers(query)
            .iter()
            .map(|q| q.plugin_id.clone())
            .collect();
        let mut pairs: Vec<(String, ScoreReplyRx)> =
            Vec::with_capacity(providers.len());
        for plugin_id in providers {
            if pending.contains_key(&plugin_id) {
                continue; // query already outstanding for this provider
            }
            let Some(&session) = self.plugin_sessions.get(&plugin_id) else {
                continue;
            };
            let Some(handle) = self.plugin_workers.get(&plugin_id) else {
                continue;
            };
            let (reply_tx, reply_rx) = mpsc::channel();
            let task = PluginTask::Query {
                session,
                query: String::from(query),
                ctx: ctx.clone(),
                params: HashMap::new(),
                assembly: Vec::new(), // session pose, not a composition
                reply: reply_tx,
            };
            if handle.submit(task).is_ok() {
                pairs.push((plugin_id, reply_rx));
            }
        }
        pairs
    }

    /// Whether any whole-assembly `score` query is currently in flight (a
    /// provider has an outstanding reply slot in `pending_score_queries`).
    /// Lets a caller distinguish "a scorer was kicked and a reply is coming"
    /// from "no scorer exists, nothing was queued" right after
    /// [`Self::request_scores`], so a bring-up wait that watches for a stamped
    /// score does not hang when no provider ever queued.
    #[cfg(not(target_arch = "wasm32"))]
    #[must_use]
    pub fn has_pending_score_queries(&self) -> bool {
        !self.pending_score_queries.is_empty()
    }

    /// Fire a composition-score request at every `score` provider for the
    /// caller's correlation `request_id`, carrying the assembly bytes of
    /// the composition to score. Non-blocking: each provider's reply lands
    /// on a receiver stored in `pending_composition_scores` under
    /// `request_id` and drained by [`Self::poll_composition_scores`]. A
    /// `request_id` already in flight is skipped (coalesces a fast edit
    /// stream against a slow scorer). No-op with no providers.
    ///
    /// Distinct from [`Self::request_scores`]: that fans the whole-assembly
    /// `score` Query (the plugin's session pose) keyed by plugin id; this
    /// scores a specific supplied composition keyed by `request_id` so the
    /// host attributes the result to one edit / checkpoint.
    #[cfg(not(target_arch = "wasm32"))]
    // Public API consumed by foldit-core; the owned `assembly` is the
    // cross-crate call contract. Changing it to `&[u8]` would force an
    // out-of-crate signature change, so the by-value arg is kept.
    #[allow(clippy::needless_pass_by_value)]
    pub fn score_composition(&mut self, assembly: Vec<u8>, request_id: u64) {
        if self.pending_composition_scores.contains_key(&request_id) {
            return; // a composition score is already outstanding for this id
        }
        let providers: Vec<String> = self
            .plugin_registry
            .query_providers("score")
            .iter()
            .map(|q| q.plugin_id.clone())
            .collect();
        let mut receivers = Vec::with_capacity(providers.len());
        for plugin_id in providers {
            let Some(&session) = self.plugin_sessions.get(&plugin_id) else {
                continue;
            };
            let Some(handle) = self.plugin_workers.get(&plugin_id) else {
                continue;
            };
            let (reply_tx, reply_rx) = mpsc::channel();
            let task = PluginTask::Query {
                session,
                query: String::from("score"),
                ctx: DispatchContext::default(),
                params: HashMap::new(),
                assembly: assembly.clone(),
                reply: reply_tx,
            };
            if handle.submit(task).is_ok() {
                receivers.push(reply_rx);
            }
        }
        if !receivers.is_empty() {
            let _ = self
                .pending_composition_scores
                .insert(request_id, receivers);
        }
    }

    /// Drain whatever composition-score replies have arrived since the last
    /// call. Non-blocking `try_recv`; a `request_id` whose providers have
    /// not all replied stays pending. Returns one `(request_id,
    /// ScoreReport)` per provider reply this call (the caller routes each to
    /// its target and merges by last-writer when a target has more than one
    /// provider). Errors and undecodable replies are logged and dropped; the
    /// provider's receiver clears either way, so the next assembly change
    /// re-fires it.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn poll_composition_scores(
        &mut self,
    ) -> Vec<(u64, proto::ScoreReport)> {
        let mut out: Vec<(u64, proto::ScoreReport)> = Vec::new();
        let mut done_ids: Vec<u64> = Vec::new();
        for (request_id, receivers) in &mut self.pending_composition_scores {
            receivers.retain(|rx| match rx.try_recv() {
                Ok(Ok(bytes)) => {
                    match <proto::ScoreReport as prost::Message>::decode(
                        &bytes[..],
                    ) {
                        Ok(report) => out.push((*request_id, report)),
                        Err(e) => log::warn!(
                            "[Orchestrator] poll_composition_scores: rid \
                             {request_id} returned undecodable ScoreReport: \
                             {e}"
                        ),
                    }
                    false // reply delivered; drop this receiver
                }
                Ok(Err(e)) => {
                    log::warn!(
                        "[Orchestrator] poll_composition_scores: rid \
                         {request_id} score failed: {e}"
                    );
                    false
                }
                Err(mpsc::TryRecvError::Empty) => true, // still in flight
                Err(mpsc::TryRecvError::Disconnected) => false, // worker gone
            });
            if receivers.is_empty() {
                done_ids.push(*request_id);
            }
        }
        for id in done_ids {
            let _ = self.pending_composition_scores.remove(&id);
        }
        out
    }

    /// Drain whatever async `score` replies have arrived since the last
    /// call. Non-blocking `try_recv`; a provider whose reply is not yet
    /// ready stays pending. Returns one decoded
    /// [`proto::ScoreReport`] per provider that replied this call. Errors
    /// and undecodable replies are logged and dropped; the provider's
    /// pending slot clears either way, so the next assembly change
    /// re-fires it.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn poll_score_results(
        &mut self,
    ) -> HashMap<String, proto::ScoreReport> {
        let mut out: HashMap<String, proto::ScoreReport> = HashMap::new();
        let mut done: Vec<String> = Vec::new();
        for (plugin_id, rx) in &self.pending_score_queries {
            match rx.try_recv() {
                Ok(Ok(bytes)) => {
                    match <proto::ScoreReport as prost::Message>::decode(
                        &bytes[..],
                    ) {
                        Ok(report) => {
                            let _ = out.insert(plugin_id.clone(), report);
                        }
                        Err(e) => log::warn!(
                            "[Orchestrator] poll_score_results: plugin \
                             {plugin_id} returned undecodable ScoreReport: {e}"
                        ),
                    }
                    done.push(plugin_id.clone());
                }
                Ok(Err(e)) => {
                    log::warn!(
                        "[Orchestrator] poll_score_results: plugin \
                         {plugin_id} score query failed: {e}"
                    );
                    done.push(plugin_id.clone());
                }
                Err(mpsc::TryRecvError::Empty) => {} // still in flight
                Err(mpsc::TryRecvError::Disconnected) => {
                    done.push(plugin_id.clone()); // worker gone; clear slot
                }
            }
        }
        for id in done {
            let _ = self.pending_score_queries.remove(&id);
        }
        out
    }
}
