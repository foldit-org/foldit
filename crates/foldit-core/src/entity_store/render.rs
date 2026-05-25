//! viso projection: hand the current head assembly to the renderer and
//! describe focus targets by entity name. Names `viso` types directly;
//! replacing these reach-throughs with a stream-subscribed projector is
//! Reshape work, not part of this split.

use std::sync::Arc;

use super::EntityStore;

impl EntityStore {
    /// Push the current `head_assembly()` snapshot to viso. Each push
    /// stamps a fresh `publish_seq` onto the Assembly so viso's
    /// generation-gate (`poll_assembly`) sees a different number on
    /// every call — without that, the second-and-subsequent publishes
    /// would silently skip because `Assembly::new` always starts at
    /// generation 0.
    pub fn publish_to(&mut self, engine: &mut viso::VisoEngine) {
        let mut asm = self.head_assembly();
        self.publish_seq = self.publish_seq.saturating_add(1);
        asm.set_generation(self.publish_seq);
        engine.set_assembly(Arc::new(asm));
    }

    /// Atomic topology swap: hand the current `head_assembly()` to
    /// viso and have it tear down scene-local state + force-sync in
    /// one shot. Use for puzzle / file reloads where leftover
    /// per-entity state from the previous topology would otherwise
    /// linger until the next render tick.
    pub fn replace_in(&mut self, engine: &mut viso::VisoEngine) {
        let mut asm = self.head_assembly();
        self.publish_seq = self.publish_seq.saturating_add(1);
        asm.set_generation(self.publish_seq);
        engine.replace_assembly(Arc::new(asm));
    }

    /// Build a focus description from focus + entity names.
    pub fn focus_description(&self, focus: &viso::Focus) -> String {
        match focus {
            viso::Focus::Session => {
                let count = self.metadata.len();
                format!("Session ({count} entities)")
            }
            viso::Focus::Entity(id) => self
                .metadata
                .get(id)
                .map(|m| m.name.clone())
                .unwrap_or_else(|| format!("Entity {}", id.raw())),
        }
    }
}
