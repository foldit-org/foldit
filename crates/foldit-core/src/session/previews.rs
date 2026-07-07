//! The in-flight op-stream preview token maps and their preview mechanics.

use std::collections::HashMap;

use molex::entity::molecule::id::EntityId;

use super::Session;

/// Per-token preview registries for an in-flight op stream, keyed by the
/// edit/request token.
pub(super) struct Previews {
    creates: HashMap<u64, (molex::EntityId, usize)>,
    /// Live in-place preview ghosts, each `(ghost entity id, last atom count)`.
    inplace: HashMap<u64, (molex::EntityId, usize)>,
}

impl Default for Previews {
    fn default() -> Self {
        Self::new()
    }
}

impl Previews {
    pub(crate) fn new() -> Self {
        Self {
            creates: HashMap::new(),
            inplace: HashMap::new(),
        }
    }

    pub(crate) fn clear(&mut self) {
        self.creates.clear();
        self.inplace.clear();
    }
}

impl Session {
    /// Stream one diffusion frame of an entity-creating op into a live preview
    /// entity: cheap coord update on matching topology, fresh-id rebuild when
    /// the atom count changes.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn stream_preview_frame(&mut self, token: u64, assembly: &molex::Assembly) {
        let Some(entity) = assembly.entities().first() else {
            return;
        };
        // Rebuild protein chains as one continuous segment so noisy
        // intermediate coordinates render as a connected backbone.
        let payload: molex::MoleculeEntity = entity.to_continuous();
        let atoms = payload.atom_count();
        match self.previews.creates.get(&token).copied() {
            Some((preview_id, prev_atoms)) if prev_atoms == atoms => {
                let _ = self.update_preview(preview_id, payload);
            }
            // Atom count changed: a same-id coord update would desync viso's
            // topology vs positions. Rebuild under a fresh id.
            Some((preview_id, _)) => {
                let _ = self.remove_preview(preview_id);
                let id = self.insert_design_preview(payload);
                self.set_entity_provisional(id, true);
                let _ = self.previews.creates.insert(token, (id, atoms));
            }
            None => {
                let id = self.insert_design_preview(payload);
                self.set_entity_provisional(id, true);
                let _ = self.previews.creates.insert(token, (id, atoms));
            }
        }
    }

    /// Seed a preview-style op's discardable ghost by cloning the target
    /// `lane_id` into a provisional transient tracked under `token`.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn seed_inplace_preview(&mut self, token: u64, lane_id: EntityId, name: String) {
        let Some(clone) = self.entity(lane_id).cloned() else {
            return;
        };
        let preview_id = self.insert_preview(clone, name);
        self.set_entity_provisional(preview_id, true);
        let atom_count = self
            .entity(preview_id)
            .map_or(0, molex::MoleculeEntity::atom_count);
        let _ = self
            .previews
            .inplace
            .insert(token, (preview_id, atom_count));
    }

    /// Apply one streaming frame of a preview-style op to its discardable
    /// ghost, leaving the real lane untouched. No-op when no ghost is tracked.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn stream_inplace_preview_frame(&mut self, token: u64, assembly: &molex::Assembly) {
        let Some((preview_id, prev_atoms)) = self.previews.inplace.get(&token).copied() else {
            return;
        };
        let Some(entity) = assembly.entities().first() else {
            return;
        };
        let payload: molex::MoleculeEntity = (**entity).clone();
        let atoms = payload.atom_count();
        if prev_atoms == atoms {
            let _ = self.update_preview(preview_id, payload);
        } else {
            let name = self
                .name(preview_id)
                .map_or_else(String::new, str::to_owned);
            let _ = self.remove_preview(preview_id);
            let id = self.insert_preview(payload, name);
            self.set_entity_provisional(id, true);
            let _ = self.previews.inplace.insert(token, (id, atoms));
        }
    }

    /// Insert a streamed design frame as a transient preview, returning its id.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn insert_design_preview(
        &mut self,
        payload: molex::MoleculeEntity,
    ) -> molex::EntityId {
        self.insert_preview(payload, String::from("RFdiffusion3 design"))
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn discard_inplace_ghost(&mut self, token: u64) {
        if let Some((id, _)) = self.previews.inplace.remove(&token) {
            let _ = self.remove_preview(id);
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn discard_created_preview(&mut self, token: u64) {
        if let Some((id, _)) = self.previews.creates.remove(&token) {
            let _ = self.remove_preview(id);
        }
    }

    pub(crate) fn has_active_creates_previews(&self) -> bool {
        !self.previews.creates.is_empty()
    }
}
