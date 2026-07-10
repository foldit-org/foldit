//! Window management module: winit event loop, webview, frame timing, IPC dispatch.
//!
//! `AppRunner` owns the window-layer state and holds `App` by value.
//! It implements `ApplicationHandler` and delegates domain logic to `App` via method calls.
//!
//! Two webview backends sit behind the [`Webview`] alias. macOS and Windows
//! parent a `wry::WebView` to the winit window as a transparent child. Linux
//! cannot: `WebKitGTK` paints an opaque backing into any on-screen X window and
//! would hide the renderer, so [`crate::offscreen_webview`] runs it off-screen
//! and viso composites the result as a texture.

use foldit_core::{App, HostEffects};
use foldit_gui::IpcMessage;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::PhysicalKey;
use winit::window::{Window, WindowId};

#[cfg(target_os = "linux")]
use crate::offscreen_webview::OffscreenWebview;

#[cfg(target_os = "linux")]
type Webview = OffscreenWebview;
#[cfg(not(target_os = "linux"))]
type Webview = wry::WebView;

/// Window-layer state that wraps `App` and implements `ApplicationHandler`.
/// `App` owns the [`foldit_gui::GuiState`] mirror and the
/// Loading → `InPuzzle` state-machine; the runner is purely the
/// wry/winit + dev-server shell.
/// Minimum spacing between frontend pushes. The event loop runs uncapped
/// (`ControlFlow::Poll` plus a self-sustaining `request_redraw`), so without
/// this it issues `evaluate_script` tens of thousands of times per second —
/// far past what the WebContent process can drain, which exhausts memory in
/// a process the host never sees.
const FRONTEND_PUSH_INTERVAL: Duration = Duration::from_millis(16);

/// Resolve or reject a JS-side pending request via `window.__onResponse`.
/// Shared by the immediate reply path and the deferred one, which answers
/// from `DesktopEffects` a tick or more after the request arrived.
fn respond_to_webview(
    webview: Option<&wry::WebView>,
    wish_id: &str,
    result: &foldit_gui::RequestResult,
) {
    let Some(webview) = webview else {
        return;
    };
    let (ok, payload) = match result {
        Ok(v) => (true, v.clone()),
        Err(msg) => (false, serde_json::Value::String(msg.clone())),
    };
    // Serializing a `&str` to a JSON string is infallible, so the unwrap
    // cannot fire (the only error sources in `to_string` are custom
    // Serialize impls and non-string map keys).
    #[allow(
        clippy::unwrap_used,
        reason = "serde_json::to_string over a &str is infallible"
    )]
    let script = format!(
        "if(window.__onResponse)window.__onResponse({},{},{})",
        serde_json::to_string(wish_id).unwrap(),
        ok,
        payload,
    );
    let _ = webview.evaluate_script(&script);
}

pub struct AppRunner {
    app: App,
    window: Option<Arc<Window>>,
    webview: Option<Webview>,
    ipc_rx: Option<std::sync::mpsc::Receiver<IpcMessage>>,
    webview_ready: bool,
    last_frame: Instant,
    /// When the frontend last accepted a push. `evaluate_script` enqueues an
    /// async IPC message to the WebContent process; the event loop can issue
    /// them far faster than WebKit drains them, so pushes are rate-limited to
    /// `FRONTEND_PUSH_INTERVAL`. `None` until the first push.
    last_frontend_push: Option<Instant>,
    /// Last applied render size, to avoid redundant resizes
    last_render_size: (u32, u32),
    /// Structure load is deferred until the webview loading screen is visible
    init_pending: bool,
    /// Timeout for webview readiness — load anyway if webview takes too long
    init_deadline: Option<Instant>,
    /// Shared log buffer from `tee_logger` (drained each frame into frontend state)
    log_buffer: crate::tee_logger::LogBuffer,
    /// Async runtime that owns the puzzle-progress disk I/O. opfs reads/writes
    /// run on its worker threads, never on the event-loop thread.
    progress_runtime: tokio::runtime::Runtime,
    /// Receiver for the one-shot startup load of the persisted progress map.
    /// `tick_frame` drains it; `Some(bytes)` is the on-disk map, and the
    /// channel staying empty (or closing on a first-run miss) means no merge.
    progress_load_rx: Receiver<Vec<u8>>,
    /// Sender half handed to the startup load task; held so the receiver does
    /// not see a premature disconnect before the task runs.
    progress_load_tx: Option<Sender<Vec<u8>>>,
    // `pub(crate)` so the dev-server methods in `webview_assets` can drive them.
    #[cfg(debug_assertions)]
    pub(crate) dev_server: Option<std::process::Child>,
    #[cfg(debug_assertions)]
    pub(crate) dev_server_available: bool,
}

impl AppRunner {
    /// Filename under `~/.foldit/` for the persisted high-score progress map.
    const PROGRESS_FILE: &'static str = "progress.json";

    #[allow(
        clippy::expect_used,
        reason = "the progress runtime is built once at binary startup; a failure to spawn worker threads is unrecoverable and should abort loudly"
    )]
    fn new(app: App, log_buffer: crate::tee_logger::LogBuffer) -> Self {
        let progress_runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("Failed to build progress runtime");
        let (progress_load_tx, progress_load_rx) = std::sync::mpsc::channel::<Vec<u8>>();
        Self {
            app,
            window: None,
            webview: None,
            ipc_rx: None,
            webview_ready: false,
            last_frame: Instant::now(),
            last_frontend_push: None,
            last_render_size: (0, 0),
            init_pending: false,
            init_deadline: None,
            log_buffer,
            progress_runtime,
            progress_load_rx,
            progress_load_tx: Some(progress_load_tx),
            #[cfg(debug_assertions)]
            dev_server: None,
            #[cfg(debug_assertions)]
            dev_server_available: false,
        }
    }

    /// Resolve the `~/.foldit/` data directory, creating it if needed. Returns
    /// `None` when the home directory cannot be located or the directory
    /// cannot be created, in which case progress persistence is skipped.
    fn foldit_data_dir() -> Option<PathBuf> {
        let dir = dirs::home_dir()?.join(".foldit");
        if let Err(e) = std::fs::create_dir_all(&dir) {
            log::warn!("Could not create {}: {e}", dir.display());
            return None;
        }
        Some(dir)
    }

    /// Spawn the one-shot startup read of the persisted progress map onto the
    /// runtime's worker thread. On success the bytes are sent through
    /// `progress_load_rx`; a missing file (first run) or any read error sends
    /// nothing, leaving the live map untouched. Called once when the App is
    /// ready to merge.
    fn spawn_progress_load(&mut self) {
        let Some(tx) = self.progress_load_tx.take() else {
            return;
        };
        let Some(dir) = Self::foldit_data_dir() else {
            return;
        };
        self.progress_runtime.spawn(async move {
            use opfs::persistent::DirectoryHandle as DirHandle;
            use opfs::{DirectoryHandle as _, FileHandle as _, GetFileHandleOptions};

            let handle = DirHandle::from(dir);
            let opts = GetFileHandleOptions { create: false };
            let file = match handle
                .get_file_handle_with_options(Self::PROGRESS_FILE, &opts)
                .await
            {
                Ok(f) => f,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
                Err(e) => {
                    log::warn!("Could not open progress file: {e}");
                    return;
                }
            };
            match file.read().await {
                Ok(bytes) => {
                    let _ = tx.send(bytes);
                }
                Err(e) => log::warn!("Could not read progress file: {e}"),
            }
        });
    }

    /// Drain IPC messages from the webview and dispatch them.
    fn process_ipc_messages(&mut self) {
        let Some(rx) = &self.ipc_rx else {
            return;
        };
        let messages: Vec<IpcMessage> = rx.try_iter().collect();
        for msg in messages {
            match msg {
                IpcMessage::Ready => {
                    log::info!("Webview ready");
                    self.webview_ready = true;
                    // App's `on_ready` impl marks every section dirty so the
                    // next push emits a full snapshot.
                    self.app.on_ready();
                }
                IpcMessage::ViewportInput(input) => self.app.handle_viewport_input(input),
                IpcMessage::DispatchOp(op) => self.app.on_dispatch_op(op),
                IpcMessage::AppCommand(command) => self.app.handle_app_command(command),
                IpcMessage::SetSelection { entries } => self.app.handle_set_selection(entries),
                IpcMessage::UpdateStream { request_id, params } => {
                    self.app.on_update_stream(request_id, params);
                }
                IpcMessage::OpenSessionDialog => self.open_session_dialog(),
                // A plugin query round-trips to a worker that is serialising
                // tasks behind whatever step it is running, so it is fired
                // async and answered from `App::tick` when the reply lands.
                // Every other request kind is local and answers here.
                IpcMessage::Request {
                    wish_id,
                    kind: foldit_gui::RequestKind::PluginQuery,
                    payload,
                } => {
                    if let Err(e) = self.app.begin_plugin_query(&wish_id, &payload) {
                        self.send_response_to_webview(&wish_id, &Err(e));
                    }
                }
                IpcMessage::Request {
                    wish_id,
                    kind,
                    payload,
                } => {
                    let result = self.app.handle_request(kind, payload);
                    self.send_response_to_webview(&wish_id, &result);
                }
            }
        }
    }

    /// Open the native "Load Session" file picker, classify the chosen file,
    /// and route it to the existing load path. Runs on the event-loop thread
    /// (rfd requires it); the modal blocks this frame for its lifetime, which
    /// is the intended behavior for a file picker.
    fn open_session_dialog(&mut self) {
        use foldit_core::puzzle::SessionLoadKind;
        use foldit_gui::AppCommand;

        let Some(path) = rfd::FileDialog::new()
            .set_title("Load Session")
            .add_filter("Foldit session", &["toml", "pdb", "cif", "mmcif", "bcif"])
            .pick_file()
        else {
            return; // user cancelled
        };

        match foldit_core::puzzle::classify_session_path(&path) {
            SessionLoadKind::PuzzleDir(dir) => {
                self.app.handle_app_command(AppCommand::LoadPuzzleDir {
                    path: dir.to_string_lossy().into_owned(),
                });
            }
            SessionLoadKind::Structure(file) => {
                self.app.handle_app_command(AppCommand::LoadStructure {
                    path: file.to_string_lossy().into_owned(),
                });
            }
            SessionLoadKind::Unsupported => {
                log::warn!("Load Session: unsupported selection {}", path.display());
            }
        }
    }

    /// Resolve or reject a JS-side pending request via window.__onResponse.
    fn send_response_to_webview(&self, wish_id: &str, result: &foldit_gui::RequestResult) {
        respond_to_webview(self.webview.as_ref(), wish_id, result);
    }

    /// Resize the webview to match a new window size (physical pixels).
    fn resize_webview(&mut self, new_size: winit::dpi::PhysicalSize<u32>) {
        let Some(webview) = self.webview.as_mut() else {
            return;
        };
        #[cfg(target_os = "linux")]
        webview.resize(new_size);
        #[cfg(not(target_os = "linux"))]
        {
            use wry::dpi::{PhysicalPosition, PhysicalSize};
            let _ = webview.set_bounds(wry::Rect {
                position: PhysicalPosition::new(0, 0).into(),
                size: PhysicalSize::new(new_size.width, new_size.height).into(),
            });
        }
    }

    /// Blit the webview's latest paint into viso's overlay texture, which the
    /// next `App::render` composites over the 3D scene.
    #[cfg(target_os = "linux")]
    fn composite_webview(&mut self) {
        // Cloning these `Arc` handles is what lets the overlay install below
        // borrow `App` mutably while the webview still holds a texture view.
        let (Some(device), Some(queue)) = (
            self.app.wgpu_device().cloned(),
            self.app.wgpu_queue().cloned(),
        ) else {
            return;
        };
        let Some(webview) = self.webview.as_mut() else {
            return;
        };
        if let Some(view) = webview.ensure_texture(&device) {
            self.app.set_overlay_texture(Some(view));
        }
        webview.upload(&queue);
    }

    /// Build the `WebKitGTK` view that renders off-screen into viso's overlay.
    #[cfg(target_os = "linux")]
    fn create_webview(window: &Arc<Window>) -> (Option<Webview>, Receiver<IpcMessage>) {
        use crate::offscreen_webview::Content;
        #[cfg(not(debug_assertions))]
        use crate::offscreen_webview::Scheme;

        let (ipc_tx, ipc_rx) = std::sync::mpsc::channel::<IpcMessage>();

        #[cfg(debug_assertions)]
        let content = Content {
            entry: "http://localhost:5173".to_owned(),
            scheme: None,
        };
        #[cfg(not(debug_assertions))]
        let content = {
            let assets = crate::webview_assets::AssetResolver::new();
            Content {
                entry: Self::RELEASE_URL.to_owned(),
                scheme: Some(Scheme {
                    name: "foldit".to_owned(),
                    resolve: Box::new(move |path| assets.resolve(path)),
                }),
            }
        };

        let webview = OffscreenWebview::new(
            window.inner_size(),
            content,
            Self::INIT_SCRIPT,
            ipc_tx,
            Self::handle_ipc,
        );
        let webview = match webview {
            Ok(webview) => {
                log::info!("offscreen WebKitGTK webview created");
                webview.set_scale_factor(window.scale_factor());
                webview.focus();
                Some(webview)
            }
            Err(e) => {
                log::error!("Failed to create the offscreen webview: {e}");
                None
            }
        };

        (webview, ipc_rx)
    }

    /// Create the wry webview as a child of the winit window (debug: connects to dev server).
    #[cfg(all(debug_assertions, not(target_os = "linux")))]
    fn create_webview(window: &Arc<Window>) -> (Option<Webview>, Receiver<IpcMessage>) {
        let (ipc_tx, ipc_rx) = std::sync::mpsc::channel::<IpcMessage>();

        let webview = {
            use wry::dpi::{PhysicalPosition, PhysicalSize};
            let inner = window.inner_size();

            let builder = wry::WebViewBuilder::new()
                .with_transparent(true)
                .with_devtools(true)
                .with_initialization_script(Self::INIT_SCRIPT)
                .with_url("http://localhost:5173")
                .with_ipc_handler({
                    let ipc_tx = ipc_tx;
                    move |req| Self::handle_ipc(&ipc_tx, req.body())
                })
                .with_bounds(wry::Rect {
                    position: PhysicalPosition::new(0, 0).into(),
                    size: PhysicalSize::new(inner.width, inner.height).into(),
                });

            match builder.build_as_child(window) {
                Ok(wv) => {
                    log::info!("wry webview created (debug, dev server)");
                    Some(wv)
                }
                Err(e) => {
                    log::error!("Failed to create wry webview: {e}");
                    None
                }
            }
        };

        (webview, ipc_rx)
    }

    /// Per-frame update: process IPC, drive `App::tick`, render, push
    /// dirty frontend bytes to the webview. `App` owns the drive loop
    /// (`SessionUpdate` drain, score poll, engine update, visualization update,
    /// state-machine, `populate_frontend`) — the host just sequences the
    /// IPC / surface / render side around it.
    fn tick_frame(&mut self) {
        let now = Instant::now();
        let dt = now.duration_since(self.last_frame);
        self.last_frame = now;

        // WebKitGTK runs on the GTK main loop, which winit does not drive. Pump
        // all pending GTK events each frame or the webview never lays out,
        // paints, or delivers its IPC messages.
        #[cfg(target_os = "linux")]
        while gtk::events_pending() {
            gtk::main_iteration_do(false);
        }

        // Process IPC messages (needed to detect webview_ready during init)
        self.process_ipc_messages();

        // Deferred startup arm: wait for the webview to show the loading
        // screen before arming bring-up. `begin_startup` is non-blocking (it
        // only kicks the plugin warms + stashes the bootstrap path), and the
        // per-frame `app.tick` below advances the startup state-machine one
        // step at a time, so the loading screen keeps animating throughout.
        // The wgpu RenderContext is already initialized (in `resumed()` before
        // webview attachment), so each frame the engine renders its empty scene
        // (which clears to black) and presents it; the surface shows black
        // matching the loading-screen background rather than the OS window
        // default, until the webview paints.
        if self.init_pending {
            let should_init = self.webview_ready
                || self.webview.is_none()
                || self.init_deadline.is_some_and(|d| now > d);

            if should_init {
                log::info!("Startup arming (webview_ready={})", self.webview_ready);
                self.init_pending = false;
                self.init_deadline = None;
                // Arm the non-blocking startup machine once. From here the
                // per-frame `app.tick` drives the warm connect, plugin Init,
                // normalize, and first score across frames.
                self.app.begin_startup();
                // Kick the async read of persisted progress; `tick_frame`
                // merges the result via `import_progress` once it lands.
                self.spawn_progress_load();
                // Fall through to tick + render this frame.
            } else {
                // Webview still loading. Present the engine's empty scene so the
                // surface clears to black (matching the loading-screen
                // background) instead of the OS compositor default, then keep
                // pumping frames so the webview can paint.
                self.app.render();
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
                return;
            }
        }

        // Ensure render surface always matches actual window size.
        // We do this every frame because Resized events can be unreliable
        // on Windows (stale WM_SIZE, timing issues with child windows).
        if let Some(ws) = self.window.as_ref().map(|window| window.inner_size()) {
            let size = (ws.width, ws.height);
            if size != self.last_render_size && size.0 > 0 && size.1 > 0 {
                log::info!(
                    "tick_frame resize: {}x{} (was {}x{})",
                    size.0,
                    size.1,
                    self.last_render_size.0,
                    self.last_render_size.1
                );
                self.app.resize(size.0, size.1);
                self.resize_webview(ws);
                self.last_render_size = size;
            }
        }

        // Drain log buffer into App-owned frontend state before the tick.
        if let Ok(buf) = self.log_buffer.lock() {
            if !buf.is_empty() {
                let log_text: String = buf.iter().cloned().collect::<Vec<_>>().join("\n");
                self.app.set_frontend_log(log_text);
            }
        }

        // App-owned drive loop: backend updates → `SessionUpdate` drain →
        // broadcaster + render projector → score poll → engine update +
        // visualization → state-machine → populate_frontend. Tail pushes
        // (state, tail-tip, fullscreen, persist) fire through `fx`.
        let mut fx = DesktopEffects {
            webview: self.webview.as_ref(),
            webview_ready: self.webview_ready,
            window: self.window.as_ref(),
            runtime: &self.progress_runtime,
            last_frontend_push: &mut self.last_frontend_push,
        };
        self.app.tick(dt.as_secs_f32(), &mut fx);

        #[cfg(target_os = "linux")]
        self.composite_webview();
        self.app.render();

        // Merge any landed progress load (projects next frame).
        if let Ok(bytes) = self.progress_load_rx.try_recv() {
            self.app.import_progress(&bytes);
        }

        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }
}

/// Evaluate `script` in the page and discard the outcome. The two backends
/// disagree on the return type; nothing here can act on either.
fn evaluate_script(webview: &Webview, script: &str) {
    #[cfg(target_os = "linux")]
    webview.evaluate_script(script);
    #[cfg(not(target_os = "linux"))]
    let _ = webview.evaluate_script(script);
}

/// Transient per-frame effect sink built from `AppRunner`'s non-`App`
/// fields and passed into `App::tick`.
struct DesktopEffects<'a> {
    webview: Option<&'a Webview>,
    webview_ready: bool,
    window: Option<&'a Arc<Window>>,
    runtime: &'a tokio::runtime::Runtime,
    last_frontend_push: &'a mut Option<Instant>,
}

impl HostEffects for DesktopEffects<'_> {
    fn may_push_frontend(&mut self) -> bool {
        let now = Instant::now();
        if self
            .last_frontend_push
            .is_some_and(|prev| now.duration_since(prev) < FRONTEND_PUSH_INTERVAL)
        {
            return false;
        }
        *self.last_frontend_push = Some(now);
        true
    }

    fn push_response(
        &mut self,
        wish_id: &str,
        result: &foldit_gui::RequestResult,
    ) {
        if !self.webview_ready {
            return;
        }
        respond_to_webview(self.webview, wish_id, result);
    }

    fn push_state(&mut self, json: &[u8]) {
        if !self.webview_ready {
            return;
        }
        let Ok(payload) = std::str::from_utf8(json) else {
            return;
        };
        if let Some(webview) = self.webview {
            let script = format!("if(window.__onStateUpdate)window.__onStateUpdate({payload})");
            evaluate_script(webview, &script);
        }
    }

    fn push_tail(&mut self, update: foldit_core::TailUpdate) {
        if !self.webview_ready {
            return;
        }
        let Some(webview) = self.webview else {
            return;
        };
        let script = match update {
            foldit_core::TailUpdate::Position(x, y) => {
                format!("if(window.__onTailUpdate)window.__onTailUpdate({x},{y})")
            }
            foldit_core::TailUpdate::Hide => {
                "if(window.__onTailUpdate)window.__onTailUpdate(null)".to_owned()
            }
        };
        evaluate_script(webview, &script);
    }

    fn set_fullscreen(&mut self, value: bool) {
        if let Some(window) = self.window {
            window.set_fullscreen(value.then(|| winit::window::Fullscreen::Borderless(None)));
        }
    }

    fn persist_progress(&mut self, bytes: Vec<u8>) {
        let Some(dir) = AppRunner::foldit_data_dir() else {
            return;
        };
        self.runtime.spawn(async move {
            use opfs::persistent::DirectoryHandle as DirHandle;
            use opfs::{
                CreateWritableOptions, DirectoryHandle as _, FileHandle as _, GetFileHandleOptions,
                WritableFileStream as _,
            };

            let handle = DirHandle::from(dir);
            let get_opts = GetFileHandleOptions { create: true };
            let mut file = match handle
                .get_file_handle_with_options(AppRunner::PROGRESS_FILE, &get_opts)
                .await
            {
                Ok(f) => f,
                Err(e) => {
                    log::warn!("Could not open progress file for write: {e}");
                    return;
                }
            };
            let write_opts = CreateWritableOptions {
                keep_existing_data: false,
            };
            let mut writer = match file.create_writable_with_options(&write_opts).await {
                Ok(w) => w,
                Err(e) => {
                    log::warn!("Could not open progress writer: {e}");
                    return;
                }
            };
            if let Err(e) = writer.write_at_cursor_pos(&bytes).await {
                log::warn!("Could not write progress: {e}");
                return;
            }
            if let Err(e) = writer.close().await {
                log::warn!("Could not flush progress: {e}");
            }
        });
    }
}

impl ApplicationHandler for AppRunner {
    #[allow(
        clippy::expect_used,
        reason = "window creation is binary startup; failure is unrecoverable and should abort loudly"
    )]
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_none() {
            // Create window. The window is shown immediately rather than created
            // hidden and revealed on a first-paint signal: WKWebView and WebView2
            // both refuse to lay out and paint their content -- and even to run JS
            // -- while the host window is hidden, so the Electron-style "create
            // hidden, show on ready-to-show" pattern is not available. The load
            // gap before the webview's first paint is instead covered by
            // presenting an opaque black frame (see the init_pending wait in
            // `tick_frame`) that matches the loading-screen background.
            // On Windows, disable WS_CLIPCHILDREN so the wry child HWND
            // doesn't occlude the wgpu DirectComposition swap chain.
            #[allow(unused_mut)]
            let mut attrs = Window::default_attributes()
                .with_title("Foldit")
                .with_inner_size(winit::dpi::LogicalSize::new(1280, 720));
            #[cfg(target_os = "windows")]
            {
                use winit::platform::windows::WindowAttributesExtWindows;
                attrs = attrs.with_clip_children(false);
            }
            let window = Arc::new(
                event_loop
                    .create_window(attrs)
                    .expect("Failed to create window"),
            );

            // CRITICAL ORDERING (macOS): create the wgpu Surface BEFORE attaching
            // the wry WebView. `wgpu::Instance::create_surface` calls `setLayer:`
            // on the contentView, replacing it with a CAMetalLayer. If the
            // WKWebView is already a subview at that point its backing layer
            // never recovers and only `toggleFullScreen` heals it. Matches the
            // canonical wry/examples/wgpu.rs ordering. The slow part of init
            // (structure load, Rosetta session) stays deferred to tick_frame so
            // the webview loading screen is visible during it.
            create_render_context(&mut self.app, window.clone());

            #[cfg(debug_assertions)]
            let build_webview = self.dev_server_available;
            #[cfg(not(debug_assertions))]
            let build_webview = true;

            if build_webview {
                let (webview, ipc_rx) = Self::create_webview(&window);
                self.webview = webview;
                self.ipc_rx = Some(ipc_rx);
            } else {
                log::info!("No dev server available, running without webview overlay");
            }

            // Defer startup until the webview loading screen is visible.
            // tick_frame arms the non-blocking startup machine
            // (`begin_startup`) once webview_ready fires (or after a timeout
            // if the webview is slow / absent); the per-frame tick advances it
            // from there.
            self.init_pending = true;
            self.init_deadline = Some(Instant::now() + std::time::Duration::from_secs(5));

            window.request_redraw();
            self.window = Some(window);
        }
    }

    #[allow(
        clippy::cast_possible_truncation,
        reason = "winit delivers cursor positions and scroll deltas as f64; the renderer consumes f32 screen coordinates, where this precision reduction is intended and harmless"
    )]
    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        // The offscreen webview is unmapped, so X routes it nothing; every
        // event it should see has to be synthesized from winit's.
        #[cfg(target_os = "linux")]
        if let Some(webview) = self.webview.as_mut() {
            webview.on_window_event(&event);
        }

        match event {
            WindowEvent::CloseRequested => {
                #[cfg(debug_assertions)]
                self.kill_dev_server();
                self.app.shutdown();
                event_loop.exit();
            }

            WindowEvent::Resized(newsize) => {
                log::info!(
                    "Resized event: {}x{} (last_render_size: {}x{})",
                    newsize.width,
                    newsize.height,
                    self.last_render_size.0,
                    self.last_render_size.1,
                );
                let size = (newsize.width, newsize.height);
                if size.0 > 0 && size.1 > 0 {
                    self.app.resize(size.0, size.1);
                    self.resize_webview(newsize);
                    self.last_render_size = size;
                }
            }

            WindowEvent::ScaleFactorChanged { .. } => {
                if let Some((actual, scale)) = self
                    .window
                    .as_ref()
                    .map(|window| (window.inner_size(), window.scale_factor()))
                {
                    self.app.set_surface_scale(scale);
                    #[cfg(target_os = "linux")]
                    if let Some(webview) = &self.webview {
                        webview.set_scale_factor(scale);
                    }
                    let size = (actual.width, actual.height);
                    if size.0 > 0 && size.1 > 0 {
                        self.app.resize(size.0, size.1);
                        self.resize_webview(actual);
                        self.last_render_size = size;
                    }
                }
            }

            WindowEvent::KeyboardInput { event, .. }
                if !self.webview_ready && event.state == ElementState::Pressed =>
            {
                if let PhysicalKey::Code(key) = event.physical_key {
                    self.app.handle_keybinding(&format!("{key:?}"));
                }
            }

            WindowEvent::RedrawRequested => {
                self.tick_frame();
            }

            WindowEvent::MouseInput { button, state, .. } if !self.webview_ready => {
                let pressed = state == ElementState::Pressed;
                let viso_button = match button {
                    winit::event::MouseButton::Left => viso::MouseButton::Left,
                    winit::event::MouseButton::Right => viso::MouseButton::Right,
                    winit::event::MouseButton::Middle => viso::MouseButton::Middle,
                    _ => return,
                };
                self.app.handle_native_mouse_input(viso_button, pressed);
            }

            WindowEvent::CursorMoved { position, .. } if !self.webview_ready => {
                self.app
                    .handle_native_cursor_moved(position.x as f32, position.y as f32);
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }

            WindowEvent::MouseWheel { delta, .. } if !self.webview_ready => {
                let scroll_delta = match delta {
                    winit::event::MouseScrollDelta::LineDelta(_, y) => y,
                    winit::event::MouseScrollDelta::PixelDelta(pos) => pos.y as f32 * 0.01,
                };
                self.app.handle_native_mouse_wheel(scroll_delta);
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }

            WindowEvent::ModifiersChanged(modifiers) => {
                self.app
                    .handle_native_modifiers(modifiers.state().shift_key());
            }

            _ => (),
        }
    }

    /// On Windows, `WM_PAINT` (which winit maps to `RedrawRequested`) is a
    /// low-priority message that can be starved when the `WebView2` child window
    /// keeps the message queue busy. The `request_redraw()` →
    /// `RedrawRequested` chain then stalls, freezing the frame loop until an
    /// external event (resize, move) forces a repaint. Requesting a redraw
    /// from `about_to_wait` — called on every event-loop iteration under
    /// `ControlFlow::Poll` — guarantees continuous frame generation regardless
    /// of Windows message priority.
    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }
}

#[cfg(debug_assertions)]
impl Drop for AppRunner {
    fn drop(&mut self) {
        self.kill_dev_server();
    }
}

/// Run the application event loop. This function never returns.
#[allow(
    clippy::expect_used,
    reason = "event-loop setup is binary startup; failure is unrecoverable and should abort loudly"
)]
pub fn run(app: App, log_buffer: crate::tee_logger::LogBuffer) -> ! {
    let mut runner = AppRunner::new(app, log_buffer);

    // In debug, spawn dev server and wait for it before opening the window.
    #[cfg(debug_assertions)]
    runner.ensure_dev_server();

    // GTK must be initialized on the main thread before any WebKitGTK view is
    // built. winit is pinned to X11 (via Xwayland when the session is Wayland)
    // to match the `GDK_BACKEND=x11` pin in `main`, keeping both toolkits on one
    // display connection.
    #[cfg(target_os = "linux")]
    let event_loop = {
        use winit::platform::x11::EventLoopBuilderExtX11;
        gtk::init().expect("Failed to initialize GTK");
        EventLoop::builder()
            .with_x11()
            .build()
            .expect("Failed to create event loop")
    };
    #[cfg(not(target_os = "linux"))]
    let event_loop = EventLoop::new().expect("Failed to create event loop");
    event_loop.set_control_flow(ControlFlow::Poll);
    event_loop.run_app(&mut runner).expect("Event loop error");
    // Explicit drop before process::exit -- otherwise the runner's
    // Drop (which kills the dev server in debug builds) is skipped
    // and pnpm/vite leak as orphans on a clean shutdown.
    drop(runner);
    std::process::exit(0);
}

/// Build a wgpu `RenderContext` against a winit window, construct the
/// `VisoEngine`, apply desktop-only tweaks (render scale based on DPI), and
/// hand the engine to `App`. The session drives the view preset on every
/// structure-load (it is the source of truth for view state), so no preset
/// is applied to the engine here.
///
/// Must run BEFORE the wry `WebView` is attached as a child of the window —
/// on macOS, `wgpu::Instance::create_surface` calls `setLayer:` on the
/// contentView, replacing it with a `CAMetalLayer`; if the `WKWebView` is
/// already a subview at that point its backing layer never recovers.
/// (Apple Forums 124688, wry#1335.) Matches wry/examples/wgpu.rs ordering.
fn create_render_context(app: &mut foldit_core::App, window: Arc<Window>) {
    let size = window.inner_size();
    let scale = window.scale_factor();
    log::info!(
        "create_render_context: inner_size={}x{}, scale_factor={}",
        size.width,
        size.height,
        scale
    );

    let render_scale = if scale < 2.0 { 2 } else { 1 };
    pollster::block_on(app.init_desktop_gpu(window, (size.width, size.height), render_scale));
}
