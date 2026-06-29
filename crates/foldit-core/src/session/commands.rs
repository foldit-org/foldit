//! Selection / preview session commands.

use foldit_gui::HistoryCommand;
use molex::entity::molecule::id::EntityId;
use viso::{classify_click_for_selection, ClickEvent, ClickSelectionAction};

use super::{Session, SessionError};

/// Outcome of a [`HistoryCommand`] dispatch.
enum HistoryOutcome {
    /// Checkpoint head moved.
    HeadMoved,
    /// The command had no follow-up (a no-op, or a mutation that emits its
    /// own covering `SessionUpdate`).
    Noop,
}

impl Session {
    /// Cancel the in-flight operation: drop any in-progress preview
    /// entities and republish. Selection is untouched.
    pub(crate) fn cancel_operations(&mut self) {
        log::info!("Cancelling current operation");
        let preview_ids: Vec<EntityId> = self.preview_ids().collect();
        if !preview_ids.is_empty() {
            for id in &preview_ids {
                self.remove_preview(*id);
            }
            log::info!("Removed {} in-progress preview entities", preview_ids.len());
        }
    }

    /// Apply a viso click-event to the selection. Empty-area clicks clear;
    /// a non-empty expansion replaces (no modifier) or toggles (shift) on a
    /// per-residue basis.
    pub(crate) fn apply_click_to_selection(&mut self, click: &ClickEvent) {
        match classify_click_for_selection(click) {
            ClickSelectionAction::Clear => {
                self.clear_selection();
            }
            ClickSelectionAction::Replace(residues) => {
                self.clear_selection();
                for (entity, residue) in residues {
                    self.select_residue(entity, residue);
                }
            }
            ClickSelectionAction::Toggle(residues) => {
                for (entity, residue) in residues {
                    let _ = self.toggle_residue(entity, residue);
                }
            }
        }
    }

    /// Replace the selection with `entries` of `(raw_entity_id, residues)`.
    /// Raw ids are resolved against live entities; unknown ids are dropped.
    /// Empty input clears the selection.
    pub(crate) fn set_selection_entries(
        &mut self,
        entries: impl IntoIterator<Item = (u32, Vec<u32>)>,
    ) {
        self.clear_selection();
        for (entity_id, residues) in entries {
            let Some(entity) = self.ids().find(|id| id.raw() == entity_id) else {
                log::trace!("set_selection_entries: unknown entity_id {entity_id} (dropping)");
                continue;
            };
            self.set_residues_on(entity, residues);
        }
    }

    /// Apply a [`HistoryCommand`] to the session. Returns the request-ids
    /// whose score targets the caller should drop (non-empty only for an
    /// `AbortAction` that discarded open edits). Refusals are logged; the head
    /// not moving is the GUI's feedback. The match is exhaustive: adding a
    /// variant without a handler is a compile error.
    pub(crate) fn apply_history_command(&mut self, cmd: &HistoryCommand) -> Vec<u64> {
        let mut aborted: Vec<u64> = Vec::new();
        let result: Result<HistoryOutcome, SessionError> = match *cmd {
            HistoryCommand::JumpCheckpoint { id } => self
                .jump_checkpoint(id.into_inner())
                .map(|_| HistoryOutcome::HeadMoved),
            HistoryCommand::Undo => self.undo().map(|opt| {
                if opt.is_some() {
                    HistoryOutcome::HeadMoved
                } else {
                    log::info!("Undo: already at root");
                    HistoryOutcome::Noop
                }
            }),
            HistoryCommand::Redo { branch } => self
                .redo(branch.map(foldit_gui::WireId::into_inner))
                .map(|opt| {
                    if opt.is_some() {
                        HistoryOutcome::HeadMoved
                    } else {
                        log::info!("Redo: nowhere forward to go");
                        HistoryOutcome::Noop
                    }
                }),
            HistoryCommand::LaneUndo { entity, target } => self
                .lane_undo(entity, target.into_inner())
                .map(|_| HistoryOutcome::HeadMoved),
            HistoryCommand::LaneRedo { entity, branch } => self
                .lane_redo(entity, branch.map(foldit_gui::WireId::into_inner))
                .map(|_| HistoryOutcome::HeadMoved),
            HistoryCommand::PinCheckpoint { id } => self
                .pin_checkpoint(id.into_inner())
                .map(|()| HistoryOutcome::Noop),
            HistoryCommand::UnpinCheckpoint { id } => self
                .unpin_checkpoint(id.into_inner())
                .map(|()| HistoryOutcome::Noop),
            HistoryCommand::SetExcludeFromBest { id, exclude } => self
                .set_exclude_from_best(id.into_inner(), exclude)
                .map(|()| HistoryOutcome::Noop),
            HistoryCommand::AbortAction => {
                // "Discard the running action." Targeting a single edit
                // no-ops once two edits run concurrently, so discard every
                // open edit instead of silently doing nothing.
                let rids: Vec<u64> = self.pending_request_ids().collect();
                if rids.is_empty() {
                    Ok(HistoryOutcome::Noop)
                } else {
                    for rid in rids {
                        if let Err(e) = self.abort_action(rid) {
                            log::warn!("abort_action({rid}) failed: {e}");
                        }
                        aborted.push(rid);
                    }
                    Ok(HistoryOutcome::HeadMoved)
                }
            }
        };

        match result {
            Ok(HistoryOutcome::HeadMoved | HistoryOutcome::Noop) => {}
            Err(e) => log::warn!("history command refused: {e}"),
        }
        aborted
    }
}
