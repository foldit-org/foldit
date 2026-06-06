//! App-owned viso projection.
//!
//! Consumes the [`SessionUpdate`] stream and rebuilds the head `Assembly`
//! once per drain to publish to viso. Owns the publish-generation
//! counter and the last-published id set, the latter only to pick
//! between `set_assembly` (steady-state coord update or same-membership
//! reorder) and `replace_assembly` (topology swap: an entity actually
//! joined or left, tearing down per-entity scene-local state). Both
//! stamp a fresh `publish_seq` so viso's `poll_assembly` gate sees a
//! different number on every publish.

use std::collections::BTreeSet;

use crate::session::{Session, SessionUpdate, SessionUpdateConsumer};

/// App-owned viso projector. Holds the monotonic publish counter that
/// every published `Assembly` is stamped with, plus the entity-id set
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
    /// Entity ids of the last published assembly, as a membership set.
    /// Compared against the next drain's id set to choose between
    /// `set_assembly` (same membership -- only coords differ, or the
    /// canonical order shifted) and `replace_assembly` (an id actually
    /// joined or left). A pure reorder is *not* a topology change: viso
    /// keys every entity by id and reconciles by membership on sync, so
    /// a same-set publish re-derives correctly through `set_assembly`
    /// without the scene-local teardown `replace_assembly` forces.
    last_published_ids: BTreeSet<molex::entity::molecule::id::EntityId>,
}

impl RenderProjector {
    pub fn new() -> Self {
        Self {
            publish_seq: 0,
            last_published_ids: BTreeSet::new(),
        }
    }
}

/// Consume a drained `SessionUpdate` batch and publish the current head
/// assembly to viso. No-ops when the batch is empty (no publishes mean
/// no wasted assembly builds or generation bumps). Picks
/// `replace_assembly` only when the entity-id *set* changed since the
/// last publish (an id joined or left); `set_assembly` otherwise, which
/// covers both a steady-state coord update and a same-membership
/// reorder.
impl SessionUpdateConsumer<viso::VisoEngine> for RenderProjector {
    fn consume(
        &mut self,
        changes: &[SessionUpdate],
        doc: &Session,
        engine: &mut viso::VisoEngine,
    ) {
        if changes.is_empty() {
            return;
        }
        let mut asm = doc.head_assembly();
        let new_ids: BTreeSet<molex::entity::molecule::id::EntityId> =
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
