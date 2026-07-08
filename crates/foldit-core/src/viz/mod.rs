//! Plugin-sourced viz: the pure proto -> struct decoders (`clashes`,
//! `connections`, `exposed_hydrophobics`, `voids`) plus [`Viz`], the App-owned
//! overlay cache that drives them from the at-rest plugin queries.

use std::collections::{BTreeSet, HashMap};

use crate::app::score_coordinator::ScoreCoordinator;
use crate::runner_client::RunnerClient;
use crate::session::Session;

pub mod clashes;
pub mod connections;
pub mod exposed_hydrophobics;
pub mod voids;

type ViewOptions = viso::options::VisoOptions;

/// App-owned derived overlay cache. Regenerated from the structure via plugin
/// queries; never serialized or history-versioned, cleared by [`Self::reset`].
/// Holds the connections set the render projector stamps on each publish and
/// the three structural-viz overlay payloads (voids, clashes, exposed
/// beads) the engine receives on [`Self::push`] when [`Self::dirty`] is set.
#[derive(Default)]
pub struct Viz {
    /// Held rendering connections (hbonds + disulfides). `Some` means a plugin
    /// is the live provider and this atom-index map is stamped verbatim; `None`
    /// falls back to molex's geometric detection per publish.
    connections: Option<HashMap<molex::ConnectionType, Vec<molex::AtomLink>>>,
    /// Entity-id set the held connections were queried for; a head topology
    /// change invalidates the held map.
    connections_topology_ids: BTreeSet<molex::EntityId>,
    /// External void distance field; the cleared form clears the engine's set.
    void_field: voids::VoidFieldData,
    /// Steric-clash arcs resolved to viso endpoints; empty clears the arcs.
    clashes: Vec<viso::ClashInfo>,
    /// Exposed-hydrophobic grease beads resolved to viso refs; empty clears.
    exposed: Vec<viso::ExposedHydrophobicInfo>,
    /// Set when any overlay payload changed since the last [`Self::push`].
    dirty: bool,
}

impl Viz {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Drop the whole overlay cache back to its empty initial state.
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }

    /// The held connections map for the render projector to stamp, or `None`
    /// when no plugin provides them.
    pub(crate) const fn held_connections(
        &self,
    ) -> Option<&HashMap<molex::ConnectionType, Vec<molex::AtomLink>>> {
        self.connections.as_ref()
    }

    /// Refresh the held rendering connections from the plugin's `connections`
    /// query. Synchronous (decoded inline). Clears the held set when no plugin
    /// advertises the query; drops it on a head topology change. Assumes a
    /// present engine; gating is the caller's.
    pub(crate) fn refresh_connections(&mut self, rc: &mut RunnerClient, session: &Session) {
        if !rc.supports_query("connections") {
            self.connections = None;
            self.connections_topology_ids.clear();
            return;
        }

        let head_ids: BTreeSet<molex::EntityId> = session
            .head_assembly()
            .entities()
            .iter()
            .map(|e| e.id())
            .collect();
        if head_ids != self.connections_topology_ids {
            self.connections = None;
            self.connections_topology_ids = head_ids;
        }

        let bytes = rc.request_query_bytes("connections");
        let held = if bytes.is_empty() {
            HashMap::new()
        } else {
            <foldit_runner::proto::plugin::ConnectionReport as prost::Message>::decode(
                bytes.as_slice(),
            )
            .map_or_else(
                |_| HashMap::new(),
                |report| connections::connections_from_report(&report, &session.head_assembly()),
            )
        };
        self.connections = Some(held);
    }

    /// Fire the three structural-viz overlay queries against the at-rest pose.
    /// Each overlay is an immediate clear when hidden / unsupported, otherwise
    /// an async request whose reply [`Self::apply_replies`] applies. Assumes a
    /// present engine; gating is the caller's.
    pub(crate) fn step(
        &mut self,
        rc: &mut RunnerClient,
        session: &Session,
        scores: &mut ScoreCoordinator,
        view_options: &ViewOptions,
    ) {
        fire::<Voids>(rc, view_options, &mut self.void_field, &mut self.dirty);
        fire::<Clashes>(rc, view_options, &mut self.clashes, &mut self.dirty);
        self.fire_exposed(rc, session, scores, view_options);
    }

    /// Fire every viz channel once for a freshly-settled session: the
    /// synchronous connections refresh plus the three overlay queries (inert
    /// when their toggles are off or no plugin advertises them).
    pub(crate) fn replay(
        &mut self,
        rc: &mut RunnerClient,
        session: &Session,
        scores: &mut ScoreCoordinator,
        view_options: &ViewOptions,
    ) {
        self.refresh_connections(rc, session);
        self.step(rc, session, scores, view_options);
    }

    /// Fire the `exposed_hydrophobics` query, which serves both the bead
    /// overlay (gated on its toggle) and the puzzle met-filter bonus (gated on
    /// an active `ExposedCount` filter), so it requests on `show || filter`.
    /// When neither is wanted, or no plugin advertises it, clear the beads and
    /// drop the `exposed_count` bonus entry as two explicit writes.
    fn fire_exposed(
        &mut self,
        rc: &mut RunnerClient,
        session: &Session,
        scores: &mut ScoreCoordinator,
        view_options: &ViewOptions,
    ) {
        let show = view_options.display.show_exposed_hydrophobics();
        let filter_active = session.puzzle().is_some_and(|p| {
            p.filters
                .iter()
                .any(|f| f.kind == "ExposedCount" && f.plugin.is_none())
        });
        if (!show && !filter_active) || !rc.supports_query("exposed_hydrophobics") {
            self.exposed = Vec::new();
            self.dirty = true;
            scores.set_filter_bonus_entry("exposed_count", 0.0);
            return;
        }
        rc.request_query("exposed_hydrophobics");
    }

    /// Apply async query replies drained from the orchestrator into the overlay
    /// cache, marking it dirty. `voids` / `clashes` route through the generic
    /// [`apply`]; `exposed_hydrophobics` decodes once and feeds both the score
    /// (the met-filter bonus, unconditional) and the bead overlay (gated on its
    /// toggle). `connections` stays synchronous; any other id is ignored.
    pub(crate) fn apply_replies(
        &mut self,
        session: &Session,
        scores: &mut ScoreCoordinator,
        view_options: &ViewOptions,
        results: Vec<(String, Vec<u8>)>,
    ) {
        for (id, bytes) in results {
            match id.as_str() {
                "voids" => apply::<Voids>(&bytes, session, &mut self.void_field, &mut self.dirty),
                "clashes" => apply::<Clashes>(&bytes, session, &mut self.clashes, &mut self.dirty),
                "exposed_hydrophobics" => {
                    let report = exposed_hydrophobics::exposed_from_bytes(&bytes);
                    apply_exposed_score(session, scores, report.exposed.len());
                    self.exposed = if view_options.display.show_exposed_hydrophobics() {
                        resolve_exposed(session, &report)
                    } else {
                        Vec::new()
                    };
                    self.dirty = true;
                }
                other => log::trace!("apply_replies: ignoring query id '{other}'"),
            }
        }
    }

    /// Push the overlay cache to the engine and clear the dirty flag. No-op
    /// when clean.
    pub(crate) fn push(&mut self, engine: &mut viso::VisoEngine) {
        if !self.dirty {
            return;
        }
        Voids::push(engine, &self.void_field);
        Clashes::push(engine, &self.clashes);
        engine.update_exposed_hydrophobics(self.exposed.clone());
        self.dirty = false;
    }
}

/// A structural-viz overlay whose at-rest refresh follows the same
/// toggle/supports/fire-then-decode template. The diverging pieces (display
/// toggle, decode + resolve, engine channel) are the methods.
trait OverlayKind {
    const QUERY: &'static str;
    type Payload: Default;
    fn shown(view: &ViewOptions) -> bool;
    fn decode_resolve(bytes: &[u8], session: &Session) -> Self::Payload;
    fn push(engine: &mut viso::VisoEngine, payload: &Self::Payload);
}

struct Voids;
impl OverlayKind for Voids {
    const QUERY: &'static str = "voids";
    type Payload = voids::VoidFieldData;
    fn shown(view: &ViewOptions) -> bool {
        view.display.show_cavities()
    }
    fn decode_resolve(bytes: &[u8], _session: &Session) -> Self::Payload {
        voids::void_field_from_bytes(bytes)
    }
    fn push(engine: &mut viso::VisoEngine, payload: &Self::Payload) {
        engine.set_external_void_field(
            payload.dims,
            payload.origin,
            payload.spacing,
            payload.phi.clone(),
            payload.threshold,
        );
    }
}

struct Clashes;
impl OverlayKind for Clashes {
    const QUERY: &'static str = "clashes";
    type Payload = Vec<viso::ClashInfo>;
    fn shown(view: &ViewOptions) -> bool {
        view.display.show_clashes()
    }
    fn decode_resolve(bytes: &[u8], session: &Session) -> Self::Payload {
        // Drop the whole clash if either endpoint's entity no longer resolves
        // (a panel can race a structure swap, leaving a stale id).
        clashes::clashes_from_bytes(bytes)
            .clashes
            .iter()
            .filter_map(|clash| {
                let a = clash_endpoint(session, &clash.a)?;
                let b = clash_endpoint(session, &clash.b)?;
                Some(viso::ClashInfo {
                    a,
                    b,
                    severity: clash.severity,
                })
            })
            .collect()
    }
    fn push(engine: &mut viso::VisoEngine, payload: &Self::Payload) {
        engine.update_clashes(payload.clone());
    }
}

/// Fire (or synchronously clear) one overlay query. Clears the slot and marks
/// dirty when the overlay is hidden or unsupported; otherwise requests it.
fn fire<K: OverlayKind>(
    rc: &mut RunnerClient,
    view: &ViewOptions,
    slot: &mut K::Payload,
    dirty: &mut bool,
) {
    if !K::shown(view) || !rc.supports_query(K::QUERY) {
        *slot = K::Payload::default();
        *dirty = true;
    } else {
        rc.request_query(K::QUERY);
    }
}

/// Decode and resolve one overlay reply into its slot, marking dirty.
fn apply<K: OverlayKind>(bytes: &[u8], session: &Session, slot: &mut K::Payload, dirty: &mut bool) {
    *slot = K::decode_resolve(bytes, session);
    *dirty = true;
}

/// Resolve a decoded clash endpoint to a viso endpoint, mapping the proto
/// `entity_id` to a live molex `EntityId`. `None` when the id matches no
/// current entity.
fn clash_endpoint(session: &Session, end: &clashes::ClashEnd) -> Option<viso::ClashEndpoint> {
    let entity = session.resolve_entity(end.entity_id)?;
    Some(viso::ClashEndpoint {
        entity,
        residue: end.residue_index,
        atom_name: end.atom_name.clone(),
    })
}

/// Recompute the puzzle met-filter bonus from the raw flagged `count` and upsert
/// it under the `exposed_count` label. The score consumer of the
/// `exposed_hydrophobics` reply, independent of the bead overlay toggle. The
/// `filter_bonus` channel has other writers (the async `r_free` result, and any
/// future labeled objective), so this stays label-scoped and never bulk-replaces
/// the channel, which would clobber their entries.
fn apply_exposed_score(session: &Session, scores: &mut ScoreCoordinator, count: usize) {
    let count = u32::try_from(count).unwrap_or(u32::MAX);
    let bonus = session.puzzle().map_or(0.0, |p| {
        crate::scores::exposed_count_bonus(&p.filters, count)
    });
    scores.set_filter_bonus_entry("exposed_count", bonus);
}

/// Resolve a decoded exposed-hydrophobic report into viso bead refs, dropping
/// residues whose entity no longer resolves.
fn resolve_exposed(
    session: &Session,
    report: &exposed_hydrophobics::ExposedHydroData,
) -> Vec<viso::ExposedHydrophobicInfo> {
    report
        .exposed
        .iter()
        .filter_map(|residue| {
            let entity = session.resolve_entity(residue.entity_id)?;
            Some(viso::ExposedHydrophobicInfo {
                entity,
                residue: residue.residue_index,
            })
        })
        .collect()
}

/// Refresh the engine's per-residue non-designable overlay from the puzzle's
/// design gating, desaturating locked residues. Static per puzzle; assumes a
/// present engine, gating is the caller's.
pub fn refresh_design_gating(session: &Session, engine: &mut viso::VisoEngine) {
    use std::collections::BTreeMap;

    if !session.design_gating_active() {
        engine.set_non_designable(&BTreeMap::new());
        return;
    }

    let head = session.head_assembly();
    let mut non_designable: BTreeMap<molex::EntityId, BTreeSet<u32>> = BTreeMap::new();
    for entity in head.entities() {
        let eid = entity.id();
        let count = u32::try_from(entity.residue_count()).unwrap_or(u32::MAX);
        let locked: BTreeSet<u32> = (0..count)
            .filter(|&res| !session.is_designable(eid, res))
            .collect();
        if !locked.is_empty() {
            non_designable.insert(eid, locked);
        }
    }

    engine.set_non_designable(&non_designable);
}
