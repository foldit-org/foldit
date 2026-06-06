//! GUI projection state for the third `SessionUpdate` consumer.
//!
//! `GuiProjector` is the state half of the GUI consumer: a single
//! history-version debounce cursor. Its `consume` method — the projection
//! that mirrors `Session` / `VisoEngine` / `RunnerClient` state into
//! `FrontendState` — lives in [`crate::app`] next to the projection
//! helpers it calls (`head_score`, `project_history`, `bubble_to_payload`).
//! The scoring-mode display policy, tutorial-bubble flow, and puzzle
//! objective live on [`crate::session::Session`] and reach the consumer
//! through their own `SessionUpdate` variants.
//!
//! Unlike [`crate::render_projector::RenderProjector`] and the plugin
//! broadcaster, the GUI consumer also reads the History cursor below: the
//! history channel picks up score-driven `live_version` bumps through the
//! cursor's debounce rather than reprojecting the whole panel each tick.

use web_time::Instant;

/// State for the GUI consumer (see `GuiProjector::consume` in
/// [`crate::app`]): the history-version debounce cursor.
pub(crate) struct GuiProjector {
    /// Debounce cursor for the history channel (topology + live).
    pub(crate) history_sync: HistorySyncCursor,
}

impl GuiProjector {
    pub(crate) fn new() -> Self {
        Self {
            history_sync: HistorySyncCursor {
                last_topology: None,
                last_live: None,
                last_live_push_at: None,
            },
        }
    }
}

/// Tracks the last history versions pushed to the frontend so the GUI
/// consumer can debounce/skip redundant reprojections.
pub(crate) struct HistorySyncCursor {
    /// Last `History::topology_version()` pushed. `None` forces an
    /// initial push (G5: no `u64::MAX` sentinel).
    pub(crate) last_topology: Option<u64>,
    /// Last `History::live_version()` pushed; mid-action score updates only.
    pub(crate) last_live: Option<u64>,
    /// Wall-clock of the last live push. Gates the 50ms (20Hz) debounce.
    pub(crate) last_live_push_at: Option<Instant>,
}
