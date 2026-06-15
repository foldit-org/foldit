use crate::app::App;

impl App {
    /// Resolve a proto `entity_id` (`u64`) to a live molex `EntityId` against
    /// the current session. Returns `None` when the id matches no current
    /// entity (a panel can race a structure swap, leaving a stale id). The
    /// same `id.raw()` lookup the dispatch and pull-drag paths use, widened to
    /// the proto's `u64`.
    #[cfg(not(target_arch = "wasm32"))]
    fn resolve_entity(&self, entity_id: u64) -> Option<molex::EntityId> {
        self.store.ids().find(|id| u64::from(id.raw()) == entity_id)
    }

    /// Refresh the held rendering connections (hydrogen bonds + disulfides)
    /// from the plugin's `connections` query and choose the per-publish
    /// connection provider. Runs before the render projector publishes, on the
    /// at-rest geometry gate, so the connections track the committed pose.
    ///
    /// Two outcomes, keyed wholesale on whether a plugin advertises the query
    /// ([`crate::runner_client::RunnerClient::supports_query`]):
    ///
    /// - A plugin advertises `connections`: it is the live provider. Re-query
    ///   the report, decode it against the head assembly into stable
    ///   atom-index links ([`crate::viz::connections::connections_from_report`]), and store
    ///   them as the held set. The projector is told to stamp this held set
    ///   verbatim on every publish - molex's geometric fallback is NOT run.
    /// - No plugin advertises it: drop any held set and tell the projector to
    ///   fall back to molex geometry per publish (today's viewer behavior).
    ///
    /// The held set is decoded once against the current head; its `AtomId`
    /// indices are stable across coord-only changes, so it survives
    /// re-application across publishes. A topology change (new puzzle or an
    /// entity joining/leaving) drops the held set before re-querying, so stale
    /// ids from a prior load are never re-applied (entity ids can be reused
    /// across loads). The map carries hydrogen bonds and disulfides only;
    /// clash has its own path and never enters here.
    ///
    /// Self-gates on the engine being present (the projector publishes into
    /// it). Until a plugin implements `connections` this leaves the projector
    /// on the molex fallback, matching the prior behavior.
    #[cfg(not(target_arch = "wasm32"))]
    pub(in crate::app) fn refresh_connections(&mut self) {
        if self.engine.is_none() {
            return;
        }
        // No plugin provides connections: drop any held set and put the
        // projector on the molex geometric fallback (the viewer-only path).
        if !self.runner_client.supports_query("connections") {
            self.held_connections = None;
            self.connections_topology_ids.clear();
            self.render_projector.set_publish_connections(None);
            return;
        }
        // Plugin provider. Drop the held set on a topology change so stale ids
        // from a prior load are never re-applied (ids can be reused).
        let head_ids: std::collections::BTreeSet<molex::EntityId> =
            self.store.head_assembly().entities().iter().map(|e| e.id()).collect();
        if head_ids != self.connections_topology_ids {
            self.held_connections = None;
            self.connections_topology_ids = head_ids;
        }
        // Re-query and decode against the current head. The query path
        // swallows errors at `trace` level, so an at-rest miss never spams the
        // log; empty / errored bytes decode to an empty map (no connections).
        let bytes = self.runner_client.request_query_bytes("connections");
        let held = if bytes.is_empty() {
            std::collections::HashMap::new()
        } else {
            match <foldit_runner::proto::plugin::ConnectionReport as prost::Message>::decode(
                bytes.as_slice(),
            ) {
                Ok(report) => crate::viz::connections::connections_from_report(&report, &self.store.head_assembly()),
                Err(_) => std::collections::HashMap::new(),
            }
        };
        self.held_connections = Some(held.clone());
        // Stamp the held set verbatim on every publish; molex geometry is not
        // run while the plugin is the provider.
        self.render_projector.set_publish_connections(Some(held));
    }

    /// Refresh the engine's external (host-supplied) void field from the
    /// plugin's `voids` query. Runs after the render projector publishes, on
    /// the at-rest geometry gate (or a cavity-toggle flip), so the voids track
    /// the committed pose.
    ///
    /// Gated three ways and inert until all hold: the engine must be present
    /// (like every engine-touching arm), the cavity display must be ON (the
    /// external field is additive to the engine's built-in `show_cavities`
    /// path, so there is nothing to show when cavities are hidden), and a
    /// plugin must advertise the `voids` query
    /// ([`crate::runner_client::RunnerClient::supports_query`]). When the
    /// display is off, this clears any previously pushed external field so
    /// toggling cavities off removes them.
    ///
    /// Decode is the pure [`crate::viz::voids::void_field_from_bytes`] helper; the
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
        if !self.runner_client.supports_query("voids") {
            return;
        }
        let bytes = self.runner_client.request_query_bytes("voids");
        let field = crate::viz::voids::void_field_from_bytes(&bytes);
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

    /// Refresh the engine's steric-clash arcs from the plugin's `clashes`
    /// query. Runs after the render projector publishes, on the at-rest
    /// geometry gate (or a clash-toggle flip), so the clashes track the
    /// committed pose.
    ///
    /// Gated three ways: the engine must be present (like every engine-touching
    /// arm), the clash display must be ON, and a plugin must advertise the
    /// `clashes` query
    /// ([`crate::runner_client::RunnerClient::supports_query`]). When the
    /// display is off this clears any previously pushed arcs and stops; when no
    /// plugin advertises the query, this clears any previously pushed clash set
    /// so a structure swap to a clash-less plugin removes stale arcs.
    ///
    /// Decode is the pure [`crate::viz::clashes::clashes_from_bytes`] helper; each
    /// decoded endpoint's proto `entity_id` is mapped to a molex `EntityId`
    /// against the live session, and a clash whose endpoint does not resolve to
    /// a current entity is dropped. The push is
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
        if !self.runner_client.supports_query("clashes") {
            if let Some(engine) = self.engine.as_mut() {
                engine.update_clashes(Vec::new());
            }
            return;
        }
        let bytes = self.runner_client.request_query_bytes("clashes");
        let report = crate::viz::clashes::clashes_from_bytes(&bytes);
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
    fn clash_endpoint(&self, end: &crate::viz::clashes::ClashEnd) -> Option<viso::ClashEndpoint> {
        let entity = self.resolve_entity(end.entity_id)?;
        Some(viso::ClashEndpoint {
            entity,
            residue: end.residue_index,
            atom_name: end.atom_name.clone(),
        })
    }

    /// Refresh the engine's exposed-hydrophobic grease beads and the loaded
    /// puzzle's met-objective bonus from the plugin's `exposed_hydrophobics`
    /// query. Runs after the render projector publishes, on the at-rest
    /// geometry gate (or an exposed-hydrophobic-toggle flip), so the flagged
    /// residues and the objective count track the committed pose.
    ///
    /// The query runs when EITHER the exposed-hydrophobic display is ON OR the
    /// loaded puzzle declares an active `exposed_count` objective (and a
    /// plugin advertises the query,
    /// [`crate::runner_client::RunnerClient::supports_query`]).
    /// Decoupling the query from the viz toggle keeps scoring correct when the
    /// player hides the beads: the objective bonus is recomputed from the live
    /// count regardless of the toggle. The viso bead push stays gated on the
    /// display toggle ALONE - with the toggle off but the objective active,
    /// the query runs for the count but no beads are drawn (an empty set is
    /// pushed, clearing any stale beads).
    ///
    /// Gated three ways at the top: the engine must be present (like every
    /// engine-touching arm); the display must be ON or an `exposed_count`
    /// objective active; and a plugin must advertise the query. When none of
    /// those hold this clears any previously pushed beads, zeroes the
    /// objective bonus, and stops.
    ///
    /// Decode is the pure [`crate::viz::exposed_hydrophobics::exposed_from_bytes`]
    /// helper; each decoded residue's proto `entity_id` is mapped to a molex
    /// `EntityId` against the live session, and a residue whose `entity_id`
    /// does not resolve to a current entity is dropped. The push is
    /// [`viso::VisoEngine::update_exposed_hydrophobics`], which resolves the
    /// per-entity refs itself and renders the grease beads (the host computes
    /// no flat residue index). An empty / errored / unsupported query yields
    /// an empty set, which clears the beads and zeroes the bonus. The query
    /// path swallows errors at `trace` level, so an at-rest miss never spams
    /// the log; until the plugin implements `exposed_hydrophobics` the whole
    /// path is an inert no-op.
    #[cfg(not(target_arch = "wasm32"))]
    pub(in crate::app) fn refresh_exposed_hydrophobics(&mut self) {
        if self.engine.is_none() {
            return;
        }
        let show = self.view_options.display.show_exposed_hydrophobics();
        // The objective is active only when the loaded puzzle declares an
        // `exposed_count` objective; that is what makes the query run even
        // with the viz toggle off so the score still responds to burying.
        let objective_active = self
            .store
            .puzzle()
            .is_some_and(|p| p.objectives.iter().any(|o| o.kind == "exposed_count"));
        // Neither the display nor an objective wants the query: clear any
        // beads we pushed earlier, zero the bonus, and stop before any query
        // work, so toggling the option off removes the beads.
        if !show && !objective_active {
            if let Some(engine) = self.engine.as_mut() {
                engine.update_exposed_hydrophobics(Vec::new());
            }
            self.store.set_objective_bonus(0.0);
            return;
        }
        // No plugin advertises `exposed_hydrophobics`: clear any set we pushed
        // earlier, zero the bonus, and stop, so swapping to a detector-less
        // structure removes stale beads and drops a stale bonus.
        if !self.runner_client.supports_query("exposed_hydrophobics") {
            if let Some(engine) = self.engine.as_mut() {
                engine.update_exposed_hydrophobics(Vec::new());
            }
            self.store.set_objective_bonus(0.0);
            return;
        }
        let bytes = self.runner_client.request_query_bytes("exposed_hydrophobics");
        let report = crate::viz::exposed_hydrophobics::exposed_from_bytes(&bytes);
        // Evaluate every active `exposed_count` objective on the loaded puzzle
        // against the live count and store the met-bonus total. Folded into
        // the headline game score by the score path before the raw->game map.
        // No puzzle (free-form) yields no objectives -> zero bonus.
        let count = u32::try_from(report.exposed.len()).unwrap_or(u32::MAX);
        let bonus = self.store.puzzle().map_or(0.0, |p| {
            crate::app::score_apply::exposed_count_bonus(&p.objectives, count)
        });
        self.store.set_objective_bonus(bonus);
        // Beads stay gated on the display toggle ALONE: with the objective
        // active but the toggle off, push an empty set so no beads the player
        // didn't ask for are drawn (and any stale set is cleared).
        if !show {
            if let Some(engine) = self.engine.as_mut() {
                engine.update_exposed_hydrophobics(Vec::new());
            }
            return;
        }
        let mut infos: Vec<viso::ExposedHydrophobicInfo> =
            Vec::with_capacity(report.exposed.len());
        for residue in &report.exposed {
            // Map the residue; drop it if its entity_id does not resolve to a
            // current entity (a panel can race a structure swap, leaving a
            // stale id).
            let Some(entity) = self.resolve_entity(residue.entity_id) else {
                continue;
            };
            infos.push(viso::ExposedHydrophobicInfo {
                entity,
                residue: residue.residue_index,
            });
        }
        if let Some(engine) = self.engine.as_mut() {
            engine.update_exposed_hydrophobics(infos);
        }
    }
}
