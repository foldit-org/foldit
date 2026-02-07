pub mod actions;
pub mod state;

use bitflags::bitflags;
use serde::Serialize;

pub use actions::{ActionId, ParameterizedAction, ViewportInput};
pub use state::*;

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct DirtyFlags: u32 {
        const SCORE     = 0b0000001;
        const SELECTION = 0b0000010;
        const PANELS    = 0b0000100;
        const VIEW      = 0b0001000;
        const UI        = 0b0010000;
        const LOADING   = 0b0100000;
        const ACTIONS   = 0b1000000;
    }
}

/// State sections that get pushed to the frontend when dirty.
///
/// Only dirty sections are serialized and emitted via Tauri events.
/// The frontend merges partial updates into its local store.
#[derive(Debug, Clone, Serialize)]
pub struct FrontendState {
    pub score: ScoreSection,
    pub selection: SelectionSection,
    pub view: ViewSection,
    pub panels: PanelSection,
    pub ui: UISection,
    pub actions: ActionsSection,
    pub loading: LoadingSection,
    #[serde(skip)]
    dirty: DirtyFlags,
}

impl Default for FrontendState {
    fn default() -> Self {
        Self {
            score: ScoreSection::default(),
            selection: SelectionSection::default(),
            view: ViewSection::default(),
            panels: PanelSection::default(),
            ui: UISection::default(),
            actions: ActionsSection::default(),
            loading: LoadingSection::default(),
            dirty: DirtyFlags::empty(),
        }
    }
}

impl FrontendState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set all sections as dirty (used after initial hydration)
    pub fn mark_all_dirty(&mut self) {
        self.dirty = DirtyFlags::all();
    }

    /// Mark specific sections as dirty
    pub fn mark_dirty(&mut self, flags: DirtyFlags) {
        self.dirty |= flags;
    }

    /// Take and clear dirty flags, returning what was dirty
    pub fn take_dirty(&mut self) -> DirtyFlags {
        let flags = self.dirty;
        self.dirty = DirtyFlags::empty();
        flags
    }

    /// Check if any section is dirty
    pub fn is_dirty(&self) -> bool {
        !self.dirty.is_empty()
    }

    /// Score section accessors
    pub fn score(&self) -> &ScoreSection {
        &self.score
    }

    pub fn set_score(&mut self, value: f64, invalid: bool) {
        self.score.value = value;
        self.score.invalid = invalid;
        self.dirty |= DirtyFlags::SCORE;
    }

    /// Selection section accessors
    pub fn selection(&self) -> &SelectionSection {
        &self.selection
    }

    pub fn set_selection(&mut self, residues: Vec<bool>) {
        self.selection.residues = residues;
        self.dirty |= DirtyFlags::SELECTION;
    }

    /// View section accessors
    pub fn view(&self) -> &ViewSection {
        &self.view
    }

    pub fn set_view_mode(&mut self, mode: state::ViewMode) {
        self.view.mode = mode;
        self.dirty |= DirtyFlags::VIEW;
    }

    /// UI section accessors
    pub fn ui(&self) -> &UISection {
        &self.ui
    }

    pub fn set_fps(&mut self, fps: f32) {
        self.ui.fps = fps;
        self.dirty |= DirtyFlags::UI;
    }

    pub fn set_text_bubble(&mut self, text: Option<String>) {
        self.ui.text_bubble = text;
        self.dirty |= DirtyFlags::UI;
    }

    /// Loading section accessors
    pub fn loading(&self) -> &LoadingSection {
        &self.loading
    }

    pub fn set_loading_progress(&mut self, progress: Option<f32>) {
        self.loading.progress = progress;
        self.dirty |= DirtyFlags::LOADING;
    }

    pub fn set_puzzle_loaded(&mut self, loaded: bool) {
        self.loading.puzzle_loaded = loaded;
        self.dirty |= DirtyFlags::LOADING;
    }

    /// Actions section accessors
    pub fn actions(&self) -> &ActionsSection {
        &self.actions
    }

    pub fn set_actions(&mut self, available: Vec<state::ActionInfo>) {
        self.actions.available = available;
        self.dirty |= DirtyFlags::ACTIONS;
    }
}
