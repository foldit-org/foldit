//! Pull-drag gesture handling on `App`: the pointer-lifecycle FSM that
//! opens, updates, and tears down a pull stream against the routing policy.

use molex::entity::molecule::id::EntityId;
use viso::{ClickEvent, Focus, VisoEngine};

use crate::app::App;
use crate::history::CheckpointKind;
use crate::runner_client::{
    build_pull_info, route_atom_pick, route_residue_pick, PullDrag, PullRoute, StreamStartIntent,
};
use crate::session::Session;

/// Advance focus one step through `focusable`, wrapping back to
/// [`Focus::All`] after the last entity. `All` advances to the first
/// focusable entity (or stays `All` when none are focusable). The
/// Tab-cycle step, owned by foldit-core; `focusable` is viso's
/// eligibility list (`engine.focusable_entities()`).
pub(in crate::app) fn next_focus(current: Focus, focusable: &[EntityId]) -> Focus {
    match current {
        Focus::All => focusable
            .first()
            .map_or(Focus::All, |&id| Focus::Entity(id)),
        Focus::Entity(cur) => match focusable.iter().position(|&id| id == cur) {
            Some(i) if i + 1 < focusable.len() => Focus::Entity(focusable[i + 1]),
            _ => Focus::All,
        },
    }
}

/// Resolve the residue currently under the cursor to its session
/// `(EntityId, residue)`, or `None` when the hover is empty / an atom or the
/// flat index does not map to a live entity (before the first rebuild, or out
/// of range). The raw entity id is mapped to the session [`EntityId`] the same
/// way the pull-drag path does, matching on `id.raw()`.
pub(in crate::app) fn hovered_segment_target(
    engine: &VisoEngine,
    store: &Session,
) -> Option<(EntityId, usize)> {
    let viso::PickTarget::Residue(flat) = engine.hovered_target() else {
        return None;
    };
    let (raw_entity, local_residue) = engine.flat_to_entity_residue(flat)?;
    let eid = store.ids().find(|id| id.raw() == raw_entity)?;
    Some((eid, local_residue as usize))
}

impl App {
    /// Pull-drag interception, run ahead of viso's regular input routing so
    /// an active drag suppresses camera rotation/pan. Handles the three
    /// gesture phases (pull-start on the first held move after a resolved
    /// down-target, an active drag's move, and the active drag's release),
    /// each finalizing the viewport and reporting the gesture consumed.
    /// Returns `true` when it claimed the input (the caller then returns
    /// early), `false` on fall-through to the regular routing below.
    // The match-arm guard checks `pending_pull_origin.is_some()`, so the
    // `take().expect()` below is provably `Some`.
    #[allow(clippy::expect_used)]
    pub(crate) fn try_pull_drag_interception(&mut self, input: &foldit_gui::ViewportInput) -> bool {
        use foldit_gui::ViewportInput;
        match input {
            ViewportInput::PointerMove { x, y, .. }
                if self
                    .harness
                    .engine
                    .as_ref()
                    .is_some_and(viso::VisoEngine::mouse_pressed)
                    && !self.runner_client.has_active_pull_drag()
                    && self.runner_client.has_pending_pull_origin() =>
            {
                // The pull intent was locked at button-down; the move
                // only supplies the drag endpoint. Take the route so
                // this gesture makes at most one start attempt - a
                // failed stream start falls through to camera for the
                // rest of the drag rather than retrying mid-gesture.
                let route = self
                    .runner_client
                    .take_pending_pull_origin()
                    .expect("guard guarantees Some");
                if self.begin_pull_drag_from_route(&route, *x, *y) {
                    // viso recorded the press; drop its mouse
                    // state so the now-suppressed pointer-up
                    // can't fire a stray click → selection.
                    if let Some(engine) = self.harness.engine.as_mut() {
                        engine.release_mouse_state();
                    }
                    self.update_pull_drag(*x, *y);
                    self.finalize_viewport_input();
                    return true;
                }
            }
            ViewportInput::PointerMove { x, y, .. }
                if self.runner_client.has_active_pull_drag() =>
            {
                self.update_pull_drag(*x, *y);
                self.finalize_viewport_input();
                return true;
            }
            ViewportInput::PointerUp { .. } if self.runner_client.has_active_pull_drag() => {
                self.end_pull_drag();
                self.finalize_viewport_input();
                return true;
            }
            _ => {}
        }
        false
    }

    /// Called after the pull-drag interception path. Mirrors the
    /// trailing visualization update the regular `handle_viewport_input`
    /// flow does (the `SessionUpdate` drain itself is tick-driven now).
    /// Pre-snapshots the pull info so the engine borrow doesn't overlap
    /// with the live pull-drag state held in the plugin driver.
    fn finalize_viewport_input(&mut self) {
        let pull = self.runner_client.pull_drag_pull_info();
        self.harness.update_visualizations(pull);
    }

    /// Classify the pick under `(x, y)` into a pull route, or `None` if
    /// the target is empty / non-pullable (non-protein entity, hydrogen
    /// atom, no atom under the cursor). Pure resolution: reads the engine
    /// pick + store but mutates nothing, so it can run at `PointerDown`
    /// against the just-fed down-cursor to lock the pull's anchor. Takes
    /// `engine` + `store` as borrows rather than `&self` so the caller can
    /// invoke it against `self.harness.engine` and `self.store` as disjoint
    /// field borrows.
    pub(crate) fn resolve_pull_route(
        engine: &viso::VisoEngine,
        store: &Session,
        x: f32,
        y: f32,
    ) -> Option<PullRoute> {
        match engine.hovered_target() {
            viso::PickTarget::Atom {
                entity_id,
                atom_idx,
            } => route_atom_pick(store, entity_id, atom_idx),
            viso::PickTarget::Residue(flat) => {
                engine.picked_residue_atom(flat, (x, y)).and_then(|picked| {
                    let molex_id = store.ids().find(|id| id.raw() == picked.entity_id)?;
                    route_residue_pick(
                        store,
                        flat,
                        &picked.atom_name,
                        molex_id,
                        picked.local_residue,
                    )
                })
            }
            viso::PickTarget::None => None,
        }
    }

    /// Open a pull-drag stream from a pre-resolved `route` (locked at
    /// button-down) with `(x, y)` as the current drag endpoint: dispatch
    /// the matching pull op-id, open the history edit, and install drag
    /// state. Returns true if the stream started (so the caller suppresses
    /// the regular viso input flow), false if `start_stream` failed (the
    /// gesture then falls through to camera handling).
    fn begin_pull_drag_from_route(&mut self, route: &PullRoute, x: f32, y: f32) -> bool {
        let pull_info = build_pull_info(route, (x, y));

        let store = &self.store;
        let intent = StreamStartIntent {
            op_id: route.op_id,
            focused_entity: route.entity_id,
            residue_in_entity: route.residue_in_entity,
            atom_name: route.atom_name.clone(),
        };
        let (rid, plugin_id) = match self
            .runner_client
            .start_stream(&intent, |id| store.entity_type(id))
        {
            Ok(v) => v,
            Err(e) => {
                log::warn!(
                    "begin_pull_drag_from_route: start_stream {:?} failed: {e:?}",
                    route.op_id,
                );
                return false;
            }
        };

        // History side-effect - same shape as button-driven dispatch
        // so the drag's eventual commit_action lands as a regular
        // PluginOp entry. Failure is non-fatal (commit_action becomes
        // a no-op on an idle store).
        let action_entity = self
            .store
            .ids()
            .find(|id| id.raw() == route.entity_id.raw());
        if let Some(entity) = action_entity {
            let kind = CheckpointKind::PluginOp {
                plugin_id: plugin_id.clone(),
                op_id: String::from(route.op_id),
                display: String::from("Pull"),
            };
            // Open the edit under the dispatch's request_id; the stream
            // table is keyed by the same id, so the terminal commit lands
            // on this edit.
            if let Err(e) = self.store.begin_action(
                [entity],
                kind,
                String::from("Pull"),
                rid,
                std::collections::BTreeMap::new(),
            ) {
                log::trace!("begin_pull_drag_from_route: begin_action skipped: {e}");
            }
        }

        self.runner_client.set_pull_drag(PullDrag {
            request_id: rid,
            plugin_id,
            pull_info,
        });
        true
    }

    /// Pointer-move during an active drag: re-resolve the world-space
    /// drag target through the camera, and push a single-key
    /// `endpoint` Vec3 update to the running stream. Also refreshes
    /// `pull_info.screen_target` so the next visualization pass moves
    /// the cone tip with the cursor.
    fn update_pull_drag(&mut self, x: f32, y: f32) {
        let Some(drag) = self.runner_client.pull_drag_mut() else {
            return;
        };
        drag.pull_info.screen_target = (x, y);
        let (residue, atom_name, plugin_id, request_id) = (
            drag.pull_info.atom.residue,
            drag.pull_info.atom.atom_name.clone(),
            drag.plugin_id.clone(),
            drag.request_id,
        );

        let Some(atom_pos) = self.harness.resolve_atom_position(residue, &atom_name) else {
            return;
        };
        let Some(target) = self
            .harness
            .screen_to_world_at_depth(glam::Vec2::new(x, y), atom_pos)
        else {
            return;
        };

        let mut params = std::collections::HashMap::new();
        let _ = params.insert(
            String::from("endpoint"),
            foldit_runner::orchestrator::ParamValue::Vec3([target.x, target.y, target.z]),
        );
        self.runner_client
            .update_stream(request_id, &plugin_id, params);
    }

    /// Pointer-up (or any cancel signal): tear down the drag state
    /// and ask the orchestrator to cancel the stream. The stream's
    /// terminal `PluginUpdate::Cancelled` flows through
    /// `apply_backend_updates` → `commit_action`, so the partial pull
    /// becomes a permanent undo entry.
    fn end_pull_drag(&mut self) {
        let Some(drag) = self.runner_client.take_pull_drag() else {
            return;
        };
        self.runner_client
            .end_stream(drag.request_id, &drag.plugin_id);
    }

    /// Toggle the segment-info panel for `(eid, res)`: close it when it is
    /// already open for this exact residue, otherwise open / re-target it
    /// (re-tabbing a different residue closes the old target and opens the
    /// new one in a single press).
    fn toggle_segment(&mut self, eid: EntityId, res: usize) {
        if self.gui.segment_target().map(|t| (t.entity, t.residue)) == Some((eid, res)) {
            self.gui.close_segment();
        } else {
            self.open_segment(eid, res);
        }
    }

    /// Apply the viewport-input actions deferred past the `engine` borrow:
    /// a classified click (a left-release that picked a residue / empty
    /// background) updates the selection; a Tab over a residue toggles the
    /// segment-info panel; ESC cancels the in-flight op; and a resolved
    /// plugin hotkey op dispatches. Run after the trailing visualization
    /// update so `&mut self` is free.
    pub(in crate::app) fn apply_deferred_viewport_actions(
        &mut self,
        pending_click: Option<ClickEvent>,
        pending_escape: bool,
        pending_segment_toggle: Option<(EntityId, usize)>,
        #[cfg(not(target_arch = "wasm32"))] pending_hotkey_op: Option<String>,
        #[cfg(not(target_arch = "wasm32"))] pending_toggle_picker: Option<String>,
    ) {
        // Apply any pending click (a left-release that classified as a
        // click) to the selection; the empty-background case clears it, a
        // residue hit selects.
        if let Some(click) = pending_click {
            self.store.apply_click_to_selection(&click);
        }

        if let Some((eid, res)) = pending_segment_toggle {
            self.toggle_segment(eid, res);
        }

        if pending_escape {
            // ESC cancels everything cancellable (weight downloads excepted;
            // see `cancel_streams`). Routes through the one cancel owner so it
            // matches a toast's X button.
            self.cancel_action(None);
        }

        // A hotkey resolved in the `Key` arm dispatches through the same
        // sink a button click uses; built-ins already won by
        // `handle_key_press` being checked first.
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(op_id) = pending_hotkey_op {
            log::info!("hotkey -> dispatch plugin op {op_id:?}");
            self.pending_dispatches.push(foldit_gui::OpDispatch {
                op_id,
                focused_entity_id: None,
                params: std::collections::HashMap::new(),
            });
        }

        // A native picker-toggle key flips the host-owned open picker rather
        // than dispatching an op: open it if closed, close it if this op's
        // picker is already open. No `OpDispatch` is pushed.
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(op_id) = pending_toggle_picker {
            let next = if self.gui.action_picker_open() == Some(op_id.as_str()) {
                None
            } else {
                Some(op_id)
            };
            self.gui.set_action_picker_open(next);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::next_focus;
    use molex::entity::molecule::id::{EntityId, EntityIdAllocator};
    use viso::Focus;

    /// Mint a sequence of distinct entity ids in a test-local order.
    fn mint_ids(n: usize) -> Vec<EntityId> {
        let mut alloc = EntityIdAllocator::new();
        (0..n).map(|_| alloc.allocate()).collect()
    }

    #[test]
    fn next_focus_cycles_then_wraps_to_all() {
        // The Tab-cycle step, owned by foldit-core: `All` -> first
        // focusable -> ... -> last -> back to `All`.
        let ids = mint_ids(2);
        assert_eq!(
            next_focus(Focus::All, &ids),
            Focus::Entity(ids[0]),
            "All advances to the first focusable entity",
        );
        assert_eq!(
            next_focus(Focus::Entity(ids[0]), &ids),
            Focus::Entity(ids[1]),
            "a focused entity advances to the next in the list",
        );
        assert_eq!(
            next_focus(Focus::Entity(ids[1]), &ids),
            Focus::All,
            "the last focusable entity wraps back to All",
        );
        // An entity that has left the focusable list (e.g. hidden) also
        // wraps to All rather than getting stuck.
        assert_eq!(next_focus(Focus::Entity(ids[1]), &ids[..1]), Focus::All);
        // No focusable entities: All stays All.
        assert_eq!(next_focus(Focus::All, &[]), Focus::All);
    }
}
