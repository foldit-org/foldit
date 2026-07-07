//! Engine harness: owns the viso engine + its keybinding table and exposes
//! the lifecycle, feed, projection, and key-dispatch seam the `App` drives.

use viso::{ClickEvent, Focus, KeyBindings, VisoEngine};

use molex::entity::molecule::id::EntityId;

#[cfg(not(target_arch = "wasm32"))]
use crate::runner_client::{HotkeyOwner, RunnerClient};
use crate::session::Session;

use super::input::{hovered_segment_target, next_focus};

pub(in crate::app) struct EngineHarness {
    pub(in crate::app) engine: Option<VisoEngine>,
    pub(in crate::app) keybindings: KeyBindings,
}

impl EngineHarness {
    pub(in crate::app) fn new() -> Self {
        let mut keybindings = KeyBindings::default();
        // Focus is foldit-core session state: neutralize viso's Tab/Backquote
        // focus bindings so the key path drives `Session::set_focus` first.
        keybindings.insert("Tab".to_owned(), Box::new(|_: &mut VisoEngine| {}));
        keybindings.insert("Backquote".to_owned(), Box::new(|_: &mut VisoEngine| {}));
        Self {
            engine: None,
            keybindings,
        }
    }

    pub(in crate::app) fn attach(&mut self, engine: VisoEngine) {
        self.engine = Some(engine);
    }

    pub(in crate::app) fn shutdown(&mut self) {
        if let Some(engine) = &mut self.engine {
            engine.shutdown();
        }
    }

    pub(in crate::app) fn resize(&mut self, width: u32, height: u32) {
        if let Some(engine) = &mut self.engine {
            engine.resize(width, height);
        }
    }

    pub(in crate::app) fn set_surface_scale(&mut self, scale_factor: f64) {
        if let Some(engine) = &mut self.engine {
            engine.set_render_scale(if scale_factor < 2.0 { 2 } else { 1 });
        }
    }

    pub(in crate::app) fn update(&mut self, dt: f32) {
        if let Some(engine) = &mut self.engine {
            engine.update(dt);
        }
    }

    pub(in crate::app) fn render(&mut self) {
        if let Some(engine) = &mut self.engine {
            match engine.render() {
                Ok(()) => {}
                Err(viso::SurfaceError::Timeout) => {}
                Err(e) => log::error!("Render error: {e:?}"),
            }
        }
    }

    /// Project a world point to screen pixels, or `None` with no engine / when
    /// the point is off-screen.
    pub(in crate::app) fn world_to_screen(&self, world: glam::Vec3) -> Option<glam::Vec2> {
        self.engine.as_ref()?.world_to_screen(world)
    }

    /// Unproject `screen` onto the plane through `world_point`, or `None` with
    /// no engine.
    pub(in crate::app) fn screen_to_world_at_depth(
        &self,
        screen: glam::Vec2,
        world_point: glam::Vec3,
    ) -> Option<glam::Vec3> {
        Some(
            self.engine
                .as_ref()?
                .screen_to_world_at_depth(screen, world_point),
        )
    }

    /// Current world position of `atom_name` in `residue`, or `None` with no
    /// engine / when the atom can't be resolved.
    pub(in crate::app) fn resolve_atom_position(
        &self,
        residue: u32,
        atom_name: &str,
    ) -> Option<glam::Vec3> {
        self.engine
            .as_ref()?
            .resolve_atom_position(residue, atom_name)
    }

    pub(in crate::app) fn feed_pointer_down(
        &mut self,
        button: viso::MouseButton,
        x: f32,
        y: f32,
        shift: bool,
    ) {
        let Some(engine) = self.engine.as_mut() else {
            return;
        };
        engine.feed_modifiers(shift);
        engine.set_cursor_pos(x, y);
        engine.feed_pointer_motion(x, y);
        let _ = engine.feed_pointer_button(button, true);
    }

    /// Feed a pointer release; returns the click event when the release
    /// classified as a click.
    pub(in crate::app) fn feed_pointer_up(
        &mut self,
        button: viso::MouseButton,
        x: f32,
        y: f32,
        shift: bool,
    ) -> Option<ClickEvent> {
        let engine = self.engine.as_mut()?;
        engine.feed_modifiers(shift);
        engine.set_cursor_pos(x, y);
        engine.feed_pointer_motion(x, y);
        engine.feed_pointer_button(button, false)
    }

    pub(in crate::app) fn feed_pointer_move(&mut self, x: f32, y: f32, shift: bool) {
        let Some(engine) = self.engine.as_mut() else {
            return;
        };
        engine.feed_modifiers(shift);
        engine.set_cursor_pos(x, y);
        engine.feed_pointer_motion(x, y);
    }

    /// Feed a raw button press/release (native path, no cursor update); returns
    /// the click event when the release classified as a click.
    pub(in crate::app) fn feed_button(
        &mut self,
        button: viso::MouseButton,
        pressed: bool,
    ) -> Option<ClickEvent> {
        self.engine.as_mut()?.feed_pointer_button(button, pressed)
    }

    pub(in crate::app) fn feed_scroll(&mut self, delta: f32) {
        if let Some(engine) = self.engine.as_mut() {
            engine.feed_scroll(delta);
        }
    }

    pub(in crate::app) fn feed_modifiers(&mut self, shift: bool) {
        if let Some(engine) = self.engine.as_mut() {
            engine.feed_modifiers(shift);
        }
    }

    pub(in crate::app) fn set_cursor_pos(&mut self, x: f32, y: f32) {
        if let Some(engine) = self.engine.as_mut() {
            engine.set_cursor_pos(x, y);
        }
    }

    /// Update the drag/pull visualization. The pull capsule + cone arrow
    /// render whenever `pull` is `Some` from a live drag; cleared otherwise.
    pub(in crate::app) fn update_visualizations(&mut self, pull: Option<viso::PullInfo>) {
        if let Some(engine) = self.engine.as_mut() {
            engine.update_pull(pull);
        }
    }

    /// Decode a webview pointer-button index (DOM `MouseEvent.button`) into the
    /// viso button. `None` for any other index.
    pub(in crate::app) const fn decode_mouse_button(button: u8) -> Option<viso::MouseButton> {
        match button {
            0 => Some(viso::MouseButton::Left),
            2 => Some(viso::MouseButton::Right),
            1 => Some(viso::MouseButton::Middle),
            _ => None,
        }
    }

    /// The single foldit key-semantics table. Drives the engine (KeyR/KeyT,
    /// viso dispatch), mutates session focus in place, and returns the actions
    /// the caller must apply past the engine borrow.
    pub(in crate::app) fn handle_key(
        code: &str,
        engine: &mut VisoEngine,
        keybindings: &KeyBindings,
        store: &mut Session,
        #[cfg(not(target_arch = "wasm32"))] runner_client: &RunnerClient,
    ) -> KeyOutcome {
        let mut outcome = KeyOutcome {
            escape: false,
            segment_toggle: None,
            #[cfg(not(target_arch = "wasm32"))]
            hotkey_op: None,
            #[cfg(not(target_arch = "wasm32"))]
            toggle_picker: None,
        };
        match code {
            // Auto-rotate binding is intentionally dropped in foldit.
            "KeyR" => {}
            "KeyT" => {
                if engine.has_trajectory() {
                    engine.toggle_trajectory();
                } else if let Some(path) = trajectory_path_from_args() {
                    engine.load_trajectory(std::path::Path::new(&path));
                } else {
                    log::info!("No trajectory loaded. Pass --trajectory <path.dcd> to load one.");
                }
            }
            // ESC is cancel-only.
            "Escape" => outcome.escape = true,
            "Tab" => {
                // Tab over a residue drives the segment-info panel (deferred,
                // since the toggle needs `&mut self`); over empty space / an
                // atom it falls through to the focus cycle.
                if let Some(target) = hovered_segment_target(engine, store) {
                    outcome.segment_toggle = Some(target);
                } else {
                    let next = next_focus(store.focus(), &engine.focusable_entities());
                    store.set_focus(next);
                    log::info!(
                        "Focus: {}",
                        crate::render_projector::focus_description(store, store.focus())
                    );
                }
            }
            "Backquote" => {
                store.set_focus(Focus::All);
                log::info!(
                    "Focus: {}",
                    crate::render_projector::focus_description(store, store.focus())
                );
            }
            other => {
                if !keybindings.dispatch(other, engine) {
                    // No viso built-in claims this key - resolve it against the
                    // plugin hotkey catalog.
                    #[cfg(not(target_arch = "wasm32"))]
                    {
                        // Native picker-toggle flips a host-owned picker
                        // open/closed rather than dispatching an op.
                        match runner_client.resolve_hotkey(other) {
                            HotkeyOwner::Plugin { op_id, .. } => {
                                outcome.hotkey_op = Some(op_id);
                            }
                            HotkeyOwner::Native { op_id } => {
                                outcome.toggle_picker = Some(op_id);
                            }
                            HotkeyOwner::None => {
                                log::debug!("Unhandled key code from frontend: {other}");
                            }
                        }
                    }
                    #[cfg(target_arch = "wasm32")]
                    log::debug!("Unhandled key code from frontend: {other}");
                }
            }
        }
        outcome
    }
}

/// Results of a key press the caller applies.
pub(in crate::app) struct KeyOutcome {
    pub(in crate::app) escape: bool,
    /// A Tab press over a residue: the segment-info panel toggle.
    pub(in crate::app) segment_toggle: Option<(EntityId, usize)>,
    #[cfg(not(target_arch = "wasm32"))]
    pub(in crate::app) hotkey_op: Option<String>,
    /// A native key that toggles a host-owned action picker (op_id whose
    /// picker to flip open/closed). Distinct from `hotkey_op`, which
    /// dispatches an op.
    #[cfg(not(target_arch = "wasm32"))]
    pub(in crate::app) toggle_picker: Option<String>,
}

/// The trajectory path from command-line arguments, read on a `KeyT` press.
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
