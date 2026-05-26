//! App-owned viso projection.
//!
//! Owns the publish generation counter, hands the current head assembly
//! to viso (set / replace), and describes focus targets by entity name.
//! Relocated here from `Document` so `Document` stays molex-only: it no
//! longer names `viso` types or tracks the publish sequence.
//!
//! RX7 keeps render driven at the current explicit call sites; making
//! this projector consume the `SceneChange` spine is RX13's tick.

use crate::document::Document;

/// App-owned viso projector. Holds the monotonic publish counter that
/// every published `Assembly` is stamped with: without a fresh
/// generation per publish, viso's `poll_assembly` gate skips the
/// second-and-subsequent publishes, because a freshly built `Assembly`
/// always starts at generation 0.
pub(crate) struct RenderProjector {
    /// Monotonic counter stamped onto every published `Assembly`.
    /// Incremented on every `publish` / `replace`. Lives here rather
    /// than on `Document`, so `Document::reset` no longer touches it: a
    /// fresh post-reset publish still advances the counter, and viso
    /// never sees the generation go backwards.
    publish_seq: u64,
}

impl RenderProjector {
    pub fn new() -> Self {
        Self { publish_seq: 0 }
    }

    /// Push the current `head_assembly()` snapshot to viso. Each push
    /// stamps a fresh `publish_seq` onto the `Assembly` so viso's
    /// generation gate (`poll_assembly`) sees a different number on
    /// every call; without that, the second-and-subsequent publishes
    /// would silently skip. Was `Document::publish_to`.
    pub fn publish(&mut self, doc: &Document, engine: &mut viso::VisoEngine) {
        let mut asm = doc.head_assembly();
        self.publish_seq = self.publish_seq.saturating_add(1);
        asm.set_generation(self.publish_seq);
        engine.set_assembly(std::sync::Arc::new(asm));
    }

    /// Atomic topology swap: hand the current `head_assembly()` to viso
    /// and have it tear down scene-local state plus force-sync in one
    /// shot. Use for puzzle / file reloads where leftover per-entity
    /// state from the previous topology would otherwise linger until the
    /// next render tick. Was `Document::replace_in`.
    pub fn replace(&mut self, doc: &Document, engine: &mut viso::VisoEngine) {
        let mut asm = doc.head_assembly();
        self.publish_seq = self.publish_seq.saturating_add(1);
        asm.set_generation(self.publish_seq);
        engine.replace_assembly(std::sync::Arc::new(asm));
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
