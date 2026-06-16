use crate::app::App;

impl App {
    /// Resolve a proto `entity_id` (`u64`) to a live molex `EntityId` against
    /// the current session.
    #[cfg(not(target_arch = "wasm32"))]
    fn resolve_entity(&self, entity_id: u64) -> Option<molex::EntityId> {
        self.store.ids().find(|id| u64::from(id.raw()) == entity_id)
    }

    /// Refresh the held rendering connections (hbonds, disulfides, clashes)
    /// from the plugin's `connections` query and choose the per-publish
    /// connection provider.
    #[cfg(not(target_arch = "wasm32"))]
    pub(in crate::app) fn refresh_connections(&mut self) {
        if self.engine.is_none() {
            return;
        }

        // Connections are loaded into Foldit via a plugin query,
        // namely from Rosetta- could eventually be done directly in molex
        //
        // Falls back to naive molex connection implementations in the case
        // that the Rosetta plugin is not present
        if !self.runner_client.supports_query("connections") {
            self.held_connections = None;
            self.connections_topology_ids.clear();
            self.render_projector.set_publish_connections(None);
            return;
        }

        // Collect entity ids from the current session's assembly
        let head_ids: std::collections::BTreeSet<molex::EntityId> = self
            .store
            .head_assembly()
            .entities()
            .iter()
            .map(|e| e.id())
            .collect();

        if head_ids != self.connections_topology_ids {
            self.held_connections = None;
            self.connections_topology_ids = head_ids;
        }

        // Query Rosetta plugin for all connection data
        let bytes = self.runner_client.request_query_bytes("connections");

        let held = if bytes.is_empty() {
            std::collections::HashMap::new()
        } else {
            match <foldit_runner::proto::plugin::ConnectionReport as prost::Message>::decode(
                bytes.as_slice(),
            ) {
                Ok(report) => crate::viz::connections::connections_from_report(
                    &report,
                    &self.store.head_assembly(),
                ),
                Err(_) => std::collections::HashMap::new(),
            }
        };
        self.held_connections = Some(held.clone());

        self.render_projector.set_publish_connections(Some(held));
    }

    /// Refresh the engine's external (host-supplied) void field from the
    /// plugin's `voids` query. Runs after the render projector publishes, on
    /// the at-rest geometry gate (or a cavity-toggle flip), so the voids track
    /// the committed pose.
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

        if !self.runner_client.supports_query("voids") {
            return;
        }

        // Run the voids query to the rosetta plugin
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
            let (Some(a), Some(b)) = (self.clash_endpoint(&clash.a), self.clash_endpoint(&clash.b))
            else {
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

    /// Refresh the engine's per-residue non-designable overlay from the
    /// loaded puzzle's design gating. The overlay desaturates locked
    /// residues toward white so the player can see which parts of the
    /// structure may not be mutated.
    #[cfg(not(target_arch = "wasm32"))]
    pub(in crate::app) fn refresh_design_gating(&mut self) {
        use std::collections::{BTreeMap, BTreeSet};

        if self.engine.is_none() {
            return;
        }

        if !self.store.design_gating_active() {
            if let Some(engine) = self.engine.as_mut() {
                engine.set_non_designable(&BTreeMap::new());
            }
            return;
        }

        // Build the non-designable set per entity from the head assembly's
        // residue counts: every residue the session reports as not designable.
        let head = self.store.head_assembly();
        let mut non_designable: BTreeMap<molex::EntityId, BTreeSet<u32>> = BTreeMap::new();
        for entity in head.entities() {
            let eid = entity.id();
            let count = u32::try_from(entity.residue_count()).unwrap_or(u32::MAX);
            let locked: BTreeSet<u32> = (0..count)
                .filter(|&res| !self.store.is_designable(eid, res))
                .collect();
            if !locked.is_empty() {
                non_designable.insert(eid, locked);
            }
        }

        if let Some(engine) = self.engine.as_mut() {
            engine.set_non_designable(&non_designable);
        }
    }

    /// Refresh the engine's exposed-hydrophobic grease beads and the loaded
    /// puzzle's met-filter bonus from the plugin's `exposed_hydrophobics`
    /// query. Runs after the render projector publishes, on the at-rest
    /// geometry gate (or an exposed-hydrophobic-toggle flip), so the flagged
    /// residues and the filter count track the committed pose.
    #[cfg(not(target_arch = "wasm32"))]
    pub(in crate::app) fn refresh_exposed_hydrophobics(&mut self) {
        if self.engine.is_none() {
            return;
        }
        let show = self.view_options.display.show_exposed_hydrophobics();

        let filter_active = self.store.puzzle().is_some_and(|p| {
            p.filters
                .iter()
                .any(|f| f.kind == "ExposedCount" && f.plugin.is_none())
        });

        if !show && !filter_active {
            if let Some(engine) = self.engine.as_mut() {
                engine.update_exposed_hydrophobics(Vec::new());
            }
            self.store.set_filter_bonus(Vec::new());
            return;
        }

        // No plugin advertises `exposed_hydrophobics`: clear any set we pushed
        // earlier, clear the bonus, and stop, so swapping to a detector-less
        // structure removes stale beads and drops a stale bonus.
        if !self.runner_client.supports_query("exposed_hydrophobics") {
            if let Some(engine) = self.engine.as_mut() {
                engine.update_exposed_hydrophobics(Vec::new());
            }
            self.store.set_filter_bonus(Vec::new());
            return;
        }

        // Run the Rosetta query for exposed hydrophobics
        let bytes = self
            .runner_client
            .request_query_bytes("exposed_hydrophobics");

        let report = crate::viz::exposed_hydrophobics::exposed_from_bytes(&bytes);
        let count = u32::try_from(report.exposed.len()).unwrap_or(u32::MAX);
        let bonus = self.store.puzzle().map_or(0.0, |p| {
            crate::app::score_apply::exposed_count_bonus(&p.filters, count)
        });

        if bonus == 0.0 {
            self.store.set_filter_bonus(Vec::new());
        } else {
            self.store
                .set_filter_bonus(vec![("exposed_count".to_owned(), bonus)]);
        }

        if !show {
            if let Some(engine) = self.engine.as_mut() {
                engine.update_exposed_hydrophobics(Vec::new());
            }
            return;
        }

        let mut infos: Vec<viso::ExposedHydrophobicInfo> = Vec::with_capacity(report.exposed.len());
        for residue in &report.exposed {
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
