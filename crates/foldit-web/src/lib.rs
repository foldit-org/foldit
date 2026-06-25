//! WASM entry point for the Foldit web build.
//!
//! Mounts viso into a host-provided `<canvas>`, owns the
//! `requestAnimationFrame` loop, and exposes the [`FolditApp`] surface to
//! JavaScript via `wasm-bindgen`. The IPC envelope is the same one used
//! by the wry desktop transport (see `foldit_gui::bridge`); only the
//! delivery mechanism changes; JS calls Rust functions directly via
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

// Many transitive deps (base64, bitflags, thiserror, wit-bindgen, and the
// windows/foreign-types families pulled in by wgpu/winit) resolve at two
// major versions in the tree; the duplication is not controllable here.
#![allow(
    clippy::multiple_crate_versions,
    reason = "duplicate dep versions come from transitive deps, not controllable here"
)]
#![cfg(target_arch = "wasm32")]

mod host;

use std::cell::RefCell;
use std::rc::Rc;

use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::HtmlCanvasElement;

pub use wasm_bindgen_rayon::init_thread_pool;

use foldit_core::App;
use foldit_gui::bridge::{self, RequestKind, RequestResult};
use foldit_gui::{
    AppCommand, Dispatcher, EntitySelection, OpDispatch, ViewportInput,
};
use viso::{RenderContext, VisoEngine};
use viso::options::VisoOptions;

#[wasm_bindgen]
pub fn init() {
    console_error_panic_hook::set_once();
    let _ = console_log::init_with_level(log::Level::Info);
}

type AppHandle = Rc<RefCell<App>>;
type JsCallback = Rc<RefCell<Option<js_sys::Function>>>;

/// Single-threaded handoff for the async OPFS progress load. The startup
/// `spawn_local` task stashes the read bytes here; the rAF loop drains it
/// once and merges via `App::import_progress`. No cross-thread channel is
/// needed because wasm is single-threaded.
type ProgressLoadCell = Rc<RefCell<Option<Vec<u8>>>>;

/// Filename under the origin-private OPFS root for the persisted progress map.
const PROGRESS_FILE: &str = "progress.json";

/// JS-facing application handle. Owns the host-agnostic [`App`] (which
/// in turn owns the orchestrator, entity store, and history) plus the
/// JS callbacks for state-push and async-request response delivery.
#[wasm_bindgen]
pub struct FolditApp {
    app: AppHandle,
    state_cb: JsCallback,
    response_cb: JsCallback,
}

#[wasm_bindgen]
impl FolditApp {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Self {
        // Web doesn't load by filesystem path; the host fetches
        // structure bytes and feeds them in via the orchestrator.
        let app = App::new(Box::new(host::WebHost));
        Self {
            app: Rc::new(RefCell::new(app)),
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

        // Kick the one-shot async read of the persisted progress map. The
        // result is stashed into the shared cell and merged by the rAF loop
        // once it lands (a missing file on first run leaves the cell empty).
        let progress_load: ProgressLoadCell = Rc::new(RefCell::new(None));
        spawn_progress_load(progress_load.clone());

        // start the render loop
        spawn_render_loop(self.app.clone(), self.state_cb.clone(), progress_load);

        Ok(())
    }

    /// Forward a viewport input event. `json` is the JSON-encoded
    /// `ViewportInput` enum payload (same shape as the wry transport).
    #[wasm_bindgen(js_name = viewportInput)]
    pub fn viewport_input(&self, json: &str) -> Result<(), JsValue> {
        let input: ViewportInput = serde_json::from_str(json)
            .map_err(|e| JsValue::from_str(&format!("viewport_input parse: {e}")))?;
        self.app.borrow_mut().on_viewport_input(input);
        Ok(())
    }

    /// Forward a native GUI / chrome command. `json` is the JSON-encoded
    /// `AppCommand` enum.
    #[wasm_bindgen(js_name = appCommand)]
    pub fn app_command(&self, json: &str) -> Result<(), JsValue> {
        let command: AppCommand = serde_json::from_str(json)
            .map_err(|e| JsValue::from_str(&format!("app_command parse: {e}")))?;
        self.app.borrow_mut().on_app_command(command);
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

    /// Forward a panel-originated selection mutation. `json` is the
    /// JSON-encoded `Vec<EntitySelection>` (matches the
    /// `IpcMessage::SetSelection { entries }` payload).
    #[wasm_bindgen(js_name = setSelection)]
    pub fn set_selection(&self, json: &str) -> Result<(), JsValue> {
        let entries: Vec<EntitySelection> = serde_json::from_str(json)
            .map_err(|e| JsValue::from_str(&format!("set_selection parse: {e}")))?;
        self.app.borrow_mut().on_set_selection(entries);
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

// Outbound transport: async-request response delivery.
//
// State push is driven inline by the rAF loop calling
// `App::serialize_frontend_dirty`; only the response channel needs a
// long-lived JS-callback bridge here.

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

// rAF loop: self-rescheduling closure that drives App::tick(dt) and
// then drains the App-owned FrontendState dirty diff into the JS
// state callback.

fn spawn_render_loop(app: AppHandle, state_cb: JsCallback, progress_load: ProgressLoadCell) {
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
        let dt = ((now - *last_ts.borrow()) / 1000.0).max(0.0) as f32;
        *last_ts.borrow_mut() = now;

        {
            let mut app = app.borrow_mut();
            app.tick(dt);
            app.render();
            if let Some(bytes) = app.serialize_frontend_dirty() {
                if let Some(cb) = state_cb.borrow().clone() {
                    // bytes is already JSON-encoded UTF-8.
                    if let Ok(json) = std::str::from_utf8(&bytes) {
                        let _ = cb.call1(&JsValue::NULL, &JsValue::from_str(json));
                    }
                }
            }

            // Merge any landed progress load (once) and fire-and-forget a
            // pending save. The OPFS I/O runs on spawned tasks, so the frame
            // is never blocked on it. Mirrors the desktop host's
            // `apply_progress_persistence`.
            if let Some(bytes) = progress_load.borrow_mut().take() {
                app.import_progress(&bytes);
            }
            if let Some(bytes) = app.take_progress_to_persist() {
                spawn_progress_save(bytes);
            }
        }

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

// OPFS progress persistence: origin-private read at startup, fire-and-forget
// write when the live map changes. Both run as `spawn_local` tasks off the rAF
// frame. The OPFS error type is `JsValue` (no `io::ErrorKind`), so a missing
// file on first run surfaces as a rejected open and is treated as "no saved
// progress" rather than an error.

/// Read the persisted progress map from the OPFS root and stash the bytes into
/// the shared cell. A first-run miss (the file does not exist) leaves the cell
/// empty and is not logged as an error.
fn spawn_progress_load(cell: ProgressLoadCell) {
    wasm_bindgen_futures::spawn_local(async move {
        use opfs::persistent::{app_specific_dir, DirectoryHandle as DirHandle};
        use opfs::{DirectoryHandle as _, FileHandle as _, GetFileHandleOptions};

        let dir: DirHandle = match app_specific_dir().await {
            Ok(d) => d,
            Err(e) => {
                log::warn!("[foldit-web] could not open OPFS root: {e:?}");
                return;
            }
        };
        let opts = GetFileHandleOptions { create: false };
        let file = match dir.get_file_handle_with_options(PROGRESS_FILE, &opts).await {
            Ok(f) => f,
            // A rejected open with `create: false` is the first-run "no file"
            // signal; treat any error here as "no saved progress".
            Err(_) => return,
        };
        match file.read().await {
            Ok(bytes) => *cell.borrow_mut() = Some(bytes),
            Err(e) => log::warn!("[foldit-web] could not read progress file: {e:?}"),
        }
    });
}

/// Write the serialized progress map to the OPFS root, fire-and-forget. Errors
/// are logged and dropped; the next change re-attempts the save.
fn spawn_progress_save(bytes: Vec<u8>) {
    wasm_bindgen_futures::spawn_local(async move {
        use opfs::persistent::{app_specific_dir, DirectoryHandle as DirHandle};
        use opfs::{
            CreateWritableOptions, DirectoryHandle as _, FileHandle as _, GetFileHandleOptions,
            WritableFileStream as _,
        };

        let dir: DirHandle = match app_specific_dir().await {
            Ok(d) => d,
            Err(e) => {
                log::warn!("[foldit-web] could not open OPFS root for write: {e:?}");
                return;
            }
        };
        let get_opts = GetFileHandleOptions { create: true };
        let mut file = match dir
            .get_file_handle_with_options(PROGRESS_FILE, &get_opts)
            .await
        {
            Ok(f) => f,
            Err(e) => {
                log::warn!("[foldit-web] could not open progress file for write: {e:?}");
                return;
            }
        };
        let write_opts = CreateWritableOptions { keep_existing_data: false };
        let mut writer = match file.create_writable_with_options(&write_opts).await {
            Ok(w) => w,
            Err(e) => {
                log::warn!("[foldit-web] could not open progress writer: {e:?}");
                return;
            }
        };
        if let Err(e) = writer.write_at_cursor_pos(&bytes).await {
            log::warn!("[foldit-web] could not write progress: {e:?}");
            return;
        }
        if let Err(e) = writer.close().await {
            log::warn!("[foldit-web] could not flush progress: {e:?}");
        }
    });
}
