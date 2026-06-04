//! App-owned viso projection.
//!
//! Consumes the [`SessionUpdate`] spine and rebuilds the head `Assembly`
//! once per drain to publish to viso. Owns the publish-generation
//! counter and the last-published id list, the latter only to pick
//! between `set_assembly` (steady-state coord update) and
//! `replace_assembly` (topology swap: tears down per-entity scene-local
//! state). Both stamp a fresh `publish_seq` so viso's `poll_assembly`
//! gate sees a different number on every publish.

use crate::session::{Session, SessionUpdate};

/// App-owned viso projector. Holds the monotonic publish counter that
/// every published `Assembly` is stamped with, plus the entity-id list
/// of the last published assembly so we can detect topology change and
/// route to `replace_assembly` accordingly. The seq counter is
/// **deliberately** not reset on `Session::reset`: a fresh post-reset
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

    /// Consume a drained `SessionUpdate` batch and publish the current
    /// head assembly to viso. No-ops when the batch is empty (no
    /// publishes mean no wasted assembly builds or generation bumps).
    /// Picks `replace_assembly` when the entity id set / order has
    /// shifted since the last publish; `set_assembly` otherwise.
    ///
    /// Returns `true` when this publish routed `replace_assembly` (a
    /// topology swap), which tears down viso's per-entity scene-local
    /// state -- including the per-residue score map the Score color
    /// scheme reads. The caller uses this to invalidate any cache that
    /// shadows that now-wiped state (`App::last_pushed_scores`), so the
    /// next score reply always re-pushes rather than being suppressed as
    /// a no-op match against the pre-wipe value. Returns `false` on a
    /// steady-state `set_assembly` (or an empty batch), which preserves
    /// the score map.
    pub fn project(
        &mut self,
        changes: &[SessionUpdate],
        doc: &Session,
        engine: &mut viso::VisoEngine,
    ) -> bool {
        if changes.is_empty() {
            return false;
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
        topology_changed
    }
}

/// Build a focus description from focus + entity names. Was
/// `Session::focus_description`; moved here to keep `Session`
/// viso-free. The `All` arm reports `doc.count()` (live committed +
/// preview membership) rather than the metadata side table, which is
/// never GC'd and so over-reports the live entity count.
pub(crate) fn focus_description(doc: &Session, focus: &viso::Focus) -> String {
    match focus {
        viso::Focus::All => {
            let count = doc.count();
            format!("All ({count} entities)")
        }
        viso::Focus::Entity(id) => doc
            .metadata(*id)
            .map(|m| m.name.clone())
            .unwrap_or_else(|| format!("Entity {}", id.raw())),
    }
}
