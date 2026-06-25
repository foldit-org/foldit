//! App-owned viso projection: consumes the [`SessionUpdate`] stream and
//! rebuilds the head `Assembly` once per drain to publish to viso.

use std::collections::{BTreeSet, HashMap};

use crate::session::{Session, SessionUpdate, SessionUpdateConsumer};

/// App-owned viso projector. Holds the monotonic publish counter that
/// every published `Assembly` is stamped with. The per-session diff
/// baselines used to route publishes (last-published id set, last-pushed
/// appearance ids, last SS-bearing assembly) live on the session's
/// `VizState`, not here, so `Session::reset` clears them.
pub struct RenderProjector {
    /// Monotonic counter stamped onto every published `Assembly`.
    /// Incremented on every `project` that actually publishes. Without
    /// a fresh generation per publish, viso's `poll_assembly` gate
    /// would skip the second-and-subsequent publishes (a freshly built
    /// `Assembly` always starts at generation 0). Deliberately app-scoped:
    /// not reset on `Session::reset`, so a fresh post-reset publish still
    /// advances it and viso never sees the generation go backwards.
    publish_seq: u64,
}

impl RenderProjector {
    pub const fn new() -> Self {
        Self { publish_seq: 0 }
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
    fn project_scores(doc: &Session, engine: &mut viso::VisoEngine) {
        let Some(breakdown) = doc.current_composition_breakdown() else {
            return;
        };
        let weighted = breakdown.weighted_per_residue(doc.term_names(), doc.term_weights());
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

    /// Force a per-residue color re-push at a moment viso is known to have
    /// fully synced the current geometry (so every entity's scene-local
    /// state exists and the push is not silently dropped). Used at the
    /// startup session-entry seam, where the first score may have pushed
    /// before viso created the entity state. Re-runs the same private
    /// projection the `ScoresChanged` path uses; no-ops internally when no
    /// breakdown is stamped or it carries no per-residue rows.
    pub(crate) fn reproject_scores(doc: &Session, engine: &mut viso::VisoEngine) {
        Self::project_scores(doc, engine);
    }

    /// Force a full-rebuild republish of the current head assembly so the
    /// cartoon mesh re-bakes with the current `annotations.scores`. The
    /// cartoon tube's per-residue color is baked into the mesh at build time;
    /// viso re-reads `annotations.scores` only when it submits a full-rebuild
    /// mesh (`replace_assembly` -> `sync_now` -> `submit_full_rebuild`). At
    /// startup the geometry publishes before the first async score arrives, so
    /// the tube bakes gray, and the later score push fires only a color
    /// re-push (the separate residue-color buffer) that never re-bakes the
    /// backbone. Issuing a full rebuild AFTER the scores are present bakes the
    /// colored tube, matching steady-state gameplay where an edit's geometry
    /// change re-bakes the mesh as the score updates.
    ///
    /// Always routes to `replace_assembly` (not the membership-gated
    /// `set_assembly`): a same-topology `set_assembly` may be a coord-only
    /// update that does not re-bake colors, and we specifically need the full
    /// rebuild. The session's `last_published_ids` baseline is refreshed so
    /// the next normal `consume` does not read a spurious topology change.
    pub(crate) fn rebake_geometry(&mut self, doc: &mut Session, engine: &mut viso::VisoEngine) {
        let mut asm = doc.head_assembly();
        Self::populate_connections(doc, &mut asm);
        // Session-entry full rebuild bakes the cartoon, so it needs SS (molex
        // construction leaves `ss_types` empty; this is a load-time publish).
        asm.recompute_ss();
        let new_ids: BTreeSet<molex::entity::molecule::id::EntityId> =
            asm.entities().iter().map(|e| e.id()).collect();
        doc.viz.last_ss = Some(asm.clone());

        self.publish_seq = self.publish_seq.saturating_add(1);
        asm.set_generation(self.publish_seq);
        let asm = std::sync::Arc::new(asm);

        engine.replace_assembly(asm);
        doc.viz.last_published_ids = new_ids;
    }

    /// Stamp the rendering connections (disulfides and hydrogen bonds) onto
    /// the owned assembly before it is published. The assembly is rebuilt
    /// per conformation change and its `connections` start empty, so the
    /// owner must populate them on every publish; viso resolves the
    /// endpoints to rendered atom positions reactively. Both publish paths
    /// call this so they cannot drift apart on what gets stamped.
    ///
    /// Provider-aware on the session's held set
    /// (`crate::viz::refresh::refresh_connections` writes it earlier in the
    /// same tick): when a
    /// plugin provides connections the held atom-index map is stamped
    /// verbatim and molex's geometric fallback is NOT run; otherwise molex
    /// detects them geometrically per publish.
    fn populate_connections(doc: &Session, asm: &mut molex::Assembly) {
        let connections = doc
            .viz
            .held_connections
            .as_ref()
            .map_or_else(|| asm.detect_fallback_connections(), Clone::clone);
        asm.set_connections(connections);
    }

    /// Push the three structural-viz overlays (external cavities/voids,
    /// steric-clash arcs, exposed-hydrophobic grease beads) from the cached
    /// viz state to the engine, but only when the cache is dirty. The App's
    /// overlay refresh recomputes the payloads at rest each geometry change
    /// (and on a view toggle) and marks the cache dirty; this is the single
    /// pusher, fired on the drain. Returns `true` when it pushed so the
    /// caller can clear the dirty flag (the projector holds only a shared
    /// `&Session`, so it cannot clear the flag itself).
    ///
    /// Gating on the dirty flag is what makes the overlays freeze during a
    /// wiggle: while motion is in flight the refresh does not run, the flag
    /// stays clear, and this skips the push instead of re-pushing the stale
    /// payloads every drain.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn push_overlays(doc: &Session, engine: &mut viso::VisoEngine) -> bool {
        if !doc.viz.viz_dirty {
            return false;
        }
        let field = &doc.viz.void_field;
        engine.set_external_void_field(
            field.dims,
            field.origin,
            field.spacing,
            field.phi.clone(),
            field.threshold,
        );
        engine.update_clashes(doc.viz.clashes.clone());
        engine.update_exposed_hydrophobics(doc.viz.exposed_hydrophobics.clone());
        true
    }
}

/// Consume a drained `SessionUpdate` batch and drive viso. Five
/// independent reactions, each self-filtered by what the batch carries:
///
/// - A `SelectionChanged` sources the highlight from the authoritative
///   `Session` selection.
/// - A `FocusChanged` pushes viso's camera-framing mirror and reframes.
/// - An `EntityAppearanceChanged` reconciles the engine's per-entity
///   override working copy against the authoritative `Session` appearance
///   map: every current override is pushed, and any entity dropped from the
///   map since the last push is cleared on the engine.
/// - A geometry change (`Edit` / `HeadMoved` / preview add/discard)
///   republishes the current head assembly, picking `replace_assembly`
///   only when the entity-id *set* changed since the last publish (an id
///   joined or left) and `set_assembly` otherwise (a steady-state coord
///   update or a same-membership reorder).
/// - A `ScoresChanged` re-derives the per-residue colors from the
///   session-owned breakdown ([`Self::project_scores`]).
///
/// The `ViewOptionsChanged` reaction is NOT here: the view options live on
/// `App` (so they persist across a topology swap), and `App` applies them
/// to the engine at the same tick seam, gated on the same signal.
///
/// The selection / focus reactions run before the geometry / scores
/// reactions. A batch may carry any subset. No-ops on a batch carrying none
/// of them (e.g. a `BubbleChanged` / `PuzzleChanged`-only batch): no wasted
/// assembly builds, generation bumps, or pushes.
impl SessionUpdateConsumer<viso::VisoEngine> for RenderProjector {
    fn consume(
        &mut self,
        changes: &[SessionUpdate],
        doc: &mut Session,
        engine: &mut viso::VisoEngine,
    ) {
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
            // or removed entry). Collect the new baseline (owned) before the
            // mutable `doc.viz` write so the immutable `doc.appearance()`
            // borrow is finished by then.
            let new_pushed: BTreeSet<molex::entity::molecule::id::EntityId> = {
                let appearance = doc.appearance();
                for (id, ovr) in appearance {
                    engine.set_entity_appearance(*id, ovr.clone());
                }
                for id in &doc.viz.last_pushed_appearance {
                    if !appearance.contains_key(id) {
                        engine.clear_entity_appearance(*id);
                    }
                }
                appearance.keys().copied().collect()
            };
            doc.viz.last_pushed_appearance = new_pushed;
        }
        // The `ViewOptionsChanged` reaction (apply the App-owned options to
        // the engine) is driven by `App` at the same tick seam: the options
        // live on `App` (so they survive a topology swap), not on `Session`,
        // and this projector only takes a `&Session`.

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
            .any(|c| matches!(c, SessionUpdate::HeadMoved));

        // Geometry republish first (moot for ordering since viso retains
        // scores across `replace_assembly`, but keeps the publish before the
        // color push it sizes against).
        if has_geometry {
            let head = doc.head_assembly();
            let new_ids: BTreeSet<molex::entity::molecule::id::EntityId> =
                head.entities().iter().map(|e| e.id()).collect();
            let topology_changed = new_ids != doc.viz.last_published_ids;

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
                doc.viz.last_ss = Some(a.clone());
                a
            } else if let Some(prev) = doc.viz.last_ss.as_ref() {
                // `head` is the freshly built coord snapshot (owned here);
                // overlay the cached committed SS onto it in place. `prev`
                // (the cached committed assembly) is borrowed and left intact.
                let mut a = head;
                a.carry_ss_from(prev);
                a
            } else {
                head
            };

            Self::populate_connections(doc, &mut asm);
            self.publish_seq = self.publish_seq.saturating_add(1);
            asm.set_generation(self.publish_seq);
            let asm = std::sync::Arc::new(asm);

            if topology_changed {
                engine.replace_assembly(asm);
            } else {
                engine.set_assembly(asm);
            }
            doc.viz.last_published_ids = new_ids;
        }

        if has_scores {
            Self::project_scores(doc, engine);
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
            .metadata(id).map_or_else(|| format!("Entity {}", id.raw()), |m| m.name.clone()),
    }
}
