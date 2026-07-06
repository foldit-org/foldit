//! App-owned viso projection: consumes the [`SessionUpdate`] stream and
//! rebuilds the head `Assembly` once per drain to publish to viso.

use std::collections::{BTreeSet, HashMap};

use crate::app::score_coordinator::ScoreCoordinator;
use crate::session::{Session, SessionUpdate, SessionUpdateConsumer};

/// The inputs the render projection reads.
pub struct RenderSources<'a> {
    pub session: &'a mut Session,
    pub reapply_options: Option<viso::options::VisoOptions>,
    pub scores: &'a ScoreCoordinator,
    /// The plugin-provided connections to stamp, sourced from the App-owned
    /// `Viz` cache. `None` falls back to molex's geometric detection.
    pub held_connections: Option<&'a HashMap<molex::ConnectionType, Vec<molex::AtomLink>>>,
}

/// App-owned viso projector. Holds the monotonic publish counter and the
/// per-session diff baselines that route publishes (last-published id set,
/// last-pushed appearance ids, last SS-bearing assembly).
pub struct RenderProjector {
    /// Monotonic counter stamped onto every published `Assembly`.
    /// Incremented on every `project` that actually publishes. Without
    /// a fresh generation per publish, viso's `poll_assembly` gate
    /// would skip the second-and-subsequent publishes (a freshly built
    /// `Assembly` always starts at generation 0). Deliberately app-scoped:
    /// not reset on `Session::reset`, so a fresh post-reset publish still
    /// advances it and viso never sees the generation go backwards.
    publish_seq: u64,
    /// Entity-id set of the last published assembly, compared against the next
    /// drain's id set to choose `set_assembly` vs `replace_assembly`.
    last_published_ids: BTreeSet<molex::entity::molecule::id::EntityId>,
    /// Entity ids whose appearance overrides were last pushed to the engine
    /// working copy, used to detect an entry the session dropped since.
    last_pushed_appearance: BTreeSet<molex::entity::molecule::id::EntityId>,
    /// The last SS-bearing published assembly (the one a `recompute_ss` ran
    /// on), cached so a streaming tentative frame carries its secondary
    /// structure forward without re-running DSSP. `None` until the first
    /// committed / load publish.
    last_ss: Option<molex::Assembly>,
}

impl RenderProjector {
    pub const fn new() -> Self {
        Self {
            publish_seq: 0,
            last_published_ids: BTreeSet::new(),
            last_pushed_appearance: BTreeSet::new(),
            last_ss: None,
        }
    }

    /// Clear the diff baselines so a new puzzle reusing the outgoing puzzle's
    /// entity ids never inherits a stale baseline. Leaves `publish_seq`
    /// untouched (app-lifetime). Called at the App reset seam.
    pub(crate) fn reset_baselines(&mut self) {
        self.last_published_ids.clear();
        self.last_pushed_appearance.clear();
        self.last_ss = None;
    }

    /// Clear just the last-published id set, for the startup synthetic replay.
    pub(crate) fn clear_last_published_ids(&mut self) {
        self.last_published_ids.clear();
    }

    /// Re-derive the displayed per-residue colors from the session-owned
    /// breakdown and push them to viso. The session is the source of truth:
    /// the current composition node's RAW per-term breakdown (first open
    /// edit else committed head) weighted by the session weight map, zipped
    /// against the session `term_names`. No-op when no breakdown is stamped
    /// (e.g. on wasm, where the score path never runs) or it carries no
    /// per-residue rows.
    ///
    /// Each entity's score vector is sized to its full residue count from
    /// the head assembly; missing residues default to `0.0` (the mid-palette
    /// stop in absolute mode, the lower quantile in relative mode). viso owns
    /// the per-entity score state (it retains scores across `replace_assembly`
    /// and reconciles by id), so foldit-core keeps no shadow copy. The
    /// sizing/scatter is sourced from the session.
    fn project_scores(doc: &Session, scores: &ScoreCoordinator, engine: &mut viso::VisoEngine) {
        let Some(breakdown) = doc.current_composition_breakdown() else {
            return;
        };
        let weighted = breakdown.weighted_per_residue(scores.term_names(), scores.term_weights());
        if weighted.is_empty() {
            return;
        }
        // entity_id -> Vec<(residue_index, score)>.
        let mut per_entity: HashMap<molex::EntityId, Vec<(u32, f64)>> = HashMap::new();
        for (entity_id, residue_index, score) in weighted {
            per_entity
                .entry(entity_id)
                .or_default()
                .push((residue_index, score));
        }
        // Build (entity_id -> residue_count) once from the head assembly
        // so each entity's score vector is sized to its full residue count.
        let head = doc.head_assembly();
        let residue_counts: HashMap<molex::EntityId, usize> = head
            .entities()
            .iter()
            .map(|e| (e.id(), e.residue_count()))
            .collect();
        for (entity_id, mut entries) in per_entity {
            let Some(&residue_count) = residue_counts.get(&entity_id) else {
                log::warn!(
                    "[RenderProjector] per-residue scores for unknown entity \
                     {entity_id} (head has entities {:?})",
                    residue_counts.keys().collect::<Vec<_>>()
                );
                continue;
            };
            let mut scores = vec![0.0_f64; residue_count];
            entries.sort_unstable_by_key(|(idx, _)| *idx);
            for (idx, val) in entries {
                let i = idx as usize;
                if i < scores.len() {
                    scores[i] = val;
                }
            }
            engine.set_per_residue_scores(entity_id.raw(), Some(scores));
        }
    }

    /// Stamp the rendering connections (disulfides and hydrogen bonds) onto the
    /// owned assembly before it is published. The assembly is rebuilt per
    /// conformation change with empty `connections`, so this runs on every
    /// publish; viso resolves the endpoints reactively. Provider-aware on the
    /// held set: when a plugin provides connections the atom-index map is
    /// stamped verbatim and molex's geometric fallback is NOT run; otherwise
    /// molex detects them geometrically per publish.
    fn populate_connections(
        held: Option<&HashMap<molex::ConnectionType, Vec<molex::AtomLink>>>,
        asm: &mut molex::Assembly,
    ) {
        let connections = held.map_or_else(|| asm.detect_fallback_connections(), Clone::clone);
        asm.set_connections(connections);
    }
}

/// Consume a drained `SessionUpdate` batch and drive viso. Each reaction is
/// self-filtered by what the batch carries; a batch may carry any subset and
/// a batch carrying none is a no-op.
impl SessionUpdateConsumer for RenderProjector {
    type Sources<'a> = RenderSources<'a>;
    type Sink = viso::VisoEngine;
    type Out = ();
    fn consume(
        &mut self,
        changes: &[SessionUpdate],
        sources: RenderSources<'_>,
        engine: &mut viso::VisoEngine,
    ) {
        let RenderSources {
            session: doc,
            reapply_options,
            scores,
            held_connections,
        } = sources;
        if changes
            .iter()
            .any(|c| matches!(c, SessionUpdate::SelectionChanged))
        {
            engine.set_selection(doc.selection());
        }
        if changes.iter().any(|c| matches!(c, SessionUpdate::FocusChanged)) {
            engine.set_focus(doc.focus());
            engine.fit_camera_to_focus();
        }
        if changes
            .iter()
            .any(|c| matches!(c, SessionUpdate::EntityAppearanceChanged))
        {
            // Reconcile the engine working copy against the authoritative
            // session map: push every current override, then clear any entity
            // that was in the last push but is no longer present (an emptied
            // or removed entry).
            let appearance = doc.appearance();
            for (id, ovr) in appearance {
                engine.set_entity_appearance(*id, ovr.clone());
            }
            for id in &self.last_pushed_appearance {
                if !appearance.contains_key(id) {
                    engine.clear_entity_appearance(*id);
                }
            }
            self.last_pushed_appearance = appearance.keys().copied().collect();
        }
        if let Some(opts) = reapply_options {
            engine.set_options(opts);
        }

        // A geometry change republishes the head assembly; a `ScoresChanged`
        // re-derives the per-residue colors. A batch can carry both (a
        // first-op tick: topology republish + the score that arrived the same
        // tick), one, or (for a steady-state rescore reply) only the score.
        // Self-filter so a score-only batch never republishes geometry and a
        // geometry-only batch never re-pushes colors.
        let has_geometry = changes.iter().any(SessionUpdate::is_geometry);
        let has_scores = changes
            .iter()
            .any(|c| matches!(c, SessionUpdate::ScoresChanged));
        let committed_geometry = changes
            .iter()
            .any(|c| matches!(c, SessionUpdate::HeadMoved { .. }));

        // Geometry republish first (moot for ordering since viso retains
        // scores across `replace_assembly`, but keeps the publish before the
        // color push it sizes against).
        if has_geometry {
            let head = doc.head_assembly();
            let new_ids: BTreeSet<molex::entity::molecule::id::EntityId> =
                head.entities().iter().map(|e| e.id()).collect();
            let topology_changed = new_ids != self.last_published_ids;

            // SS is opt-in on molex construction (`ss_types` starts empty). On a
            // committed / topology-changing publish (load, action commit, entity
            // create) recompute it and cache the SS-bearing assembly. A streaming
            // tentative `Edit` frame publishes the head assembly's real identity
            // and coords (so mid-stream mutations animate) with the last
            // committed secondary structure overlaid from `last_ss`: SS is
            // per-residue metadata independent of atom positions, carried forward
            // by residue count without re-running DSSP (which per streamed frame
            // was the wiggle/shake stall), so the cartoon keeps its helices and
            // sheets during streaming. With no cached SS yet (`last_ss` is None)
            // the head publishes as-is and the cartoon flattens to coil until the
            // first committed publish.
            let mut asm = if committed_geometry || topology_changed {
                let mut a = head;
                a.recompute_ss();
                self.last_ss = Some(a.clone());
                a
            } else if let Some(prev) = self.last_ss.as_ref() {
                // `head` is the freshly built coord snapshot (owned here);
                // overlay the cached committed SS onto it in place. `prev`
                // (the cached committed assembly) is borrowed and left intact.
                let mut a = head;
                a.carry_ss_from(prev);
                a
            } else {
                head
            };

            Self::populate_connections(held_connections, &mut asm);
            self.publish_seq = self.publish_seq.saturating_add(1);
            asm.set_generation(self.publish_seq);
            let asm = std::sync::Arc::new(asm);

            if topology_changed {
                engine.replace_assembly(asm);
            } else {
                engine.set_assembly(asm);
            }
            self.last_published_ids = new_ids;
        }

        // A navigation batch carries no `ScoresChanged`, so the `has_scores`
        // push below never fires for it. This branch clears stale colors and
        // re-derives from the navigated node's own retained breakdown; a
        // never-scored node stays cleared until an async rescore lands.
        let head_nav_only = changes
            .iter()
            .any(|c| matches!(c, SessionUpdate::HeadMoved { .. }))
            && !changes
                .iter()
                .any(|u| u.is_geometry() && !matches!(u, SessionUpdate::HeadMoved { .. }));
        if head_nav_only {
            let ids: Vec<molex::EntityId> = doc.ids().collect();
            for eid in ids {
                engine.set_per_residue_scores(eid.raw(), None);
            }
            Self::project_scores(doc, scores, engine);
        }

        if has_scores {
            Self::project_scores(doc, scores, engine);
        }
    }
}

/// Build a focus description from focus + entity names. The `All` arm
/// reports `doc.count()` (live committed + preview membership) rather than
/// the metadata side table, which is never GC'd and so over-reports the
/// live entity count.
pub fn focus_description(doc: &Session, focus: viso::Focus) -> String {
    match focus {
        viso::Focus::All => {
            let count = doc.count();
            format!("All ({count} entities)")
        }
        viso::Focus::Entity(id) => doc
            .name(id).map_or_else(|| format!("Entity {}", id.raw()), str::to_owned),
    }
}
