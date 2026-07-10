//! `Session`'s mutating surface: every `&mut self` method funnels its
//! state change through the [`Session::apply`] emit gate. Split out of
//! `session/mod.rs` to keep both files readable; the inherent impl lives
//! in this child module of `session`, so the methods stay callable
//! everywhere and reach `Session`'s private fields + the `pub(super)`
//! [`Session::apply`] funnel without any visibility widening.

use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Arc;

use molex::entity::molecule::id::{EntityId, EntityIdAllocator};
use molex::MoleculeEntity;
use viso::Focus;

use crate::history::{CheckpointId, CheckpointKind, EntitySnapshotId, FilterStatus, History};

/// Whether `dst` and `src` describe the same atoms, so a stream frame can be
/// applied by overwriting the value columns instead of cloning the entity.
///
/// This partitions `MoleculeEntity` into *identity* — everything checked
/// here, plus the bonds and segment breaks derived from it — and *value* —
/// the columns [`overwrite_value_columns`] copies. Only identity forces the
/// clone path; a value change is applied in place.
///
/// Deliberately stricter than an atom-count compare. A same-size mutation
/// (ASP -> ASN: eight atoms either way) leaves counts and residue ranges
/// untouched while changing residue names, elements and bonds; such a frame
/// must clone. Each check is an allocation-free scan, cheaper than the clone
/// it guards.
fn composition_matches(dst: &MoleculeEntity, src: &MoleculeEntity) -> bool {
    let (cd, cs) = (dst.columns(), src.columns());
    if dst.molecule_type() != src.molecule_type()
        || dst.atom_count() != src.atom_count()
        || cd.element != cs.element
        || cd.name != cs.name
        || cd.formal_charge != cs.formal_charge
    {
        return false;
    }
    match (dst.residues(), src.residues()) {
        (None, None) => true,
        (Some(a), Some(b)) => {
            a.len() == b.len()
                && a.iter().zip(b.iter()).all(|(x, y)| {
                    x.name == y.name
                        && x.atom_range == y.atom_range
                        && x.variants == y.variants
                })
        }
        _ => false,
    }
}

/// Copy the per-atom columns a plugin owns without changing identity:
/// `position` and `observed`. Guarded by [`composition_matches`], so the
/// column lengths and atom ordering already match.
///
/// `b_factor` and `occupancy` are deliberately retained, not copied: they are
/// static crystallographic input metadata the plugin neither consumes nor
/// produces, so `dst`'s parsed values must survive a value-only frame.
/// [`composition_matches`] has asserted identical atom count and ordering, so
/// the retained columns stay index-aligned with the incoming positions.
fn overwrite_value_columns(dst: &mut MoleculeEntity, src: &MoleculeEntity) {
    let src_cols = src.columns();
    let dst_cols = dst.columns_mut();
    dst_cols.position.copy_from_slice(&src_cols.position);
    dst_cols.observed.copy_from_slice(&src_cols.observed);
}

/// Carry the crystallographic columns (`b_factor`, `occupancy`) from `old`
/// onto `new`, matching atoms by `(residue index, atom name)`. Atom indices
/// are not usable here: residue completion inserts hydrogens, so `old` and
/// `new` disagree on atom count and ordering. Atoms in `new` with no name
/// match in the same residue (completion-added hydrogens) keep whatever they
/// arrived with.
///
/// A no-op when either entity lacks residues, or the two residue counts
/// differ: a residue-count change means the two are not the same molecule
/// residue-for-residue, so a partial carry would misattribute B-factors.
fn carry_crystallographic_columns(new: &mut MoleculeEntity, old: &MoleculeEntity) {
    let (Some(old_res), Some(new_res)) = (old.residues(), new.residues()) else {
        log::debug!("carry_crystallographic_columns: non-polymer entity, skipping");
        return;
    };
    if old_res.len() != new_res.len() {
        log::debug!(
            "carry_crystallographic_columns: residue count {} -> {}, skipping",
            old_res.len(),
            new_res.len()
        );
        return;
    }

    let old_cols = old.columns();
    let mut by_key: HashMap<(usize, &[u8]), (f32, f32)> = HashMap::new();
    for (res_idx, res) in old_res.iter().enumerate() {
        for atom_idx in res.atom_range.clone() {
            let _ = by_key.insert(
                (
                    res_idx,
                    crate::atom_name::trimmed_atom_name(&old_cols.name[atom_idx]),
                ),
                (old_cols.b_factor[atom_idx], old_cols.occupancy[atom_idx]),
            );
        }
    }

    // Resolve the write plan while `new`'s residues/columns are borrowed
    // immutably, then apply it under the mutable borrow.
    let writes: Vec<(usize, f32, f32)> = {
        let new_cols = new.columns();
        let mut w = Vec::new();
        for (res_idx, res) in new_res.iter().enumerate() {
            for atom_idx in res.atom_range.clone() {
                let key = (
                    res_idx,
                    crate::atom_name::trimmed_atom_name(&new_cols.name[atom_idx]),
                );
                if let Some(&(b, occ)) = by_key.get(&key) {
                    w.push((atom_idx, b, occ));
                }
            }
        }
        w
    };

    let new_cols = new.columns_mut();
    for (atom_idx, b, occ) in writes {
        new_cols.b_factor[atom_idx] = b;
        new_cols.occupancy[atom_idx] = occ;
    }
}

use super::change::HeadMoveCause;
use super::{Puzzle, Session, SessionError, SessionUpdate};

impl Session {
    /// Begin a streaming action over `entities` under the caller-supplied
    /// `request_id` (allocated by the orchestrator, the single id
    /// authority). Opens one tentative lane per entity, each forked from
    /// its own current lane head. Opens the edit under `request_id` (the
    /// caller already holds it). A single-entity action passes a
    /// one-element set; the multi-entity post-Init adoption passes
    /// the full touched set. Refused if any named entity has no committed
    /// lane or already holds an open tentative. `selection` is the per-entity
    /// residue set the action operates on, retained on the pending edit for
    /// the pulse overlay; an empty map marks a whole-lane op.
    pub fn begin_action(
        &mut self,
        entities: impl IntoIterator<Item = EntityId>,
        kind: CheckpointKind,
        label: impl Into<Cow<'static, str>>,
        request_id: u64,
        selection: std::collections::BTreeMap<EntityId, BTreeSet<u32>>,
    ) -> Result<(), SessionError> {
        self.history
            .begin_action(entities, kind, label.into(), request_id, selection)?;
        Ok(())
    }

    /// Per-cycle update of the in-flight action. Mutates the tentative
    /// snapshot's payload via `Arc::make_mut` and updates the tentative
    /// checkpoint's score / filter status. Bumps `live_version` only
    /// (no DAG topology change).
    ///
    /// Emits one tentative [`SessionUpdate::Edit`] carrying the locked
    /// entity's post-mutation coordinates. The runner projector skips
    /// tentative edits (plugins don't see live frames); it completes the
    /// `SessionUpdate` stream for the render projector.
    pub fn action_update(
        &mut self,
        request_id: u64,
        raw_score: Option<f64>,
        game_score: Option<f64>,
        filter_status: Option<FilterStatus>,
        mutate: impl FnMut(&mut MoleculeEntity),
    ) -> Result<(), SessionError> {
        self.history
            .action_update(request_id, raw_score, game_score, filter_status, mutate)?;
        self.apply(SessionUpdate::Edit { tentative: true });
        Ok(())
    }

    /// Commit the action identified by `request_id`. Composes and mints
    /// the committed checkpoint from the committed graph head plus the
    /// edit's lanes; recomputes best cursors. Returns the now-committed
    /// checkpoint id.
    pub fn commit_action(&mut self, request_id: u64) -> Result<CheckpointId, SessionError> {
        let ckpt = self.history.commit_action(request_id)?;
        self.apply(SessionUpdate::HeadMoved {
            cause: HeadMoveCause::Commit,
        });
        Ok(ckpt)
    }

    /// Commit the action identified by `request_id` and re-open an edit
    /// over the same lanes under the same id for the next segment of a
    /// multi-segment preview op. Mints the committed checkpoint exactly
    /// like [`Self::commit_action`], then re-forks each lane from its
    /// just-committed head reusing the prior edit's kind and label.
    /// Returns the committed checkpoint id.
    pub fn commit_and_reopen(&mut self, request_id: u64) -> Result<CheckpointId, SessionError> {
        let ckpt = self.history.commit_and_reopen(request_id)?;
        self.apply(SessionUpdate::HeadMoved {
            cause: HeadMoveCause::Commit,
        });
        Ok(ckpt)
    }

    /// Abort the action identified by `request_id`. Removes its tentative
    /// snapshot(s); lane heads fall back to their parents.
    pub fn abort_action(&mut self, request_id: u64) -> Result<(), SessionError> {
        self.history.abort_action(request_id)?;
        self.apply(SessionUpdate::HeadMoved {
            cause: HeadMoveCause::Navigate,
        });
        Ok(())
    }

    /// Move checkpoint head to its parent. Returns the new head id, or
    /// `None` if already at root (in which case nothing is emitted).
    pub fn undo(&mut self) -> Result<Option<CheckpointId>, SessionError> {
        let moved = self.history.undo()?;
        if moved.is_some() {
            self.apply(SessionUpdate::HeadMoved {
                cause: HeadMoveCause::Navigate,
            });
        }
        Ok(moved)
    }

    /// Move checkpoint head forward to a child. `branch` picks among
    /// multiple children. Returns the new head id, or `None` at a leaf
    /// (in which case nothing is emitted).
    pub fn redo(
        &mut self,
        branch: Option<CheckpointId>,
    ) -> Result<Option<CheckpointId>, SessionError> {
        let moved = self.history.redo(branch)?;
        if moved.is_some() {
            self.apply(SessionUpdate::HeadMoved {
                cause: HeadMoveCause::Navigate,
            });
        }
        Ok(moved)
    }

    /// Jump checkpoint head to `id`.
    pub fn jump_checkpoint(&mut self, id: CheckpointId) -> Result<CheckpointId, SessionError> {
        let ckpt = self.history.jump_checkpoint(id)?;
        self.apply(SessionUpdate::HeadMoved {
            cause: HeadMoveCause::Navigate,
        });
        Ok(ckpt)
    }

    /// Per-entity revert: move `entity`'s lane head to `target`. Pushes
    /// a `LaneUndo` checkpoint mirroring the new lane head.
    pub fn lane_undo(
        &mut self,
        entity: EntityId,
        target: EntitySnapshotId,
    ) -> Result<CheckpointId, SessionError> {
        let ckpt = self.history.lane_undo(entity, target)?;
        self.apply(SessionUpdate::HeadMoved {
            cause: HeadMoveCause::Navigate,
        });
        Ok(ckpt)
    }

    /// Per-entity redo: move `entity`'s lane head to a child of the
    /// current lane head. `branch` picks among multiple children.
    pub fn lane_redo(
        &mut self,
        entity: EntityId,
        branch: Option<EntitySnapshotId>,
    ) -> Result<CheckpointId, SessionError> {
        let ckpt = self.history.lane_redo(entity, branch)?;
        self.apply(SessionUpdate::HeadMoved {
            cause: HeadMoveCause::Navigate,
        });
        Ok(ckpt)
    }

    pub fn pin_checkpoint(&mut self, id: CheckpointId) -> Result<(), SessionError> {
        self.history.pin_checkpoint(id)?;
        self.apply(SessionUpdate::CurationChanged);
        Ok(())
    }

    pub fn unpin_checkpoint(&mut self, id: CheckpointId) -> Result<(), SessionError> {
        self.history.unpin_checkpoint(id)?;
        self.apply(SessionUpdate::CurationChanged);
        Ok(())
    }

    pub fn set_exclude_from_best(
        &mut self,
        id: CheckpointId,
        exclude: bool,
    ) -> Result<(), SessionError> {
        self.history.set_exclude_from_best(id, exclude)?;
        self.apply(SessionUpdate::CurationChanged);
        Ok(())
    }

    /// Stamp scores on the current head checkpoint in place. Canonical
    /// score write: updates the head checkpoint, bumps the History's
    /// `live_version` (the history panel's live cursor), and emits one
    /// [`SessionUpdate::ScoresChanged`] when a value was actually written
    /// (the GUI score-widget channel). Plugins compute their own scores
    /// and never observe this signal.
    pub fn set_head_scores(
        &mut self,
        raw_score: Option<f64>,
        game_score: Option<f64>,
        breakdown: Option<crate::scores::StoredBreakdown>,
    ) {
        if self
            .history
            .set_head_scores(raw_score, game_score, breakdown)
        {
            self.apply(SessionUpdate::ScoresChanged);
        }
    }

    /// Stamp a composition score on the open edit `request_id`. Targets the
    /// named edit so two concurrent edits' scores never collide; the score
    /// transfers onto the checkpoint that edit mints at commit. Emits one
    /// [`SessionUpdate::ScoresChanged`] when a value was actually written.
    pub fn set_edit_scores(
        &mut self,
        request_id: u64,
        raw_score: Option<f64>,
        game_score: Option<f64>,
        breakdown: Option<crate::scores::StoredBreakdown>,
    ) {
        if self
            .history
            .set_edit_scores(request_id, raw_score, game_score, breakdown)
        {
            self.apply(SessionUpdate::ScoresChanged);
        }
    }

    /// Stamp a composition score on the committed checkpoint `id` (the
    /// commit-time stamp once the reply for its composed union returns).
    /// Emits one [`SessionUpdate::ScoresChanged`] when a value was actually
    /// written.
    pub fn set_checkpoint_scores(
        &mut self,
        id: CheckpointId,
        raw_score: Option<f64>,
        game_score: Option<f64>,
        breakdown: Option<crate::scores::StoredBreakdown>,
    ) {
        if self
            .history
            .set_checkpoint_scores(id, raw_score, game_score, breakdown)
        {
            self.apply(SessionUpdate::ScoresChanged);
        }
    }

    /// Insert a new preview entity. Allocates a fresh id, sets the
    /// entity's id to it, and stores it in `transient` plus
    /// `metadata`. Bypasses history.
    pub fn insert_preview(&mut self, mut entity: MoleculeEntity, name: String) -> EntityId {
        let id = self.allocator.allocate();
        entity.set_id(id);
        let _ = self.transient.insert(id, Arc::new(entity));
        let _ = self.metadata.insert(id, Arc::from(name));
        self.apply(SessionUpdate::PreviewAdded);
        id
    }

    /// Remove a preview (cancel / error path). Drops the metadata too.
    /// Returns `true` if a preview was removed.
    pub fn remove_preview(&mut self, id: EntityId) -> bool {
        if self.transient.shift_remove(&id).is_none() {
            return false;
        }
        let _ = self.metadata.shift_remove(&id);
        self.apply(SessionUpdate::PreviewDiscarded);
        true
    }

    /// Update an existing preview's geometry in place (a streaming
    /// frame). Keeps the same id and metadata, so the published id set is
    /// unchanged and the render projector animates coords without a
    /// topology swap. No-op (returns `false`) if `id` is not a preview.
    pub fn update_preview(&mut self, id: EntityId, mut entity: MoleculeEntity) -> bool {
        if !self.transient.contains_key(&id) {
            return false;
        }
        entity.set_id(id);
        let _ = self.transient.insert(id, Arc::new(entity));
        self.apply(SessionUpdate::PreviewUpdated);
        true
    }

    /// Promote a preview into history. Removes it from `transient` and
    /// pushes one checkpoint via [`History::add_entity`] with `kind`
    /// (typically [`CheckpointKind::PromotedPreview`] or one of the
    /// ML kinds). Optionally stamps a final name.
    /// Refused if the preview is unknown or an action is in flight.
    pub fn promote_preview(
        &mut self,
        id: EntityId,
        kind: CheckpointKind,
        name: Option<String>,
        label: impl Into<Cow<'static, str>>,
    ) -> Result<CheckpointId, SessionError> {
        let payload = self
            .transient
            .shift_remove(&id)
            .ok_or(SessionError::NotAPreview { id })?;

        if let Some(n) = name {
            let _ = self.metadata.insert(id, Arc::from(n));
        }

        match self.history.add_entity(id, payload, kind, label.into()) {
            Ok(ckpt) => {
                self.apply(SessionUpdate::HeadMoved {
                    cause: HeadMoveCause::Commit,
                });
                Ok(ckpt)
            }
            Err(e) => {
                // Restore the transient entry on failure so the caller
                // can retry. We can't recover the original payload
                // because `add_entity` consumed it on the error path
                // before failing - but the only failure modes are
                // ActiveActionInProgress and EntityAlreadyExists, both
                // of which are caller-fixable; rebuilding the payload
                // from a re-snapshotted entity is out of scope here.
                Err(SessionError::History(e))
            }
        }
    }

    /// Move one freshly-loaded entity through the preview→promote pipeline
    /// so it lands in history with an `AddEntity` checkpoint. Returns the
    /// committed [`EntityId`].
    ///
    /// Ambient (water / ion / solvent) and zero-residue entities - the
    /// het-residue stubs the parser emits for cofactors / waters in
    /// structure files - are kept as previews (transient) so viso still
    /// renders them, but they DO NOT push a history checkpoint. They aren't
    /// undoable from the user's perspective; pushing one `AddEntity` per
    /// stub clutters the history (`1bfe` produced 3 root-level dots: one
    /// `Loaded` + two `AddEntity` for chain A and a water).
    ///
    /// The initial load now seeds through [`Self::seed_history_with_entities`];
    /// this single-entity add path stays for the runtime add case (not yet
    /// wired on a production path) and is exercised by the session tests.
    #[allow(dead_code)]
    pub(crate) fn load_entity_into_history(
        &mut self,
        entity: molex::MoleculeEntity,
        name: &str,
    ) -> Option<EntityId> {
        use molex::MoleculeType;
        let mol_type = entity.molecule_type();
        let is_ambient = matches!(
            mol_type,
            MoleculeType::Water | MoleculeType::Ion | MoleculeType::Solvent
        );
        let zero_residue = entity.residue_count() == 0;
        let id = self.insert_preview(entity, name.to_owned());
        if is_ambient || zero_residue {
            // Leave it transient: visible in viso, absent from history.
            return Some(id);
        }
        match self.promote_preview(
            id,
            CheckpointKind::AddEntity {
                entity: id,
                kind: mol_type,
            },
            None,
            std::borrow::Cow::Owned(format!("Loaded {name}")),
        ) {
            Ok(_) => Some(id),
            Err(e) => {
                log::error!("Failed to promote loaded entity '{name}': {e}");
                None
            }
        }
    }

    /// Seed a fresh history with the loaded structure as its single "Initial
    /// state" root: every non-ambient entity becomes a root snapshot under one
    /// `Loaded` checkpoint, instead of each entity pushing its own `AddEntity`
    /// dot atop a separate empty root.
    ///
    /// Ambient (water / ion / solvent) and zero-residue entities - the
    /// het-residue stubs the parser emits for cofactors / waters - stay
    /// transient (visible in viso, absent from history), mirroring
    /// [`Self::load_entity_into_history`].
    ///
    /// The caller must present a clean allocator (call [`Self::reset`] first if
    /// a prior structure was loaded) and hold no in-flight action. Returns one
    /// id per input entity, in input order (ambient included).
    pub(crate) fn seed_history_with_entities(
        &mut self,
        entities: impl IntoIterator<Item = molex::MoleculeEntity>,
        source: std::path::PathBuf,
        name: &str,
    ) -> Vec<molex::EntityId> {
        use molex::MoleculeType;
        let mut ids: Vec<EntityId> = Vec::new();
        let mut seed: Vec<(EntityId, MoleculeEntity)> = Vec::new();
        for mut entity in entities {
            let is_ambient = matches!(
                entity.molecule_type(),
                MoleculeType::Water | MoleculeType::Ion | MoleculeType::Solvent
            );
            let zero_residue = entity.residue_count() == 0;
            if is_ambient || zero_residue {
                ids.push(self.insert_preview(entity, name.to_owned()));
            } else {
                let id = self.allocator.allocate();
                entity.set_id(id);
                let _ = self.metadata.insert(id, Arc::from(name));
                ids.push(id);
                seed.push((id, entity));
            }
        }
        self.history = History::new(seed, source);
        self.apply(SessionUpdate::HeadMoved {
            cause: HeadMoveCause::Commit,
        });
        ids
    }

    /// Overwrite the ongoing action's tentative payload from a streaming
    /// assembly. `action_update` fans the closure across every lane the edit
    /// locked, and each lane is rewritten only when the incoming assembly
    /// carries a matching entity id - so a single-entity edit rewrites its one
    /// lane and a multi-entity edit (post-Init adoption) rewrites each of
    /// its lanes that the stream touched. Score fields are propagated when the
    /// plugin embedded a total; per-residue / game scoring stay on their own
    /// refresh path.
    ///
    /// Returns `true` if at least one payload swap actually fired.
    pub(crate) fn apply_streaming_assembly(
        &mut self,
        incoming: &molex::Assembly,
        raw_score: Option<f64>,
        request_id: u64,
    ) -> bool {
        let mut applied = false;
        let res = self.action_update(request_id, raw_score, raw_score, None, |entity_mut| {
            if let Some(src) = incoming.entity(entity_mut.id()) {
                if composition_matches(entity_mut, src) {
                    // Value-only stream frame (wiggle/shake/pull): overwrite
                    // the per-atom columns in place. The `clone()` below
                    // reallocates the bond list and residue table too, per
                    // frame, for identity the plugin did not touch.
                    overwrite_value_columns(entity_mut, src);
                } else {
                    // Identity changed (post-Init adoption adds completion
                    // hydrogens): adopt the incoming entity wholesale, then
                    // carry the parsed crystallographic columns across by
                    // (residue, atom name) - the plugin does not own them and
                    // atom indices no longer line up.
                    let old = std::mem::replace(entity_mut, src.clone());
                    carry_crystallographic_columns(entity_mut, &old);
                }
                applied = true;
            }
        });
        if let Err(e) = res {
            log::trace!("action_update skipped: {e}");
            return false;
        }
        applied
    }

    // Selection: ambient residue selection (not history-versioned).

    /// Mark a single residue on `entity` as selected. Idempotent:
    /// re-selecting an already-selected residue is a no-op (still emits).
    pub fn select_residue(&mut self, entity: EntityId, residue_index: u32) {
        self.selection
            .entry(entity)
            .or_default()
            .insert(residue_index);
        self.apply(SessionUpdate::SelectionChanged);
    }

    /// Bulk-replace the selection on a single entity. The provided
    /// residues become the entity's full set (not merged into the
    /// existing one). An empty input removes the entity entry.
    pub fn set_residues_on(&mut self, entity: EntityId, residues: impl IntoIterator<Item = u32>) {
        let set: BTreeSet<u32> = residues.into_iter().collect();
        if set.is_empty() {
            self.selection.remove(&entity);
        } else {
            self.selection.insert(entity, set);
        }
        self.apply(SessionUpdate::SelectionChanged);
    }

    /// Drop the entire selection across all entities.
    pub fn clear_selection(&mut self) {
        self.selection.clear();
        self.apply(SessionUpdate::SelectionChanged);
    }

    /// Flip the selected state of `(entity, residue_index)` and return
    /// the new state (`true` if now selected, `false` if now
    /// deselected). Maintains the empty-set-removal invariant.
    pub fn toggle_residue(&mut self, entity: EntityId, residue_index: u32) -> bool {
        let set = self.selection.entry(entity).or_default();
        let now_selected = set.insert(residue_index);
        if !now_selected {
            set.remove(&residue_index);
            if set.is_empty() {
                self.selection.remove(&entity);
            }
        }
        self.apply(SessionUpdate::SelectionChanged);
        now_selected
    }

    // Per-entity appearance: ambient render overrides (not history-versioned).

    /// Merge a single appearance override field into an entity's overrides.
    /// Clones the entity's current overrides (or the default when none),
    /// merges `field`/`value` via [`viso::DisplayOverrides::apply_json_field`],
    /// then either stores the merged overrides back or removes the entry when
    /// the merge left it empty. An unknown field is logged and skipped (no
    /// state change, no emit), matching the engine's own merge. On a
    /// successful merge emits exactly one
    /// [`SessionUpdate::EntityAppearanceChanged`].
    pub fn set_entity_appearance_field(
        &mut self,
        id: EntityId,
        field: &str,
        value: &serde_json::Value,
    ) {
        let mut ovr = self.appearance.get(&id).cloned().unwrap_or_default();
        if let Err(unknown) = ovr.apply_json_field(field, value) {
            log::warn!("Unknown entity appearance field: {unknown}");
            return;
        }
        if ovr.is_empty() {
            self.appearance.remove(&id);
        } else {
            self.appearance.insert(id, ovr);
        }
        self.apply(SessionUpdate::EntityAppearanceChanged);
    }

    /// Set (or clear) an entity's `provisional` appearance override directly,
    /// without going through the JSON field path: `provisional` is
    /// host-internal transient render state (a discardable preview ghost),
    /// not a GUI-editable appearance field, so it has no `apply_json_field`
    /// arm. Clones the entity's current overrides (or the default when none),
    /// sets `provisional` to `Some(true)` when `on`, else `None`, leaving the
    /// other override fields intact, then stores the result back or removes
    /// the entry when it left nothing set. Emits exactly one
    /// [`SessionUpdate::EntityAppearanceChanged`].
    pub fn set_entity_provisional(&mut self, id: EntityId, on: bool) {
        let mut ovr = self.appearance.get(&id).cloned().unwrap_or_default();
        ovr.provisional = on.then_some(true);
        if ovr.is_empty() {
            self.appearance.remove(&id);
        } else {
            self.appearance.insert(id, ovr);
        }
        self.apply(SessionUpdate::EntityAppearanceChanged);
    }

    /// Remove an entity's whole appearance override entry, reverting it to
    /// inherited/global appearance. Emits exactly one
    /// [`SessionUpdate::EntityAppearanceChanged`] only when an entry was
    /// actually removed; the render projector's removal-diff then clears
    /// the engine working copy. Clearing an absent id is a silent no-op
    /// (no emit), so a stray reset never drives a wasted reconcile.
    pub fn clear_entity_appearance(&mut self, id: EntityId) {
        if self.appearance.remove(&id).is_some() {
            self.apply(SessionUpdate::EntityAppearanceChanged);
        }
    }

    // Focus: ambient session focus (not history-versioned).

    /// Set the session focus. Emits exactly one
    /// [`SessionUpdate::FocusChanged`] when the value changes; an
    /// idempotent re-focus emits nothing.
    pub fn set_focus(&mut self, focus: Focus) {
        if self.focus != focus {
            self.focus = focus;
            self.apply(SessionUpdate::FocusChanged);
        }
    }

    // Puzzle add-on + tutorial bubbles: ambient session state (not history-versioned).

    /// Install a puzzle add-on (a puzzle load). Always emits
    /// [`SessionUpdate::PuzzleChanged`].
    pub fn set_puzzle(&mut self, puzzle: Puzzle) {
        self.puzzle = Some(puzzle);
        self.apply(SessionUpdate::PuzzleChanged);
    }

    /// Install (or clear) the free-form session density computed on a
    /// `--with-density` load. Silent: it is load-time state consumed by the
    /// plugin-bringup path, not a change signal any projector listens on.
    pub fn set_session_density(&mut self, d: Option<crate::puzzle_load::DensityAsset>) {
        self.density = d;
    }

    /// Install this session's structure factors. Load-time state consumed by
    /// the plugin-bringup path; no projector listens on it.
    pub fn set_session_reflns(&mut self, r: Option<crate::puzzle_load::ReflnsAsset>) {
        self.reflns = r;
    }

    /// Drop the puzzle add-on and revert to the free-form session (a
    /// free-form structure load). Emits [`SessionUpdate::PuzzleChanged`]
    /// only when there was a puzzle to clear.
    pub fn clear_puzzle(&mut self) {
        let changed = self.puzzle.is_some();
        self.puzzle = None;
        if changed {
            self.apply(SessionUpdate::PuzzleChanged);
        }
    }

    /// Install the resolved per-entity design gating on the loaded puzzle.
    /// Called by the load path after the chain->EntityId mapping is known
    /// (the entities must already be in history). Silent (no `SessionUpdate`):
    /// it is load-time state set once before the first projection, and the
    /// design-gating projector reads it by query at projection time rather
    /// than off a change signal. A no-op when no puzzle is installed
    /// (free-form load).
    pub(crate) fn set_puzzle_design_gating(
        &mut self,
        gating: Option<std::collections::BTreeMap<EntityId, crate::puzzle_setup::DesignMask>>,
    ) {
        if let Some(puzzle) = self.puzzle.as_mut() {
            puzzle.set_design_gating(gating);
        }
    }

    /// Register a newly-adopted design entity as fully designable, so every
    /// residue on `entity` answers `true` to [`Session::is_designable`]. A
    /// no-op when no puzzle is installed or gating is not already active.
    pub(crate) fn register_full_designable_entity(
        &mut self,
        entity: EntityId,
        residue_count: usize,
    ) {
        if let Some(puzzle) = self.puzzle.as_mut() {
            puzzle.register_full_designable_entity(entity, residue_count);
        }
    }

    /// Begin a session over a freshly-loaded structure: install its display
    /// `title` and `puzzle` add-on in one funnel. `puzzle` is `Some` for a
    /// campaign/intro puzzle load (carrying its filters + tutorial
    /// bubbles) and `None` for a free-form structure load. The single
    /// create seam every load path routes through. The `PuzzleChanged`
    /// comes from the inner [`Self::set_puzzle`] / [`Self::clear_puzzle`];
    /// a title-only change (a free-form reload that leaves `puzzle` `None`)
    /// is silent here, so its caller raises the puzzle-panel refresh
    /// explicitly.
    pub fn start(&mut self, title: String, puzzle: Option<Puzzle>) {
        self.title = title;
        match puzzle {
            Some(p) => self.set_puzzle(p),
            None => self.clear_puzzle(),
        }
    }

    /// Step the tutorial-bubble cursor of the active puzzle. No-op when no
    /// puzzle is loaded or the puzzle carries no tutorial sequence. Forward
    /// saturates at the sequence length (one past the last bubble; the GUI
    /// then shows no bubble); back saturates at 0. Emits
    /// [`SessionUpdate::BubbleChanged`] only when the cursor actually moves
    /// - a step at either clamp is silent.
    pub fn advance_bubble(&mut self, back: bool) {
        let moved = self.puzzle.as_mut().is_some_and(|p| p.advance_bubble(back));
        if moved {
            self.apply(SessionUpdate::BubbleChanged);
        }
    }

    /// Drop the entire history graph, clear metadata and transient.
    /// After `reset`, the store is back to the empty initial state;
    /// callers populate it via the preview API + `promote_preview`, which
    /// runs through the same recorded path as RF3 / RFD3 / MPNN
    /// promotions, by design.
    pub fn reset(&mut self) {
        self.metadata.clear();
        self.transient.clear();
        self.allocator = EntityIdAllocator::new();
        self.history = History::new(std::iter::empty(), PathBuf::new());
        // Everything below is ambient session state tied to the outgoing
        // structure: the entity-id-keyed maps (selection, appearance)
        // would alias the incoming assembly's reused ids (the allocator
        // restarts), and the rest (focus, puzzle) belongs to the structure
        // being dropped. Cleared silently; the reset's own `HeadMoved` below
        // stands in for the topology swap.
        self.selection.clear();
        self.appearance.clear();
        self.previews.clear();
        self.focus = Focus::default();
        self.puzzle = None;
        self.density = None;
        // `title` and the view options + active preset are left untouched: the
        // following load re-sets the title via the `start` seam, and view state
        // lives on `App` and persists there, so each carries across the swap.
        // Drop any changes emitted before the reset - they describe state
        // that no longer exists. Cleared BEFORE the reset's own emit below
        // so that change survives. The runner projector's published snapshot
        // is intentionally NOT cleared: the post-reset empty-assembly diff
        // still advances the host's gen counter, so plugins never see
        // `from_gen` go backwards.
        self.pending_updates.clear();
        self.apply(SessionUpdate::HeadMoved {
            cause: HeadMoveCause::Navigate,
        });
    }
}

#[cfg(test)]
#[allow(
    clippy::float_cmp,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap
)]
mod tests {
    use super::*;
    use molex::entity::molecule::atom::Atom;
    use molex::entity::molecule::polymer::Residue;
    use molex::entity::molecule::protein::ProteinEntity;
    use molex::Element;

    fn pad4(name: &str) -> [u8; 4] {
        let mut n = [b' '; 4];
        for (i, b) in name.bytes().take(4).enumerate() {
            n[i] = b;
        }
        n
    }

    /// Build a single continuous protein chain from per-residue atom specs
    /// `(name, element, b_factor)`. Positions increase monotonically so a
    /// stray position-carry would be visible; occupancy defaults to 1.0.
    /// Atoms are given in canonical order (`N, CA, C, O, ..., H`) so the
    /// constructor's canonicalize leaves storage order as written.
    fn protein_from(residue_specs: &[Vec<(&str, Element, f32)>]) -> MoleculeEntity {
        let mut atoms = Vec::new();
        let mut residues = Vec::new();
        let mut coord = 0.0_f32;
        for (r, spec) in residue_specs.iter().enumerate() {
            let start = atoms.len();
            for &(name, element, b) in spec {
                coord += 1.0;
                atoms.push(Atom {
                    position: glam::Vec3::splat(coord),
                    occupancy: 1.0,
                    b_factor: b,
                    element,
                    name: pad4(name),
                    formal_charge: 0,
                    observed: true,
                });
            }
            let end = atoms.len();
            residues.push(Residue {
                name: *b"ALA",
                label_seq_id: r as i32 + 1,
                auth_seq_id: None,
                auth_comp_id: None,
                ins_code: None,
                atom_range: start..end,
                variants: Vec::new(),
            });
        }
        let id = EntityIdAllocator::new().allocate();
        MoleculeEntity::Protein(ProteinEntity::new_continuous(
            id,
            atoms,
            residues,
            "A".to_owned(),
        ))
    }

    #[test]
    fn fast_path_retains_crystallographic_columns_and_takes_positions() {
        let mut dst = protein_from(&[vec![
            ("N", Element::N, 30.0),
            ("CA", Element::C, 31.0),
            ("C", Element::C, 32.0),
            ("O", Element::O, 33.0),
        ]]);
        for o in &mut dst.columns_mut().occupancy {
            *o = 0.5;
        }

        // Same composition, but the plugin frame zeroes b/occupancy and moves
        // the coordinates.
        let mut src = protein_from(&[vec![
            ("N", Element::N, 0.0),
            ("CA", Element::C, 0.0),
            ("C", Element::C, 0.0),
            ("O", Element::O, 0.0),
        ]]);
        for p in &mut src.columns_mut().position {
            *p += glam::Vec3::splat(100.0);
        }

        assert!(composition_matches(&dst, &src));
        let src_positions = src.columns().position.clone();

        overwrite_value_columns(&mut dst, &src);

        let cols = dst.columns();
        assert_eq!(cols.b_factor, vec![30.0, 31.0, 32.0, 33.0]);
        assert!(cols.occupancy.iter().all(|&o| o == 0.5));
        assert_eq!(cols.position, src_positions);
    }

    #[test]
    fn slow_path_carries_b_across_added_hydrogens() {
        let old = protein_from(&[
            vec![
                ("N", Element::N, 11.0),
                ("CA", Element::C, 12.0),
                ("C", Element::C, 13.0),
                ("O", Element::O, 14.0),
            ],
            vec![
                ("N", Element::N, 21.0),
                ("CA", Element::C, 22.0),
                ("C", Element::C, 23.0),
                ("O", Element::O, 24.0),
            ],
        ]);
        // Same residues, each grown by a hydrogen; all B zeroed. Atom indices
        // now disagree with `old` from residue 1 onward.
        let mut new = protein_from(&[
            vec![
                ("N", Element::N, 0.0),
                ("CA", Element::C, 0.0),
                ("C", Element::C, 0.0),
                ("O", Element::O, 0.0),
                ("H", Element::H, 0.0),
            ],
            vec![
                ("N", Element::N, 0.0),
                ("CA", Element::C, 0.0),
                ("C", Element::C, 0.0),
                ("O", Element::O, 0.0),
                ("H", Element::H, 0.0),
            ],
        ]);

        carry_crystallographic_columns(&mut new, &old);

        let b = &new.columns().b_factor;
        // Residue 0 (indices 0..5): heavy matched, H untouched.
        assert_eq!(b[0], 11.0);
        assert_eq!(b[1], 12.0);
        assert_eq!(b[2], 13.0);
        assert_eq!(b[3], 14.0);
        assert_eq!(b[4], 0.0);
        // Residue 1 (indices 5..10): matched by name despite the index shift.
        assert_eq!(b[5], 21.0);
        assert_eq!(b[6], 22.0);
        assert_eq!(b[7], 23.0);
        assert_eq!(b[8], 24.0);
        assert_eq!(b[9], 0.0);
    }

    #[test]
    fn slow_path_residue_count_mismatch_is_noop() {
        let old = protein_from(&[
            vec![
                ("N", Element::N, 5.0),
                ("CA", Element::C, 5.0),
                ("C", Element::C, 5.0),
                ("O", Element::O, 5.0),
            ],
            vec![
                ("N", Element::N, 6.0),
                ("CA", Element::C, 6.0),
                ("C", Element::C, 6.0),
                ("O", Element::O, 6.0),
            ],
        ]);
        let mut new = protein_from(&[
            vec![
                ("N", Element::N, 7.0),
                ("CA", Element::C, 7.0),
                ("C", Element::C, 7.0),
                ("O", Element::O, 7.0),
            ],
            vec![
                ("N", Element::N, 7.0),
                ("CA", Element::C, 7.0),
                ("C", Element::C, 7.0),
                ("O", Element::O, 7.0),
            ],
            vec![
                ("N", Element::N, 7.0),
                ("CA", Element::C, 7.0),
                ("C", Element::C, 7.0),
                ("O", Element::O, 7.0),
            ],
        ]);
        let before = new.columns().b_factor.clone();

        carry_crystallographic_columns(&mut new, &old);

        assert_eq!(new.columns().b_factor, before);
    }

    #[test]
    fn slow_path_matches_across_null_and_space_padding() {
        // Same residue, same logical atom (CA), but `old` stores the name
        // null-padded and `new` stores it space-padded (the two paddings
        // coexist across molex producers). Keying on the raw 4-byte buffer
        // would never match these two, silently dropping the carry; trimming
        // the padding first makes them match.
        let mut old = protein_from(&[vec![
            ("N", Element::N, 11.0),
            ("CA", Element::C, 42.0),
            ("C", Element::C, 13.0),
            ("O", Element::O, 14.0),
        ]]);
        old.columns_mut().name[1] = [b'C', b'A', 0, 0];
        let mut new = protein_from(&[vec![
            ("N", Element::N, 0.0),
            ("CA", Element::C, 0.0),
            ("C", Element::C, 0.0),
            ("O", Element::O, 0.0),
        ]]);
        new.columns_mut().name[1] = [b'C', b'A', b' ', b' '];

        carry_crystallographic_columns(&mut new, &old);

        // The CA B-factor crosses the padding boundary.
        assert_eq!(new.columns().b_factor[1], 42.0);
    }

    #[test]
    fn slow_path_keys_by_name_not_index() {
        let old = protein_from(&[vec![
            ("N", Element::N, 10.0),
            ("CA", Element::C, 20.0),
            ("C", Element::C, 30.0),
            ("O", Element::O, 40.0),
        ]]);
        let mut new = protein_from(&[vec![
            ("N", Element::N, 0.0),
            ("CA", Element::C, 0.0),
            ("C", Element::C, 0.0),
            ("O", Element::O, 0.0),
        ]]);
        // Force storage order and name to disagree: swap the N and O cells so
        // index 0 holds "O" and index 3 holds "N". Constructing with scrambled
        // input would just re-canonicalize, so permute in place after build.
        new.columns_mut().name.swap(0, 3);

        carry_crystallographic_columns(&mut new, &old);

        let cols = new.columns();
        // B follows the NAME. Index-keying would have written old[0]=10.0 at
        // index 0 (which now holds "O").
        assert_eq!(cols.name[0], pad4("O"));
        assert_eq!(cols.b_factor[0], 40.0);
        assert_eq!(cols.name[3], pad4("N"));
        assert_eq!(cols.b_factor[3], 10.0);
        assert_eq!(cols.b_factor[1], 20.0);
        assert_eq!(cols.b_factor[2], 30.0);
    }
}
