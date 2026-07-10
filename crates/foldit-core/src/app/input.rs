use molex::entity::molecule::id::EntityId;
use viso::ClickEvent;

use crate::app::harness::EngineHarness;
use crate::app::App;

pub(in crate::app) use super::gesture::next_focus;
pub(in crate::app) use super::pick::hovered_segment_target;

impl App {
    /// Dispatch a keybinding by physical-key string ("`KeyR`", "`KeyT`",
    /// "Tab", ...). Hosts convert their native keycode to this string
    /// before calling (winit: `format!("{key:?}")`; web: DOM `code`).
    /// Returns true when the press produced a deferred action. Routes through
    /// the same key table + deferred sink the webview path uses.
    pub fn handle_keybinding(&mut self, key_str: &str) -> bool {
        let Some(engine) = self.harness.engine.as_mut() else {
            return false;
        };
        let out = EngineHarness::handle_key(
            key_str,
            engine,
            &self.harness.keybindings,
            &mut self.store,
            #[cfg(not(target_arch = "wasm32"))]
            &self.runner_client,
        );
        #[cfg(not(target_arch = "wasm32"))]
        let produced = out.escape
            || out.segment_toggle.is_some()
            || out.hotkey_op.is_some()
            || out.toggle_picker.is_some();
        #[cfg(target_arch = "wasm32")]
        let produced = out.escape || out.segment_toggle.is_some();
        self.apply_deferred_viewport_actions(
            None,
            out.escape,
            out.segment_toggle,
            #[cfg(not(target_arch = "wasm32"))]
            out.hotkey_op,
            #[cfg(not(target_arch = "wasm32"))]
            out.toggle_picker,
        );
        produced
    }

    /// Route a viewport input event from the webview into viso and the
    /// pull-drag system.
    ///
    /// # Panics
    ///
    /// Panics if internal pull-drag state is inconsistent (a resolved
    /// pull origin is expected to be present once a guard has confirmed
    /// it); this indicates a logic bug, not bad input.
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

        // Right-button drag grows a world-space selection sphere. It is
        // intercepted first (and on every target) so the right button never
        // reaches viso, where it would otherwise be an inert press.
        if self.try_sphere_select_interception(&input) {
            return;
        }

        #[cfg(not(target_arch = "wasm32"))]
        if self.try_pull_drag_interception(&input) {
            return;
        }

        if self.harness.engine.is_none() {
            return;
        }

        // Each harness feed/key call ends its borrow before the next; the
        // actions below are deferred past them because they need `&mut self`.
        let mut pending_click: Option<ClickEvent> = None;
        let mut pending_escape = false;
        let mut pending_segment_toggle: Option<(EntityId, usize)> = None;
        #[cfg(not(target_arch = "wasm32"))]
        let mut pending_hotkey_op: Option<String> = None;
        #[cfg(not(target_arch = "wasm32"))]
        let mut pending_toggle_picker: Option<String> = None;

        match input {
            ViewportInput::PointerDown {
                x,
                y,
                button,
                shift,
                ..
            } => {
                let Some(viso_button) = EngineHarness::decode_mouse_button(button) else {
                    return;
                };
                self.harness.feed_pointer_down(viso_button, x, y, shift);
                #[cfg(not(target_arch = "wasm32"))]
                self.latch_pull_origin(x, y, button);
            }
            ViewportInput::PointerUp {
                x,
                y,
                button,
                shift,
                ..
            } => {
                let Some(viso_button) = EngineHarness::decode_mouse_button(button) else {
                    return;
                };
                pending_click = self.harness.feed_pointer_up(viso_button, x, y, shift);
                // Gesture over: a pull that started already took the route
                // (it's `None`); a click / camera-rotate gesture that never
                // pulled drops its stored origin here.
                #[cfg(not(target_arch = "wasm32"))]
                self.runner_client.set_pending_pull_origin(None);
            }
            ViewportInput::PointerMove { x, y, shift, .. } => {
                self.harness.feed_pointer_move(x, y, shift);
            }
            ViewportInput::Scroll { delta } => {
                self.harness.feed_scroll(delta);
            }
            ViewportInput::Key { code, pressed } => {
                if pressed {
                    if let Some(engine) = self.harness.engine.as_mut() {
                        let out = EngineHarness::handle_key(
                            &code,
                            engine,
                            &self.harness.keybindings,
                            &mut self.store,
                            #[cfg(not(target_arch = "wasm32"))]
                            &self.runner_client,
                        );
                        pending_escape = out.escape;
                        pending_segment_toggle = out.segment_toggle;
                        #[cfg(not(target_arch = "wasm32"))]
                        {
                            pending_hotkey_op = out.hotkey_op;
                            pending_toggle_picker = out.toggle_picker;
                        }
                    }
                }
            }
            ViewportInput::Resize { .. } => {
                // Ignored: JS sends CSS pixels (logical) which are wrong on HiDPI.
            }
        }

        #[cfg(not(target_arch = "wasm32"))]
        let pull = self.runner_client.pull_drag_pull_info();
        #[cfg(target_arch = "wasm32")]
        let pull: Option<viso::PullInfo> = None;
        self.harness.update_visualizations(pull);

        self.apply_deferred_viewport_actions(
            pending_click,
            pending_escape,
            pending_segment_toggle,
            #[cfg(not(target_arch = "wasm32"))]
            pending_hotkey_op,
            #[cfg(not(target_arch = "wasm32"))]
            pending_toggle_picker,
        );
    }

    /// Lock the pull intent at the down-target. The cursor was just fed to
    /// (x, y), so resolving the route here captures what is under the press; a
    /// later move can only supply the drag endpoint. Only the left button
    /// anchors a pull: the right button is claimed by sphere-select, and the
    /// middle button is inert in viso.
    #[cfg(not(target_arch = "wasm32"))]
    fn latch_pull_origin(&mut self, x: f32, y: f32, button: u8) {
        let origin = if button == 0 {
            self.harness
                .engine
                .as_ref()
                .and_then(|engine| Self::resolve_pull_route(engine, &self.store, x, y))
        } else {
            None
        };
        self.runner_client.set_pending_pull_origin(origin);
    }

    pub fn handle_native_mouse_input(&mut self, button: viso::MouseButton, pressed: bool) {
        let pending_click = self.harness.feed_button(button, pressed);
        self.harness.update_visualizations(None);
        if let Some(click) = pending_click {
            self.store.apply_click_to_selection(&click);
        }
    }

    pub fn handle_native_cursor_moved(&mut self, x: f32, y: f32) {
        self.harness.set_cursor_pos(x, y);
        self.harness.update_visualizations(None);
    }

    /// Forward a scroll delta in viso "logical scroll units" (winit
    /// `LineDelta(_, y)` passes `y` directly; `PixelDelta(_, y)` should
    /// pass `y * 0.01`). Conversion lives in the host.
    pub fn handle_native_mouse_wheel(&mut self, scroll_delta: f32) {
        self.harness.feed_scroll(scroll_delta);
    }

    pub fn handle_native_modifiers(&mut self, shift: bool) {
        self.harness.feed_modifiers(shift);
    }

    pub fn update_frame_visuals(&mut self) {
        #[cfg(not(target_arch = "wasm32"))]
        let pull = self.runner_client.pull_drag_pull_info();
        #[cfg(target_arch = "wasm32")]
        let pull: Option<viso::PullInfo> = None;
        self.harness.update_visualizations(pull);
    }
}
