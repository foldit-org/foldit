//! At-rest viz coordinators: refresh the session-held viz channels from the
//! plugin queries.
//!
//! Unlike the pure decoders beside them, these take explicit core borrows
//! (`RunnerClient`, `Session`, `ViewOptions`): each fires its plugin query,
//! decodes the reply through the sibling decoders, and writes the result into
//! the session's [`crate::session::VizState`] (marking it dirty for the render
//! projector to push). The caller gates each on the engine being present; the
//! functions themselves no longer check for it.

use crate::runner_client::RunnerClient;
use crate::session::Session;

type ViewOptions = viso::options::VisoOptions;

/// Refresh the held rendering connections (hbonds, disulfides, clashes)
/// from the plugin's `connections` query and choose the per-publish
/// connection provider.
pub fn refresh_connections(runner_client: &mut RunnerClient, store: &mut Session) {
    // Connections are loaded into Foldit via a plugin query,
    // namely from Rosetta- could eventually be done directly in molex
    //
    // Falls back to naive molex connection implementations in the case
    // that the Rosetta plugin is not present
    if !runner_client.supports_query("connections") {
        store.viz.held_connections = None;
        store.viz.connections_topology_ids.clear();
        return;
    }

    // Collect entity ids from the current session's assembly
    let head_ids: std::collections::BTreeSet<molex::EntityId> = store
        .head_assembly()
        .entities()
        .iter()
        .map(|e| e.id())
        .collect();

    if head_ids != store.viz.connections_topology_ids {
        store.viz.held_connections = None;
        store.viz.connections_topology_ids = head_ids;
    }

    // Query Rosetta plugin for all connection data
    let bytes = runner_client.request_query_bytes("connections");

    let held = if bytes.is_empty() {
        std::collections::HashMap::new()
    } else {
        <foldit_runner::proto::plugin::ConnectionReport as prost::Message>::decode(bytes.as_slice())
            .map_or_else(
                |_| std::collections::HashMap::new(),
                |report| {
                    crate::viz::connections::connections_from_report(&report, &store.head_assembly())
                },
            )
    };
    store.viz.held_connections = Some(held);
}

/// Refresh the cached external (host-supplied) void field from the
/// plugin's `voids` query and mark the overlay cache dirty. Runs on the
/// at-rest geometry gate (or a cavity-toggle flip), so the voids track the
/// committed pose; the render projector pushes the cached field to the
/// engine on the drain when the cache is dirty.
pub fn refresh_external_cavities(
    runner_client: &mut RunnerClient,
    store: &mut Session,
    view_options: &ViewOptions,
) {
    // Cavities hidden: store the cleared field (the engine reads it as
    // "drop the external set") and mark dirty so the toggle-off propagates.
    if !view_options.display.show_cavities() {
        store.viz.void_field = crate::viz::voids::VoidFieldData::default();
        store.viz.viz_dirty = true;
        return;
    }

    if !runner_client.supports_query("voids") {
        return;
    }

    // Run the voids query to the rosetta plugin
    let bytes = runner_client.request_query_bytes("voids");

    store.viz.void_field = crate::viz::voids::void_field_from_bytes(&bytes);
    store.viz.viz_dirty = true;
}

/// Refresh the cached steric-clash arcs from the plugin's `clashes`
/// query and mark the overlay cache dirty. Runs on the at-rest geometry
/// gate (or a clash-toggle flip), so the clashes track the committed pose;
/// the render projector pushes the cached arcs to the engine on the drain
/// when the cache is dirty.
pub fn refresh_clashes(
    runner_client: &mut RunnerClient,
    store: &mut Session,
    view_options: &ViewOptions,
) {
    // Clash display off: store the cleared (empty) set, before any query
    // work, so toggling clashes off removes them.
    if !view_options.display.show_clashes() {
        store.viz.clashes = Vec::new();
        store.viz.viz_dirty = true;
        return;
    }
    // No plugin advertises `clashes`: store the cleared (empty) set, so
    // swapping to a clash-less structure removes stale arcs.
    if !runner_client.supports_query("clashes") {
        store.viz.clashes = Vec::new();
        store.viz.viz_dirty = true;
        return;
    }
    let bytes = runner_client.request_query_bytes("clashes");
    let report = crate::viz::clashes::clashes_from_bytes(&bytes);
    let mut infos: Vec<viso::ClashInfo> = Vec::with_capacity(report.clashes.len());
    for clash in &report.clashes {
        // Map both endpoints; drop the whole clash if either endpoint's
        // entity_id does not resolve to a current entity (a panel can race
        // a structure swap, leaving a stale id).
        let (Some(a), Some(b)) = (clash_endpoint(store, &clash.a), clash_endpoint(store, &clash.b))
        else {
            continue;
        };
        infos.push(viso::ClashInfo {
            a,
            b,
            severity: clash.severity,
        });
    }
    store.viz.clashes = infos;
    store.viz.viz_dirty = true;
}

/// Map a decoded clash endpoint into a viso [`viso::ClashEndpoint`],
/// resolving the proto `entity_id` (`u64`) to a live molex `EntityId`. The
/// entity-local `residue_index` and `atom_name` pass straight through; the
/// host computes no flat residue index (viso resolves the per-entity ref).
/// Returns `None` when the `entity_id` matches no current entity.
fn clash_endpoint(
    store: &Session,
    end: &crate::viz::clashes::ClashEnd,
) -> Option<viso::ClashEndpoint> {
    let entity = store.resolve_entity(end.entity_id)?;
    Some(viso::ClashEndpoint {
        entity,
        residue: end.residue_index,
        atom_name: end.atom_name.clone(),
    })
}

/// Refresh the cached exposed-hydrophobic grease beads and the loaded
/// puzzle's met-filter bonus from the plugin's `exposed_hydrophobics`
/// query. Runs on the at-rest geometry gate (or an
/// exposed-hydrophobic-toggle flip), so the flagged residues and the
/// filter count track the committed pose. The bead overlay is cached and
/// marked dirty for the render projector to push on the drain; the
/// met-filter bonus is a direct session write (it feeds scoring, not viz).
pub fn refresh_exposed_hydrophobics(
    runner_client: &mut RunnerClient,
    store: &mut Session,
    view_options: &ViewOptions,
) {
    let show = view_options.display.show_exposed_hydrophobics();

    let filter_active = store.puzzle().is_some_and(|p| {
        p.filters
            .iter()
            .any(|f| f.kind == "ExposedCount" && f.plugin.is_none())
    });

    if !show && !filter_active {
        store.viz.exposed_hydrophobics = Vec::new();
        store.viz.viz_dirty = true;
        store.set_filter_bonus(Vec::new());
        return;
    }

    // No plugin advertises `exposed_hydrophobics`: store the cleared
    // (empty) set, clear the bonus, and stop, so swapping to a
    // detector-less structure removes stale beads and drops a stale bonus.
    if !runner_client.supports_query("exposed_hydrophobics") {
        store.viz.exposed_hydrophobics = Vec::new();
        store.viz.viz_dirty = true;
        store.set_filter_bonus(Vec::new());
        return;
    }

    // Run the Rosetta query for exposed hydrophobics
    let bytes = runner_client.request_query_bytes("exposed_hydrophobics");

    let report = crate::viz::exposed_hydrophobics::exposed_from_bytes(&bytes);
    let count = u32::try_from(report.exposed.len()).unwrap_or(u32::MAX);
    let bonus = store.puzzle().map_or(0.0, |p| {
        crate::app::score_apply::exposed_count_bonus(&p.filters, count)
    });

    if bonus == 0.0 {
        store.set_filter_bonus(Vec::new());
    } else {
        store.set_filter_bonus(vec![("exposed_count".to_owned(), bonus)]);
    }

    if !show {
        store.viz.exposed_hydrophobics = Vec::new();
        store.viz.viz_dirty = true;
        return;
    }

    let mut infos: Vec<viso::ExposedHydrophobicInfo> = Vec::with_capacity(report.exposed.len());
    for residue in &report.exposed {
        let Some(entity) = store.resolve_entity(residue.entity_id) else {
            continue;
        };
        infos.push(viso::ExposedHydrophobicInfo {
            entity,
            residue: residue.residue_index,
        });
    }
    store.viz.exposed_hydrophobics = infos;
    store.viz.viz_dirty = true;
}

/// Refresh the engine's per-residue non-designable overlay from the loaded
/// puzzle's design gating. The overlay desaturates locked residues toward
/// white so the player can see which parts of the structure may not be
/// mutated. The caller passes a present engine; this no longer gates on one.
pub fn refresh_design_gating(store: &Session, engine: &mut viso::VisoEngine) {
    use std::collections::{BTreeMap, BTreeSet};

    if !store.design_gating_active() {
        engine.set_non_designable(&BTreeMap::new());
        return;
    }

    // Build the non-designable set per entity from the head assembly's
    // residue counts: every residue the session reports as not designable.
    let head = store.head_assembly();
    let mut non_designable: BTreeMap<molex::EntityId, BTreeSet<u32>> = BTreeMap::new();
    for entity in head.entities() {
        let eid = entity.id();
        let count = u32::try_from(entity.residue_count()).unwrap_or(u32::MAX);
        let locked: BTreeSet<u32> = (0..count)
            .filter(|&res| !store.is_designable(eid, res))
            .collect();
        if !locked.is_empty() {
            non_designable.insert(eid, locked);
        }
    }

    engine.set_non_designable(&non_designable);
}
