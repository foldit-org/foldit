//! Window management module: winit event loop, wry webview, frame timing, IPC dispatch.
//!
//! `AppRunner` owns the window-layer state and holds `App` by value.
//! It implements `ApplicationHandler` and delegates domain logic to `App` via method calls.

use foldit_core::App;
use foldit_gui::IpcMessage;
use std::sync::Arc;
use std::time::Instant;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::PhysicalKey;
use winit::window::{Window, WindowId};

/// Window-layer state that wraps `App` and implements `ApplicationHandler`.
/// `App` owns the [`foldit_gui::FrontendState`] mirror and the
/// Loading → `InPuzzle` state-machine now (RX13); the runner is purely
/// the wry/winit + dev-server shell.
pub struct AppRunner {
    app: App,
    window: Option<Arc<Window>>,
    webview: Option<wry::WebView>,
    ipc_rx: Option<std::sync::mpsc::Receiver<IpcMessage>>,
    webview_ready: bool,
    last_frame: Instant,
    /// Last applied render size, to avoid redundant resizes
    last_render_size: (u32, u32),
    /// Structure load is deferred until the webview loading screen is visible
    init_pending: bool,
    /// Timeout for webview readiness — load anyway if webview takes too long
    init_deadline: Option<Instant>,
    /// Shared log buffer from `tee_logger` (drained each frame into frontend state)
    log_buffer: crate::tee_logger::LogBuffer,
    // `pub(crate)` so the dev-server methods in `webview_assets` can drive them.
    #[cfg(debug_assertions)]
    pub(crate) dev_server: Option<std::process::Child>,
    #[cfg(debug_assertions)]
    pub(crate) dev_server_available: bool,
}

impl AppRunner {
    fn new(app: App, log_buffer: crate::tee_logger::LogBuffer) -> Self {
        Self {
            app,
            window: None,
            webview: None,
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
        use foldit_gui::Dispatcher;
        let Some(rx) = &self.ipc_rx else {
            return;
        };
        let messages: Vec<IpcMessage> = rx.try_iter().collect();
        for msg in messages {
            match msg {
                IpcMessage::Ready => {
                    log::info!("Webview ready");
                    self.webview_ready = true;
                    // App owns the FrontendState mirror (RX13) — its
                    // `on_ready` impl marks every section dirty so the
                    // next push emits a full snapshot.
                    self.app.on_ready();
                }
                IpcMessage::ViewportInput(input) => self.app.on_viewport_input(input),
                IpcMessage::DispatchOp(op) => self.app.on_dispatch_op(op),
                IpcMessage::AppCommand(command) => self.app.on_app_command(command),
                IpcMessage::SetSelection { entries } => self.app.on_set_selection(entries),
                IpcMessage::Request { wish_id, kind, payload } => {
                    let result = self.app.handle_request(kind, payload);
                    self.send_response_to_webview(&wish_id, &result);
                }
            }
        }
    }

    /// Resolve or reject a JS-side pending request via window.__onResponse.
    fn send_response_to_webview(
        &self,
        wish_id: &str,
        result: &foldit_gui::RequestResult,
    ) {
        let Some(ref webview) = self.webview else {
            return;
        };
        let (ok, payload) = match result {
            Ok(v) => (true, v.clone()),
            Err(msg) => (false, serde_json::Value::String(msg.clone())),
        };
        // Serializing a `&str` to a JSON string is infallible, so the
        // unwrap cannot fire (the only error sources in `to_string` are
        // custom Serialize impls and non-string map keys).
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

    /// Ship any dirty sections of the App-owned `FrontendState` to the
    /// webview. `App::tick` already populated the frontend on this
    /// frame; the host only does the log-mirror handoff and the IPC
    /// transport.
    fn push_dirty_state_to_webview(&mut self) {
        // Drain log buffer into App-owned frontend state.
        if let Ok(buf) = self.log_buffer.lock() {
            if !buf.is_empty() {
                let log_text: String = buf.iter().cloned().collect::<Vec<_>>().join("\n");
                self.app.set_frontend_log(log_text);
            }
        }

        if !self.webview_ready {
            return;
        }
        let Some(bytes) = self.app.serialize_frontend_dirty() else {
            return;
        };
        let Ok(payload) = std::str::from_utf8(&bytes) else {
            return;
        };
        if let Some(ref webview) = self.webview {
            let script = format!(
                "if(window.__onStateUpdate)window.__onStateUpdate({payload})"
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
                    let ipc_tx = ipc_tx;
                    move |req| Self::handle_ipc(&ipc_tx, &req)
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

        // Process IPC messages (needed to detect webview_ready during init)
        self.process_ipc_messages();

        // Deferred structure load: wait for the webview to show the loading
        // screen before blocking the main thread with file I/O and Rosetta
        // session creation. The wgpu RenderContext is already initialized
        // (in `resumed()` before webview attachment) so the engine is alive
        // and rendering throughout this period.
        if self.init_pending {
            let should_init = self.webview_ready
                || self.webview.is_none()
                || self.init_deadline.is_some_and(|d| now > d);

            if should_init {
                log::info!("Deferred structure load starting (webview_ready={})", self.webview_ready);
                self.init_pending = false;
                self.init_deadline = None;
                // App-lifecycle warm-up: discover + warm plugins (spawn
                // workers, load backends, NO session) once, before the
                // first structure load creates the sessions. Runs on the
                // same deferred gate (webview-ready / timeout / no-webview)
                // so it never blocks the surface-creation path.
                self.app.warm_plugins();
                self.app.load_initial_structure();
                log::info!("Structure loaded, awaiting initial score");
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

        // App-owned drive loop: backend updates → `SessionUpdate` drain →
        // broadcaster + render projector → score poll → engine update +
        // visualization → state-machine → populate_frontend.
        self.app.tick(dt.as_secs_f32());

        // Render the engine surface.
        self.app.render();

        // Ship any dirty frontend bytes to the webview.
        self.push_dirty_state_to_webview();

        // Request next frame
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }
}

impl ApplicationHandler for AppRunner {
    #[allow(
        clippy::expect_used,
        reason = "window creation is binary startup; failure is unrecoverable and should abort loudly"
    )]
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_none() {
            // Create window.
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

            // Defer the slow part (structure load, Rosetta session) until the
            // webview loading screen is visible. tick_frame calls
            // load_initial_structure once webview_ready fires (or after a
            // timeout if the webview is slow / absent).
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
                self.app.handle_native_modifiers(modifiers.state().shift_key());
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
#[allow(
    clippy::expect_used,
    reason = "event-loop setup is binary startup; failure is unrecoverable and should abort loudly"
)]
pub fn run(
    app: App,
    log_buffer: crate::tee_logger::LogBuffer,
) -> ! {
    let mut runner = AppRunner::new(app, log_buffer);

    // In debug, spawn dev server and wait for it before opening the window.
    #[cfg(debug_assertions)]
    runner.ensure_dev_server();

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
/// `VisoEngine`, apply desktop-only tweaks (default view preset, render
/// scale based on DPI), and hand the engine to `App`.
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

    let context = match pollster::block_on(viso::RenderContext::new(window, (size.width, size.height))) {
        Ok(ctx) => ctx,
        Err(e) => {
            log::error!("Failed to initialize GPU render context: {e:?}");
            return;
        }
    };

    let mut engine = match viso::VisoEngine::new(context, viso::options::VisoOptions::default()) {
        Ok(e) => e,
        Err(e) => {
            log::error!("Failed to initialize engine: {e:?}");
            return;
        }
    };

    let presets_dir = std::path::Path::new("assets/view_presets");
    engine.load_preset("default", presets_dir);
    engine.set_render_scale(if scale < 2.0 { 2 } else { 1 });

    app.attach_engine(engine);
}
