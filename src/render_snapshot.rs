//! Triple-buffered render snapshot pattern.
//!
//! Decouples the render path from live App state so that expensive
//! Rosetta/ML processing doesn't stall a render frame. The main thread
//! writes a snapshot after mutations, and the render path reads the latest.

use foldit_conv::secondary_structure::SSType;
use foldit_render::animation::AnimationAction;
use foldit_render::band_renderer::BandRenderInfo;
use foldit_render::pull_renderer::PullRenderInfo;
use glam::Vec3;

/// Snapshot of all render-relevant state from App.
///
/// Written by the main thread after processing mutations.
/// Read by the render path at frame start.
#[derive(Clone)]
pub struct RenderSnapshot {
    /// Pending animation action (scene geometry changed)
    pub pending_action: Option<AnimationAction>,

    /// Aggregated scene data — only present when pending_action is Some
    pub backbone_chains: Vec<Vec<Vec3>>,
    pub sidechain_positions: Vec<Vec3>,
    pub sidechain_hydrophobicity: Vec<bool>,
    pub sidechain_residue_indices: Vec<u32>,
    pub sidechain_atom_names: Vec<String>,
    pub sidechain_bonds: Vec<(u32, u32)>,
    pub backbone_sidechain_bonds: Vec<(Vec3, u32)>,
    pub all_positions: Vec<Vec3>,

    /// Pre-computed SS types from ss_override (if any structure has one)
    pub ss_types: Option<Vec<SSType>>,

    /// Band visualization state
    pub bands: Vec<BandRenderInfo>,
    pub bands_dirty: bool,

    /// Pull visualization state
    pub pull: Option<PullRenderInfo>,
    pub pull_dirty: bool,

    /// Whether to fit camera to all positions (on structure load)
    pub fit_camera: bool,

    /// Frame counter (monotonically increasing, used to detect staleness)
    pub generation: u64,
}

impl Default for RenderSnapshot {
    fn default() -> Self {
        Self {
            pending_action: None,
            backbone_chains: Vec::new(),
            sidechain_positions: Vec::new(),
            sidechain_hydrophobicity: Vec::new(),
            sidechain_residue_indices: Vec::new(),
            sidechain_atom_names: Vec::new(),
            sidechain_bonds: Vec::new(),
            backbone_sidechain_bonds: Vec::new(),
            all_positions: Vec::new(),
            ss_types: None,
            bands: Vec::new(),
            bands_dirty: false,
            pull: None,
            pull_dirty: false,
            fit_camera: false,
            generation: 0,
        }
    }
}

/// Writer side — owned by the main thread.
/// Coalesces mutations via a render_dirty flag before writing.
pub struct SnapshotWriter {
    writer: triple_buffer::Input<RenderSnapshot>,
    render_dirty: bool,
    generation: u64,
}

/// Reader side — owned by the render path.
pub struct SnapshotReader {
    reader: triple_buffer::Output<RenderSnapshot>,
    last_generation: u64,
}

/// Create a new triple-buffered snapshot pair.
pub fn create_snapshot_buffer() -> (SnapshotWriter, SnapshotReader) {
    let (input, output) = triple_buffer::triple_buffer(&RenderSnapshot::default());
    (
        SnapshotWriter {
            writer: input,
            render_dirty: false,
            generation: 0,
        },
        SnapshotReader {
            reader: output,
            last_generation: 0,
        },
    )
}

impl SnapshotWriter {
    /// Mark that render-relevant state has changed.
    /// The actual snapshot write is deferred to `flush()`.
    pub fn mark_dirty(&mut self) {
        self.render_dirty = true;
    }

    /// Check if dirty (mutations pending).
    pub fn is_dirty(&self) -> bool {
        self.render_dirty
    }

    /// Write a snapshot if dirty. Returns true if a write occurred.
    pub fn flush(&mut self, snapshot: RenderSnapshot) -> bool {
        if !self.render_dirty {
            return false;
        }
        self.render_dirty = false;
        self.generation += 1;
        let mut snap = snapshot;
        snap.generation = self.generation;
        self.writer.write(snap);
        true
    }

    /// Force-write a snapshot regardless of dirty flag.
    pub fn force_write(&mut self, snapshot: RenderSnapshot) {
        self.generation += 1;
        let mut snap = snapshot;
        snap.generation = self.generation;
        self.writer.write(snap);
        self.render_dirty = false;
    }
}

impl SnapshotReader {
    /// Read the latest snapshot. Returns Some if a new snapshot is available
    /// since the last read.
    pub fn try_read(&mut self) -> Option<&RenderSnapshot> {
        let snap = self.reader.read();
        if snap.generation > self.last_generation {
            self.last_generation = snap.generation;
            Some(snap)
        } else {
            None
        }
    }
}
