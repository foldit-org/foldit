//! winit events → synthesized `GdkEvent`s → `gtk_main_do_event`.
//!
//! The offscreen window is never mapped, so X delivers it no input at all.
//! Handing GTK's own dispatcher hand-built events puts them through `WebKit`'s
//! normal input pipeline, which is what makes the page see
//! `event.isTrusted === true`, run native click side effects, and honor IME
//! and shortcut gating — none of which a `dispatchEvent` shim can offer.
//!
//! ## Refcount discipline
//!
//! `gdk_event_free` unrefs the window and device it finds in the event, so
//! every pointer written into a raw event struct is ref-bumped first. The
//! device goes in through `gdk_event_set_device`, which refs on its own and
//! knows where each event type stores it.

use gtk::gdk::ffi as gdk_ffi;
use gtk::gdk::{Display, EventType, ModifierType};
use gtk::glib::translate::ToGlibPtr;
use gtk::glib::{self, gobject_ffi};
use gtk::prelude::*;
use gtk::{gdk, gdk::keys::constants as key};
use webkit2gtk::WebView;
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::platform::scancode::PhysicalKeyExtScancode;

/// GDK smooth-scroll deltas are in wheel steps, which `WebKitGTK` expands to
/// `Scrollbar::pixelsPerLineStep()` pixels each. Pixel-precise winit deltas are
/// divided by the same constant so a 40 px trackpad flick scrolls 40 px.
const PIXELS_PER_LINE_STEP: f64 = 40.0;

/// Modifier and pointer state, tracked because every `GdkEvent` carries the
/// full mask while winit reports each part only when it changes.
pub(super) struct InputState {
    keyboard: ModifierType,
    buttons: ModifierType,
    cursor: (f64, f64),
}

impl InputState {
    pub(super) const fn new() -> Self {
        Self {
            keyboard: ModifierType::empty(),
            buttons: ModifierType::empty(),
            cursor: (0.0, 0.0),
        }
    }
}

/// Translate one winit event and hand it to GTK. Events with no GDK
/// counterpart are ignored.
pub(super) fn dispatch(webview: &WebView, state: &mut InputState, event: &WindowEvent) {
    match event {
        WindowEvent::ModifiersChanged(modifiers) => {
            state.keyboard = keyboard_mask(modifiers.state());
        }

        WindowEvent::CursorMoved { position, .. } => {
            state.cursor = (position.x, position.y);
            send(webview, state, EventType::MotionNotify, |event, ctx| {
                let motion = event.downcast_mut::<gdk::EventMotion>()?;
                let raw = motion.as_mut();
                raw.time = ctx.time;
                raw.state = ctx.mask;
                (raw.x, raw.y) = ctx.cursor;
                (raw.x_root, raw.y_root) = ctx.cursor;
                raw.is_hint = 0;
                ctx.attach_window(&mut raw.window);
                Some(())
            });
        }

        WindowEvent::CursorEntered { .. } => crossing(webview, state, EventType::EnterNotify),
        WindowEvent::CursorLeft { .. } => crossing(webview, state, EventType::LeaveNotify),

        WindowEvent::MouseInput {
            state: element_state,
            button,
            ..
        } => {
            let Some((index, mask)) = mouse_button(*button) else {
                return;
            };
            let pressed = *element_state == ElementState::Pressed;
            let kind = if pressed {
                EventType::ButtonPress
            } else {
                EventType::ButtonRelease
            };
            // X11 puts the button's own bit in the mask of its release but not
            // of its press, which is what updating around the build produces.
            send(webview, state, kind, |event, ctx| {
                let button_event = event.downcast_mut::<gdk::EventButton>()?;
                let raw = button_event.as_mut();
                raw.time = ctx.time;
                raw.state = ctx.mask;
                (raw.x, raw.y) = ctx.cursor;
                (raw.x_root, raw.y_root) = ctx.cursor;
                raw.button = index;
                ctx.attach_window(&mut raw.window);
                Some(())
            });
            state.buttons.set(mask, pressed);
        }

        WindowEvent::MouseWheel { delta, .. } => {
            // A positive GDK delta scrolls the content up, the opposite of
            // winit's convention.
            let (delta_x, delta_y) = match *delta {
                MouseScrollDelta::LineDelta(x, y) => (-f64::from(x), -f64::from(y)),
                MouseScrollDelta::PixelDelta(pos) => (
                    -pos.x / PIXELS_PER_LINE_STEP,
                    -pos.y / PIXELS_PER_LINE_STEP,
                ),
            };
            send(webview, state, EventType::Scroll, |event, ctx| {
                let scroll = event.downcast_mut::<gdk::EventScroll>()?;
                let raw = scroll.as_mut();
                raw.time = ctx.time;
                raw.state = ctx.mask;
                (raw.x, raw.y) = ctx.cursor;
                (raw.x_root, raw.y_root) = ctx.cursor;
                raw.direction = gdk_ffi::GDK_SCROLL_SMOOTH;
                raw.delta_x = delta_x;
                raw.delta_y = delta_y;
                raw.is_stop = 0;
                ctx.attach_window(&mut raw.window);
                Some(())
            });
        }

        WindowEvent::KeyboardInput {
            event: key_event,
            is_synthetic: false,
            ..
        } => keyboard(webview, state, key_event),

        _ => (),
    }
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "X11 keycodes are u16 by definition"
)]
fn keyboard(webview: &WebView, state: &InputState, key_event: &KeyEvent) {
    let keyval = keyval(&key_event.logical_key);
    // WebKit derives `KeyboardEvent.code` from the hardware keycode, which the
    // frontend's hotkeys (`KeyM`, …) match on. X11 keycodes are evdev scancodes
    // offset by 8.
    let keycode = key_event
        .physical_key
        .to_scancode()
        .map_or(0, |scancode| (scancode + 8) as u16);
    let is_modifier = u32::from(is_modifier_key(&key_event.logical_key));
    let kind = if key_event.state == ElementState::Pressed {
        EventType::KeyPress
    } else {
        EventType::KeyRelease
    };
    send(webview, state, kind, |event, ctx| {
        let key_event = event.downcast_mut::<gdk::EventKey>()?;
        let raw = key_event.as_mut();
        raw.time = ctx.time;
        raw.state = ctx.mask;
        raw.keyval = keyval;
        raw.length = 0;
        raw.string = std::ptr::null_mut();
        raw.hardware_keycode = keycode;
        raw.group = 0;
        raw.is_modifier = is_modifier;
        ctx.attach_window(&mut raw.window);
        Some(())
    });
}

fn crossing(webview: &WebView, state: &InputState, kind: EventType) {
    send(webview, state, kind, |event, ctx| {
        let crossing = event.downcast_mut::<gdk::EventCrossing>()?;
        let raw = crossing.as_mut();
        raw.time = ctx.time;
        raw.state = ctx.mask;
        (raw.x, raw.y) = ctx.cursor;
        (raw.x_root, raw.y_root) = ctx.cursor;
        raw.mode = gdk_ffi::GDK_CROSSING_NORMAL;
        raw.detail = gdk_ffi::GDK_NOTIFY_NONLINEAR;
        raw.focus = 0;
        ctx.attach_window(&mut raw.window);
        Some(())
    });
}

/// The fields every event type carries, resolved once per dispatch.
struct Context {
    window: gdk::Window,
    mask: u32,
    cursor: (f64, f64),
    time: u32,
}

impl Context {
    /// Store a ref-bumped `GdkWindow` into a raw event field, balancing the
    /// unref `gdk_event_free` will perform.
    fn attach_window(&self, slot: &mut *mut gdk_ffi::GdkWindow) {
        let window: *mut gdk_ffi::GdkWindow = self.window.to_glib_none().0;
        unsafe {
            gobject_ffi::g_object_ref(window.cast());
            *slot = window;
        }
    }
}

/// Build an event of `kind` through `build`, then route it. Drops the event
/// when the webview is unrealized (no `GdkWindow`) or the downcast fails,
/// neither of which happens once `show_all` has run.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "GDK event timestamps are u32 milliseconds and wrap by design"
)]
fn send(
    webview: &WebView,
    state: &InputState,
    kind: EventType,
    build: impl FnOnce(&mut gdk::Event, &Context) -> Option<()>,
) {
    let Some(window) = webview.window() else {
        return;
    };
    let ctx = Context {
        window,
        mask: (state.keyboard | state.buttons).bits(),
        cursor: state.cursor,
        // WebKit derives click counts and key repeat from event timestamps, so
        // a real monotonic clock is needed; `gtk::current_event_time()` reports
        // `GDK_CURRENT_TIME` (zero) for events GTK never saw.
        time: (glib::monotonic_time() / 1000) as u32,
    };

    let mut event = gdk::Event::new(kind);
    if build(&mut event, &ctx).is_none() {
        return;
    }
    if let Some(device) = Display::default()
        .and_then(|display| display.default_seat())
        .and_then(|seat| seat.pointer())
    {
        event.set_device(Some(&device));
    }
    gtk::main_do_event(&mut event);
}

/// GDK button index, plus the mask that button contributes while held.
const fn mouse_button(button: MouseButton) -> Option<(u32, ModifierType)> {
    match button {
        MouseButton::Left => Some((1, ModifierType::BUTTON1_MASK)),
        MouseButton::Middle => Some((2, ModifierType::BUTTON2_MASK)),
        MouseButton::Right => Some((3, ModifierType::BUTTON3_MASK)),
        MouseButton::Back => Some((8, ModifierType::BUTTON4_MASK)),
        MouseButton::Forward => Some((9, ModifierType::BUTTON5_MASK)),
        MouseButton::Other(_) => None,
    }
}

fn keyboard_mask(modifiers: ModifiersState) -> ModifierType {
    let mut mask = ModifierType::empty();
    mask.set(ModifierType::SHIFT_MASK, modifiers.shift_key());
    mask.set(ModifierType::CONTROL_MASK, modifiers.control_key());
    mask.set(ModifierType::MOD1_MASK, modifiers.alt_key());
    mask.set(ModifierType::SUPER_MASK, modifiers.super_key());
    mask
}

const fn is_modifier_key(logical: &Key) -> bool {
    matches!(
        logical,
        Key::Named(
            NamedKey::Shift
                | NamedKey::Control
                | NamedKey::Alt
                | NamedKey::Super
                | NamedKey::CapsLock
                | NamedKey::NumLock
        )
    )
}

/// Map a winit logical key to a GDK keysym. The character payload already
/// reflects the active layout and shift level, so it wins; named keys carry no
/// character and fall through to the table.
fn keyval(logical: &Key) -> u32 {
    let named = match logical {
        Key::Character(text) => {
            return text
                .chars()
                .next()
                .map_or(*key::VoidSymbol, |c| *gdk::keys::Key::from_unicode(c));
        }
        Key::Named(named) => *named,
        Key::Dead(_) | Key::Unidentified(_) => return *key::VoidSymbol,
    };
    *match named {
        NamedKey::Space => key::space,
        NamedKey::Enter => key::Return,
        NamedKey::Tab => key::Tab,
        NamedKey::Escape => key::Escape,
        NamedKey::Backspace => key::BackSpace,
        NamedKey::Delete => key::Delete,
        NamedKey::Insert => key::Insert,
        NamedKey::ArrowUp => key::Up,
        NamedKey::ArrowDown => key::Down,
        NamedKey::ArrowLeft => key::Left,
        NamedKey::ArrowRight => key::Right,
        NamedKey::Home => key::Home,
        NamedKey::End => key::End,
        NamedKey::PageUp => key::Page_Up,
        NamedKey::PageDown => key::Page_Down,
        NamedKey::Shift => key::Shift_L,
        NamedKey::Control => key::Control_L,
        NamedKey::Alt => key::Alt_L,
        NamedKey::Super => key::Super_L,
        NamedKey::CapsLock => key::Caps_Lock,
        NamedKey::NumLock => key::Num_Lock,
        NamedKey::F1 => key::F1,
        NamedKey::F2 => key::F2,
        NamedKey::F3 => key::F3,
        NamedKey::F4 => key::F4,
        NamedKey::F5 => key::F5,
        NamedKey::F6 => key::F6,
        NamedKey::F7 => key::F7,
        NamedKey::F8 => key::F8,
        NamedKey::F9 => key::F9,
        NamedKey::F10 => key::F10,
        NamedKey::F11 => key::F11,
        NamedKey::F12 => key::F12,
        _ => key::VoidSymbol,
    }
}
