use crate::app::App;

/// Sum the RAW score bonus of every `exposed_count` objective met at the
/// given exposed-hydrophobic `count`. An `exposed_count` objective awards
/// its `bonus` when `count < max` (the legacy `ExposedCount` filter: max=1
/// means the win is `count == 0`), else `0`. An objective with no `max`
/// (malformed) and any non-`exposed_count` kind contribute nothing
/// (forward-compatible: an unknown objective type parses but is inert). The
/// result is a RAW delta the score path folds in before the raw->game map.
#[must_use]
pub(in crate::app) fn exposed_count_bonus(
    objectives: &[crate::puzzle::Objective],
    count: u32,
) -> f64 {
    objectives
        .iter()
        .filter(|o| o.kind == "exposed_count")
        .filter_map(|o| o.max.map(|max| (max, o.bonus)))
        .map(|(max, bonus)| if count < max { f64::from(bonus) } else { 0.0 })
        .sum()
}

impl App {
    /// Refresh the engine's exposed-hydrophobic grease beads and the loaded
    /// puzzle's met-objective bonus from the plugin's `exposed_hydrophobics`
    /// query. Mirrors the at-rest `score`, `voids`, and `clashes` requests:
    /// it runs only when the scene is at rest, on a geometry change, with no
    /// edit open, so the flagged residues and the objective count track the
    /// committed pose without a per-frame hot loop.
    ///
    /// The query runs when EITHER the exposed-hydrophobic display is ON OR the
    /// loaded puzzle declares an active `exposed_count` objective (and a
    /// plugin advertises the query,
    /// [`crate::runner_client::RunnerClient::supports_exposed_hydrophobics`]).
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
    /// Decode is the pure [`crate::exposed_hydrophobics::exposed_from_bytes`]
    /// helper; each decoded residue's proto `entity_id` is mapped to a molex
    /// `EntityId` against the live session (the same `id.raw()` lookup the
    /// dispatch and pull-drag paths use, widened to the proto's `u64`), and a
    /// residue whose `entity_id` does not resolve to a current entity is
    /// dropped. The push is
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
        if !self.runner_client.supports_exposed_hydrophobics() {
            if let Some(engine) = self.engine.as_mut() {
                engine.update_exposed_hydrophobics(Vec::new());
            }
            self.store.set_objective_bonus(0.0);
            return;
        }
        let bytes = self.runner_client.request_exposed_hydrophobics_bytes();
        let report = crate::exposed_hydrophobics::exposed_from_bytes(&bytes);
        // TEMPORARY DEBUG: log the raw count the detector returned so a
        // runtime check can distinguish "detector returned N" from "detector
        // returned 0 / render issue". Removed in a later cleanup pass.
        log::info!(
            "EXPOSED_HYDRO_DEBUG_COUNT detector returned {} exposed residues",
            report.exposed.len()
        );
        // Evaluate every active `exposed_count` objective on the loaded puzzle
        // against the live count and store the met-bonus total. Folded into
        // the headline game score by the score path before the raw->game map.
        // No puzzle (free-form) yields no objectives -> zero bonus.
        let count = u32::try_from(report.exposed.len()).unwrap_or(u32::MAX);
        let bonus = self
            .store
            .puzzle()
            .map_or(0.0, |p| exposed_count_bonus(&p.objectives, count));
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
            let Some(entity) = self
                .store
                .ids()
                .find(|id| u64::from(id.raw()) == residue.entity_id)
            else {
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

#[cfg(test)]
mod tests {
    use super::exposed_count_bonus;
    use crate::puzzle::Objective;

    fn exposed_count(max: u32, bonus: f32) -> Objective {
        Objective {
            kind: "exposed_count".to_owned(),
            max: Some(max),
            bonus,
        }
    }

    #[test]
    fn bonus_awarded_below_max() {
        // max=1: the win is count==0; count 0 < 1 awards the bonus.
        let objs = [exposed_count(1, -100.0)];
        assert_eq!(exposed_count_bonus(&objs, 0), -100.0);
    }

    #[test]
    fn no_bonus_at_or_above_max() {
        let objs = [exposed_count(1, -100.0)];
        assert_eq!(exposed_count_bonus(&objs, 1), 0.0);
        assert_eq!(exposed_count_bonus(&objs, 5), 0.0);
    }

    #[test]
    fn unknown_kind_is_inert() {
        let objs = [Objective {
            kind: "some_future_kind".to_owned(),
            max: Some(1),
            bonus: -100.0,
        }];
        assert_eq!(exposed_count_bonus(&objs, 0), 0.0);
    }

    #[test]
    fn empty_objectives_yield_zero() {
        assert_eq!(exposed_count_bonus(&[], 0), 0.0);
    }
}
