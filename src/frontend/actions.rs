use serde::{Deserialize, Serialize};

/// Viewport input events forwarded from the frontend overlay
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum ViewportInput {
    PointerDown {
        x: f32,
        y: f32,
        button: u8,
    },
    PointerUp {
        x: f32,
        y: f32,
        button: u8,
    },
    PointerMove {
        x: f32,
        y: f32,
        dx: f32,
        dy: f32,
    },
    Scroll {
        delta: f32,
    },
    Key {
        code: String,
        pressed: bool,
    },
    Resize {
        width: u32,
        height: u32,
    },
}

/// Simple action identifiers (no parameters needed)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u32)]
pub enum ActionId {
    ToggleWiggle = 0,
    ToggleShake = 1,
    RunPrediction = 2,
    RunMPNN = 3,
    RunDiffusion = 4,
    ToggleViewMode = 5,
    ToggleBackboneQuality = 6,
    ToggleDesignedStructures = 7,
    CycleFocus = 8,
    RemoveStructure = 9,
    Cancel = 10,
    Undo = 11,
    Redo = 12,
}

/// Actions that carry additional parameters
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ParameterizedAction {
    CreateBand {
        res1: u32,
        atom1: String,
        res2: u32,
        atom2: String,
    },
    RemoveBand {
        band_id: u32,
    },
    SetViewOption {
        key: String,
        value: serde_json::Value,
    },
    LoadStructure {
        path: String,
    },
}
