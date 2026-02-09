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
}

impl AppRunner {
    fn new(app: App, frontend: foldit_frontend::FrontendState) -> Self {
        Self {
            app,
            window: None,
            webview: None,
            frontend,
            ipc_rx: None,
            webview_ready: false,
            last_frame: Instant::now(),
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
                    log::info!("Webview ready, pushing full state");
                    self.webview_ready = true;
                    self.frontend.mark_all_dirty();
                    self.push_full_state_to_webview();
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
                    let script =
                        format!("if(window.__onInitialState)window.__onInitialState({})", json);
                    let _ = webview.evaluate_script(&script);
                }
                Err(e) => log::warn!("Failed to serialize initial state: {}", e),
            }
        }
    }

    /// Push dirty FrontendState sections to the webview.
    fn push_dirty_state_to_webview(&mut self) {
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

    /// Resize the webview to match a new window size.
    fn resize_webview(&self, new_size: winit::dpi::PhysicalSize<u32>) {
        if let (Some(ref webview), Some(ref window)) = (&self.webview, &self.window) {
            use wry::dpi::{LogicalPosition, LogicalSize};
            let scale = window.scale_factor();
            let logical_w = new_size.width as f64 / scale;
            let logical_h = new_size.height as f64 / scale;
            let _ = webview.set_bounds(wry::Rect {
                position: LogicalPosition::new(0.0, 0.0).into(),
                size: LogicalSize::new(logical_w, logical_h).into(),
            });
        }
    }

    /// Check if the frontend dev server is reachable at localhost:5173.
    /// Tries both IPv6 (::1) and IPv4 (127.0.0.1) since Vite defaults to IPv6 on macOS.
    fn frontend_server_available() -> bool {
        use std::net::TcpStream;
        use std::time::Duration;
        let timeout = Duration::from_millis(200);
        TcpStream::connect_timeout(&"[::1]:5173".parse().unwrap(), timeout).is_ok()
            || TcpStream::connect_timeout(&"127.0.0.1:5173".parse().unwrap(), timeout).is_ok()
    }

    /// Create the wry webview as a child of the winit window.
    fn create_webview(window: &Arc<Window>) -> (Option<wry::WebView>, std::sync::mpsc::Receiver<IpcMessage>) {
        let (ipc_tx, ipc_rx) = std::sync::mpsc::channel::<IpcMessage>();

        let webview = {
            use wry::dpi::{LogicalPosition, LogicalSize};
            let inner = window.inner_size();
            let scale = window.scale_factor();
            let logical_w = inner.width as f64 / scale;
            let logical_h = inner.height as f64 / scale;

            let builder = wry::WebViewBuilder::new()
                .with_transparent(true)
                .with_devtools(true)
                .with_initialization_script("window.isWebview = true;")
                .with_url("http://localhost:5173")
                .with_ipc_handler(move |req| {
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
                })
                .with_bounds(wry::Rect {
                    position: LogicalPosition::new(0.0, 0.0).into(),
                    size: LogicalSize::new(logical_w, logical_h).into(),
                });

            match builder.build_as_child(window) {
                Ok(wv) => {
                    log::info!("wry webview created successfully");
                    // #[cfg(debug_assertions)]
                    // wv.open_devtools();
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

        // Process IPC messages from webview
        self.process_ipc_messages();

        // Process ML updates
        self.app.process_ml_updates();

        // Process Rosetta updates
        self.app.process_rosetta_updates();

        // Sync engine with scene if dirty
        self.app.sync_engine_with_scene();

        // Update visual effect
        let _intensity = self.app.tick_effects(dt.as_secs_f32());

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
            // Create window
            let window = Arc::new(
                event_loop
                    .create_window(
                        Window::default_attributes()
                            .with_title("Foldit ML Render")
                            .with_inner_size(winit::dpi::LogicalSize::new(1280, 720)),
                    )
                    .expect("Failed to create window"),
            );

            // Initialize App domain logic (engine, structure, ML, Rosetta)
            self.app.initialize_with_window(window.clone());

            // Mark frontend dirty so the first push sends everything
            self.frontend.set_puzzle_loaded(true);
            self.frontend.mark_all_dirty();

            // Only create webview if the frontend dev server is likely running.
            // Check by attempting a quick connect to localhost:5173.
            if Self::frontend_server_available() {
                let (webview, ipc_rx) = Self::create_webview(&window);
                self.webview = webview;
                self.ipc_rx = Some(ipc_rx);
            } else {
                log::info!("No frontend dev server detected at localhost:5173, running without webview overlay");
            }

            window.request_redraw();
            self.window = Some(window);
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => {
                self.app.shutdown();
                event_loop.exit();
            }

            WindowEvent::Resized(newsize) => {
                log::info!("WindowEvent::Resized: {}x{}", newsize.width, newsize.height);
                self.app.resize(newsize.width, newsize.height);
                self.resize_webview(newsize);
            }

            WindowEvent::ScaleFactorChanged { .. } => {
                if let Some(window) = &self.window {
                    let newsize = window.inner_size();
                    self.app.set_surface_scale(window.scale_factor());
                    self.app.resize(newsize.width, newsize.height);
                    self.resize_webview(newsize);
                }
            }

            WindowEvent::KeyboardInput { event, .. }
                if !self.webview_ready && event.state == ElementState::Pressed =>
            {
                if let PhysicalKey::Code(key) = event.physical_key {
                    self.app.handle_key(key);
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

/// Run the application event loop. This function never returns.
pub(crate) fn run(app: App, frontend: foldit_frontend::FrontendState) -> ! {
    let mut runner = AppRunner::new(app, frontend);
    let event_loop = EventLoop::new().expect("Failed to create event loop");
    event_loop.set_control_flow(ControlFlow::Poll);
    event_loop.run_app(&mut runner).expect("Event loop error");
    std::process::exit(0);
}
