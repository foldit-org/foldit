//! App-owned viso projection.
//!
//! Consumes the [`SessionUpdate`] stream and rebuilds the head `Assembly`
//! once per drain to publish to viso. Owns the publish-generation
//! counter and the last-published id set, the latter only to pick
//! between `set_assembly` (steady-state coord update or same-membership
//! reorder) and `replace_assembly` (topology swap: an entity actually
//! joined or left, tearing down per-entity scene-local state). Both
//! stamp a fresh `publish_seq` so viso's `poll_assembly` gate sees a
//! different number on every publish.

use std::collections::{BTreeSet, HashMap};

use crate::session::{Session, SessionUpdate, SessionUpdateConsumer};

/// App-owned viso projector. Holds the monotonic publish counter that
/// every published `Assembly` is stamped with, plus the entity-id set
/// of the last published assembly so we can detect topology change and
/// route to `replace_assembly` accordingly. The seq counter is
/// **deliberately** not reset on `Session::reset`: a fresh post-reset
/// publish still advances it, and viso never sees the generation go
/// backwards.
pub struct RenderProjector {
    /// Monotonic counter stamped onto every published `Assembly`.
    /// Incremented on every `project` that actually publishes. Without
    /// a fresh generation per publish, viso's `poll_assembly` gate
    /// would skip the second-and-subsequent publishes (a freshly built
    /// `Assembly` always starts at generation 0).
    publish_seq: u64,
    /// Entity ids of the last published assembly, as a membership set.
    /// Compared against the next drain's id set to choose between
    /// `set_assembly` (same membership -- only coords differ, or the
    /// canonical order shifted) and `replace_assembly` (an id actually
    /// joined or left). A pure reorder is *not* a topology change: viso
    /// keys every entity by id and reconciles by membership on sync, so
    /// a same-set publish re-derives correctly through `set_assembly`
    /// without the scene-local teardown `replace_assembly` forces.
    last_published_ids: BTreeSet<molex::entity::molecule::id::EntityId>,
}

impl RenderProjector {
    pub const fn new() -> Self {
        Self {
            publish_seq: 0,
            last_published_ids: BTreeSet::new(),
        }
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
    /// and reconciles by id), so foldit-core keeps no shadow copy. This
    /// reproduces the sizing/scatter the old direct App push performed,
    /// sourced now from the session instead of the just-arrived report.
    fn project_scores(doc: &Session, engine: &mut viso::VisoEngine) {
        let Some(breakdown) = doc.current_composition_breakdown() else {
            return;
        };
        let weighted = breakdown.weighted_per_residue(doc.term_names(), doc.term_weights());
        if weighted.is_empty() {
            return;
        }
        // entity_id -> Vec<(residue_index, score)>.
        let mut per_entity: HashMap<u32, Vec<(u32, f64)>> = HashMap::new();
        for (entity_id, residue_index, score) in weighted {
            #[allow(clippy::cast_possible_truncation)]
            let entity_id = entity_id as u32;
            per_entity
                .entry(entity_id)
                .or_default()
                .push((residue_index, score));
        }
        // Build (raw_entity_id -> residue_count) once from the head assembly
        // so each entity's score vector is sized to its full residue count.
        let head = doc.head_assembly();
        let residue_counts: HashMap<u32, usize> = head
            .entities()
            .iter()
            .map(|e| (e.id().raw(), e.residue_count()))
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
            engine.set_per_residue_scores(entity_id, Some(scores));
        }
    }
}

/// Consume a drained `SessionUpdate` batch and drive viso. Two
/// independent reactions, self-filtered by what the batch carries:
///
/// - A geometry change (`Edit` / `HeadMoved` / preview add/discard)
///   republishes the current head assembly, picking `replace_assembly`
///   only when the entity-id *set* changed since the last publish (an id
///   joined or left) and `set_assembly` otherwise (a steady-state coord
///   update or a same-membership reorder).
/// - A `ScoresChanged` re-derives the per-residue colors from the
///   session-owned breakdown ([`Self::project_scores`]).
///
/// A batch may carry both (a first-op tick), one, or neither. No-ops when
/// it carries neither (no wasted assembly builds, generation bumps, or
/// color pushes).
impl SessionUpdateConsumer<viso::VisoEngine> for RenderProjector {
    fn consume(
        &mut self,
        changes: &[SessionUpdate],
        doc: &Session,
        engine: &mut viso::VisoEngine,
    ) {
        // A geometry change republishes the head assembly; a `ScoresChanged`
        // re-derives the per-residue colors. A batch can carry both (a
        // first-op tick: topology republish + the score that arrived the same
        // tick), one, or (for a steady-state rescore reply) only the score.
        // Self-filter so a score-only batch never republishes geometry and a
        // geometry-only batch never re-pushes colors.
        let has_geometry = changes.iter().any(|c| {
            matches!(
                c,
                SessionUpdate::Edit { .. }
                    | SessionUpdate::HeadMoved
                    | SessionUpdate::PreviewAdded
                    | SessionUpdate::PreviewDiscarded
            )
        });
        let has_scores = changes
            .iter()
            .any(|c| matches!(c, SessionUpdate::ScoresChanged));

        // Geometry republish first (moot for ordering since viso retains
        // scores across `replace_assembly`, but keeps the publish before the
        // color push it sizes against).
        if has_geometry {
            let mut asm = doc.head_assembly();
            let new_ids: BTreeSet<molex::entity::molecule::id::EntityId> =
                asm.entities().iter().map(|e| e.id()).collect();
            let topology_changed = new_ids != self.last_published_ids;

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

        if has_scores {
            Self::project_scores(doc, engine);
        }
    }
}

/// Build a focus description from focus + entity names. Was
/// `Session::focus_description`; moved here to keep `Session`
/// viso-free. The `All` arm reports `doc.count()` (live committed +
/// preview membership) rather than the metadata side table, which is
/// never GC'd and so over-reports the live entity count.
pub fn focus_description(doc: &Session, focus: &viso::Focus) -> String {
    match focus {
        viso::Focus::All => {
            let count = doc.count();
            format!("All ({count} entities)")
        }
        viso::Focus::Entity(id) => doc
            .metadata(*id).map_or_else(|| format!("Entity {}", id.raw()), |m| m.name.clone()),
    }
}
