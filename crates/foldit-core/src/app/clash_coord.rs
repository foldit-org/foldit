use crate::app::App;

impl App {
    /// Refresh the engine's steric-clash arcs from the plugin's `clashes`
    /// query. Mirrors the at-rest `score` and `voids` requests: it runs only
    /// when the scene is at rest, on a geometry change, with no edit open, so
    /// the clashes track the committed pose without a per-frame hot loop.
    ///
    /// Gated three ways: the engine must be present (like every engine-touching
    /// arm), the clash display must be ON, and a plugin must advertise the
    /// `clashes` query
    /// ([`crate::runner_client::RunnerClient::supports_clashes`]). When the
    /// display is off this clears any previously pushed arcs and stops; when no
    /// plugin advertises the query, this clears any previously pushed clash set
    /// so a structure swap to a clash-less plugin removes stale arcs.
    ///
    /// Decode is the pure [`crate::clashes::clashes_from_bytes`] helper; each
    /// decoded endpoint's proto `entity_id` is mapped to a molex `EntityId`
    /// against the live session (the same `id.raw()` lookup the dispatch and
    /// pull-drag paths use, widened to the proto's `u64`), and a clash whose
    /// endpoint does not resolve to a current entity is dropped. The push is
    /// [`viso::VisoEngine::update_clashes`], which resolves the per-entity refs
    /// itself and renders arcs (the host computes no flat residue index). An
    /// empty / errored / unsupported query yields an empty set, which clears
    /// the arcs. The query path swallows errors at `trace` level, so an
    /// at-rest miss never spams the log; until the plugin implements `clashes`
    /// the whole path is an inert no-op.
    #[cfg(not(target_arch = "wasm32"))]
    pub(in crate::app) fn refresh_clashes(&mut self) {
        if self.engine.is_none() {
            return;
        }
        // Clash display off: clear any arcs we pushed earlier and stop, before
        // any query work, so toggling clashes off removes them.
        if !self.view_options.display.show_clashes() {
            if let Some(engine) = self.engine.as_mut() {
                engine.update_clashes(Vec::new());
            }
            return;
        }
        // No plugin advertises `clashes`: clear any set we pushed earlier and
        // stop, so swapping to a clash-less structure removes stale arcs.
        if !self.runner_client.supports_clashes() {
            if let Some(engine) = self.engine.as_mut() {
                engine.update_clashes(Vec::new());
            }
            return;
        }
        let bytes = self.runner_client.request_clashes_bytes();
        let report = crate::clashes::clashes_from_bytes(&bytes);
        let mut infos: Vec<viso::ClashInfo> = Vec::with_capacity(report.clashes.len());
        for clash in &report.clashes {
            // Map both endpoints; drop the whole clash if either endpoint's
            // entity_id does not resolve to a current entity (a panel can race
            // a structure swap, leaving a stale id).
            let (Some(a), Some(b)) = (
                self.clash_endpoint(&clash.a),
                self.clash_endpoint(&clash.b),
            ) else {
                continue;
            };
            infos.push(viso::ClashInfo {
                a,
                b,
                severity: clash.severity,
            });
        }
        if let Some(engine) = self.engine.as_mut() {
            engine.update_clashes(infos);
        }
    }

    /// Map a decoded clash endpoint into a viso [`viso::ClashEndpoint`],
    /// resolving the proto `entity_id` (`u64`) to a live molex `EntityId`. The
    /// entity-local `residue_index` and `atom_name` pass straight through; the
    /// host computes no flat residue index (viso resolves the per-entity ref).
    /// Returns `None` when the `entity_id` matches no current entity.
    #[cfg(not(target_arch = "wasm32"))]
    fn clash_endpoint(&self, end: &crate::clashes::ClashEnd) -> Option<viso::ClashEndpoint> {
        let entity = self
            .store
            .ids()
            .find(|id| u64::from(id.raw()) == end.entity_id)?;
        Some(viso::ClashEndpoint {
            entity,
            residue: end.residue_index,
            atom_name: end.atom_name.clone(),
        })
    }
}
