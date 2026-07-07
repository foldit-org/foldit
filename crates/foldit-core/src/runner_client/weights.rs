//! Model-weights readiness paths.
//!
//! Fire the fan-out `weights_status` query at every provider and drain the
//! per-plugin replies, decoding each plugin's opaque `{ready,present,missing}`
//! JSON into the host-owned [`WeightsState`]. The catalog projection reads the
//! resulting map to swap a not-ready plugin's buttons for a download button.

#[cfg(not(target_arch = "wasm32"))]
use super::{RunnerClient, WeightsState};

/// The plugin-side `weights_status` reply: whether the model weights are
/// present on disk, plus the present/missing asset lists (unused here but
/// carried so the decode tolerates the full payload).
#[cfg(not(target_arch = "wasm32"))]
#[derive(serde::Deserialize)]
struct WeightsStatusReply {
    ready: bool,
    #[serde(default)]
    #[allow(dead_code)]
    present: Vec<String>,
    #[serde(default)]
    #[allow(dead_code)]
    missing: Vec<String>,
}

#[cfg(not(target_arch = "wasm32"))]
impl RunnerClient {
    /// Fire a non-blocking `weights_status` query at every provider with none
    /// already in flight. Replies land on stored receivers drained by
    /// [`Self::poll_weights_status`]. No-op when no orchestrator exists.
    pub(crate) fn request_weights_status(&mut self) {
        if let Some(orch) = self.orchestrator.as_mut() {
            orch.request_weights_status();
        }
    }

    /// Drain whatever async `weights_status` replies have arrived, decode each
    /// plugin's JSON reply, and record its readiness in the weights map.
    /// Non-blocking; a plugin whose JSON fails to decode is logged and skipped
    /// rather than crashing the poll. Returns `true` when any plugin's state
    /// changed this call, so the caller can re-project the action catalog to
    /// reflect the button swap.
    pub(crate) fn poll_weights_status(&mut self) -> bool {
        let Some(orch) = self.orchestrator.as_mut() else {
            return false;
        };
        let replies = orch.poll_weights_status();
        let mut changed = false;
        for (plugin_id, bytes) in replies {
            let reply: WeightsStatusReply = match serde_json::from_slice(&bytes) {
                Ok(r) => r,
                Err(e) => {
                    log::warn!(
                        "[RunnerClient] weights_status decode failed for '{plugin_id}': {e}"
                    );
                    continue;
                }
            };
            let state = if reply.ready {
                WeightsState::Ready
            } else {
                WeightsState::Missing
            };
            let changed_here = self.weights.get(&plugin_id) != Some(&state);
            self.weights.insert(plugin_id, state);
            if changed_here {
                changed = true;
            }
        }
        changed
    }
}
