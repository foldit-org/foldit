//! Session density: staging the structure factors a provider plugin folds into
//! a map, and the render lane for the map it hands back.
//!
//! The map itself is produced by the plugin that declares `provides_density`
//! and read into the session through the well-known `density` query. This
//! module only stages that plugin's input and turns the mrc bytes it returns
//! into the cropped map the viso engine renders.

use molex::entity::molecule::id::EntityId;
use molex::MoleculeEntity;

use super::App;

impl App {
    /// Clone the committed head entities, excluding transient previews, so the
    /// render lane never crops against a discardable ghost.
    #[cfg(not(target_arch = "wasm32"))]
    pub(in crate::app) fn committed_head_entities(&self) -> Vec<MoleculeEntity> {
        let previews: std::collections::HashSet<EntityId> = self.store.preview_ids().collect();
        self.store
            .ids()
            .filter(|id| !previews.contains(id))
            .filter_map(|id| self.store.entity(id).cloned())
            .collect()
    }

    /// Resolve the structure factors for `name` and stash them on the session,
    /// so plugin bring-up hands them to the density provider.
    ///
    /// The host does not resolve the space group. The sf-cif commonly omits
    /// symmetry, and the Hermann-Mauguin lookup lives behind molex's `xtal`
    /// feature, which the host no longer carries; the coordinate cif rides
    /// along instead and the provider reads symmetry from it.
    ///
    /// Every failure branch warns and returns, so a missing sf.cif degrades the
    /// load to viewer-only rather than aborting it.
    #[cfg(not(target_arch = "wasm32"))]
    pub(in crate::app) fn load_with_density(&mut self, name: &str) {
        let sf_path = match crate::structure_io::resolve_sf_cif(name) {
            Ok(p) => p,
            Err(e) => {
                log::warn!("[App] --with-density: no structure factors for '{name}': {e}");
                return;
            }
        };
        let sf_text = match std::fs::read_to_string(&sf_path) {
            Ok(t) => t,
            Err(e) => {
                log::warn!("[App] --with-density: failed to read {sf_path}: {e}");
                return;
            }
        };
        let coord_path = std::path::Path::new("assets/models").join(format!("{name}.cif"));

        self.store
            .set_session_reflns(Some(crate::puzzle_load::ReflnsAsset {
                sf_text,
                coord_path: Some(coord_path),
                space_group: None,
            }));
        log::info!("[App] --with-density: structure factors staged for '{name}'");
    }

    /// Read the density provider's map into the session and refresh the render
    /// lane. No-op when no plugin provides one, or the puzzle has no
    /// reflections.
    #[cfg(not(target_arch = "wasm32"))]
    pub(in crate::app) fn adopt_provider_density(&mut self) {
        let Some(map) = self.runner_client.fetch_density() else {
            return;
        };
        log::info!("[App] adopted density map '{}' from its provider", map.name);
        self.store.set_session_density(Some(map));
        self.refresh_density_render();
    }

    /// Rebuild the render-lane map from the session's mrc bytes: decode the
    /// full-cell map, then crop to a sub-block around the model so
    /// symmetry-mate blobs drop out.
    ///
    /// The render map id is retained on `self.density_map_id`: a prior map is
    /// removed from the engine before the new one is uploaded, so a refresh
    /// replaces rather than stacks.
    #[cfg(not(target_arch = "wasm32"))]
    pub(in crate::app) fn refresh_density_render(&mut self) {
        let Some(asset) = self.store.session_density() else {
            return;
        };
        let density = match molex::adapters::mrc::mrc_to_density(&asset.bytes) {
            Ok(d) => d,
            Err(e) => {
                log::warn!("[App] density: cannot decode map bytes: {e}; keeping prior map");
                return;
            }
        };

        let entities = self.committed_head_entities();
        let table = molex::adapters::table::AtomTable::from_entities(&entities);
        let positions: Vec<[f32; 3]> = table.position.iter().map(glam::Vec3::to_array).collect();
        let cropped = density.crop_to_points(&positions, 3);

        if let Some(old) = self.density_map_id.take() {
            if let Some(engine) = self.harness.engine.as_mut() {
                engine.remove_density(old);
            }
        }
        if let Some(engine) = self.harness.engine.as_mut() {
            let map_id = engine.load_density(cropped);
            engine.set_density_opacity(map_id, 0.7);
            self.density_map_id = Some(map_id);
        }
    }
}
