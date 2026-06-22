//! Webview frontend serving: dev-server (debug) and embedded-asset (release) wiring.

use crate::window::AppRunner;
use foldit_gui::bridge;
use foldit_gui::IpcMessage;

impl AppRunner {
    /// True if *something* is listening on TCP 5173. Tries IPv6 and
    /// IPv4 because vite's default `localhost` binding is IPv6-only on
    /// macOS, IPv4-only on some Linux distros, and dual-stack
    /// elsewhere -- a single-family probe loses to that.
    #[cfg(debug_assertions)]
    fn dev_server_port_bound() -> bool {
        use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
        use std::time::Duration;
        let timeout = Duration::from_millis(200);
        let v6 = SocketAddr::from((Ipv6Addr::LOCALHOST, 5173));
        let v4 = SocketAddr::from((Ipv4Addr::LOCALHOST, 5173));
        TcpStream::connect_timeout(&v6, timeout).is_ok()
            || TcpStream::connect_timeout(&v4, timeout).is_ok()
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
                log::warn!("Killing orphan dev server process group pgid={pgid}");
                let _ = Command::new("kill")
                    .args(["-KILL", "--", &format!("-{pgid}")])
                    .status();
            } else {
                log::warn!("Killing orphan dev server pid={pid}");
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
    pub(super) fn ensure_dev_server(&mut self) {
        use std::process::{Command, Stdio};
        use std::thread;
        use std::time::Duration;

        if Self::dev_server_port_bound() {
            log::warn!(
                "Port 5173 already bound -- evicting before spawning a fresh \
                 Vite dev server (orphan from a previous run, another foldit \
                 checkout, or unrelated server)."
            );
            Self::kill_orphan_on_port_5173();

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
            "Spawning Vite dev server (bun run dev) in {}",
            frontend_dir.display()
        );

        // Inherit stdout + stderr so vite's own logs ("VITE v5.x ready
        // in 320 ms ... Local: http://localhost:5173/") land in the
        // user's terminal -- without this, a bun/vite startup hang
        // is invisible and the only signal is our 5-second waiting
        // ticks. `bun` is a native exe on every platform, so Command
        // resolves it directly.
        #[cfg(windows)]
        let result = Command::new("bun")
            .args(["run", "dev"])
            .current_dir(&frontend_dir)
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn();

        #[cfg(unix)]
        let result = {
            use std::os::unix::process::CommandExt;
            Command::new("bun")
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
                log::error!("Failed to spawn Vite dev server: {e}");
                return;
            }
        };
        let pid = child.id();
        log::info!("Vite dev server spawned (pid: {pid})");
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
    pub(super) fn kill_dev_server(&mut self) {
        if let Some(ref mut child) = self.dev_server {
            let pid = child.id();
            log::info!("Killing dev server (pid: {pid})...");
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
                    .args(["-9", "--", &format!("-{pid}")])
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

    /// Resolve the directory holding the built frontend (`index.html` +
    /// `assets/`). Mirrors [`foldit_core::locate_plugins_root`]'s
    /// bundle-vs-dev resolution so the exe serves the GUI from the right
    /// place regardless of launch cwd. Both layouts put it at
    /// `assets/gui` relative to some ancestor of the executable:
    ///
    ///   * Bundle: `assets/gui/` next to the executable (xtask `bundle()`
    ///     copies the repo's `assets/gui` there).
    ///   * Dev release build (`cargo run --release`, `target/release/foldit`):
    ///     no sibling `assets/`, so a higher ancestor is the repo root whose
    ///     `assets/gui` `cargo xtask build-gui` writes.
    ///
    /// `FOLDIT_GUI_ROOT` overrides both. Returns `None` if nothing resolves,
    /// which the caller logs once and the protocol handler then 404s (blank
    /// webview) -- the same surface as a genuinely missing asset.
    #[cfg(not(debug_assertions))]
    fn resolve_gui_root() -> Option<std::path::PathBuf> {
        if let Some(env) = std::env::var_os("FOLDIT_GUI_ROOT") {
            let p = std::path::PathBuf::from(env);
            if p.is_dir() {
                return Some(p);
            }
        }
        // Walk up from the executable. Iteration 0 (the exe's own dir) is the
        // bundle layout; a higher ancestor is the dev-tree repo root.
        let exe = std::env::current_exe().ok()?;
        let mut cursor = exe.parent()?.to_path_buf();
        loop {
            let candidate = cursor.join("assets/gui");
            if candidate.is_dir() {
                return Some(candidate);
            }
            if !cursor.pop() {
                break;
            }
        }
        None
    }

    /// Create the wry webview for release mode, serving assets via custom protocol.
    #[cfg(not(debug_assertions))]
    pub(super) fn create_webview_release(
        window: &std::sync::Arc<winit::window::Window>,
    ) -> (Option<wry::WebView>, std::sync::mpsc::Receiver<IpcMessage>) {
        let (ipc_tx, ipc_rx) = std::sync::mpsc::channel::<IpcMessage>();

        let webview = {
            use wry::dpi::{PhysicalPosition, PhysicalSize};
            let inner = window.inner_size();

            // Resolved once at builder construction so the protocol
            // closure (Fn, called many times) captures a cheap clone
            // rather than re-walking on every request.
            let plugins_root = crate::plugin_assets::resolve_plugins_root();
            // Manifest-declared `[[panels]]` entrypoints; the only `.mjs`
            // paths the protocol will serve in release (dev has no such gate).
            let ui_entrypoints = foldit_core::locate_plugin_ui_entrypoints();
            let gui_root = Self::resolve_gui_root();
            if gui_root.is_none() {
                log::error!(
                    "Frontend assets directory not found (looked for `gui/` next \
                     to the executable and `assets/gui` up-tree from it); the \
                     webview will be blank. Set FOLDIT_GUI_ROOT or run \
                     `cargo xtask build-gui`."
                );
            }

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
                        return match crate::plugin_assets::serve(request_path, root, &ui_entrypoints) {
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

                    let Some(root) = gui_root.as_ref() else {
                        return wry::http::Response::builder()
                            .status(404)
                            .body(Cow::Borrowed(b"Not Found" as &[u8]))
                            .unwrap();
                    };
                    let asset_path = root.join(path);
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
                    move |req| Self::handle_ipc(&ipc_tx, &req)
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

    /// Shared IPC initialization script injected into the webview.
    /// `pub(super)` so the debug `create_webview` in `window` can reuse it.
    pub(super) const INIT_SCRIPT: &str = r"
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
    ";

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

    /// Shared IPC handler for webview messages.
    ///
    /// `console` is handled inline as a desktop-only logging side effect.
    /// Everything else delegates to `foldit_gui::bridge::decode::from_json`.
    pub(super) fn handle_ipc(ipc_tx: &std::sync::mpsc::Sender<IpcMessage>, req: &wry::http::Request<String>) {
        let body = req.body();
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(body) {
            if val.get("cmd").and_then(|v| v.as_str()) == Some("console") {
                let level = val.get("level").and_then(|v| v.as_str()).unwrap_or("log");
                let msg = val.get("msg").and_then(|v| v.as_str()).unwrap_or("");
                match level {
                    "error" => log::error!("[JS] {msg}"),
                    "warn" => log::warn!("[JS] {msg}"),
                    _ => log::info!("[JS] {msg}"),
                }
                return;
            }
        }
        match bridge::decode::from_json(body) {
            Some(msg) => {
                let _ = ipc_tx.send(msg);
            }
            None => log::debug!("Unrecognized IPC body: {body}"),
        }
    }
}
