//! Window management module: winit event loop, wry webview, frame timing, IPC dispatch.
//!
//! `AppRunner` owns the window-layer state and holds `App` by value.
//! It implements `ApplicationHandler` and delegates domain logic to `App` via method calls.

use foldit_core::App;
use foldit_gui::bridge;
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
/// Loading → InPuzzle state-machine now (RX13); the runner is purely
/// the wry/winit + dev-server shell.
pub(crate) struct AppRunner {
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
    /// Shared log buffer from tee_logger (drained each frame into frontend state)
    log_buffer: crate::tee_logger::LogBuffer,
    #[cfg(debug_assertions)]
    dev_server: Option<std::process::Child>,
    #[cfg(debug_assertions)]
    dev_server_available: bool,
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
                    // App owns the FrontendState mirror (RX13) — its
                    // `on_ready` impl marks every section dirty so the
                    // next push emits a full snapshot.
                    self.app.on_ready();
                }
                IpcMessage::ViewportInput(input) => self.app.on_viewport_input(input),
                IpcMessage::TriggerAction(action) => self.app.on_trigger_action(action),
                IpcMessage::DispatchOp(op) => self.app.on_dispatch_op(op),
                IpcMessage::ParameterizedAction(action) => self.app.on_parameterized_action(action),
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
        let script = format!(
            "if(window.__onResponse)window.__onResponse({},{},{})",
            serde_json::to_string(wish_id).unwrap(),
            ok,
            payload,
        );
        let _ = webview.evaluate_script(&script);
    }

    /// Ship any dirty sections of the App-owned FrontendState to the
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

    /// True if *something* is listening on TCP 5173. Tries IPv6 and
    /// IPv4 because vite's default `localhost` binding is IPv6-only on
    /// macOS, IPv4-only on some Linux distros, and dual-stack
    /// elsewhere -- a single-family probe loses to that.
    #[cfg(debug_assertions)]
    fn dev_server_port_bound() -> bool {
        use std::net::TcpStream;
        use std::time::Duration;
        let timeout = Duration::from_millis(200);
        TcpStream::connect_timeout(&"[::1]:5173".parse().unwrap(), timeout)
            .is_ok()
            || TcpStream::connect_timeout(
                &"127.0.0.1:5173".parse().unwrap(),
                timeout,
            )
            .is_ok()
    }

    /// SIGKILL whatever is bound to port 5173 (and its process group). Used to
    /// evict an orphan vite from a crashed previous run before respawning.
    #[cfg(all(debug_assertions, unix))]
    fn kill_orphan_on_port_5173() {
        use std::process::Command;
        // lsof -ti :5173 → PIDs of every process with port 5173 open.
        let lsof_out = match Command::new("lsof").args(["-ti", ":5173"]).output() {
            Ok(o) if o.status.success() => o.stdout,
            _ => return,
        };
        let pids: Vec<i32> = String::from_utf8_lossy(&lsof_out)
            .lines()
            .filter_map(|s| s.trim().parse::<i32>().ok())
            .collect();
        for pid in pids {
            // Kill the whole process group (pnpm + node/vite + esbuild) by
            // looking up its pgid; fall back to pid-only kill if that fails.
            let pgid = Command::new("ps")
                .args(["-o", "pgid=", "-p", &pid.to_string()])
                .output()
                .ok()
                .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse::<i32>().ok());
            if let Some(pgid) = pgid {
                log::warn!("Killing orphan dev server process group pgid={}", pgid);
                let _ = Command::new("kill")
                    .args(["-KILL", "--", &format!("-{}", pgid)])
                    .status();
            } else {
                log::warn!("Killing orphan dev server pid={}", pid);
                let _ = Command::new("kill").args(["-KILL", &pid.to_string()]).status();
            }
        }
    }

    #[cfg(all(debug_assertions, windows))]
    fn kill_orphan_on_port_5173() {
        use std::process::Command;
        // PowerShell: find PIDs owning TCP 5173 and force-kill each tree.
        let ps_cmd = "Get-NetTCPConnection -LocalPort 5173 -State Listen -ErrorAction SilentlyContinue | Select-Object -ExpandProperty OwningProcess";
        let out = match Command::new("powershell").args(["-NoProfile", "-Command", ps_cmd]).output() {
            Ok(o) if o.status.success() => o.stdout,
            _ => return,
        };
        for pid in String::from_utf8_lossy(&out).lines().filter_map(|s| s.trim().parse::<u32>().ok()) {
            log::warn!("Killing orphan dev server pid={}", pid);
            let _ = Command::new("taskkill")
                .args(["/F", "/T", "/PID", &pid.to_string()])
                .status();
        }
    }

    /// Resolve the absolute path to `crates/foldit-gui/js`. Tries
    /// (in order):
    ///
    ///   1. `FOLDIT_FRONTEND_DIR` env override.
    ///   2. Walk up from `CARGO_MANIFEST_DIR` (= this crate's dir at
    ///      compile time) looking for `crates/foldit-gui/js`.
    ///   3. Walk up from `current_exe()` looking for the same.
    ///
    /// Returns `None` if none resolve. Used by the dev server spawn
    /// so we never feed `pnpm` a relative path that depends on
    /// whatever cwd `cargo run` happened to inherit.
    #[cfg(debug_assertions)]
    fn locate_frontend_dir() -> Option<std::path::PathBuf> {
        if let Some(env) = std::env::var_os("FOLDIT_FRONTEND_DIR") {
            let p = std::path::PathBuf::from(env);
            if p.is_dir() {
                return Some(p);
            }
        }
        // CARGO_MANIFEST_DIR is the compile-time dir of foldit-desktop,
        // i.e. .../foldit/crates/foldit-desktop. Two levels up is
        // workspace root.
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let mut cursor = std::path::PathBuf::from(manifest_dir);
        loop {
            let candidate = cursor.join("crates/foldit-gui/js");
            if candidate.is_dir() {
                return Some(candidate);
            }
            if !cursor.pop() {
                break;
            }
        }
        let exe = std::env::current_exe().ok()?;
        let mut cursor = exe.parent()?.to_path_buf();
        loop {
            let candidate = cursor.join("crates/foldit-gui/js");
            if candidate.is_dir() {
                return Some(candidate);
            }
            if !cursor.pop() {
                break;
            }
        }
        None
    }

    /// Spawn a fresh Vite dev server and block until it serves the
    /// foldit GUI. Always evicts whatever owns 5173 first -- prior
    /// runs leak (SIGINT doesn't propagate across the dev server's
    /// own process group), other vite instances on the same port
    /// would shadow this one, etc. Cheaper to start clean than to
    /// reason about whose vite we inherited.
    #[cfg(debug_assertions)]
    fn ensure_dev_server(&mut self) {
        if Self::dev_server_port_bound() {
            log::warn!(
                "Port 5173 already bound -- evicting before spawning a fresh \
                 Vite dev server (orphan from a previous run, another foldit \
                 checkout, or unrelated server)."
            );
            Self::kill_orphan_on_port_5173();

            use std::thread;
            use std::time::Duration;
            for _ in 0..50 {
                if !Self::dev_server_port_bound() {
                    break;
                }
                thread::sleep(Duration::from_millis(100));
            }
            if Self::dev_server_port_bound() {
                log::error!(
                    "Failed to free port 5173 within ~5s; cannot start fresh \
                     dev server. Run `lsof -ti :5173 | xargs kill -9` and \
                     retry."
                );
                return;
            }
        }

        use std::process::{Command, Stdio};

        // Resolve the frontend dir absolutely. Don't trust the
        // process cwd -- `cargo run` from anywhere other than the
        // workspace root would land us somewhere wrong, and the
        // failure mode is invisible (pnpm hangs in a stale dir
        // without printing anything if stdout was piped to /dev/null).
        let Some(frontend_dir) = Self::locate_frontend_dir() else {
            log::warn!(
                "Frontend directory not found (looked next to executable + \
                 walked up from CARGO_MANIFEST_DIR for crates/foldit-gui/js); \
                 skipping dev server"
            );
            return;
        };
        log::info!(
            "Spawning Vite dev server (pnpm run dev) in {}",
            frontend_dir.display()
        );

        // Inherit stdout + stderr so vite's own logs ("VITE v5.x ready
        // in 320 ms ... Local: http://localhost:5173/") land in the
        // user's terminal -- without this, a pnpm/vite startup hang
        // is invisible and the only signal is our 5-second waiting
        // ticks.
        #[cfg(windows)]
        let result = Command::new("pnpm.cmd")
            .args(["run", "dev"])
            .current_dir(&frontend_dir)
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn();

        #[cfg(unix)]
        let result = {
            use std::os::unix::process::CommandExt;
            Command::new("pnpm")
                .args(["run", "dev"])
                .current_dir(&frontend_dir)
                .stdin(Stdio::null())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .process_group(0) // Own process group so we can kill the whole tree
                .spawn()
        };

        let child = match result {
            Ok(c) => c,
            Err(e) => {
                log::error!("Failed to spawn Vite dev server: {}", e);
                return;
            }
        };
        let pid = child.id();
        log::info!("Vite dev server spawned (pid: {})", pid);
        // Register the dev server's pgid with the cleanup module so
        // SIGINT/SIGTERM tear it down alongside ML workers. Drop /
        // explicit kill_dev_server unregisters it.
        foldit_runner::register_worker_pgid(pid);
        self.dev_server = Some(child);

        // Wait for vite to bind 5173. We always evict orphans before
        // spawning, so port_bound becoming true after our spawn
        // unambiguously means *our* vite is up -- no fingerprint
        // probe needed. Cap at 30 s; vite's own log says "ready in
        // <1s" once it's actually started, and any longer pause
        // means pnpm/vite hung on something visible (now that
        // stdout/stderr are inherited).
        use std::thread;
        use std::time::Duration;
        let total_ticks = 150; // 150 * 200ms = 30s
        for i in 0..total_ticks {
            if Self::dev_server_port_bound() {
                log::info!("Dev server ready after ~{}ms", i * 200);
                self.dev_server_available = true;
                return;
            }
            thread::sleep(Duration::from_millis(200));
        }
        log::error!(
            "Dev server did not bind 5173 within 30s. Inspect the inherited \
             pnpm/vite output above for the cause."
        );
    }

    /// Kill the dev server child process if running. Idempotent --
    /// safe to call from both signal-driven and Drop paths.
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
            #[cfg(unix)]
            {
                // Kill the entire process group (pnpm + node/vite children).
                // The child was spawned with process_group(0) so its pid IS
                // the pgid. Negative pid tells kill(1) to signal the group.
                let _ = std::process::Command::new("kill")
                    .args(["-9", "--", &format!("-{}", pid)])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
            }
            let _ = child.wait();
            // Drop the cleanup-module registration so the signal
            // handler doesn't try to kill a stale pgid on a later
            // SIGINT (a fresh ensure_dev_server may reuse the pid
            // space).
            foldit_runner::unregister_worker_pgid(pid);
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

            // Resolved once at builder construction so the protocol
            // closure (Fn, called many times) captures a cheap clone
            // rather than re-walking on every request.
            let plugins_root = crate::plugin_assets::resolve_plugins_root();

            let builder = wry::WebViewBuilder::new()
                .with_transparent(true)
                .with_initialization_script(Self::INIT_SCRIPT)
                .with_custom_protocol("foldit".into(), move |_webview_id, request| {
                    use std::borrow::Cow;

                    let request_path = request.uri().path();

                    if request_path.starts_with("/plugins/") {
                        let Some(root) = plugins_root.as_ref() else {
                            return wry::http::Response::builder()
                                .status(404)
                                .body(Cow::Borrowed(b"Not Found" as &[u8]))
                                .unwrap();
                        };
                        return match crate::plugin_assets::serve(request_path, root) {
                            crate::plugin_assets::AssetResponse::Ok { bytes, mime } => {
                                wry::http::Response::builder()
                                    .status(200)
                                    .header("Content-Type", mime)
                                    .header("Access-Control-Allow-Origin", "*")
                                    .body(Cow::Owned(bytes))
                                    .unwrap()
                            }
                            crate::plugin_assets::AssetResponse::NotFound => {
                                wry::http::Response::builder()
                                    .status(404)
                                    .body(Cow::Borrowed(b"Not Found" as &[u8]))
                                    .unwrap()
                            }
                        };
                    }

                    let path = if request_path == "/" || request_path.is_empty() {
                        "index.html"
                    } else {
                        request_path.trim_start_matches('/')
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
    ///
    /// `console` is handled inline as a desktop-only logging side effect.
    /// Everything else delegates to `foldit_gui::bridge::decode::from_json`.
    fn handle_ipc(ipc_tx: &std::sync::mpsc::Sender<IpcMessage>, req: wry::http::Request<String>) {
        let body = req.body();
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(body) {
            if val.get("cmd").and_then(|v| v.as_str()) == Some("console") {
                let level = val.get("level").and_then(|v| v.as_str()).unwrap_or("log");
                let msg = val.get("msg").and_then(|v| v.as_str()).unwrap_or("");
                match level {
                    "error" => log::error!("[JS] {}", msg),
                    "warn" => log::warn!("[JS] {}", msg),
                    _ => log::info!("[JS] {}", msg),
                }
                return;
            }
        }
        match bridge::decode::from_json(body) {
            Some(msg) => {
                let _ = ipc_tx.send(msg);
            }
            None => log::debug!("Unrecognized IPC body: {}", body),
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

    /// Per-frame update: process IPC, drive `App::tick`, render, push
    /// dirty frontend bytes to the webview. `App` owns the drive loop
    /// (spine drain, score poll, engine update, visualization update,
    /// state-machine, populate_frontend) — the host just sequences the
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

        // App-owned drive loop (RX13): backend updates → spine drain →
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
pub(crate) fn run(
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
/// Must run BEFORE the wry WebView is attached as a child of the window —
/// on macOS, `wgpu::Instance::create_surface` calls `setLayer:` on the
/// contentView, replacing it with a CAMetalLayer; if the WKWebView is
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
            log::error!("Failed to initialize GPU render context: {:?}", e);
            return;
        }
    };

    let mut engine = match viso::VisoEngine::new(context, viso::options::VisoOptions::default()) {
        Ok(e) => e,
        Err(e) => {
            log::error!("Failed to initialize engine: {:?}", e);
            return;
        }
    };

    let presets_dir = std::path::Path::new("assets/view_presets");
    engine.load_preset("default", presets_dir);
    engine.set_render_scale(if scale < 2.0 { 2 } else { 1 });

    app.attach_engine(engine);
}
