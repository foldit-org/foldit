// `thiserror` 1.x and 2.x both appear in the dependency tree via transitive
// deps we do not control; the duplication is not resolvable from this crate.
#![allow(
    clippy::multiple_crate_versions,
    reason = "duplicate thiserror versions come from transitive deps, not controllable here"
)]

pub mod actions;
pub mod bridge;
pub mod state;
pub mod wire;

use bitflags::bitflags;
use serde::Serialize;

pub use actions::{AppCommand, OpDispatch, ViewportInput};
pub use bridge::{Dispatcher, IpcMessage, RequestKind, RequestResult, Transport};
pub use state::*;
pub use wire::{
    CheckpointId, CheckpointInfo, CheckpointKindTag, EntitySnapshotId, FilterStatus,
    HistoryCommand, HistoryLiveUpdate, HistorySection, WireId,
};

bitflags! {
    #[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
    pub struct DirtyFlags: u32 {
        const SCORE        = 0b000_0000_0001;
        const SELECTION    = 0b000_0000_0010;
        const VIEW         = 0b000_0000_0100;
        const UI           = 0b000_0000_1000;
        const LOADING      = 0b000_0001_0000;
        const ACTIONS      = 0b000_0010_0000;
        const SCENE        = 0b000_0100_0000;
        const PUZZLE       = 0b000_1000_0000;
        const APP_STATE    = 0b001_0000_0000;
        const HISTORY      = 0b010_0000_0000;
        /// Tentative-score patch (`HistoryLiveUpdate`); does NOT
        /// reproject the full graph.
        const HISTORY_LIVE = 0b100_0000_0000;
        /// Active tutorial bubble changed (cursor moved, or sequence
        /// cleared). Triggers a re-push of `ui.text_bubble` without
        /// touching the rest of the UI section.
        const TEXT_BUBBLE  = 0b1000_0000_0000;
        /// Open segment-info target changed, or a score update refreshed
        /// the open segment's energies.
        const SEGMENT      = 0b1_0000_0000_0000;
        /// Panel open/closed set or a panel position changed.
        const PANELS       = 0b10_0000_0000_0000;
        /// Puzzle high-score progress changed (a new best recorded, or
        /// progress cleared).
        const PROGRESS     = 0b100_0000_0000_0000;
    }
}

/// State sections that get pushed to the GUI when dirty.
///
/// Only dirty sections are serialized and emitted via Tauri events.
/// The GUI merges partial updates into its local store.
#[derive(Debug, Clone, Serialize)]
pub struct FrontendState {
    /// Top-level lifecycle phase. Primary gate for what the GUI renders at the
    /// root level. Drives the `LoadingScreen` → game UI transition; backend
    /// advances this through the startup phases and flips it to `InSession`
    /// once the first score lands.
    pub app_state: AppPhase,
    pub score: ScoreSection,
    pub puzzle: PuzzleSection,
    pub selection: SelectionSection,
    pub view: ViewSection,
    pub ui: UISection,
    pub actions: ActionsSection,
    pub loading: LoadingSection,
    pub scene: SceneSection,
    pub history: HistorySection,
    /// Small payload pushed alongside (or instead of) `history` when
    /// only the running tentative's score / label changed. Set with
    /// `set_history_live`; cleared after the next push by the
    /// `take_dirty` cycle. Frontend patches the matching checkpoint in
    /// `state.history.checkpoints` rather than re-rendering.
    pub history_live: Option<HistoryLiveUpdate>,
    /// Per-residue segment-info panel payload. `None` when no segment is
    /// open. Set by the GUI projection's segment arm.
    pub segment_info: Option<SegmentInfo>,
    /// Backend-authoritative panel open/closed set and per-panel
    /// positions. Always present; empty when no panels are open and none
    /// have been moved. Set by the GUI projection's panels arm.
    pub panels: PanelsSection,
    /// Backend-authoritative puzzle high-score progress. Always present;
    /// empty until the player scores on a puzzle. Set by the GUI
    /// projection's progress arm.
    pub progress: ProgressSection,
    #[serde(skip)]
    dirty: DirtyFlags,
}

impl Default for FrontendState {
    fn default() -> Self {
        Self {
            app_state: AppPhase::Initializing,
            score: ScoreSection::default(),
            puzzle: PuzzleSection::default(),
            selection: SelectionSection::default(),
            view: ViewSection::default(),
            ui: UISection::default(),
            actions: ActionsSection::default(),
            loading: LoadingSection::default(),
            scene: SceneSection::default(),
            history: HistorySection::default(),
            history_live: None,
            segment_info: None,
            panels: PanelsSection::default(),
            progress: ProgressSection::default(),
            dirty: DirtyFlags::empty(),
        }
    }
}

impl FrontendState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Top-level state accessors. Backend advances the phase through startup
    /// and flips it to `InSession` once the first score lands.
    #[must_use]
    pub const fn app_state(&self) -> AppPhase {
        self.app_state
    }

    pub fn set_app_state(&mut self, state: AppPhase) {
        if self.app_state != state {
            self.app_state = state;
            self.dirty |= DirtyFlags::APP_STATE;
        }
    }

    /// Set all sections as dirty (used after initial hydration)
    pub const fn mark_all_dirty(&mut self) {
        self.dirty = DirtyFlags::all();
    }

    /// Mark specific sections as dirty
    pub fn mark_dirty(&mut self, flags: DirtyFlags) {
        self.dirty |= flags;
    }

    /// Take and clear dirty flags, returning what was dirty
    pub const fn take_dirty(&mut self) -> DirtyFlags {
        let flags = self.dirty;
        self.dirty = DirtyFlags::empty();
        flags
    }

    pub fn set_score(&mut self, value: f64, invalid: bool) {
        self.score.value = value;
        self.score.invalid = invalid;
        self.dirty |= DirtyFlags::SCORE;
    }

    pub fn set_score_title(&mut self, title: String) {
        self.score.title = title;
        self.dirty |= DirtyFlags::SCORE;
    }

    /// Replace the segment-info payload (or clear it with `None`). Marks
    /// `SEGMENT` dirty unconditionally; the projection only calls this
    /// when the open target changed or its energies were refreshed.
    pub fn set_segment_info(&mut self, info: Option<SegmentInfo>) {
        self.segment_info = info;
        self.dirty |= DirtyFlags::SEGMENT;
    }

    /// Replace the panels section (open set + positions). Marks `PANELS`
    /// dirty unconditionally; the projection only calls this when the
    /// backend panel state actually changed.
    pub fn set_panels(&mut self, panels: PanelsSection) {
        self.panels = panels;
        self.dirty |= DirtyFlags::PANELS;
    }

    /// Replace the progress section. Marks `PROGRESS` dirty
    /// unconditionally; the projection only calls this when the backend
    /// progress map actually changed.
    pub fn set_progress(&mut self, progress: ProgressSection) {
        self.progress = progress;
        self.dirty |= DirtyFlags::PROGRESS;
    }

    /// Use `set_puzzle_game` for tutorial/campaign puzzles (with
    /// target/starting scores from the toml) and `set_puzzle_scientist`
    /// for free-form / CLI loads.
    pub fn set_puzzle_game(
        &mut self,
        puzzle_id: u32,
        title: String,
        starting_score: f64,
        target_score: f64,
    ) {
        self.puzzle = PuzzleSection {
            mode: ScoringMode::Game,
            puzzle_id,
            title,
            starting_score,
            target_score,
            complete: false,
        };
        self.dirty |= DirtyFlags::PUZZLE;
    }

    pub fn set_puzzle_scientist(&mut self, title: String) {
        self.puzzle = PuzzleSection {
            mode: ScoringMode::Scientist,
            puzzle_id: 0,
            title,
            starting_score: 0.0,
            target_score: 0.0,
            complete: false,
        };
        self.dirty |= DirtyFlags::PUZZLE;
    }

    /// Latch the puzzle as complete. Idempotent — only marks dirty on the
    /// false→true transition so the frontend sees a single victory event.
    pub fn mark_puzzle_complete(&mut self) {
        if !self.puzzle.complete {
            self.puzzle.complete = true;
            self.dirty |= DirtyFlags::PUZZLE;
        }
    }

    pub fn set_fps(&mut self, fps: f32) {
        self.ui.fps = fps;
        self.dirty |= DirtyFlags::UI;
    }

    /// Replace the active text bubble (or clear with `None`). Marks UI
    /// dirty unconditionally so the frontend sees explicit clears even
    /// when re-setting an equivalent payload.
    pub fn set_text_bubble(&mut self, bubble: Option<TextBubblePayload>) {
        self.ui.text_bubble = bubble;
        self.dirty |= DirtyFlags::UI;
    }

    pub fn set_log(&mut self, log: String) {
        self.ui.log = log;
        self.dirty |= DirtyFlags::UI;
    }

    pub fn set_loading_progress(&mut self, progress: Option<f32>) {
        self.loading.progress = progress;
        self.dirty |= DirtyFlags::LOADING;
    }

    pub fn set_puzzle_loaded(&mut self, loaded: bool) {
        self.loading.puzzle_loaded = loaded;
        self.dirty |= DirtyFlags::LOADING;
    }

    pub fn set_actions(
        &mut self,
        available: Vec<state::ActionInfo>,
        groups: Vec<state::PluginGroupInfo>,
    ) {
        self.actions.available = available;
        self.actions.groups = groups;
        self.dirty |= DirtyFlags::ACTIONS;
    }

    /// Replace the per-entity selection list. Marks `SELECTION` dirty
    /// unconditionally — callers only invoke this when [`App::selection`]
    /// has actually changed.
    pub fn set_selection(&mut self, entries: Vec<state::EntitySelection>) {
        self.selection.entries = entries;
        self.dirty |= DirtyFlags::SELECTION;
    }

    pub fn set_scene_entities(&mut self, entities: Vec<state::SceneEntityInfo>) {
        if self.scene.entities != entities {
            self.scene.entities = entities;
            self.dirty |= DirtyFlags::SCENE;
        }
    }

    /// Set the currently-focused entity (mirrors viso's `Focus`). Pass
    /// `None` for whole-session focus.
    pub fn set_focused_entity(&mut self, focused: Option<u32>) {
        if self.scene.focused_entity != focused {
            self.scene.focused_entity = focused;
            self.dirty |= DirtyFlags::SCENE;
        }
    }

    /// History section accessors.
    #[must_use]
    pub const fn history(&self) -> &HistorySection {
        &self.history
    }

    /// Replace the history section. Marks dirty unconditionally (caller
    /// only invokes this when `History::topology_version()` has bumped).
    pub fn set_history(&mut self, history: HistorySection) {
        self.history = history;
        self.dirty |= DirtyFlags::HISTORY;
    }

    /// Stage a live tentative-score patch. Marks `HISTORY_LIVE` dirty
    /// only — does NOT mark `HISTORY` (no full reproject). Callers
    /// invoke this when only `History::live_version()` ticked. Frontend
    /// patches the matching checkpoint in place.
    pub fn set_history_live(&mut self, update: HistoryLiveUpdate) {
        self.history_live = Some(update);
        self.dirty |= DirtyFlags::HISTORY_LIVE;
    }
}
