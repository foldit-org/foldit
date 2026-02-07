use serde::{Deserialize, Serialize};

/// Current score and validity state
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScoreSection {
    pub value: f64,
    pub invalid: bool,
}

impl Default for ScoreSection {
    fn default() -> Self {
        Self {
            value: 0.0,
            invalid: true,
        }
    }
}

/// Per-residue selection state
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SelectionSection {
    pub residues: Vec<bool>,
}

impl Default for SelectionSection {
    fn default() -> Self {
        Self {
            residues: Vec::new(),
        }
    }
}

/// Rendering view mode
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ViewMode {
    Tube,
    Ribbon,
}

impl Default for ViewMode {
    fn default() -> Self {
        ViewMode::Ribbon
    }
}

/// View display options
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ViewOptions {
    pub show_sidechains: bool,
    pub show_backbone_quality: bool,
    pub show_designed_structures: bool,
}

impl Default for ViewOptions {
    fn default() -> Self {
        Self {
            show_sidechains: true,
            show_backbone_quality: false,
            show_designed_structures: true,
        }
    }
}

/// Current view state
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ViewSection {
    pub mode: ViewMode,
    pub options: ViewOptions,
}

impl Default for ViewSection {
    fn default() -> Self {
        Self {
            mode: ViewMode::default(),
            options: ViewOptions::default(),
        }
    }
}

/// Panel data sections for the UI
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PanelSection {
    pub rama_points: Vec<(f64, f64)>,
    pub alignment_info: Option<String>,
}

impl Default for PanelSection {
    fn default() -> Self {
        Self {
            rama_points: Vec::new(),
            alignment_info: None,
        }
    }
}

/// Transient UI state pushed from backend
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UISection {
    pub text_bubble: Option<String>,
    pub visible_panels: Vec<String>,
    pub fps: f32,
}

impl Default for UISection {
    fn default() -> Self {
        Self {
            text_bubble: None,
            visible_panels: Vec::new(),
            fps: 0.0,
        }
    }
}

/// Information about an available action
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActionInfo {
    pub id: u32,
    pub name: String,
    pub enabled: bool,
    pub active: bool,
}

/// Available actions and their current state
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActionsSection {
    pub available: Vec<ActionInfo>,
}

impl Default for ActionsSection {
    fn default() -> Self {
        Self {
            available: Vec::new(),
        }
    }
}

/// Loading/progress state
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LoadingSection {
    pub progress: Option<f32>,
    pub puzzle_loaded: bool,
}

impl Default for LoadingSection {
    fn default() -> Self {
        Self {
            progress: None,
            puzzle_loaded: false,
        }
    }
}
