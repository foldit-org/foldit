//! Foldit crystallography plugin.
//!
//! * builds `ExperimentalData` from the structure-factor cif delivered at Init,
//! * computes the electron-density map and publishes it through the well-known
//!   `density` query (the host stores it and forwards it to `uses_density`
//!   plugins),
//! * runs B-factor refinement as the streaming `refine_b` op,
//! * reports R-free, and its puzzle-objective bonus, through the well-known
//!   `score` query.
//!
//! The plugin creates its own wgpu device: a `wgpu::Device` is process-local
//! and cannot be shared across the worker boundary.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use foldit_plugin_sdk::proto::plugin as proto;
use foldit_plugin_sdk::{
    export_plugin, AssemblyPayload, DispatchContext, ParamValue, Plugin, PluginError, Result,
};
use molex::adapters::table::AtomTable;
use molex::xtal::{ExperimentalData, WgpuDevice};
use molex::{Assembly, MoleculeEntity};
use prost::Message as _;

/// Streaming op id. Must match `[[buttons]].op` in `plugin.toml`.
const OP_REFINE_B: &str = "refine_b";
/// Well-known query: publish the density map back to the host.
const QUERY_DENSITY: &str = "density";
/// Well-known query: every scoring plugin registers this.
const QUERY_SCORE: &str = "score";

/// Init asset whose name ends here carries the structure-factor cif text.
const SF_CIF_SUFFIX: &str = ".sf.cif";
/// Init asset whose name ends here carries the coordinate cif, read for the
/// space group when the puzzle declares no explicit override.
const COORD_CIF_SUFFIX: &str = ".coord.cif";
/// Explicit space-group override from `[puzzle.reflns]`.
const PARAM_SPACE_GROUP: &str = "xtal.space_group";
/// Puzzle name; seeds the deterministic R-free flag set.
const PARAM_PUZZLE_NAME: &str = "xtal.name";

/// Outer refinement cycles.
const N_MACRO_CYCLES: usize = 5;

/// Raw rosetta energy per unit of game score. `bonus_breakdown` entries carry
/// raw energy, so a game reward converts back through this factor.
const GAME_PER_RAW: f64 = -10.0;

fn op_err(code: &str, message: impl Into<String>) -> PluginError {
    PluginError::Op {
        code: code.to_owned(),
        message: message.into(),
    }
}

// ---------------------------------------------------------------------------
// Puzzle-objective bonus, forwarded from the host as `filter.<i>.*` params.
// ---------------------------------------------------------------------------

/// The `rfree_bonus` filter this puzzle forwarded to us, if any. Reward decays
/// to zero once `r_free` reaches `target`, so it is always non-negative.
#[derive(Debug, Clone, Copy)]
struct RFreeBonus {
    target: f64,
    weight: f64,
    exponent: f64,
}

impl RFreeBonus {
    fn reward_game(self, r_free: f64) -> f64 {
        self.weight * (self.target - r_free).max(0.0).powf(self.exponent)
    }
}

/// Filters arrive flattened as `filter.<i>.type` plus `filter.<i>.<key>`, all
/// String-typed. Only a filter this plugin owns is forwarded, so a matching
/// `type` is enough to claim it.
fn parse_rfree_bonus(params: &HashMap<String, ParamValue>) -> Option<RFreeBonus> {
    let as_f64 = |k: &str| -> Option<f64> {
        match params.get(k)? {
            ParamValue::Float(f) => Some(f64::from(*f)),
            ParamValue::Int(i) => Some(f64::from(*i)),
            ParamValue::String(s) => s.parse().ok(),
            _ => None,
        }
    };
    for i in 0..64 {
        let kind = match params.get(&format!("filter.{i}.type")) {
            Some(ParamValue::String(s)) => s.clone(),
            _ => continue,
        };
        if kind != "rfree_bonus" {
            continue;
        }
        return Some(RFreeBonus {
            target: as_f64(&format!("filter.{i}.target"))?,
            weight: as_f64(&format!("filter.{i}.weight"))?,
            // Forward-compatible: an absent exponent is quadratic.
            exponent: as_f64(&format!("filter.{i}.exponent")).unwrap_or(2.0),
        });
    }
    None
}

// ---------------------------------------------------------------------------
// Session + stream state
// ---------------------------------------------------------------------------

/// The map this plugin last computed, in the shape the host's `DensityAsset`
/// expects.
#[derive(Clone)]
struct DensityMap {
    name: String,
    bytes: Vec<u8>,
    resolution: f32,
}

struct Session {
    assembly: Assembly,
    /// `None` when the puzzle shipped no usable reflections; every xtal op then
    /// reports `NO_EXPERIMENTAL_DATA` rather than silently no-opping.
    data: Option<Arc<ExperimentalData>>,
    map_name: String,
    density: Option<DensityMap>,
    rfree_bonus: Option<RFreeBonus>,
}

/// Progress ticks from molex's solver. Both loops converge early, so neither
/// count is a fixed denominator; the fraction fills within a cycle and resets
/// when the cycle advances.
#[derive(Default, Clone, Copy)]
struct Progress {
    macro_cycle: usize,
    inner_iter: usize,
    inner_total: usize,
}

enum Outcome {
    Done {
        full_b: Vec<f32>,
        r_work: f64,
        r_free: f64,
    },
    Failed(String),
    Cancelled,
}

struct RefineStream {
    session: u64,
    progress: Arc<Mutex<Progress>>,
    cancel: Arc<AtomicBool>,
    outcome: Arc<Mutex<Option<Outcome>>>,
}

// ---------------------------------------------------------------------------

struct XtalPlugin {
    /// This process's own device. `None` routes every kernel to the CPU path.
    device: Option<WgpuDevice>,
    sessions: Mutex<HashMap<u64, Session>>,
    streams: Mutex<HashMap<u64, RefineStream>>,
    next_session: AtomicU64,
}

impl XtalPlugin {
    fn new() -> Self {
        // cubecl initialises the runtime lazily on first use.
        Self {
            device: Some(WgpuDevice::default()),
            sessions: Mutex::new(HashMap::new()),
            streams: Mutex::new(HashMap::new()),
            next_session: AtomicU64::new(0),
        }
    }

    fn sessions(&self) -> Result<MutexGuard<'_, HashMap<u64, Session>>> {
        self.sessions
            .lock()
            .map_err(|_| op_err("POISONED", "xtal session state was poisoned by a panic"))
    }

    fn streams(&self) -> Result<MutexGuard<'_, HashMap<u64, RefineStream>>> {
        self.streams
            .lock()
            .map_err(|_| op_err("POISONED", "xtal stream state was poisoned by a panic"))
    }

    fn density_grid(&self, data: &ExperimentalData, table: &AtomTable) -> Option<molex::xtal::DensityGrid> {
        self.device.as_ref().map_or_else(
            || {
                log::info!("[xtal] density on CPU (no device)");
                molex::xtal::density_from_atom_table(data, table)
            },
            |dev| molex::xtal::density_from_atom_table_gpu(data, table, dev),
        )
    }

    fn r_factors(&self, data: &ExperimentalData, table: &AtomTable) -> Option<(f64, f64)> {
        self.device.as_ref().map_or_else(
            || molex::xtal::r_factors_from_atom_table(data, table),
            |dev| molex::xtal::r_factors_from_atom_table_gpu(data, table, dev),
        )
    }

    /// Recompute the full-cell map for `session`'s current model. The host
    /// re-crops it for rendering.
    fn recompute_density(&self, session: &mut Session) {
        let Some(data) = session.data.clone() else {
            return;
        };
        let entities: Vec<MoleculeEntity> = session
            .assembly
            .entities()
            .iter()
            .map(|e| (**e).clone())
            .collect();
        let table = AtomTable::from_entities(&entities);
        let Some(grid) = self.density_grid(&data, &table) else {
            log::warn!("[xtal] density computation failed; keeping the prior map");
            return;
        };
        let density =
            molex::xtal::density_from_grid(&grid, &data.unit_cell, data.space_group_number());
        #[allow(clippy::cast_possible_truncation)]
        let resolution = data.d_min() as f32;
        session.density = Some(DensityMap {
            name: session.map_name.clone(),
            bytes: molex::adapters::mrc::density_to_mrc_bytes(&density),
            resolution,
        });
    }

    /// Score `entities` against the session's reflections: R-work / R-free plus
    /// the puzzle's forwarded R-free bonus, already converted to raw energy.
    fn score_report(&self, session: &Session, entities: &[MoleculeEntity]) -> proto::ScoreReport {
        let mut report = proto::ScoreReport::default();
        let Some(data) = session.data.as_ref() else {
            return report;
        };
        let table = AtomTable::from_entities(entities);
        let Some((r_work, r_free)) = self.r_factors(data, &table) else {
            return report;
        };
        #[allow(clippy::cast_possible_truncation)]
        {
            report.term_names = vec!["r_work".to_owned(), "r_free".to_owned()];
            report.whole_pose_terms = vec![r_work as f32, r_free as f32];
        }
        if let Some(bonus) = session.rfree_bonus {
            // `bonus_breakdown` is raw energy; the host adds it into the total.
            #[allow(clippy::cast_possible_truncation)]
            let raw = (bonus.reward_game(r_free) / GAME_PER_RAW) as f32;
            report.bonus_breakdown = vec![proto::BonusContribution {
                kind: "r_free".to_owned(),
                value: raw,
            }];
        }
        report
    }
}

/// Resolve the space group: an explicit `[puzzle.reflns]` override wins,
/// otherwise read `_symmetry.space_group_name_H-M` off the coordinate cif
/// (falling back to `_space_group.name_H-M_full`).
fn resolve_space_group(
    params: &HashMap<String, ParamValue>,
    coord_cif: Option<&str>,
) -> Option<u16> {
    if let Some(ParamValue::Int(sg)) = params.get(PARAM_SPACE_GROUP) {
        return u16::try_from(*sg).ok();
    }
    let doc = molex::adapters::cif::parse(coord_cif?).ok()?;
    let block = doc.blocks.first()?;
    let hm = block
        .get("_symmetry.space_group_name_H-M")
        .and_then(molex::adapters::cif::Value::as_str)
        .or_else(|| {
            block
                .get("_space_group.name_H-M_full")
                .and_then(molex::adapters::cif::Value::as_str)
        })?;
    molex::xtal::space_group_number_from_name(hm)
}

fn asset_text<'a>(assets: &'a [proto::PuzzleAsset], suffix: &str) -> Option<&'a str> {
    let asset = assets.iter().find(|a| a.name.ends_with(suffix))?;
    std::str::from_utf8(&asset.data).ok()
}

impl Plugin for XtalPlugin {
    fn init(
        &self,
        assembly_bytes: &[u8],
        assets: &[proto::PuzzleAsset],
        params: &HashMap<String, ParamValue>,
    ) -> Result<(u64, Vec<u8>)> {
        let assembly = Assembly::from_bytes(assembly_bytes)
            .map_err(|e| op_err("INVALID_ASSEMBLY", e.to_string()))?;

        let name = match params.get(PARAM_PUZZLE_NAME) {
            Some(ParamValue::String(s)) => s.clone(),
            _ => "puzzle".to_owned(),
        };

        // A puzzle with no structure factors is a normal, non-xtal puzzle: keep
        // the session so the ops refuse with a clear code.
        let data = asset_text(assets, SF_CIF_SUFFIX).and_then(|sf_text| {
            let sg = resolve_space_group(params, asset_text(assets, COORD_CIF_SUFFIX))?;
            let seed = molex::xtal::deterministic_free_flag_seed(&name);
            ExperimentalData::from_sf_cif_with_spacegroup(sf_text, sg, 0.05, seed).map(Arc::new)
        });
        if data.is_none() {
            log::info!("[xtal] no usable reflections for '{name}'; xtal ops will refuse");
        }

        let mut session = Session {
            assembly,
            data,
            map_name: format!("{name}-density.mrc"),
            density: None,
            rfree_bonus: parse_rfree_bonus(params),
        };
        self.recompute_density(&mut session);

        let id = self.next_session.fetch_add(1, Ordering::Relaxed) + 1;
        let _ = self.sessions()?.insert(id, session);
        // No post-init normalization: the host keeps its input assembly.
        Ok((id, Vec::new()))
    }

    fn register(&self) -> Result<proto::PluginRegistration> {
        Ok(proto::PluginRegistration {
            id: "xtal".to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            operations: vec![proto::PluginOp {
                id: OP_REFINE_B.to_owned(),
                display_name: "Refine B".to_owned(),
                description: "Refine per-atom B-factors against the experimental map".to_owned(),
                kind: proto::OpKind::Stream as i32,
                params: vec![],
                compatible_focus_types: vec![],
                creates_entities: false,
                requires_focus: false,
                ui: None,
            }],
            queries: vec![
                proto::PluginQuery {
                    id: QUERY_DENSITY.to_owned(),
                    display_name: "Density map".to_owned(),
                    description: "The experimental-weighted electron-density map".to_owned(),
                    params: vec![],
                },
                proto::PluginQuery {
                    id: QUERY_SCORE.to_owned(),
                    display_name: "Crystallographic score".to_owned(),
                    description: "R-work / R-free and the puzzle's R-free bonus".to_owned(),
                    params: vec![],
                },
            ],
        })
    }

    fn update_assembly(
        &self,
        session: u64,
        payload: AssemblyPayload<'_>,
        _from_gen: u64,
        _to_gen: u64,
    ) -> Result<()> {
        let mut sessions = self.sessions()?;
        let s = sessions
            .get_mut(&session)
            .ok_or_else(|| op_err("UNKNOWN_SESSION", format!("no session {session}")))?;
        match payload {
            AssemblyPayload::Full(bytes) => {
                s.assembly = Assembly::from_bytes(bytes)
                    .map_err(|e| op_err("INVALID_ASSEMBLY", e.to_string()))?;
            }
            AssemblyPayload::Delta(bytes) => {
                let edits = molex::ops::wire::delta::deserialize_edits(bytes)
                    .map_err(|e| op_err("INVALID_DELTA", e.to_string()))?;
                s.assembly
                    .apply_edits(&edits)
                    .map_err(|e| op_err("APPLY_DELTA_FAILED", e.to_string()))?;
            }
        }
        Ok(())
    }

    fn drop_session(&self, session: u64) -> Result<()> {
        let _ = self.sessions()?.remove(&session);
        self.streams()?.retain(|_, st| st.session != session);
        Ok(())
    }

    fn start_stream(
        &self,
        session: u64,
        op: &str,
        _ctx: &DispatchContext,
        _params: &HashMap<String, ParamValue>,
        request_id: u64,
    ) -> Result<()> {
        if op != OP_REFINE_B {
            return Err(PluginError::Unsupported);
        }
        let (data, entities) = {
            let sessions = self.sessions()?;
            let s = sessions
                .get(&session)
                .ok_or_else(|| op_err("UNKNOWN_SESSION", format!("no session {session}")))?;
            let data = s.data.clone().ok_or_else(|| {
                op_err(
                    "NO_EXPERIMENTAL_DATA",
                    "this puzzle shipped no usable reflections",
                )
            })?;
            let entities: Vec<MoleculeEntity> =
                s.assembly.entities().iter().map(|e| (**e).clone()).collect();
            (data, entities)
        };

        let table = AtomTable::from_entities(&entities);
        let progress = Arc::new(Mutex::new(Progress::default()));
        let cancel = Arc::new(AtomicBool::new(false));
        let outcome: Arc<Mutex<Option<Outcome>>> = Arc::new(Mutex::new(None));
        let device = self.device.clone();

        // `tick_cancel` is consumed by the progress closure molex owns;
        // `thread_cancel` distinguishes an aborted solve from a failed one.
        let tp = Arc::clone(&progress);
        let tick_cancel = Arc::clone(&cancel);
        let thread_cancel = Arc::clone(&cancel);
        let to = Arc::clone(&outcome);
        std::thread::spawn(move || {
            let tick = move |macro_cycle, inner_iter, inner_total| {
                if let Ok(mut p) = tp.lock() {
                    *p = Progress {
                        macro_cycle,
                        inner_iter,
                        inner_total,
                    };
                }
                // Returning `false` aborts the molex solve.
                !tick_cancel.load(Ordering::Relaxed)
            };
            let result = match device.as_ref() {
                Some(dev) => molex::xtal::refine_b_from_atom_table_gpu(
                    &data,
                    &table,
                    N_MACRO_CYCLES,
                    dev,
                    tick,
                ),
                None => {
                    molex::xtal::refine_b_from_atom_table(&data, &table, N_MACRO_CYCLES, tick)
                }
            };
            // An aborted solve returns `None`; the flag tells it apart from a
            // genuine failure.
            let done = if thread_cancel.load(Ordering::Relaxed) {
                Outcome::Cancelled
            } else {
                match result {
                    Some((full_b, r_work, r_free)) => Outcome::Done {
                        full_b,
                        r_work,
                        r_free,
                    },
                    None => Outcome::Failed("B-factor refinement failed".to_owned()),
                }
            };
            if let Ok(mut slot) = to.lock() {
                *slot = Some(done);
            }
        });

        let _ = self.streams()?.insert(
            request_id,
            RefineStream {
                session,
                progress,
                cancel,
                outcome,
            },
        );
        Ok(())
    }

    fn poll_stream(&self, request_id: u64) -> Result<foldit_plugin_sdk::PollOutcome> {
        use foldit_plugin_sdk::PollOutcome;

        let (session, progress, taken) = {
            let streams = self.streams()?;
            let st = streams
                .get(&request_id)
                .ok_or_else(|| op_err("UNKNOWN_REQUEST", format!("no stream {request_id}")))?;
            let progress = *st
                .progress
                .lock()
                .map_err(|_| op_err("POISONED", "refine progress poisoned"))?;
            let taken = st
                .outcome
                .lock()
                .map_err(|_| op_err("POISONED", "refine outcome poisoned"))?
                .take();
            (st.session, progress, taken)
        };

        let Some(outcome) = taken else {
            #[allow(clippy::cast_precision_loss)]
            let fraction = progress.inner_iter as f32 / progress.inner_total.max(1) as f32;
            return Ok(PollOutcome::Pending {
                latest_assembly: None,
                progress: Some(fraction),
                stage: Some(format!(
                    "Refining B-factors - cycle {}",
                    progress.macro_cycle
                )),
                score: None,
            });
        };

        let _ = self.streams()?.remove(&request_id);

        let mut sessions = self.sessions()?;
        let s = sessions
            .get_mut(&session)
            .ok_or_else(|| op_err("UNKNOWN_SESSION", format!("no session {session}")))?;

        match outcome {
            Outcome::Failed(message) => Ok(PollOutcome::Error {
                code: "REFINE_FAILED".to_owned(),
                message,
                details: HashMap::new(),
            }),
            Outcome::Cancelled => {
                let bytes = s
                    .assembly
                    .to_bytes()
                    .map_err(|e| op_err("SERIALIZE_FAILED", e.to_string()))?;
                Ok(PollOutcome::Cancelled {
                    assembly: bytes,
                    score: None,
                })
            }
            Outcome::Done {
                full_b,
                r_work,
                r_free,
            } => {
                log::info!("[xtal] refine done: r_work={r_work:.4} r_free={r_free:.4}");
                apply_b_factors(&mut s.assembly, &full_b)?;
                // Refined B changes the map.
                self.recompute_density(s);

                let entities: Vec<MoleculeEntity> =
                    s.assembly.entities().iter().map(|e| (**e).clone()).collect();
                let score = self.score_report(s, &entities);
                let bytes = s
                    .assembly
                    .to_bytes()
                    .map_err(|e| op_err("SERIALIZE_FAILED", e.to_string()))?;
                Ok(PollOutcome::Final {
                    assembly: bytes,
                    score: Some(score),
                })
            }
        }
    }

    fn cancel_stream(&self, request_id: u64) -> Result<()> {
        // Idempotent: an already-finished stream has been removed.
        if let Some(st) = self.streams()?.get(&request_id) {
            st.cancel.store(true, Ordering::Relaxed);
        }
        Ok(())
    }

    fn query(
        &self,
        session: u64,
        query: &str,
        _ctx: &DispatchContext,
        _params: &HashMap<String, ParamValue>,
        assembly: &[u8],
    ) -> Result<Vec<u8>> {
        let sessions = self.sessions()?;
        let s = sessions
            .get(&session)
            .ok_or_else(|| op_err("UNKNOWN_SESSION", format!("no session {session}")))?;

        match query {
            QUERY_DENSITY => {
                // An empty message means "no map", not a transport error.
                let Some(map) = s.density.as_ref() else {
                    return Ok(proto::DensityMap::default().encode_to_vec());
                };
                Ok(proto::DensityMap {
                    name: map.name.clone(),
                    data: map.bytes.clone(),
                    resolution: map.resolution,
                    // Absent lets the consumer derive spacing from the header.
                    grid_spacing: None,
                }
                .encode_to_vec())
            }
            QUERY_SCORE => {
                // A non-empty `assembly` names a specific composition to score
                // (a committed head or checkpoint) instead of the live session.
                let entities: Vec<MoleculeEntity> = if assembly.is_empty() {
                    s.assembly.entities().iter().map(|e| (**e).clone()).collect()
                } else {
                    Assembly::from_bytes(assembly)
                        .map_err(|e| op_err("INVALID_ASSEMBLY", e.to_string()))?
                        .entities()
                        .iter()
                        .map(|e| (**e).clone())
                        .collect()
                };
                Ok(self.score_report(s, &entities).encode_to_vec())
            }
            _ => Err(PluginError::Unsupported),
        }
    }
}

/// Scatter the refined B column back onto `assembly`, preserving every other
/// atom field. `flat_source_indices` walks the same order as
/// `AtomTable::from_entities`, so `prov[i]` sources `full_b[i]`.
fn apply_b_factors(assembly: &mut Assembly, full_b: &[f32]) -> Result<()> {
    let entities: Vec<MoleculeEntity> = assembly.entities().iter().map(|e| (**e).clone()).collect();
    let prov = AtomTable::flat_source_indices(&entities);
    if prov.len() != full_b.len() {
        return Err(op_err(
            "SIZE_MISMATCH",
            format!(
                "refine produced {} B values for {} atoms",
                full_b.len(),
                prov.len()
            ),
        ));
    }

    let mut per_entity: HashMap<u32, Vec<(usize, f32)>> = HashMap::new();
    for (&(entity_raw, raw_idx), &b) in prov.iter().zip(full_b.iter()) {
        per_entity
            .entry(entity_raw)
            .or_default()
            .push((raw_idx as usize, b));
    }

    let mut rebuilt = entities;
    for entity in &mut rebuilt {
        if let Some(cells) = per_entity.get(&entity.id().raw()) {
            let col = &mut entity.columns_mut().b_factor;
            for &(slot, b) in cells {
                if let Some(cell) = col.get_mut(slot) {
                    *cell = b;
                }
            }
        }
    }
    *assembly = Assembly::new(rebuilt);
    Ok(())
}

fn new_plugin(_config_json: &str) -> Result<Box<dyn Plugin>> {
    Ok(Box::new(XtalPlugin::new()))
}

export_plugin!(new_plugin);
