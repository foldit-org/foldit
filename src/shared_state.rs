//! Per-entity triple buffers: lock-free coord buffering between backends
//! and the render loop.
//!
//! The Orchestrator (in foldit-runner) owns the writer side of each triple buffer.
//! The main crate's frame loop calls `drain_updates()` once per frame to read the
//! latest values and push them to viso's engine. Intermediate updates between
//! frames are automatically discarded (latest-wins semantics).
//!
//! Entity metadata (origin, role, reference CA, designed sequences) has moved
//! to [`crate::entity_store::EntityStore`].

use std::collections::HashMap;
use triple_buffer::Output;
use foldit_runner::orchestrator::BackendUpdate;
use viso::Focus;

/// Reader side of a per-entity triple buffer.
struct EntityBuffer {
    reader: Output<Option<BackendUpdate>>,
}

/// Shared state: owns all per-entity triple buffer readers.
/// The frame loop calls `drain_updates()` each frame.
pub struct SharedState {
    buffers: HashMap<u32, EntityBuffer>,
}

impl SharedState {
    pub fn new() -> Self {
        Self {
            buffers: HashMap::new(),
        }
    }

    // ── Triple buffer management ──

    /// Store a triple buffer reader for an entity.
    /// The writer side is held by the Orchestrator.
    pub fn register_entity(&mut self, id: u32, reader: Output<Option<BackendUpdate>>) {
        self.buffers.insert(id, EntityBuffer { reader });
    }

    /// Remove an entity's buffer.
    pub fn unregister_entity(&mut self, id: u32) {
        self.buffers.remove(&id);
    }

    /// Read latest updates from all entity buffers.
    /// Returns only entities that have new data since last read.
    pub fn drain_updates(&mut self) -> Vec<(u32, BackendUpdate)> {
        let mut updates = Vec::new();
        for (&id, buf) in &mut self.buffers {
            if buf.reader.update() {
                if let Some(update) = buf.reader.output_buffer_mut().take() {
                    updates.push((id, update));
                }
            }
        }
        updates
    }

    // ── Static focus helpers ──

    /// Whether focus is session-wide.
    pub fn is_session_mode(focus: &Focus) -> bool {
        matches!(focus, Focus::Session)
    }

    /// Which entity a backend operation should target.
    /// Returns `Some(id)` when focused on a specific entity.
    pub fn operation_target(focus: &Focus) -> Option<u32> {
        match focus {
            Focus::Session => None,
            Focus::Entity(id) => Some(id.raw()),
        }
    }

    /// Entity to lock for backend operations. Falls back to loaded entity.
    pub fn lock_target(focus: &Focus, loaded: Option<u32>) -> Option<u32> {
        match focus {
            Focus::Session => loaded,
            Focus::Entity(id) => Some(id.raw()),
        }
    }
}
