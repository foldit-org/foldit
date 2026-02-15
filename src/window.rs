//! Window management module: winit event loop, wry webview, frame timing, IPC dispatch.
//!
//! `AppRunner` owns the window-layer state and holds `App` by value.
//! It implements `ApplicationHandler` and delegates domain logic to `App` via method calls.

use crate::App;
use foldit_frontend::DirtyFlags;
use std::sync::Arc;
use std::time::Instant;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::PhysicalKey;
use winit::window::{Window, WindowId};

/// Messages from JS (via wry ipc_handler) to Rust
#[derive(Debug)]
pub(crate) enum IpcMessage {
    Ready,
    ViewportInput(foldit_frontend::ViewportInput),
    TriggerAction(foldit_frontend::ActionId),
    ParameterizedAction(foldit_frontend::ParameterizedAction),
}

/// Window-layer state that wraps `App` and implements `ApplicationHandler`.
pub(crate) struct AppRunner {
    app: App,
    window: Option<Arc<Window>>,
    webview: Option<wry::WebView>,
    frontend: foldit_frontend::FrontendState,
    ipc_rx: Option<std::sync::mpsc::Receiver<IpcMessage>>,
    webview_ready: bool,
    last_frame: Instant,
    /// Last applied render size, to avoid redundant resizes
    last_render_size: (u32, u32),
    /// Engine initialization is deferred until the webview loading screen is visible
    init_pending: bool,
    /// Timeout for webview readiness — initialize anyway if webview takes too long
    init_deadline: Option<Instant>,
    /// Shared log buffer from tee_logger (drained each frame into frontend state)
    log_buffer: crate::tee_logger::LogBuffer,
    #[cfg(debug_assertions)]
    dev_server: Option<std::process::Child>,
    #[cfg(debug_assertions)]
    dev_server_available: bool,
}

impl AppRunner {
    fn new(app: App, frontend: foldit_frontend::FrontendState, log_buffer: crate::tee_logger::LogBuffer) -> Self {
        Self {
            app,
            window: None,
            webview: None,
            frontend,
            ipc_rx: None,
            webview_ready: false,
            last_frame: Instant::now(),
            last_render_size: (0, 0),
            init_pending: false,
            init_deadline: None,
            log_buffer,
            #[cfg(debug_assertions)]
            dev_server: None,
            #[cfg(debug_assertions)]
            dev_server_available: false,
        }
    }

    /// Drain IPC messages from the webview and dispatch them.
    fn process_ipc_messages(&mut self) {
        let rx = match &self.ipc_rx {
            Some(rx) => rx,
            None => return,
        };
        let messages: Vec<IpcMessage> = rx.try_iter().collect();
        for msg in messages {
            match msg {
                IpcMessage::Ready => {
                    log::info!("Webview ready");
                    self.webview_ready = true;
                    self.frontend.mark_all_dirty();
                    // Only push full state if engine init is already done.
                    // If init is still pending, the init block will push state
                    // once it completes (with puzzle_loaded: true).
                    // This avoids a race where __onInitialState sends puzzle_loaded:false
                    // and a subsequent __onStateUpdate sends puzzle_loaded:true,
                    // but the Promise .then() microtask overwrites it back to false.
                    if !self.init_pending {
                        self.push_full_state_to_webview();
                    }
                }
                IpcMessage::ViewportInput(input) => self.app.handle_viewport_input(input),
                IpcMessage::TriggerAction(action) => self.app.handle_trigger_action(action),
                IpcMessage::ParameterizedAction(action) => {
                    self.app.handle_parameterized_action(action)
                }
            }
        }
    }

    /// Push the full FrontendState to the webview as initial state.
    fn push_full_state_to_webview(&self) {
        if let Some(ref webview) = self.webview {
            match serde_json::to_string(&self.frontend) {
                Ok(json) => {
                    let script = format!(
                        "if(window.__onInitialState)window.__onInitialState({})",
                        json
                    );
                    let _ = webview.evaluate_script(&script);
                }
                Err(e) => log::warn!("Failed to serialize initial state: {}", e),
            }
        }
    }

    /// Push dirty FrontendState sections to the webview.
    fn push_dirty_state_to_webview(&mut self) {
        // Drain log buffer into frontend state
        if let Ok(buf) = self.log_buffer.lock() {
            if !buf.is_empty() {
                let log_text: String = buf.iter().cloned().collect::<Vec<_>>().join("\n");
                self.frontend.set_log(log_text);
            }
        }

        // Transfer App domain state into FrontendState
        self.app.populate_frontend(&mut self.frontend);

        // Emit dirty sections to webview
        let dirty = self.frontend.take_dirty();
        if dirty.is_empty() || !self.webview_ready {
            return;
        }

        let mut update = serde_json::Map::new();

        if dirty.contains(DirtyFlags::SCORE) {
            update.insert(
                "score".into(),
                serde_json::to_value(&self.frontend.score).unwrap(),
            );
        }
        if dirty.contains(DirtyFlags::SELECTION) {
            update.insert(
                "selection".into(),
                serde_json::to_value(&self.frontend.selection).unwrap(),
            );
        }
        if dirty.contains(DirtyFlags::VIEW) {
            update.insert(
                "view".into(),
                serde_json::to_value(&self.frontend.view).unwrap(),
            );
        }
        if dirty.contains(DirtyFlags::PANELS) {
            update.insert(
                "panels".into(),
                serde_json::to_value(&self.frontend.panels).unwrap(),
            );
        }
        if dirty.contains(DirtyFlags::UI) {
            update.insert(
                "ui".into(),
                serde_json::to_value(&self.frontend.ui).unwrap(),
            );
        }
        if dirty.contains(DirtyFlags::ACTIONS) {
            update.insert(
                "actions".into(),
                serde_json::to_value(&self.frontend.actions).unwrap(),
            );
        }
        if dirty.contains(DirtyFlags::LOADING) {
            update.insert(
                "loading".into(),
                serde_json::to_value(&self.frontend.loading).unwrap(),
            );
        }

        if let Some(ref webview) = self.webview {
            let payload = serde_json::Value::Object(update);
            let script = format!(
                "if(window.__onStateUpdate)window.__onStateUpdate({})",
                payload
            );
            let _ = webview.evaluate_script(&script);
        }
    }

    /// Resize the webview to match a new window size (physical pixels).
    fn resize_webview(&self, new_size: winit::dpi::PhysicalSize<u32>) {
        if let Some(ref webview) = &self.webview {
            use wry::dpi::{PhysicalPosition, PhysicalSize};
            let _ = webview.set_bounds(wry::Rect {
                position: PhysicalPosition::new(0, 0).into(),
                size: PhysicalSize::new(new_size.width, new_size.height).into(),
            });
        }
    }

    /// Check if the frontend dev server is reachable at localhost:5173.
    #[cfg(debug_assertions)]
    fn frontend_server_available() -> bool {
        use std::net::TcpStream;
        use std::time::Duration;
        let timeout = Duration::from_millis(200);
        TcpStream::connect_timeout(&"[::1]:5173".parse().unwrap(), timeout).is_ok()
            || TcpStream::connect_timeout(&"127.0.0.1:5173".parse().unwrap(), timeout).is_ok()
    }

    /// Spawn the Vite dev server and block until it's ready (called before event loop).
    #[cfg(debug_assertions)]
    fn ensure_dev_server(&mut self) {
        if Self::frontend_server_available() {
            log::info!("Dev server already running at localhost:5173");
            self.dev_server_available = true;
            return;
        }

        use std::process::{Command, Stdio};

        let frontend_dir = std::path::Path::new("crates/foldit-frontend/js");
        if !frontend_dir.exists() {
            log::warn!(
                "Frontend directory not found at {:?}, skipping dev server",
                frontend_dir
            );
            return;
        }

        log::info!("Spawning Vite dev server...");

        #[cfg(windows)]
        let result = Command::new("pnpm.cmd")
            .arg("dev")
            .current_dir(frontend_dir)
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn();

        #[cfg(unix)]
        let result = Command::new("pnpm")
            .arg("dev")
            .current_dir(frontend_dir)
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn();

        match result {
            Ok(child) => {
                log::info!("Vite dev server spawned (pid: {})", child.id());
                self.dev_server = Some(child);
            }
            Err(e) => {
                log::error!("Failed to spawn Vite dev server: {}", e);
                return;
            }
        }

        use std::thread;
        use std::time::Duration;
        for i in 0..75 {
            if Self::frontend_server_available() {
                log::info!("Dev server ready after ~{}ms", i * 200);
                self.dev_server_available = true;
                return;
            }
            thread::sleep(Duration::from_millis(200));
        }
        log::error!("Dev server did not become available within 15s");
    }

    /// Kill the dev server child process if running.
    #[cfg(debug_assertions)]
    fn kill_dev_server(&mut self) {
        if let Some(ref mut child) = self.dev_server {
            let pid = child.id();
            log::info!("Killing dev server (pid: {})...", pid);
            #[cfg(windows)]
            {
                let _ = std::process::Command::new("taskkill")
                    .args(["/F", "/T", "/PID", &pid.to_string()])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
            }
            #[cfg(not(windows))]
            {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
        self.dev_server = None;
    }

    /// Create the wry webview for release mode, serving assets via custom protocol.
    #[cfg(not(debug_assertions))]
    fn create_webview_release(
        window: &Arc<Window>,
    ) -> (Option<wry::WebView>, std::sync::mpsc::Receiver<IpcMessage>) {
        let (ipc_tx, ipc_rx) = std::sync::mpsc::channel::<IpcMessage>();

        let webview = {
            use wry::dpi::{PhysicalPosition, PhysicalSize};
            let inner = window.inner_size();

            let builder = wry::WebViewBuilder::new()
                .with_transparent(true)
                .with_initialization_script(Self::INIT_SCRIPT)
                .with_custom_protocol("foldit".into(), |_webview_id, request| {
                    use std::borrow::Cow;

                    let path = request.uri().path();
                    let path = if path == "/" || path.is_empty() {
                        "index.html"
                    } else {
                        path.trim_start_matches('/')
                    };

                    let asset_path = std::path::Path::new("assets/gui").join(path);
                    match std::fs::read(&asset_path) {
                        Ok(content) => {
                            let mime = Self::mime_from_ext(
                                asset_path.extension().and_then(|e| e.to_str()).unwrap_or(""),
                            );
                            wry::http::Response::builder()
                                .status(200)
                                .header("Content-Type", mime)
                                .header("Access-Control-Allow-Origin", "*")
                                .body(Cow::Owned(content))
                                .unwrap()
                        }
                        Err(_) => wry::http::Response::builder()
                            .status(404)
                            .body(Cow::Borrowed(b"Not Found" as &[u8]))
                            .unwrap(),
                    }
                })
                .with_url({
                    #[cfg(windows)]
                    { "http://foldit.localhost/index.html" }
                    #[cfg(not(windows))]
                    { "foldit://localhost/index.html" }
                })
                .with_ipc_handler({
                    let ipc_tx = ipc_tx.clone();
                    move |req| Self::handle_ipc(&ipc_tx, req)
                })
                .with_bounds(wry::Rect {
                    position: PhysicalPosition::new(0, 0).into(),
                    size: PhysicalSize::new(inner.width, inner.height).into(),
                });

            match builder.build_as_child(window) {
                Ok(wv) => {
                    log::info!("wry webview created (release, custom protocol)");
                    Some(wv)
                }
                Err(e) => {
                    log::error!("Failed to create wry webview: {}", e);
                    None
                }
            }
        };

        (webview, ipc_rx)
    }

    /// Infer MIME type from file extension.
    #[cfg(not(debug_assertions))]
    fn mime_from_ext(ext: &str) -> &'static str {
        match ext {
            "html" => "text/html",
            "js" | "mjs" => "application/javascript",
            "css" => "text/css",
            "wasm" => "application/wasm",
            "json" => "application/json",
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "svg" => "image/svg+xml",
            "ico" => "image/x-icon",
            "woff" => "font/woff",
            "woff2" => "font/woff2",
            "ttf" => "font/ttf",
            _ => "application/octet-stream",
        }
    }

    /// Shared IPC initialization script injected into the webview.
    const INIT_SCRIPT: &str = r#"
        window.isWebview = true;
        (function() {
            const orig = { log: console.log, warn: console.warn, error: console.error };
            function stringify(a) {
                if (a instanceof Error) return a.message + '\n' + a.stack;
                if (typeof a === 'string') return a;
                try { return JSON.stringify(a); } catch { return String(a); }
            }
            function forward(level, args) {
                try {
                    const msg = Array.from(args).map(stringify).join(' ');
                    window.ipc.postMessage(JSON.stringify({ cmd: 'console', level, msg }));
                } catch(e) {}
            }
            console.log = function() { forward('log', arguments); orig.log.apply(console, arguments); };
            console.warn = function() { forward('warn', arguments); orig.warn.apply(console, arguments); };
            console.error = function() { forward('error', arguments); orig.error.apply(console, arguments); };
            window.addEventListener('error', function(e) {
                forward('error', ['[Uncaught] ' + e.message + ' at ' + e.filename + ':' + e.lineno]);
            });
            window.addEventListener('unhandledrejection', function(e) {
                forward('error', ['[UnhandledRejection] ' + e.reason]);
            });
        })();
    "#;

    /// Shared IPC handler for webview messages.
    fn handle_ipc(ipc_tx: &std::sync::mpsc::Sender<IpcMessage>, req: wry::http::Request<String>) {
        let body = req.body();
        match serde_json::from_str::<serde_json::Value>(body) {
            Ok(val) => {
                let cmd = val.get("cmd").and_then(|v| v.as_str()).unwrap_or("");
                let msg = match cmd {
                    "ready" => Some(IpcMessage::Ready),
                    "viewport_input" => val
                        .get("data")
                        .and_then(|d| serde_json::from_value(d.clone()).ok())
                        .map(IpcMessage::ViewportInput),
                    "trigger_action" => val
                        .get("data")
                        .and_then(|d| serde_json::from_value(d.clone()).ok())
                        .map(IpcMessage::TriggerAction),
                    "parameterized_action" => val
                        .get("data")
                        .and_then(|d| serde_json::from_value(d.clone()).ok())
                        .map(IpcMessage::ParameterizedAction),
                    "console" => {
                        let level =
                            val.get("level").and_then(|v| v.as_str()).unwrap_or("log");
                        let msg = val.get("msg").and_then(|v| v.as_str()).unwrap_or("");
                        match level {
                            "error" => log::error!("[JS] {}", msg),
                            "warn" => log::warn!("[JS] {}", msg),
                            _ => log::info!("[JS] {}", msg),
                        }
                        None
                    }
                    _ => {
                        log::debug!("Unknown IPC command: {}", cmd);
                        None
                    }
                };
                if let Some(msg) = msg {
                    let _ = ipc_tx.send(msg);
                }
            }
            Err(e) => log::warn!("Failed to parse IPC message: {}", e),
        }
    }

    /// Create the wry webview as a child of the winit window (debug: connects to dev server).
    #[cfg(debug_assertions)]
    fn create_webview(
        window: &Arc<Window>,
    ) -> (Option<wry::WebView>, std::sync::mpsc::Receiver<IpcMessage>) {
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
                    let ipc_tx = ipc_tx.clone();
                    move |req| Self::handle_ipc(&ipc_tx, req)
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
                    log::error!("Failed to create wry webview: {}", e);
                    None
                }
            }
        };

        (webview, ipc_rx)
    }

    /// Per-frame update: process events, update state, render, push to webview.
    fn tick_frame(&mut self) {
        let now = Instant::now();
        let dt = now.duration_since(self.last_frame);
        self.last_frame = now;

        // Process IPC messages (needed to detect webview_ready during init)
        self.process_ipc_messages();

        // Deferred initialization: wait for the webview to show the loading screen
        // before blocking the main thread with engine/structure creation.
        if self.init_pending {
            let should_init = self.webview_ready
                || self.webview.is_none()
                || self.init_deadline.is_some_and(|d| now > d);

            if should_init {
                log::info!("Deferred init starting (webview_ready={})", self.webview_ready);
                self.init_pending = false;
                self.init_deadline = None;
                let window = self.window.as_ref().unwrap().clone();
                self.app.initialize_with_window(window);
                // Engine + puzzle loaded — ready to show the game view
                self.frontend.set_puzzle_loaded(true);
                self.frontend.set_score_title(self.app.structure_title());
                self.frontend.mark_all_dirty();
                // Push full state via __onInitialState now (the Ready handler
                // deferred this push so the JS only ever sees puzzle_loaded:true).
                if self.webview_ready {
                    self.push_full_state_to_webview();
                }
                log::info!("Deferred init complete, puzzle_loaded={}", self.frontend.loading().puzzle_loaded);
                // Fall through to render the first frame
            } else {
                // Webview still loading — keep pumping frames so it can paint
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
                return;
            }
        }

        // Ensure render surface always matches actual window size.
        // We do this every frame because Resized events can be unreliable
        // on Windows (stale WM_SIZE, timing issues with child windows).
        if let Some(ref window) = self.window {
            let ws = window.inner_size();
            let size = (ws.width, ws.height);
            if size != self.last_render_size && size.0 > 0 && size.1 > 0 {
                log::info!("tick_frame resize: {}x{} (was {}x{})",
                    size.0, size.1, self.last_render_size.0, self.last_render_size.1);
                self.app.resize(size.0, size.1);
                self.resize_webview(ws);
                self.last_render_size = size;
            }
        }

        // Process all backend updates (Rosetta + ML) via triple buffers
        self.app.apply_backend_updates();

        // Sync engine with scene if dirty (non-blocking: submits to background thread)
        self.app.sync_engine();

        // Apply any completed scene from background thread (GPU uploads only, <1ms)
        self.app.apply_pending_scene();

        // Update camera animation
        self.app.update_camera_animation(dt.as_secs_f32());

        // Update frame visuals (bands + pull tracking)
        self.app.update_frame_visuals();

        // Render
        self.app.render();

        // Push dirty state to webview
        self.push_dirty_state_to_webview();

        // Request next frame
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }
}

impl ApplicationHandler for AppRunner {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_none() {
            // Create window.
            // On Windows, disable WS_CLIPCHILDREN so the wry child HWND
            // doesn't occlude the wgpu DirectComposition swap chain.
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

            // Create webview first so the loading screen is visible before heavy init
            #[cfg(debug_assertions)]
            {
                if self.dev_server_available {
                    let (webview, ipc_rx) = Self::create_webview(&window);
                    self.webview = webview;
                    self.ipc_rx = Some(ipc_rx);
                } else {
                    log::info!("No dev server available, running without webview overlay");
                }
            }

            #[cfg(not(debug_assertions))]
            {
                let (webview, ipc_rx) = Self::create_webview_release(&window);
                self.webview = webview;
                self.ipc_rx = Some(ipc_rx);
            }

            // Defer engine initialization until the webview loading screen is up.
            // tick_frame will call initialize_with_window once webview_ready fires
            // (or after a timeout if the webview is slow / absent).
            self.init_pending = true;
            self.init_deadline = Some(Instant::now() + std::time::Duration::from_secs(5));

            window.request_redraw();
            self.window = Some(window);
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
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
                    newsize.width, newsize.height,
                    self.last_render_size.0, self.last_render_size.1,
                );
                let size = (newsize.width, newsize.height);
                if size.0 > 0 && size.1 > 0 {
                    self.app.resize(size.0, size.1);
                    self.resize_webview(newsize);
                    self.last_render_size = size;
                }
            }

            WindowEvent::ScaleFactorChanged { .. } => {
                if let Some(window) = &self.window {
                    let actual = window.inner_size();
                    self.app.set_surface_scale(window.scale_factor());
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
                    self.app.handle_keybinding(key);
                }
            }

            WindowEvent::RedrawRequested => {
                self.tick_frame();
            }

            WindowEvent::MouseInput { button, state, .. } if !self.webview_ready => {
                let pressed = state == ElementState::Pressed;
                self.app.handle_native_mouse_input(button, pressed);
            }

            WindowEvent::CursorMoved { position, .. } if !self.webview_ready => {
                self.app
                    .handle_native_cursor_moved(position.x as f32, position.y as f32);
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }

            WindowEvent::MouseWheel { delta, .. } if !self.webview_ready => {
                self.app.handle_native_mouse_wheel(delta);
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }

            WindowEvent::ModifiersChanged(modifiers) => {
                self.app.handle_native_modifiers(modifiers.state());
            }

            _ => (),
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
pub(crate) fn run(app: App, frontend: foldit_frontend::FrontendState, log_buffer: crate::tee_logger::LogBuffer) -> ! {
    let mut runner = AppRunner::new(app, frontend, log_buffer);

    // In debug, spawn dev server and wait for it before opening the window.
    #[cfg(debug_assertions)]
    runner.ensure_dev_server();

    let event_loop = EventLoop::new().expect("Failed to create event loop");
    event_loop.set_control_flow(ControlFlow::Poll);
    event_loop.run_app(&mut runner).expect("Event loop error");
    std::process::exit(0);
}
