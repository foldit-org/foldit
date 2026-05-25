//! Plugin broadcast encoding + queue: the Full/Delta fan-out payloads
//! produced by authoritative mutations, drained by `App` into the
//! orchestrator. Decoupling these from the canonical store behind a
//! stream-subscribed broadcaster is Reshape work, not part of this split.

use molex::ops::edit::AssemblyEdit;
use molex::{Assembly, MoleculeEntity};

use super::Document;

impl Document {
    /// Drain pending plugin broadcasts. Callers pump this after each
    /// action handler / keybinding / head-move and forward each entry
    /// to the orchestrator's `broadcast_to_plugins`. Always returns
    /// an empty vec in steady state.
    pub fn take_pending_broadcasts(
        &mut self,
    ) -> Vec<foldit_runner::orchestrator::BroadcastPayload> {
        std::mem::take(&mut self.pending_broadcasts)
    }

    /// Mark an authoritative mutation as complete: serialize the
    /// current `head_assembly()` and queue a `Full` broadcast for the
    /// orchestrator to fan out. Centralizes the serialize + queue
    /// logic so individual mutation sites don't open-code it.
    ///
    /// Errors only if `serialize_assembly` fails — currently
    /// impossible for in-memory Assemblies, but kept fallible for
    /// future format-version churn. Mutation-site callers ignore the
    /// error since they have no clean recovery path; the worst case
    /// is a plugin missing a broadcast and falling back to STALE_GEN
    /// recovery on its next dispatch.
    pub fn queue_full_broadcast(&mut self) -> Result<(), molex::ops::codec::AdapterError> {
        let asm = self.head_assembly();
        let bytes = molex::ops::wire::serialize_assembly(&asm)?;
        self.pending_broadcasts
            .push(foldit_runner::orchestrator::BroadcastPayload::Full(bytes));
        Ok(())
    }

    /// Queue a `Delta` broadcast carrying the given edits as DELTA01
    /// bytes. Falls back to a `Full` broadcast on any serialize error
    /// (e.g. topology edits — `AddEntity` / `RemoveEntity` — which
    /// DELTA01 rejects). The fallback keeps the broadcast queue
    /// invariant ("one payload per authoritative mutation") intact
    /// even when an unrepresentable edit slips in.
    fn queue_delta_broadcast(&mut self, edits: &[AssemblyEdit]) {
        match molex::ops::wire::delta::serialize_edits(edits) {
            Ok(bytes) => self
                .pending_broadcasts
                .push(foldit_runner::orchestrator::BroadcastPayload::Delta(bytes)),
            Err(_) => {
                let _ = self.queue_full_broadcast();
            }
        }
    }

    /// Decide whether a single-entity mutation can be broadcast as a
    /// coord-only `SetEntityCoords` delta. Returns `Some(edit)` when
    /// the prior and new payloads share id, type, atom count, and
    /// full polymer topology (residue layout + per-residue variants);
    /// returns `None` when anything else changed — at which point the
    /// caller should fall back to a `Full` broadcast.
    ///
    /// Atom-level fields beyond `position` (element, name,
    /// formal_charge, occupancy, b_factor) are intentionally not
    /// re-checked here: action lifecycle mutations only touch
    /// positions, and a delta receiver that applies `SetEntityCoords`
    /// preserves its own copy of those fields. A receiver that needs
    /// non-position atom updates falls back through the Full path.
    fn coord_only_delta(prior: &MoleculeEntity, new: &MoleculeEntity) -> Option<AssemblyEdit> {
        if prior.id() != new.id() {
            return None;
        }
        if prior.molecule_type() != new.molecule_type() {
            return None;
        }
        if prior.atom_count() != new.atom_count() {
            return None;
        }
        match (prior.residues(), new.residues()) {
            (Some(p_res), Some(n_res)) => {
                if p_res.len() != n_res.len() {
                    return None;
                }
                for (p, n) in p_res.iter().zip(n_res.iter()) {
                    if p.name != n.name
                        || p.atom_range != n.atom_range
                        || p.variants != n.variants
                    {
                        return None;
                    }
                }
            }
            (None, None) => {}
            _ => return None,
        }
        Some(AssemblyEdit::SetEntityCoords {
            entity: new.id(),
            coords: new.positions(),
        })
    }

    /// Queue the appropriate broadcast for a single-entity mutation:
    /// `Delta` (one `SetEntityCoords` edit) when the change is
    /// coord-only, otherwise `Full`. Centralizes the prior/new diff so
    /// `commit_action` and `record_entity_update` route through one
    /// decision point.
    pub(super) fn queue_single_entity_update_broadcast(
        &mut self,
        prior: &MoleculeEntity,
        new: &MoleculeEntity,
    ) {
        if let Some(edit) = Self::coord_only_delta(prior, new) {
            self.queue_delta_broadcast(std::slice::from_ref(&edit));
        } else {
            let _ = self.queue_full_broadcast();
        }
    }

    /// Whole-assembly coord diff. Returns `Some(edits)` (possibly empty
    /// when nothing moved) when every entity in `new` shares id +
    /// topology with the entity at the same position in `prior`;
    /// returns `None` otherwise (entity added, removed, reordered, or
    /// any per-entity topology divergence). Used by history navigation:
    /// same-topology nav emits Delta, cross-topology jumps fall back
    /// to Full.
    fn assembly_coord_delta(prior: &Assembly, new: &Assembly) -> Option<Vec<AssemblyEdit>> {
        let prior_entities = prior.entities();
        let new_entities = new.entities();
        if prior_entities.len() != new_entities.len() {
            return None;
        }
        let mut edits = Vec::new();
        for (p, n) in prior_entities.iter().zip(new_entities.iter()) {
            let edit = Self::coord_only_delta(p, n)?;
            if p.positions() == n.positions() {
                continue;
            }
            edits.push(edit);
        }
        Some(edits)
    }

    /// Queue the appropriate broadcast after a multi-entity mutation
    /// (history navigation): `Delta` when the post-state shares
    /// topology with `prior`, otherwise `Full`. Empty edit lists still
    /// queue a Delta — the orchestrator's gen counter advances on
    /// every authoritative mutation regardless of whether the
    /// assembly content changed.
    pub(super) fn queue_assembly_update_broadcast(&mut self, prior: &Assembly) {
        let new = self.head_assembly();
        match Self::assembly_coord_delta(prior, &new) {
            Some(edits) => self.queue_delta_broadcast(&edits),
            None => {
                let _ = self.queue_full_broadcast();
            }
        }
    }
}
