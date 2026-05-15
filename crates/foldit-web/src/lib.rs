//! WASM entry point for the Foldit web build.
//!
//! Mounts viso into a host-provided `<canvas>`, owns the
//! `requestAnimationFrame` loop, and exposes the [`FolditApp`] surface to
//! JavaScript via `wasm-bindgen`. The IPC envelope is the same one used
//! by the wry desktop transport (see `foldit_gui::bridge`); only the
//! delivery mechanism changes — JS calls Rust functions directly via
//! `wasm-bindgen` instead of round-tripping JSON through `window.ipc`.
//!
//! All host-agnostic state and dispatch logic lives in `foldit_core::App`
//! (shared with the desktop binary). This crate is a thin wasm-bindgen
//! shell over it — engine construction, rAF loop, and JS↔Rust glue.
//!
//! # Lifecycle
//!
//! ```ignore
//! await init();                                    // panic hook + console
//! await initThreadPool(navigator.hardwareConcurrency);  // rayon pool
//! const app = new FolditApp();
//! app.set_state_callback(json => onState(JSON.parse(json)));
//! app.set_response_callback((wishId, ok, payload) => __onResponse(...));
//! await app.start(canvas);                          // mounts viso, starts rAF loop
//! ```
//!
//! Outside of wasm, this crate compiles as a thin rlib so workspace-wide
//! `cargo check` succeeds on the host without invoking wasm-bindgen.

#![cfg(target_arch = "wasm32")]

use std::cell::RefCell;
use std::rc::Rc;

use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::HtmlCanvasElement;

pub use wasm_bindgen_rayon::init_thread_pool;

use foldit_core::App;
use foldit_gui::bridge::{self, RequestKind, RequestResult};
use foldit_gui::{
    ActionId, Dispatcher, FrontendState, OpDispatch, ParameterizedAction, ViewportInput,
};
use viso::{RenderContext, VisoEngine};
use viso::options::VisoOptions;

// ─────────────────────────────────────────────────────────────────────────────
// One-shot init: panic hook + console_log. Call once before constructing
// any FolditApp.
// ─────────────────────────────────────────────────────────────────────────────

#[wasm_bindgen]
pub fn init() {
    console_error_panic_hook::set_once();
    let _ = console_log::init_with_level(log::Level::Info);
}

// ─────────────────────────────────────────────────────────────────────────────
// Shared handles
// ─────────────────────────────────────────────────────────────────────────────

type AppHandle = Rc<RefCell<App>>;
type FrontendHandle = Rc<RefCell<FrontendState>>;
type JsCallback = Rc<RefCell<Option<js_sys::Function>>>;

// ─────────────────────────────────────────────────────────────────────────────
// FolditApp — JS-facing handle
// ─────────────────────────────────────────────────────────────────────────────

/// JS-facing application handle. Owns the host-agnostic [`App`] (which
/// in turn owns the orchestrator, entity store, and history) plus the
/// JS callbacks for state-push and async-request response delivery.
#[wasm_bindgen]
pub struct FolditApp {
    app: AppHandle,
    frontend: FrontendHandle,
    state_cb: JsCallback,
    response_cb: JsCallback,
}

#[wasm_bindgen]
impl FolditApp {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Self {
        // Empty pdb_path — web doesn't load by filesystem path; the host
        // fetches structure bytes and feeds them in via the orchestrator.
        let app = App::new(String::new());
        Self {
            app: Rc::new(RefCell::new(app)),
            frontend: Rc::new(RefCell::new(FrontendState::new())),
            state_cb: Rc::new(RefCell::new(None)),
            response_cb: Rc::new(RefCell::new(None)),
        }
    }

    /// Register the JS callback that receives state-section pushes. The
    /// callback is invoked as `cb(jsonString)` where `jsonString` is the
    /// stringified `Partial<FrontendState>`.
    #[wasm_bindgen(js_name = setStateCallback)]
    pub fn set_state_callback(&self, cb: js_sys::Function) {
        *self.state_cb.borrow_mut() = Some(cb);
    }

    /// Register the JS callback that resolves pending async requests.
    /// Invoked as `cb(wishId, ok, payloadJson)`.
    #[wasm_bindgen(js_name = setResponseCallback)]
    pub fn set_response_callback(&self, cb: js_sys::Function) {
        *self.response_cb.borrow_mut() = Some(cb);
    }

    /// Build a wgpu `RenderContext` against the canvas, construct a
    /// `VisoEngine`, hand it to the inner [`App`], and start the
    /// `requestAnimationFrame` loop.
    pub async fn start(&self, canvas: HtmlCanvasElement) -> Result<(), JsValue> {
        let dpr = web_sys::window()
            .map(|w| w.device_pixel_ratio())
            .unwrap_or(1.0);

        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let width = ((f64::from(canvas.client_width()) * dpr) as u32).max(1);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let height = ((f64::from(canvas.client_height()) * dpr) as u32).max(1);

        // Sync canvas drawing buffer to CSS layout size before building
        // the wgpu surface (otherwise the buffer stays at the HTML
        // default 300×150 and renders are mismatched).
        canvas.set_width(width);
        canvas.set_height(height);

        let surface_target = wgpu::SurfaceTarget::Canvas(canvas);
        let context = RenderContext::new(surface_target, (width, height))
            .await
            .map_err(|e| JsValue::from_str(&format!("RenderContext::new failed: {e:?}")))?;

        let engine = VisoEngine::new(context, VisoOptions::default())
            .map_err(|e| JsValue::from_str(&format!("VisoEngine::new failed: {e:?}")))?;

        self.app.borrow_mut().attach_engine(engine);

        // rAF loop. Self-rescheduling closure that ticks the engine once
        // per frame. Pattern lifted from viso/src/app/web/mod.rs.
        spawn_render_loop(self.app.clone());

        Ok(())
    }

    // ── Inbound IPC (JS → Rust) ─────────────────────────────────────────────

    /// Forward a viewport input event. `json` is the JSON-encoded
    /// `ViewportInput` enum payload (same shape as the wry transport).
    #[wasm_bindgen(js_name = viewportInput)]
    pub fn viewport_input(&self, json: &str) -> Result<(), JsValue> {
        let input: ViewportInput = serde_json::from_str(json)
            .map_err(|e| JsValue::from_str(&format!("viewport_input parse: {e}")))?;
        self.app.borrow_mut().on_viewport_input(input);
        Ok(())
    }

    /// Forward a parameter-less action by id.
    #[wasm_bindgen(js_name = triggerAction)]
    pub fn trigger_action(&self, id: u32) -> Result<(), JsValue> {
        let action = ActionId::try_from(id)
            .map_err(|e| JsValue::from_str(&format!("trigger_action: unknown id ({e})")))?;
        self.app.borrow_mut().on_trigger_action(action);
        Ok(())
    }

    /// Forward a parameterized action. `json` is the JSON-encoded
    /// `ParameterizedAction` enum.
    #[wasm_bindgen(js_name = parameterizedAction)]
    pub fn parameterized_action(&self, json: &str) -> Result<(), JsValue> {
        let action: ParameterizedAction = serde_json::from_str(json)
            .map_err(|e| JsValue::from_str(&format!("parameterized_action parse: {e}")))?;
        self.app.borrow_mut().on_parameterized_action(action);
        Ok(())
    }

    /// Forward a catalog-driven plugin op dispatch. `json` is the
    /// JSON-encoded `OpDispatch` payload.
    #[wasm_bindgen(js_name = dispatchOp)]
    pub fn dispatch_op(&self, json: &str) -> Result<(), JsValue> {
        let op: OpDispatch = serde_json::from_str(json)
            .map_err(|e| JsValue::from_str(&format!("dispatch_op parse: {e}")))?;
        self.app.borrow_mut().on_dispatch_op(op);
        Ok(())
    }

    /// Round-trip an async request to the backend. Returns a JS Promise
    /// (via `wasm-bindgen-futures`) that resolves with the JSON payload
    /// or rejects with the backend-provided error string.
    pub async fn request(&self, kind: String, payload: String) -> Result<JsValue, JsValue> {
        let kind: RequestKind = serde_json::from_value(serde_json::Value::String(kind))
            .map_err(|e| JsValue::from_str(&format!("request: unknown kind ({e})")))?;
        let payload: serde_json::Value = if payload.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_str(&payload)
                .map_err(|e| JsValue::from_str(&format!("request payload: {e}")))?
        };

        match self.app.borrow_mut().handle_request(kind, payload) {
            Ok(value) => Ok(JsValue::from_str(&value.to_string())),
            Err(msg) => Err(JsValue::from_str(&msg)),
        }
    }
}

impl Default for FolditApp {
    fn default() -> Self {
        Self::new()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Outbound transport: state-push + async-request response delivery.
//
// Implemented against the JS callbacks. Currently only used internally;
// when foldit_core::App grows a `populate_frontend(&mut FrontendState)`
// call site here, this transport's `send_state` will fire.
// ─────────────────────────────────────────────────────────────────────────────

#[allow(dead_code)]
struct WebTransport {
    state_cb: JsCallback,
    response_cb: JsCallback,
}

impl bridge::Transport for WebTransport {
    fn send_state(&self, payload: &serde_json::Value) {
        let Some(cb) = self.state_cb.borrow().clone() else {
            return;
        };
        let json = payload.to_string();
        let _ = cb.call1(&JsValue::NULL, &JsValue::from_str(&json));
    }

    fn send_response(&self, wish_id: &str, result: &RequestResult) {
        let Some(cb) = self.response_cb.borrow().clone() else {
            return;
        };
        let (ok, payload_str) = match result {
            Ok(v) => (true, v.to_string()),
            Err(msg) => (false, msg.clone()),
        };
        let _ = cb.call3(
            &JsValue::NULL,
            &JsValue::from_str(wish_id),
            &JsValue::from_bool(ok),
            &JsValue::from_str(&payload_str),
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// rAF loop: self-rescheduling closure that ticks the engine each frame.
// ─────────────────────────────────────────────────────────────────────────────

fn spawn_render_loop(app: AppHandle) {
    let window = match web_sys::window() {
        Some(w) => w,
        None => {
            log::error!("[foldit-web] no `window` — render loop not spawned");
            return;
        }
    };

    let f: Rc<RefCell<Option<Closure<dyn FnMut()>>>> = Rc::new(RefCell::new(None));
    let g = f.clone();

    let perf = window.performance();
    let last_ts: Rc<RefCell<f64>> = Rc::new(RefCell::new(perf.as_ref().map_or(0.0, |p| p.now())));

    *g.borrow_mut() = Some(Closure::new(move || {
        let now = perf.as_ref().map_or(0.0, |p| p.now());
        let _dt = ((now - *last_ts.borrow()) / 1000.0).max(0.0) as f32;
        *last_ts.borrow_mut() = now;

        // Tick the App; it owns the engine and drives update + render.
        // (Once App grows an explicit per-frame entry-point we'll call
        // it here. For now the engine is reachable via App; iterate.)
        let _ = app.borrow_mut();

        // Re-arm.
        if let Some(window) = web_sys::window() {
            let _ = window.request_animation_frame(
                f.borrow().as_ref().unwrap().as_ref().unchecked_ref(),
            );
        }
    }));

    let _ = window.request_animation_frame(
        g.borrow().as_ref().unwrap().as_ref().unchecked_ref(),
    );
}
