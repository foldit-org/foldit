//! The single `record` root that funnels every DAG-bearing event,
//! plus the action-lock helpers it consults.

use molex::entity::molecule::id::EntityId;

use super::{History, HistoryError, HistoryEvent, HistoryEventOutcome};

impl History {
    // ── Action-lock helpers ───────────────────────────────────────────

    /// Whether `entity`'s lane head snapshot is an open tentative (i.e.
    /// the lane already belongs to an in-flight action). `false` for an
    /// unknown entity (`do_begin` reports `UnknownEntity` for that).
    fn lane_head_is_tentative(&self, entity: EntityId) -> bool {
        self.lanes
            .get(&entity)
            .and_then(|l| l.snapshot(l.head()))
            .is_some_and(|s| s.tentative)
    }

    /// A representative locked entity to name in an `EntityLocked`
    /// refusal: the first lane of the first pending edit. Callers gate on
    /// a non-empty pending map.
    // Callers gate on a non-empty pending map, so the chain is always `Some`.
    #[allow(clippy::expect_used)]
    fn first_pending_entity(&self) -> EntityId {
        self.pending
            .values()
            .next()
            .and_then(|e| e.lanes.first())
            .map(|(eid, _)| *eid)
            .expect("first_pending_entity called with empty pending map")
    }

    // ── Private root: every DAG-bearing event funnels here ──────

    /// The single root through which every checkpoint- or lane-DAG-
    /// bearing event passes. Validates the action-lock
    /// preconditions, performs the mutation, updates `checkpoint_refs`,
    /// runs eviction, bumps `topology_version`, and asserts the cross-
    /// DAG invariant.
    ///
    /// New events land here as a new [`HistoryEvent`] variant. A
    /// sibling root would carry state this function doesn't know about
    /// and is therefore illegal.
    pub(super) fn record(
        &mut self,
        event: HistoryEvent,
    ) -> Result<HistoryEventOutcome, HistoryError> {
        // ── Action-lock pre-check ─────────────────────────────────────
        // Reframed off the pending-edit map. While any action is open
        // the committed graph head is frozen (each commit composes from
        // it), so navigation and immediate-commit mutations are refused.
        // A new action may still begin on a *free* lane (multi-lane
        // fan-out); only a lane that already has an open tentative
        // refuses begin.
        if !self.pending.is_empty() {
            match &event {
                // Commit / Abort resolve their own request_id in the arm.
                HistoryEvent::Commit { .. } | HistoryEvent::Abort { .. } => {}
                HistoryEvent::Begin { entities, .. } => {
                    if entities
                        .iter()
                        .any(|e| self.lane_head_is_tentative(*e))
                    {
                        return Err(HistoryError::ActiveActionInProgress);
                    }
                }
                // Both move the committed head, which would strand an
                // open edit's commit composition.
                HistoryEvent::RecordEntityUpdate { .. } | HistoryEvent::AddEntity { .. } => {
                    return Err(HistoryError::ActiveActionInProgress)
                }
                HistoryEvent::LaneUndo { .. }
                | HistoryEvent::LaneRedo { .. }
                | HistoryEvent::Undo
                | HistoryEvent::Redo { .. }
                | HistoryEvent::JumpCheckpoint { .. } => {
                    return Err(HistoryError::EntityLocked {
                        entity: self.first_pending_entity(),
                    })
                }
            }
        }

        // Linear-undo invariant: after a push, every checkpoint must
        // lie on the root → head path, and every snapshot must lie on
        // its lane's root → head path. Navigation-only events
        // (Undo / Redo / JumpCheckpoint) skip the prune so the user
        // can move the cursor without losing the redo path; the next
        // *push* drops it (classic editor undo).
        let is_push = matches!(
            event,
            HistoryEvent::Begin { .. }
                | HistoryEvent::RecordEntityUpdate { .. }
                | HistoryEvent::LaneUndo { .. }
                | HistoryEvent::LaneRedo { .. }
                | HistoryEvent::AddEntity { .. }
        );

        let result = match event {
            HistoryEvent::Begin {
                entities,
                kind,
                label,
                request_id,
            } => self.do_begin(&entities, kind, &label, request_id)?,
            HistoryEvent::Commit { request_id } => self.do_commit(request_id)?,
            HistoryEvent::Abort { request_id } => self.do_abort(request_id)?,
            HistoryEvent::RecordEntityUpdate {
                entity,
                kind,
                payload,
                label,
                raw_score,
                game_score,
            } => {
                self.do_record_entity_update(entity, kind, payload, label, raw_score, game_score)?
            }
            HistoryEvent::LaneUndo { entity, target } => self.do_lane_undo(entity, target)?,
            HistoryEvent::LaneRedo { entity, branch } => self.do_lane_redo(entity, branch)?,
            HistoryEvent::Undo => self.do_undo()?,
            HistoryEvent::Redo { branch } => self.do_redo(branch)?,
            HistoryEvent::JumpCheckpoint { id } => self.do_jump(id)?,
            HistoryEvent::AddEntity {
                entity_id,
                payload,
                kind,
                label,
            } => self.do_add_entity(entity_id, payload, kind, label)?,
        };

        if is_push {
            self.prune_to_head_path();
        }

        self.evict_to_budget();
        self.topology_version = self.topology_version.saturating_add(1);

        if cfg!(debug_assertions) {
            self.assert_invariant();
        }

        Ok(result)
    }
}
