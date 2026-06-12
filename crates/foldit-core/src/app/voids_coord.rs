use crate::app::App;

impl App {
    /// Refresh the engine's external (host-supplied) void field from the
    /// plugin's `voids` query. Mirrors the at-rest `score` request: it runs
    /// only when the scene is at rest, on a geometry change, with no edit
    /// open, so the voids track the committed pose without a per-frame hot
    /// loop.
    ///
    /// Gated three ways and inert until all hold: the engine must be present
    /// (like every engine-touching arm), the cavity display must be ON (the
    /// external field is additive to the engine's built-in `show_cavities`
    /// path, so there is nothing to show when cavities are hidden), and a
    /// plugin must advertise the `voids` query
    /// ([`crate::runner_client::RunnerClient::supports_voids`]). When the
    /// display is off, this clears any previously pushed external field so
    /// toggling cavities off removes them.
    ///
    /// Decode is the pure [`crate::voids::void_field_from_bytes`] helper; the
    /// push is [`viso::VisoEngine::set_external_void_field`], which meshes
    /// the field's isosurface directly (the host does not voxelize on this
    /// path). An empty / errored / unsupported query yields a cleared field,
    /// which clears the set. The query path swallows errors at `trace` level,
    /// so an at-rest miss never spams the log; until the plugin implements
    /// `voids` the whole path is an inert no-op.
    #[cfg(not(target_arch = "wasm32"))]
    pub(in crate::app) fn refresh_external_cavities(&mut self) {
        if self.engine.is_none() {
            return;
        }
        // Cavities hidden: clear any external field we pushed earlier and stop.
        if !self.view_options.display.show_cavities() {
            if let Some(engine) = self.engine.as_mut() {
                engine.set_external_void_field([0; 3], [0.0; 3], [0.0; 3], Vec::new(), 0.0);
            }
            return;
        }
        // Cavities shown but no plugin advertises `voids`: inert no-op. Do
        // not clear here; an unrelated plugin lacking the query must not wipe
        // a field another path established.
        if !self.runner_client.supports_voids() {
            return;
        }
        let bytes = self.runner_client.request_voids_bytes();
        let field = crate::voids::void_field_from_bytes(&bytes);
        if let Some(engine) = self.engine.as_mut() {
            engine.set_external_void_field(
                field.dims,
                field.origin,
                field.spacing,
                field.phi,
                field.threshold,
            );
        }
    }
}
