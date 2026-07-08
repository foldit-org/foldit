//! Streaming-poll update shape for the unified plugin protocol.
//!
//! [`PluginUpdate`] (decoded `molex::Assembly`, keyed by `RequestId`) is the
//! shape the orchestrator drains per-frame. [`PollOutcome`] (raw assembly
//! bytes, as the worker poll returns them) is its pre-decode counterpart,
//! owned by `foldit-plugin-sdk` and re-exported here.

pub use foldit_plugin_sdk::PollOutcome;

use crate::proto::plugin::ScoreReport;

// PluginUpdate — the streaming-poll shape for the unified plugin protocol.
//
// Every plugin stream (Rosetta + ML) emits these; the orchestrator drains
// `plugin_update_rx` per-frame and routes them however the host wants.

/// Generic plugin stream update, keyed by `RequestId`. Mirrors
/// `proto::plugin::PollStreamResponse`.
#[derive(Debug, Clone)]
pub enum PluginUpdate {
    /// In-progress snapshot. `latest_assembly` is the working state at
    /// poll time; not authoritative until promoted by the orchestrator
    /// on a `Final` or `Cancelled` terminal. `progress` is 0.0..1.0 if
    /// the plugin tracks it; `stage` is a human-readable string.
    Pending {
        /// Stream id the update belongs to.
        request_id: u64,
        /// Working assembly snapshot, if the plugin emits one.
        latest_assembly: Option<molex::Assembly>,
        /// Progress fraction in `[0.0, 1.0]`, if the plugin tracks it.
        progress: Option<f32>,
        /// Human-readable stage label, if provided.
        stage: Option<String>,
        /// Warm score of `latest_assembly`, if the plugin scores it.
        score: Option<ScoreReport>,
    },
    /// Accepted intermediate the host commits into canonical state while
    /// the stream keeps running. Same payload as `Pending`, but the host
    /// commits `latest_assembly` rather than treating it as a discardable
    /// preview; unlike a terminal it does not end the op (more
    /// checkpoints or a terminal follow).
    Checkpoint {
        /// Stream id the update belongs to.
        request_id: u64,
        /// Working assembly snapshot, if the plugin emits one.
        latest_assembly: Option<molex::Assembly>,
        /// Progress fraction in `[0.0, 1.0]`, if the plugin tracks it.
        progress: Option<f32>,
        /// Human-readable stage label, if provided.
        stage: Option<String>,
        /// Warm score of `latest_assembly`, if the plugin scores it.
        score: Option<ScoreReport>,
    },
    /// Stream stopped at host request and returned a working pose. The
    /// host commits `assembly` to canonical state the same way it
    /// commits a `Final`. For open-ended streaming ops (wiggle, shake,
    /// repack/design loops) this is the only terminal the user ever
    /// sees; the distinct variant lets the host treat the "user asked
    /// it to stop" path as success without carving out a code-coded
    /// branch off the failure channel.
    Cancelled {
        /// Stream id the update belongs to.
        request_id: u64,
        /// Working assembly to promote into canonical state.
        assembly: molex::Assembly,
        /// Warm score of `assembly`, if the plugin scores it.
        score: Option<ScoreReport>,
    },
    /// Final result. `assembly` is the definitive output the orchestrator
    /// promotes into canonical state. `result` is op-specific opaque
    /// payload (per-design metadata, sequences, etc.).
    Final {
        /// Stream id the update belongs to.
        request_id: u64,
        /// Definitive assembly for the orchestrator to promote.
        assembly: molex::Assembly,
        /// Op-specific opaque payload (per-design metadata, sequences,
        /// etc.).
        result: Option<Vec<u8>>,
        /// Warm score of `assembly`, if the plugin scores it.
        score: Option<ScoreReport>,
    },
    /// Op failure. Reserved for spontaneous failures (watchdog
    /// eviction, mid-action exception, transport drop). User-initiated
    /// cancels ride `Cancelled` instead. The host's terminal handler
    /// for this variant aborts the tentative and releases the lock; it
    /// does NOT commit.
    Error {
        /// Stream id the update belongs to.
        request_id: u64,
        /// Failure message.
        message: String,
    },
}
