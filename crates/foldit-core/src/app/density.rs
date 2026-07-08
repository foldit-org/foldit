//! Experimental-density map bring-up for the `--with-density` load and the
//! `[puzzle.reflns]` path: resolve structure factors + a space group, then
//! feed the computed map to both the scoring lane (an mrc `DensityAsset`
//! stashed on the session) and the render lane (the viso engine). The App-free
//! compute lives in [`crate::xtal`]; these methods own the App/session/engine
//! lane-feed around it.

use super::App;

impl App {
    /// Fetch structure factors for `name`, compute an experimental-weighted
    /// density against the just-loaded `entities`, and feed it to both lanes:
    /// the scoring lane (an mrc [`crate::puzzle_load::DensityAsset`] stashed on
    /// the session for `kick_inits`) and the render lane (the viso engine).
    /// Called before `entities` is moved into history so the atom table can be
    /// built from them by reference.
    ///
    /// Every failure branch warns and returns without touching either lane, so
    /// a missing sf.cif, an unsupported space group, or an empty reflection set
    /// degrades the load to viewer-only rather than aborting it.
    pub(in crate::app) fn load_with_density(&mut self, entities: &[molex::MoleculeEntity], name: &str) {
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

        // The sf.cif commonly omits symmetry, so resolve the space group from
        // the coordinate cif's Hermann-Mauguin name.
        let coord_path = std::path::Path::new("assets/models").join(format!("{name}.cif"));
        let Some(sg_number) = crate::xtal::read_space_group_number(&coord_path) else {
            log::warn!(
                "[App] --with-density: could not resolve a supported space group from {}; \
                 skipping density",
                coord_path.display()
            );
            return;
        };

        let Some(data) = crate::xtal::build_experimental_data(&sf_text, sg_number, name) else {
            log::warn!(
                "[App] --with-density: no usable reflections in {sf_path}; skipping density"
            );
            return;
        };

        // Retain the experimental data for later refine / map-refresh cycles,
        // then compute + upload the map through the shared tail so the map id
        // is tracked in one place.
        self.experimental_data = Some(data);
        self.refresh_density(entities, &format!("{name}-density.mrc"));
        log::info!("[App] --with-density: loaded experimental map for '{name}'");
    }

    /// Build experimental data from a puzzle's own structure factors
    /// (`[puzzle.reflns]`) and refresh the density, so a crystallographic
    /// puzzle enables the R-free objective and ships an `elec_dens` map without
    /// the `--with-density` flag. Resolves the space group from the TOML
    /// override or, failing that, the coordinate cif's Hermann-Mauguin name.
    ///
    /// Runs after the store's history is seeded (so the committed head is the
    /// map-computation input) and before plugin bring-up (so the map lands on
    /// the session for `kick_inits`). Every failure branch warns and returns,
    /// degrading the crystallography to viewer-only rather than aborting the
    /// puzzle load. `name` labels the map asset and seeds the free-flag set.
    pub(in crate::app) fn apply_puzzle_reflns(
        &mut self,
        reflns: &crate::puzzle_load::ReflnsAsset,
        name: &str,
    ) {
        let sg_number = if let Some(sg) = reflns.space_group {
            sg
        } else if let Some(coord_path) = reflns.coord_path.as_ref() {
            let Some(sg) = crate::xtal::read_space_group_number(coord_path) else {
                log::warn!(
                    "[App] puzzle reflns: could not resolve a supported space group \
                     from {}; skipping density",
                    coord_path.display()
                );
                return;
            };
            sg
        } else {
            log::warn!(
                "[App] puzzle reflns: no space_group override and no coordinate cif to \
                 read symmetry from; skipping density"
            );
            return;
        };

        let Some(data) = crate::xtal::build_experimental_data(&reflns.sf_text, sg_number, name)
        else {
            log::warn!("[App] puzzle reflns: no usable reflections; skipping density");
            return;
        };
        self.experimental_data = Some(data);
        let entities = self.committed_head_entities();
        self.refresh_density(&entities, &format!("{name}-density.mrc"));
        log::info!("[App] puzzle reflns: loaded experimental map for '{name}'");
    }

    /// Compute the experimental-weighted density from `entities` and feed both
    /// lanes: the scoring lane (an mrc [`crate::puzzle_load::DensityAsset`]
    /// stashed on the session) and the render lane (the viso engine). Reuses
    /// the retained [`molex::xtal::ExperimentalData`], so it is a no-op when no
    /// density has been loaded. `map_name` names the mrc asset.
    ///
    /// The render map id is retained on `self.density_map_id`: a prior map is
    /// removed from the engine before the recomputed one is uploaded, so a
    /// refine's map refresh replaces rather than stacks.
    pub(in crate::app) fn refresh_density(
        &mut self,
        entities: &[molex::MoleculeEntity],
        map_name: &str,
    ) {
        let Some(data) = self.experimental_data.clone() else {
            return;
        };

        let Some((asset, cropped)) =
            crate::xtal::compute_density(entities, &data, self.shared_device.as_ref(), map_name)
        else {
            log::warn!("[App] density refresh: computation failed; keeping prior map");
            return;
        };
        self.store.set_session_density(Some(asset));

        // Drop any prior map before uploading the recomputed one, so the id
        // stays single. The engine is attached before startup advances to the
        // load seam (`begin_startup` requires it), so the borrow is valid.
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
