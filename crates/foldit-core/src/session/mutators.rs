//! `Session`'s mutating surface: every `&mut self` method funnels its
//! state change through the [`Session::apply`] emit gate. Split out of
//! `session/mod.rs` to keep both files readable; the inherent impl lives
//! in this child module of `session`, so the methods stay callable
//! everywhere and reach `Session`'s private fields + the `pub(super)`
//! [`Session::apply`] funnel without any visibility widening.

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use molex::entity::molecule::id::{EntityId, EntityIdAllocator};
use molex::MoleculeEntity;
use viso::Focus;
use viso::options::VisoOptions;

use crate::history::{
    CheckpointId, CheckpointKind, EntitySnapshotId, FilterStatus, History, HistoryError,
};

use super::{EntityMetadata, EntityOrigin, Puzzle, Session, SessionError, SessionUpdate};

impl Session {
    // ── Action lifecycle (typed mutation intent) ──────────────────

    /// Begin a streaming action over `entities` under the caller-supplied
    /// `request_id` (allocated by the orchestrator, the single id
    /// authority). Opens one tentative lane per entity, each forked from
    /// its own current lane head. Opens the edit under `request_id` (the
    /// caller already holds it). A single-entity action passes a
    /// one-element set; the multi-entity post-Init normalization passes
    /// the full touched set. Refused if any named entity has no committed
    /// lane or already holds an open tentative.
    pub fn begin_action(
        &mut self,
        entities: impl IntoIterator<Item = EntityId>,
        kind: CheckpointKind,
        label: impl Into<Cow<'static, str>>,
        request_id: u64,
    ) -> Result<(), SessionError> {
        self.history
            .begin_action(entities, kind, label.into(), request_id)?;
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
        // A tentative live frame: render projector picks it up via the
        // `SessionUpdate` stream and rebuilds from `head_assembly`. Payload-less because
        // the projectors re-derive from `Session` anyway.
        self.apply(SessionUpdate::Edit { tentative: true });
        Ok(())
    }

    /// Commit the action identified by `request_id`. Composes and mints
    /// the committed checkpoint from the committed graph head plus the
    /// edit's lanes; recomputes best cursors. Returns the now-committed
    /// checkpoint id.
    pub fn commit_action(&mut self, request_id: u64) -> Result<CheckpointId, SessionError> {
        let ckpt = self.history.commit_action(request_id)?;
        self.apply(SessionUpdate::HeadMoved);
        Ok(ckpt)
    }

    /// Abort the action identified by `request_id`. Removes its tentative
    /// snapshot(s); lane heads fall back to their parents.
    pub fn abort_action(&mut self, request_id: u64) -> Result<(), SessionError> {
        self.history.abort_action(request_id)?;
        self.apply(SessionUpdate::HeadMoved);
        Ok(())
    }

    // ── Navigation (the action-lock half is enforced one layer down by
    //    `History`) ──────────────────────────────────────────────────

    /// Move checkpoint head to its parent. Returns the new head id, or
    /// `None` if already at root (in which case nothing is emitted).
    pub fn undo(&mut self) -> Result<Option<CheckpointId>, SessionError> {
        let moved = self.history.undo()?;
        if moved.is_some() {
            self.apply(SessionUpdate::HeadMoved);
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
        let head = self.history.checkpoints().head();
        let kids: Vec<CheckpointId> = self
            .history
            .checkpoint(head)
            .map(|h| h.children.iter().copied().collect())
            .unwrap_or_default();
        match (branch, kids.as_slice()) {
            (_, []) => return Ok(None),
            (Some(b), kids) if kids.contains(&b) => {}
            (Some(_), _) => {
                return Err(SessionError::History(HistoryError::NoSuchBranch))
            }
            (None, [_]) => {}
            (None, _) => {
                return Err(SessionError::History(HistoryError::AmbiguousBranch))
            }
        }
        let moved = self.history.redo(branch)?;
        if moved.is_some() {
            self.apply(SessionUpdate::HeadMoved);
        }
        Ok(moved)
    }

    /// Jump checkpoint head to `id`.
    pub fn jump_checkpoint(&mut self, id: CheckpointId) -> Result<CheckpointId, SessionError> {
        let ckpt = self.history.jump_checkpoint(id)?;
        self.apply(SessionUpdate::HeadMoved);
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
        self.apply(SessionUpdate::HeadMoved);
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
        self.apply(SessionUpdate::HeadMoved);
        Ok(ckpt)
    }

    // ── Curation ──────────────────────────────────────────────────────

    /// Pin a checkpoint as user-marked best.
    pub fn pin_checkpoint(&mut self, id: CheckpointId) -> Result<(), SessionError> {
        Ok(self.history.pin_checkpoint(id)?)
    }

    /// Unpin a checkpoint.
    pub fn unpin_checkpoint(&mut self, id: CheckpointId) -> Result<(), SessionError> {
        Ok(self.history.unpin_checkpoint(id)?)
    }

    /// Set the "exclude from best" flag.
    pub fn set_exclude_from_best(
        &mut self,
        id: CheckpointId,
        exclude: bool,
    ) -> Result<(), SessionError> {
        Ok(self.history.set_exclude_from_best(id, exclude)?)
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
        self.debug_assert_breakdown_alignment(breakdown.as_ref());
        if self.history.set_head_scores(raw_score, game_score, breakdown) {
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
        self.debug_assert_breakdown_alignment(breakdown.as_ref());
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
        self.debug_assert_breakdown_alignment(breakdown.as_ref());
        if self
            .history
            .set_checkpoint_scores(id, raw_score, game_score, breakdown)
        {
            self.apply(SessionUpdate::ScoresChanged);
        }
    }

    /// Debug-only alignment invariant: a stored breakdown's `whole_pose_terms`
    /// and every residue's `terms` must match the session `term_names`
    /// length. The render projector zips the breakdown against `term_names`
    /// when re-deriving colors; a length mismatch would silently drop the
    /// tail or misalign, so it is caught at every write site under test /
    /// debug builds. Callers set `term_names` from the same report before
    /// stamping the breakdown, so this holds by construction.
    fn debug_assert_breakdown_alignment(
        &self,
        breakdown: Option<&crate::scores::StoredBreakdown>,
    ) {
        if let Some(b) = breakdown {
            debug_assert_eq!(
                b.whole_pose_terms.len(),
                self.term_names.len(),
                "stored whole_pose_terms must align to session term_names",
            );
            for rts in &b.per_residue_terms {
                debug_assert_eq!(
                    rts.terms.len(),
                    self.term_names.len(),
                    "stored per-residue terms must align to session term_names",
                );
            }
        }
    }

    // ── Preview API - transient, never in history ─────────────────────

    /// Insert a new preview entity. Allocates a fresh id, sets the
    /// entity's id to it, and stores it in `transient` plus
    /// `metadata`. Bypasses history.
    pub fn insert_preview(
        &mut self,
        mut entity: MoleculeEntity,
        name: String,
        origin: EntityOrigin,
    ) -> EntityId {
        let id = self.allocator.allocate();
        entity.set_id(id);
        let _ = self.transient.insert(id, Arc::new(entity));
        let _ = self
            .metadata
            .insert(id, Arc::new(EntityMetadata::new(name, origin)));
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

    /// Promote a preview into history. Removes it from `transient` and
    /// pushes one checkpoint via [`History::add_entity`] with `kind`
    /// (typically [`CheckpointKind::PromotedPreview`] or one of the
    /// ML kinds). Optionally stamps final origin / name.
    /// Refused if the preview is unknown or an action is in flight.
    pub fn promote_preview(
        &mut self,
        id: EntityId,
        kind: CheckpointKind,
        origin: Option<EntityOrigin>,
        name: Option<String>,
        label: impl Into<Cow<'static, str>>,
    ) -> Result<CheckpointId, SessionError> {
        let payload = self
            .transient
            .shift_remove(&id)
            .ok_or(SessionError::NotAPreview { id })?;

        if let Some(meta_arc) = self.metadata.get_mut(&id) {
            let meta = Arc::make_mut(meta_arc);
            if let Some(o) = origin {
                meta.origin = o;
            }
            if let Some(n) = name {
                meta.name = n;
            }
        }

        match self.history.add_entity(id, payload, kind, label.into()) {
            Ok(ckpt) => {
                self.apply(SessionUpdate::HeadMoved);
                Ok(ckpt)
            }
            Err(e) => {
                // Restore the transient entry on failure so the caller
                // can retry. We can't recover the original payload
                // because `add_entity` consumed it on the error path
                // before failing - but the only failure modes are
                // ActiveActionInProgress and EntityAlreadyExists, both
                // of which are caller-fixable; rebuilding the payload
                // from a re-snapshotted entity is a section-4 concern.
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
        let id = self.insert_preview(entity, name.to_owned(), EntityOrigin::Loaded);
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

    /// Overwrite the ongoing action's tentative payload from a streaming
    /// assembly. `action_update` fans the closure across every lane the edit
    /// locked, and each lane is rewritten only when the incoming assembly
    /// carries a matching entity id - so a single-entity edit rewrites its one
    /// lane and a multi-entity edit (post-Init normalization) rewrites each of
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
                *entity_mut = src.clone();
                applied = true;
            }
        });
        if let Err(e) = res {
            log::trace!("action_update skipped: {e}");
            return false;
        }
        applied
    }

    // ── Selection ─────────────────────────────────────────────────────
    //
    // Ambient residue selection (not history-versioned). Invariant
    // maintained across every mutator: per-entity sets are never left
    // empty in the outer map. Removing the last residue on an entity
    // removes the entity entry, so `selected_entities` yields only
    // entities that currently have at least one selected residue. Each
    // mutator that changes the selection emits exactly one
    // [`SessionUpdate::SelectionChanged`] through the funnel.

    /// Mark a single residue on `entity` as selected. Idempotent:
    /// re-selecting an already-selected residue is a no-op (but still
    /// emits, matching the prior behavior where the mutator raised its
    /// dirty bits unconditionally).
    pub fn select_residue(&mut self, entity: EntityId, residue_index: u32) {
        self.selection
            .entry(entity)
            .or_default()
            .insert(residue_index);
        self.apply(SessionUpdate::SelectionChanged);
    }

    /// Mark a single residue on `entity` as deselected. Idempotent on
    /// already-empty state. If this empties the per-entity set, the
    /// entity entry is removed from the outer map (sets are never
    /// left empty). Currently only exercised by tests.
    #[allow(dead_code)]
    pub fn deselect_residue(&mut self, entity: EntityId, residue_index: u32) {
        if let Some(set) = self.selection.get_mut(&entity) {
            set.remove(&residue_index);
            if set.is_empty() {
                self.selection.remove(&entity);
            }
        }
        self.apply(SessionUpdate::SelectionChanged);
    }

    /// Bulk-replace the selection on a single entity. The provided
    /// residues become the entity's full set (not merged into the
    /// existing one). An empty input removes the entity entry.
    pub fn set_residues_on(
        &mut self,
        entity: EntityId,
        residues: impl IntoIterator<Item = u32>,
    ) {
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

    // ── Focus ─────────────────────────────────────────────────────────
    //
    // Ambient session focus (not history-versioned). Mutating it emits
    // exactly one [`SessionUpdate::FocusChanged`], and only when the value
    // actually changes - an idempotent re-focus is silent.

    /// Set the session focus. Emits exactly one
    /// [`SessionUpdate::FocusChanged`] when the value changes; an
    /// idempotent re-focus emits nothing.
    pub fn set_focus(&mut self, focus: Focus) {
        if self.focus != focus {
            self.focus = focus;
            self.apply(SessionUpdate::FocusChanged);
        }
    }

    // ── Puzzle objective + tutorial bubbles ───────────────────────────
    //
    // Ambient session state (not history-versioned). The puzzle add-on
    // carries the objective energies and the tutorial-bubble cursor. A
    // puzzle load installs it; a free-form load clears it. Installing or
    // clearing the objective emits exactly one
    // [`SessionUpdate::PuzzleChanged`]; stepping the bubble cursor emits
    // exactly one [`SessionUpdate::BubbleChanged`].

    /// Install a puzzle objective (a puzzle load). Always emits
    /// [`SessionUpdate::PuzzleChanged`].
    pub fn set_puzzle(&mut self, puzzle: Puzzle) {
        self.puzzle = Some(puzzle);
        self.apply(SessionUpdate::PuzzleChanged);
    }

    /// Drop the puzzle objective and revert to the free-form session (a
    /// free-form structure load). Emits [`SessionUpdate::PuzzleChanged`]
    /// only when there was a puzzle to clear.
    pub fn clear_puzzle(&mut self) {
        let changed = self.puzzle.is_some();
        self.puzzle = None;
        if changed {
            self.apply(SessionUpdate::PuzzleChanged);
        }
    }

    /// Begin a session over a freshly-loaded structure: install its display
    /// `title` and `puzzle` add-on in one funnel. `puzzle` is `Some` for a
    /// campaign/intro puzzle load (carrying its objective + tutorial
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
        let Some(puzzle) = self.puzzle.as_mut() else {
            return;
        };
        let Some(cursor) = puzzle.current_bubble else {
            return;
        };
        let len = puzzle.bubbles.as_ref().map_or(0, Vec::len);
        let next = if back {
            cursor.saturating_sub(1)
        } else if cursor < len {
            cursor + 1
        } else {
            cursor
        };
        if next == cursor {
            return;
        }
        puzzle.current_bubble = Some(next);
        self.apply(SessionUpdate::BubbleChanged);
    }

    // ── View options + active preset ──────────────────────────────────
    //
    // Ambient session state (not history-versioned). The active options are
    // the source of truth for what viso renders; the `App` tick applies
    // them to the engine on each [`SessionUpdate::ViewOptionsChanged`]. A
    // manual option edit clears the active preset (the options no longer
    // match a named preset); applying a preset sets both together. Each
    // mutator emits exactly one `ViewOptionsChanged`, and only when
    // something actually changes.

    /// Set the active view options (a manual edit). Clears the active preset
    /// (manually-set options no longer match any named preset). Emits
    /// [`SessionUpdate::ViewOptionsChanged`] when the options or the active
    /// preset actually change; an idempotent set emits nothing.
    pub fn set_view_options(&mut self, options: VisoOptions) {
        let changed = self.view_options != options || self.active_preset.is_some();
        self.view_options = options;
        self.active_preset = None;
        if changed {
            self.apply(SessionUpdate::ViewOptionsChanged);
        }
    }

    /// Apply a named preset: install its `options` and record `name` as the
    /// active preset. Emits [`SessionUpdate::ViewOptionsChanged`] when the
    /// options or the active preset actually change.
    pub fn apply_preset(&mut self, name: String, options: VisoOptions) {
        let changed =
            self.view_options != options || self.active_preset.as_deref() != Some(name.as_str());
        self.view_options = options;
        self.active_preset = Some(name);
        if changed {
            self.apply(SessionUpdate::ViewOptionsChanged);
        }
    }

    // ── Score-term weights ────────────────────────────────────────────

    /// Install the score-term weight map. Silent (no `SessionUpdate`): the
    /// weights change only at load, before the first score, so no consumer
    /// needs a change signal. Called once at App init; survives reloads
    /// because [`Self::reset`] leaves `term_weights` untouched.
    pub fn set_term_weights(&mut self, weights: std::collections::HashMap<String, f32>) {
        self.term_weights = weights;
    }

    /// Install the score-term name list. Silent (no `SessionUpdate`): it
    /// rides the `ScoresChanged` that the same score write emits, and on its
    /// own carries no displayable change. Re-set from each report (idempotent
    /// in steady state); survives reloads because [`Self::reset`] leaves it
    /// untouched, like `term_weights`.
    pub fn set_term_names(&mut self, names: Vec<String>) {
        self.term_names = names;
    }

    // ── Reset ─────────────────────────────────────────────────────────

    /// Drop the entire history graph, clear metadata and transient.
    /// After `reset`, the store is back to the empty initial state;
    /// callers populate it via the preview API + `promote_preview`
    /// (the puzzle-load path used to be a one-shot insert; now it's a
    /// preview-then-promote flow that runs through the same recorded
    /// path as RF3 / RFD3 / MPNN promotions, by design).
    pub fn reset(&mut self) {
        self.metadata.clear();
        self.transient.clear();
        self.allocator = EntityIdAllocator::new();
        self.history = History::new(std::iter::empty(), PathBuf::new());
        // Selection keys are entity ids from the outgoing assembly; the
        // incoming one may reuse those ids without referring to the same
        // entities (the allocator restarts), so a stale selection must be
        // dropped on every topology swap. Co-located here so both load
        // paths (puzzle + free-form structure) clear it.
        self.selection.clear();
        // Focus is ambient session state; a topology swap returns it to
        // the all-entities view. Set silently (no `FocusChanged`): viso
        // independently resets its mirror to `All` on the assembly
        // replace, and the reset's `HeadMoved` below drives the reframe.
        self.focus = Focus::default();
        // The puzzle add-on (objective + tutorial bubbles) is ambient
        // session state tied to the outgoing structure. Clear it silently
        // here (the load path that follows a reset re-installs it via the
        // `start` create seam, whose `PuzzleChanged` drives the panel); the
        // reset's own `HeadMoved` below stands in for the topology swap.
        // `title` is left untouched: the following load's `start` overwrites
        // it, and nothing reads it between the reset and that overwrite.
        // `term_weights` is likewise left untouched: the load re-sets it via
        // the App-init seam, so it carries across the topology swap.
        self.puzzle = None;
        // View options + active preset are ambient session state; a topology
        // swap resets both to defaults (view options reset per session, not
        // persist). Unlike focus (which viso re-derives on the assembly
        // replace), nothing pushes default options to the engine on its own,
        // so this emits `ViewOptionsChanged` below when there was a non-
        // default state to clear, driving the tick's `set_options` + the
        // GUI panel refresh. A load path that wants a preset then re-applies
        // it via `apply_preset`, whose own emit reads the latest options.
        let view_changed =
            self.view_options != VisoOptions::default() || self.active_preset.is_some();
        self.view_options = VisoOptions::default();
        self.active_preset = None;
        // Drop any changes emitted before the reset - they describe state
        // that no longer exists. Cleared BEFORE the reset's own emit below
        // so that change survives. The runner projector's published snapshot is
        // intentionally NOT cleared (it lives on `RunnerProjector`): the
        // post-reset empty-assembly diff still advances the host's gen
        // counter, so plugins never see `from_gen` go backwards.
        self.pending_updates.clear();
        if view_changed {
            self.apply(SessionUpdate::ViewOptionsChanged);
        }
        self.apply(SessionUpdate::HeadMoved);
    }
}
