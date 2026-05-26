//! App-owned GUI projection state, cursor-driven via [`HistorySyncCursor`].
//!
//! Holds the three GUI-facing fields that `populate_frontend` reads each
//! frame: the history-version debounce cursor, the scoring-mode display
//! policy, and the tutorial-bubble flow (bubble vector + cursor). Was
//! split across `App::history_sync` and `PuzzleSession`'s GUI half
//! before RX9 — the *puzzle objective* (id/title/scores) stays on
//! [`crate::app::PuzzleSession`] for now; RX14 finishes that split.
//!
//! Unlike [`crate::render_projector::RenderProjector`] and the
//! plugin broadcaster, this projector is **not** spine-driven: it picks
//! up score changes through the cursor's `live_version` bump rather
//! than via `take_scene_changes`, because scores deliberately are not
//! a `SceneChange` event (see `docs/foldit_core_state.md` on score
//! propagation). The host already calls `populate_frontend` per frame.

use web_time::Instant;

/// App-owned GUI projection state read by `populate_frontend`.
pub(crate) struct GuiProjector {
    /// Debounce cursor for the history channel (topology + live).
    pub(crate) history_sync: HistorySyncCursor,
    /// Which score representation (raw Rosetta vs. foldit-game) reaches
    /// the GUI. Defaults to `Scientist` on CLI bootstrap; flipped to
    /// `Game` on a campaign/intro `LoadPuzzle`.
    pub(crate) scoring_mode: foldit_gui::state::ScoringMode,
    /// Tutorial bubbles parsed from the active puzzle's TOML
    /// `[[sequence]]`. Empty for scientist-mode loads.
    pub(crate) bubbles: Vec<crate::puzzle::Bubble>,
    /// Index into `bubbles` for the currently-displayed bubble. Reset
    /// to 0 on every load. When `>= bubbles.len()` the sequence is
    /// exhausted and no bubble is shown.
    pub(crate) current_bubble: usize,
}

impl GuiProjector {
    pub(crate) fn new() -> Self {
        Self {
            history_sync: HistorySyncCursor {
                last_topology: None,
                last_live: None,
                last_live_push_at: None,
            },
            // CLI bootstrap defaults to scientist; `LoadPuzzle` flips
            // to Game when a campaign/intro puzzle loads.
            scoring_mode: foldit_gui::state::ScoringMode::Scientist,
            bubbles: Vec::new(),
            current_bubble: 0,
        }
    }
}

/// Tracks the last history versions pushed to the frontend so
/// `populate_frontend` can debounce/skip redundant reprojections.
pub(crate) struct HistorySyncCursor {
    /// Last `History::topology_version()` pushed. `None` forces an
    /// initial push (G5: no `u64::MAX` sentinel).
    pub(crate) last_topology: Option<u64>,
    /// Last `History::live_version()` pushed; mid-action score updates only.
    pub(crate) last_live: Option<u64>,
    /// Wall-clock of the last live push. Gates the 50ms (20Hz) debounce.
    pub(crate) last_live_push_at: Option<Instant>,
}
