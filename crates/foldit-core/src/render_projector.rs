//! App-owned viso projection.
//!
//! Consumes the [`SceneChange`] spine and rebuilds the head `Assembly`
//! once per drain to publish to viso. Owns the publish-generation
//! counter and the last-published id list, the latter only to pick
//! between `set_assembly` (steady-state coord update) and
//! `replace_assembly` (topology swap: tears down per-entity scene-local
//! state). Both stamp a fresh `publish_seq` so viso's `poll_assembly`
//! gate sees a different number on every publish.

use crate::document::{Document, SceneChange};

/// App-owned viso projector. Holds the monotonic publish counter that
/// every published `Assembly` is stamped with, plus the entity-id list
/// of the last published assembly so we can detect topology change and
/// route to `replace_assembly` accordingly. The seq counter is
/// **deliberately** not reset on `Document::reset`: a fresh post-reset
/// publish still advances it, and viso never sees the generation go
/// backwards.
pub(crate) struct RenderProjector {
    /// Monotonic counter stamped onto every published `Assembly`.
    /// Incremented on every `project` that actually publishes. Without
    /// a fresh generation per publish, viso's `poll_assembly` gate
    /// would skip the second-and-subsequent publishes (a freshly built
    /// `Assembly` always starts at generation 0).
    publish_seq: u64,
    /// Entity ids of the last published assembly, in canonical order.
    /// Compared against the next drain's id list to choose between
    /// `set_assembly` (same topology, only coords differ) and
    /// `replace_assembly` (any add/remove/reorder). Mirrors the
    /// broadcaster's snapshot-diff for Full vs Delta.
    last_published_ids: Vec<molex::entity::molecule::id::EntityId>,
}

impl RenderProjector {
    pub fn new() -> Self {
        Self {
            publish_seq: 0,
            last_published_ids: Vec::new(),
        }
    }

    /// Consume a drained `SceneChange` batch and publish the current
    /// head assembly to viso. No-ops when the batch is empty (no
    /// publishes mean no wasted assembly builds or generation bumps).
    /// Picks `replace_assembly` when the entity id set / order has
    /// shifted since the last publish; `set_assembly` otherwise.
    pub fn project(
        &mut self,
        changes: &[SceneChange],
        doc: &Document,
        engine: &mut viso::VisoEngine,
    ) {
        if changes.is_empty() {
            return;
        }
        let mut asm = doc.head_assembly();
        let new_ids: Vec<molex::entity::molecule::id::EntityId> =
            asm.entities().iter().map(|e| e.id()).collect();
        let topology_changed = new_ids != self.last_published_ids;

        self.publish_seq = self.publish_seq.saturating_add(1);
        asm.set_generation(self.publish_seq);
        let asm = std::sync::Arc::new(asm);

        if topology_changed {
            engine.replace_assembly(asm);
        } else {
            engine.set_assembly(asm);
        }
        self.last_published_ids = new_ids;
    }
}

/// Build a focus description from focus + entity names. Was
/// `Document::focus_description`; moved here to keep `Document`
/// viso-free. The `Session` arm reports `doc.count()` (live committed +
/// preview membership) rather than the metadata side table, which is
/// never GC'd and so over-reports the live entity count.
pub(crate) fn focus_description(doc: &Document, focus: &viso::Focus) -> String {
    match focus {
        viso::Focus::Session => {
            let count = doc.count();
            format!("Session ({count} entities)")
        }
        viso::Focus::Entity(id) => doc
            .metadata(*id)
            .map(|m| m.name.clone())
            .unwrap_or_else(|| format!("Entity {}", id.raw())),
    }
}
