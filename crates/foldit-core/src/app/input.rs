use molex::entity::molecule::id::EntityId;
use viso::{classify_click_for_selection, ClickEvent, ClickSelectionAction, Focus, VisoEngine};

use crate::app::App;
use crate::render_projector;
#[cfg(not(target_arch = "wasm32"))]
use crate::history::CheckpointKind;
#[cfg(not(target_arch = "wasm32"))]
use crate::runner_client::StreamStartIntent;
#[cfg(not(target_arch = "wasm32"))]
use crate::session::Session;

/// Advance focus one step through `focusable`, wrapping back to
/// [`Focus::All`] after the last entity. `All` advances to the first
/// focusable entity (or stays `All` when none are focusable). The
/// Tab-cycle step, owned by foldit-core now that focus is session state;
/// `focusable` is viso's eligibility list (`engine.focusable_entities()`).
fn next_focus(current: Focus, focusable: &[EntityId]) -> Focus {
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

impl App {
    // ── Keybinding dispatch ──

    /// Catalog hotkey fallback. Runs only after a viso built-in
    /// `handle_key_press` *miss*, so built-ins always win. On a match
    /// against a plugin manifest `[[buttons]]` hotkey, dispatch the op
    /// through the same `handle_dispatch_op` sink a button click uses;
    /// that sink sources the live focus + selection itself, so the
    /// hotkey op runs on the same target a button click would. Returns
    /// true if an op was dispatched.
    #[cfg(not(target_arch = "wasm32"))]
    fn try_hotkey_dispatch(&mut self, key_str: &str) -> bool {
        let op_id = self
            .runner_client
            .hotkey_to_op(key_str)
            .map(|(_plugin_id, op_id)| op_id);
        let Some(op_id) = op_id else { return false };
        log::info!("hotkey {key_str:?} -> dispatch plugin op {op_id:?}");
        self.handle_dispatch_op(foldit_gui::OpDispatch {
            op_id,
            focused_entity_id: None,
            params: std::collections::HashMap::new(),
        });
        true
    }

    #[cfg(target_arch = "wasm32")]
    fn try_hotkey_dispatch(&mut self, _key_str: &str) -> bool {
        false
    }

    /// Dispatch a keybinding by physical-key string ("`KeyR`", "`KeyT`",
    /// "Tab", ...). Hosts convert their native keycode to this string
    /// before calling (winit: `format!("{key:?}")`; web: DOM `code`).
    /// On a viso built-in miss, falls through to the plugin hotkey
    /// catalog (built-ins win by being checked first).
    pub fn handle_keybinding(&mut self, key_str: &str) -> bool {
        // foldit-specific overrides: trajectory load-on-demand, ESC =
        // cancel-in-flight-op, and the dropped auto-rotate binding.
        // These short-circuit the generic viso keybinding dispatch.
        match key_str {
            "KeyT" => {
                let Some(engine) = &mut self.engine else {
                    return false;
                };
                if engine.has_trajectory() {
                    engine.toggle_trajectory();
                } else if let Some(path) = trajectory_path_from_args() {
                    engine.load_trajectory(std::path::Path::new(&path));
                } else {
                    log::info!("No trajectory loaded. Pass --trajectory <path.dcd> to load one.");
                }
                return true;
            }
            "Escape" => {
                // ESC is cancel-only: drop in-flight streams + the
                // current operation's previews. It never clears the
                // selection (which is ambient); the only selection
                // clearer is the empty-background click.
                #[cfg(not(target_arch = "wasm32"))]
                self.runner_client.cancel_all_active_streams();
                self.cancel_operations();
                return true;
            }
            // Auto-rotate keybinding is intentionally dropped in foldit.
            "KeyR" => return true,
            _ => {}
        }

        let Some(engine) = &mut self.engine else {
            return false;
        };

        // Focus is foldit-core session state: intercept the focus gestures
        // before viso's keybinding table and mutate the session. The tick's
        // `FocusChanged` reaction pushes viso's camera mirror, reframes, and
        // raises the projector dirty (the catalog re-projects because per-op
        // availability is focus-dependent).
        if matches!(key_str, "Tab" | "Backquote") {
            let next = match key_str {
                "Tab" => next_focus(self.store.focus(), &engine.focusable_entities()),
                _ => Focus::All,
            };
            self.store.set_focus(next);
            log::info!(
                "Focus: {}",
                render_projector::focus_description(&self.store, &self.store.focus())
            );
            return true;
        }

        if !self.keybindings.dispatch(key_str, engine) {
            return self.try_hotkey_dispatch(key_str);
        }
        true
    }

    /// Cancel the in-flight operation: drop any in-progress preview
    /// entities, republish, and flag the GUI dirty. Selection is a
    /// separate concept (see `clear_selection`); cancelling an operation
    /// does not touch it. Stream lock release + commit live in
    /// `apply_backend_updates`' terminal arms; doing them here races a
    /// follow-up dispatch that's quick enough to slip in before the
    /// terminal drains. Lives on `App` so the `RenderProjector` stays a
    /// field touched only inside App methods (the coordination
    /// boundary), never threaded as a parameter.
    fn cancel_operations(&mut self) {
        if self.engine.is_none() {
            return;
        }
        log::info!("Cancelling current operation");
        let preview_ids: Vec<EntityId> = self.store.preview_ids().collect();
        if !preview_ids.is_empty() {
            for id in &preview_ids {
                self.store.remove_preview(*id);
            }
            // PreviewDiscarded rides the `SessionUpdate` stream - the next tick's render
            // projector republishes and the GUI consumer derives SCENE +
            // ACTIONS dirty from the same batch.
            log::info!("Removed {} in-progress preview entities", preview_ids.len());
        }
    }

    // ── Viewport input (from webview) ──

    pub fn handle_viewport_input(&mut self, input: foldit_gui::ViewportInput) {
        use foldit_gui::ViewportInput;

        // Pull-drag interception runs ahead of viso's regular input
        // routing so an active drag suppresses camera rotation/pan.
        // The pull target is locked at button-down, not at the move:
        // `PointerDown` resolves the pull route at the down-cursor and
        // stores it in `pending_pull_origin` (`None` when the down-target
        // is empty / non-pullable). A left-press+release with no move
        // falls through to viso as a residue selection; a press that
        // resolved to a route opens the pull on the first move with the
        // button still held, anchored to the down-target regardless of
        // where the cursor has since wandered. `mouse_pressed()` is viso's
        // own press bit, set by the preceding PointerDown.
        #[cfg(not(target_arch = "wasm32"))]
        {
            match &input {
                ViewportInput::PointerMove { x, y, .. }
                    if self
                        .engine
                        .as_ref()
                        .is_some_and(viso::VisoEngine::mouse_pressed)
                        && !self.runner_client.has_active_pull_drag()
                        && self.pending_pull_origin.is_some() =>
                {
                    // The pull intent was locked at button-down; the move
                    // only supplies the drag endpoint. Take the route so
                    // this gesture makes at most one start attempt - a
                    // failed stream start falls through to camera for the
                    // rest of the drag rather than retrying mid-gesture.
                    let route = self
                        .pending_pull_origin
                        .take()
                        .expect("guard guarantees Some");
                    if self.begin_pull_drag_from_route(route, *x, *y) {
                        // viso recorded the press; drop its mouse
                        // state so the now-suppressed pointer-up
                        // can't fire a stray click → selection.
                        if let Some(engine) = self.engine.as_mut() {
                            engine.release_mouse_state();
                        }
                        self.update_pull_drag(*x, *y);
                        self.finalize_viewport_input();
                        return;
                    }
                }
                ViewportInput::PointerMove { x, y, .. }
                    if self.runner_client.has_active_pull_drag() =>
                {
                    self.update_pull_drag(*x, *y);
                    self.finalize_viewport_input();
                    return;
                }
                ViewportInput::PointerUp { .. }
                    if self.runner_client.has_active_pull_drag() =>
                {
                    self.end_pull_drag();
                    self.finalize_viewport_input();
                    return;
                }
                _ => {}
            }
        }

        // Hotkey resolved in the `Key` arm below via a disjoint field
        // borrow (`self.runner_client`, not `self.engine`); the actual
        // dispatch is deferred to after the match so the `engine`
        // borrow is released before `handle_dispatch_op` takes
        // `&mut self`.
        #[cfg(not(target_arch = "wasm32"))]
        let mut pending_hotkey_op: Option<String> = None;
        // ESC routing needs `&mut self`, but `engine` is borrowed for
        // the rest of the match and used again by
        // `update_all_visualizations` after it. Defer past that last
        // engine use, mirroring the `pending_hotkey_op` deferral. ESC is
        // cancel-only - it never touches the selection - so the deferred
        // block is unconditional and needs no live state read.
        let mut pending_escape = false;

        let Some(engine) = &mut self.engine else {
            return;
        };

        // `Some` only if a left-button release classified as a click;
        // deferred so the selection mutations below run after the
        // `engine` borrow ends.
        let mut pending_click: Option<ClickEvent> = None;

        match input {
            ViewportInput::PointerDown {
                x,
                y,
                button,
                shift,
                ..
            } => {
                let viso_button = match button {
                    0 => viso::MouseButton::Left,
                    2 => viso::MouseButton::Right,
                    1 => viso::MouseButton::Middle,
                    _ => return,
                };
                engine.feed_modifiers(shift);
                engine.set_cursor_pos(x, y);
                engine.feed_pointer_motion(x, y);
                let _ = engine.feed_pointer_button(viso_button, true);
                // Lock the pull intent at the down-target. The engine
                // cursor was just fed to (x, y), so resolving the route
                // here captures what is under the press; a later move
                // can only supply the drag endpoint, never re-pick the
                // target. Left button only - right/middle are camera.
                #[cfg(not(target_arch = "wasm32"))]
                {
                    self.pending_pull_origin = if button == 0 {
                        Self::resolve_pull_route(engine, &self.store, x, y)
                    } else {
                        None
                    };
                }
            }
            ViewportInput::PointerUp {
                x,
                y,
                button,
                shift,
                ..
            } => {
                let viso_button = match button {
                    0 => viso::MouseButton::Left,
                    2 => viso::MouseButton::Right,
                    1 => viso::MouseButton::Middle,
                    _ => return,
                };
                engine.feed_modifiers(shift);
                engine.set_cursor_pos(x, y);
                engine.feed_pointer_motion(x, y);
                pending_click = engine.feed_pointer_button(viso_button, false);
                // Gesture over: a pull that started already took the
                // route (it's `None`); a click / camera-rotate gesture
                // that never pulled drops its stored origin here.
                #[cfg(not(target_arch = "wasm32"))]
                {
                    self.pending_pull_origin = None;
                }
            }
            ViewportInput::PointerMove { x, y, shift, .. } => {
                engine.feed_modifiers(shift);
                engine.set_cursor_pos(x, y);
                engine.feed_pointer_motion(x, y);
            }
            ViewportInput::Scroll { delta } => {
                engine.feed_scroll(delta);
            }
            ViewportInput::Key { code, pressed } => {
                if pressed {
                    // foldit-specific overrides land first; viso's
                    // generic table picks up the rest.
                    match code.as_str() {
                        // Drop viso's R-binding for turntable auto-rotate;
                        // we don't expose a rotate keybinding in foldit.
                        "KeyR" => {}
                        "KeyT" => {
                            if engine.has_trajectory() {
                                engine.toggle_trajectory();
                            } else if let Some(path) = trajectory_path_from_args() {
                                engine.load_trajectory(std::path::Path::new(&path));
                            }
                        }
                        "Escape" => {
                            // ESC cancels the in-flight action only; it
                            // never clears the selection. Resolved in the
                            // deferred block below, past the last `engine`
                            // use. Mirrors the `handle_keybinding` ESC arm.
                            pending_escape = true;
                        }
                        // Focus is foldit-core session state: mutate the
                        // session here (disjoint `self.store` borrow). The
                        // tick's `FocusChanged` reaction pushes viso's camera
                        // mirror, reframes, and raises the projector dirty.
                        "Tab" => {
                            let next =
                                next_focus(self.store.focus(), &engine.focusable_entities());
                            self.store.set_focus(next);
                        }
                        "Backquote" => {
                            self.store.set_focus(Focus::All);
                        }
                        other => {
                            if !self.keybindings.dispatch(other, engine) {
                                // No viso built-in claims this key - resolve it
                                // against the plugin hotkey catalog. Disjoint
                                // field borrow (`self.runner_client`) so it
                                // coexists with the live `engine` borrow;
                                // dispatch is deferred to after the match.
                                #[cfg(not(target_arch = "wasm32"))]
                                {
                                    pending_hotkey_op = self
                                        .runner_client
                                        .hotkey_to_op(other)
                                        .map(|(_plugin_id, op_id)| op_id);
                                    if pending_hotkey_op.is_none() {
                                        log::debug!("Unhandled key code from frontend: {other}");
                                    }
                                }
                                #[cfg(target_arch = "wasm32")]
                                log::debug!("Unhandled key code from frontend: {other}");
                            }
                        }
                    }
                }
            }
            ViewportInput::Resize { .. } => {
                // Ignored: JS sends CSS pixels (logical) which are wrong on HiDPI.
            }
        }

        // Update drag/pull/band visualizations after input
        #[cfg(not(target_arch = "wasm32"))]
        let pull = self.runner_client.pull_drag_pull_info();
        #[cfg(target_arch = "wasm32")]
        let pull: Option<viso::PullInfo> = None;
        update_all_visualizations(engine, pull);

        // `engine`'s last use was above - `&mut self` is free again, so
        // the deferred actions below can run. Apply any pending click (a
        // left-release that classified as a click) to the selection; the
        // empty-background case clears it, a residue hit selects.
        if let Some(click) = pending_click {
            self.apply_click_to_selection(&click);
        }

        if pending_escape {
            // ESC is cancel-only: drop in-flight streams + the current
            // operation's previews. The selection is ambient and left
            // untouched (the only selection clearer is the empty-click
            // path in `apply_click_to_selection`).
            #[cfg(not(target_arch = "wasm32"))]
            self.runner_client.cancel_all_active_streams();
            self.cancel_operations();
        }

        // A hotkey resolved in the `Key` arm dispatches through the same
        // sink a button click uses; built-ins already won by
        // `handle_key_press` being checked first.
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(op_id) = pending_hotkey_op {
            log::info!("hotkey -> dispatch plugin op {op_id:?}");
            self.handle_dispatch_op(foldit_gui::OpDispatch {
                op_id,
                focused_entity_id: None,
                params: std::collections::HashMap::new(),
            });
        }
    }

    /// Called after the pull-drag interception path. Mirrors the
    /// trailing visualization update the regular `handle_viewport_input`
    /// flow does (the `SessionUpdate` drain itself is tick-driven now).
    /// Pre-snapshots the pull info so the engine borrow doesn't overlap
    /// with the live pull-drag state held in the plugin driver.
    #[cfg(not(target_arch = "wasm32"))]
    fn finalize_viewport_input(&mut self) {
        let pull = self.runner_client.pull_drag_pull_info();
        if let Some(engine) = self.engine.as_mut() {
            update_all_visualizations(engine, pull);
        }
    }

    /// Classify the pick under `(x, y)` into a pull route, or `None` if
    /// the target is empty / non-pullable (non-protein entity, hydrogen
    /// atom, no atom under the cursor). Pure resolution: reads the engine
    /// pick + store but mutates nothing, so it can run at `PointerDown`
    /// against the just-fed down-cursor to lock the pull's anchor. Takes
    /// `engine` + `store` as borrows rather than `&self` so the caller can
    /// invoke it while holding a disjoint `&mut self.engine` borrow.
    #[cfg(not(target_arch = "wasm32"))]
    fn resolve_pull_route(
        engine: &viso::VisoEngine,
        store: &Session,
        x: f32,
        y: f32,
    ) -> Option<crate::pull_drag::PullRoute> {
        match engine.hovered_target() {
            viso::PickTarget::Atom {
                entity_id,
                atom_idx,
            } => crate::pull_drag::route_atom_pick(store, entity_id, atom_idx),
            viso::PickTarget::Residue(flat) => {
                engine.picked_residue_atom(flat, (x, y)).and_then(|picked| {
                    let molex_id = store.ids().find(|id| id.raw() == picked.entity_id)?;
                    crate::pull_drag::route_residue_pick(
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
    #[cfg(not(target_arch = "wasm32"))]
    fn begin_pull_drag_from_route(
        &mut self,
        route: crate::pull_drag::PullRoute,
        x: f32,
        y: f32,
    ) -> bool {
        let pull_info = crate::pull_drag::build_pull_info(&route, (x, y));

        let store = &self.store;
        let intent = StreamStartIntent {
            op_id: route.op_id,
            focused_entity: route.entity_id,
            residue_in_entity: route.residue_in_entity,
            atom_name: route.atom_name.clone(),
        };
        let (rid, plugin_id) =
            match self.runner_client.start_stream(intent, |id| store.entity_type(id)) {
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
            if let Err(e) =
                self.store.begin_action([entity], kind, String::from("Pull"), rid)
            {
                log::trace!("begin_pull_drag_from_route: begin_action skipped: {e}");
            }
        }

        self.runner_client.set_pull_drag(crate::pull_drag::PullDrag {
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
    #[cfg(not(target_arch = "wasm32"))]
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

        let Some(engine) = self.engine.as_ref() else {
            return;
        };
        let Some(atom_pos) = engine.resolve_atom_position(residue, &atom_name) else {
            return;
        };
        let target = engine.screen_to_world_at_depth(glam::Vec2::new(x, y), atom_pos);

        self.runner_client.update_stream(request_id, &plugin_id, target);
    }

    /// Pointer-up (or any cancel signal): tear down the drag state
    /// and ask the orchestrator to cancel the stream. The stream's
    /// terminal `PluginUpdate::Cancelled` flows through
    /// `apply_backend_updates` → `commit_action`, so the partial pull
    /// becomes a permanent undo entry.
    #[cfg(not(target_arch = "wasm32"))]
    fn end_pull_drag(&mut self) {
        let Some(drag) = self.runner_client.take_pull_drag() else {
            return;
        };
        self.runner_client.end_stream(drag.request_id, &drag.plugin_id);
    }
    // ── Native input (when webview is not ready) ──

    pub fn handle_native_mouse_input(&mut self, button: viso::MouseButton, pressed: bool) {
        let pending_click = if let Some(engine) = &mut self.engine {
            let click = engine.feed_pointer_button(button, pressed);
            update_all_visualizations(engine, None);
            click
        } else {
            None
        };
        if let Some(click) = pending_click {
            self.apply_click_to_selection(&click);
        }
    }

    pub fn handle_native_cursor_moved(&mut self, x: f32, y: f32) {
        if let Some(engine) = &mut self.engine {
            engine.set_cursor_pos(x, y);
            update_all_visualizations(engine, None);
        }
    }

    /// Forward a scroll delta in viso "logical scroll units" (winit
    /// `LineDelta(_, y)` passes `y` directly; `PixelDelta(_, y)` should
    /// pass `y * 0.01`). Conversion lives in the host.
    pub fn handle_native_mouse_wheel(&mut self, scroll_delta: f32) {
        if let Some(engine) = &mut self.engine {
            engine.feed_scroll(scroll_delta);
        }
    }

    pub fn handle_native_modifiers(&mut self, shift: bool) {
        if let Some(engine) = &mut self.engine {
            engine.feed_modifiers(shift);
        }
    }

    // ── Per-frame visual updates ──

    pub fn update_frame_visuals(&mut self) {
        // Pre-snapshot pull info under an immutable borrow so the
        // subsequent `&mut engine` doesn't conflict.
        #[cfg(not(target_arch = "wasm32"))]
        let pull = self.runner_client.pull_drag_pull_info();
        #[cfg(target_arch = "wasm32")]
        let pull: Option<viso::PullInfo> = None;
        let Some(engine) = &mut self.engine else {
            return;
        };
        update_all_visualizations(engine, pull);
    }
    /// Apply a viso click-event to the selection store. Empty-area
    /// clicks clear the selection; non-empty expansions either replace
    /// (no modifier) or toggle (shift held) on a per-residue basis.
    /// Targets with an empty expansion (atom picks, non-protein hits)
    /// are no-ops on shift-held click and a clear on plain click; we
    /// follow the same "replace selection with the click's expansion"
    /// rule, which collapses to "clear" when the expansion is empty.
    fn apply_click_to_selection(&mut self, click: &ClickEvent) {
        match classify_click_for_selection(click) {
            ClickSelectionAction::Clear => {
                self.store.clear_selection();
            }
            ClickSelectionAction::Replace(residues) => {
                self.store.clear_selection();
                for (entity, residue) in residues {
                    self.store.select_residue(entity, residue);
                }
            }
            ClickSelectionAction::Toggle(residues) => {
                for (entity, residue) in residues {
                    let _ = self.store.toggle_residue(entity, residue);
                }
            }
        }
    }

    /// Apply a panel-originated selection mutation: wholesale replace
    /// the current selection with `entries`. The wire-side `entity_id`
    /// is a raw `u32`; look it up against the store's existing ids
    /// instead of minting a new one through the allocator (which would
    /// silently advance and break the next genuine allocation).
    /// Entries that don't match any live entity are dropped - panels
    /// can race a structure swap, and a stale id should clear silently
    /// rather than fail loudly. An empty `entries` list clears the
    /// selection entirely. Per-entity residue lists are collected into
    /// `BTreeSet`, so duplicate or out-of-order indices in the wire
    /// payload are silently normalized.
    pub fn handle_set_selection(&mut self, entries: Vec<foldit_gui::EntitySelection>) {
        self.store.clear_selection();
        for entry in entries {
            let Some(entity) = self.store.ids().find(|id| id.raw() == entry.entity_id) else {
                log::trace!(
                    "handle_set_selection: unknown entity_id {} (dropping)",
                    entry.entity_id
                );
                continue;
            };
            self.store.set_residues_on(entity, entry.residues);
        }
    }
}

// ---------------------------------------------------------------------------
// Visualization helpers (free functions for split-borrow friendliness)
// ---------------------------------------------------------------------------

/// Update drag/pull/band visualizations. Bands are still inert (the
/// band state machine is the next item to come back online). The pull
/// capsule + cone arrow renders whenever the caller hands a
/// `Some(PullInfo)` from a live drag; clears otherwise so a finished
/// or cancelled drag leaves no overlay.
pub(in crate::app) fn update_all_visualizations(engine: &mut VisoEngine, pull: Option<viso::PullInfo>) {
    engine.update_bands(vec![]);
    engine.update_pull(pull);
}

/// Get the trajectory path from command-line arguments. CLI/host
/// utility - read once on a hotkey + reused by `LoadTrajectory`.
fn trajectory_path_from_args() -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.windows(2).find_map(|w| {
        if w[0] == "--trajectory" {
            Some(w[1].clone())
        } else {
            None
        }
    })
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
