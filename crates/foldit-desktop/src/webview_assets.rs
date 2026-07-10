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
                .and_then(|o| {
                    String::from_utf8_lossy(&o.stdout)
                        .trim()
                        .parse::<i32>()
                        .ok()
                });
            if let Some(pgid) = pgid {
                log::warn!("Killing orphan dev server process group pgid={pgid}");
                let _ = Command::new("kill")
                    .args(["-KILL", "--", &format!("-{pgid}")])
                    .status();
            } else {
                log::warn!("Killing orphan dev server pid={pid}");
                let _ = Command::new("kill")
                    .args(["-KILL", &pid.to_string()])
                    .status();
            }
        }
    }

    #[cfg(all(debug_assertions, windows))]
    fn kill_orphan_on_port_5173() {
        use std::process::Command;
        // PowerShell: find PIDs owning TCP 5173 and force-kill each tree.
        let ps_cmd = "Get-NetTCPConnection -LocalPort 5173 -State Listen -ErrorAction SilentlyContinue | Select-Object -ExpandProperty OwningProcess";
        let out = match Command::new("powershell")
            .args(["-NoProfile", "-Command", ps_cmd])
            .output()
        {
            Ok(o) if o.status.success() => o.stdout,
            _ => return,
        };
        for pid in String::from_utf8_lossy(&out)
            .lines()
            .filter_map(|s| s.trim().parse::<u32>().ok())
        {
            log::warn!("Killing orphan dev server pid={}", pid);
            let _ = Command::new("taskkill")
                .args(["/F", "/T", "/PID", &pid.to_string()])
                .status();
        }
    }

    /// Resolve the absolute path to `webview`. Tries
    /// (in order):
    ///
    ///   1. `FOLDIT_FRONTEND_DIR` env override.
    ///   2. Walk up from `CARGO_MANIFEST_DIR` (= this crate's dir at
    ///      compile time) looking for `webview`.
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
            let candidate = cursor.join("webview");
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
            let candidate = cursor.join("webview");
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
                 walked up from CARGO_MANIFEST_DIR for webview); \
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

    /// Create the wry webview for release mode, serving assets via custom protocol.
    #[cfg(all(not(debug_assertions), not(target_os = "linux")))]
    #[allow(
        clippy::expect_used,
        reason = "`Response::builder` fails only on a malformed header, and every header set here is a literal or a MIME type off the whitelist"
    )]
    pub(super) fn create_webview(
        window: &std::sync::Arc<winit::window::Window>,
    ) -> (Option<wry::WebView>, std::sync::mpsc::Receiver<IpcMessage>) {
        use std::borrow::Cow;

        let (ipc_tx, ipc_rx) = std::sync::mpsc::channel::<IpcMessage>();

        let webview = {
            use wry::dpi::{PhysicalPosition, PhysicalSize};
            let inner = window.inner_size();

            let assets = AssetResolver::new();
            let builder = wry::WebViewBuilder::new()
                .with_transparent(true)
                .with_initialization_script(Self::INIT_SCRIPT)
                .with_custom_protocol("foldit".into(), move |_webview_id, request| {
                    match assets.resolve(request.uri().path()) {
                        Some((bytes, mime)) => wry::http::Response::builder()
                            .status(200)
                            .header("Content-Type", mime)
                            .header("Access-Control-Allow-Origin", "*")
                            .body(Cow::Owned(bytes)),
                        None => wry::http::Response::builder()
                            .status(404)
                            .body(Cow::Borrowed(b"Not Found" as &[u8])),
                    }
                    .expect("static headers are well-formed")
                })
                .with_url(Self::RELEASE_URL)
                .with_ipc_handler({
                    let ipc_tx = ipc_tx.clone();
                    move |req| Self::handle_ipc(&ipc_tx, req.body())
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
                    log::error!("Failed to create wry webview: {e}");
                    None
                }
            }
        };

        (webview, ipc_rx)
    }

    /// Entry point for the release build's custom scheme. Windows' `WebView2`
    /// only accepts custom schemes mapped onto `http://<name>.localhost`.
    #[cfg(not(debug_assertions))]
    pub(super) const RELEASE_URL: &str = if cfg!(windows) {
        "http://foldit.localhost/index.html"
    } else {
        "foldit://localhost/index.html"
    };

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

    /// Shared IPC handler for webview messages.
    ///
    /// `console` is handled inline as a desktop-only logging side effect.
    /// Everything else delegates to `foldit_gui::bridge::decode::from_json`.
    pub(super) fn handle_ipc(ipc_tx: &std::sync::mpsc::Sender<IpcMessage>, body: &str) {
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

/// Serves the release build's frontend: the Vite bundle plus the two static
/// trees mounted beneath it. Roots and the panel-module allowlist are resolved
/// once at construction so a request costs only a filesystem read.
///
/// Shared by both webview backends — wry's custom protocol on macOS/Windows and
/// the `WebKitGTK` URI scheme on Linux — because the routing, the containment
/// envelope, and the fail-closed MIME whitelist must not diverge between them.
#[cfg(not(debug_assertions))]
pub struct AssetResolver {
    /// Built frontend (`index.html` + Vite's own `assets/`), the fallthrough.
    gui_root: Option<std::path::PathBuf>,
    /// Foldit-owned static assets (residue icons, …) under `/game-assets/`.
    /// Deliberately not `/assets/`, which the Vite bundle already claims.
    game_assets_root: Option<std::path::PathBuf>,
    /// Plugin trees under `/plugins/`.
    plugins_root: Option<std::path::PathBuf>,
    /// Manifest-declared `[[panels]]` entrypoints: the only `.mjs` paths the
    /// plugin tree will serve. Game assets ship none, so every `.mjs` under
    /// `/game-assets/` fails closed.
    ui_entrypoints: std::collections::HashSet<String>,
}

#[cfg(not(debug_assertions))]
impl AssetResolver {
    pub fn new() -> Self {
        let gui_root = Self::resolve_root("FOLDIT_GUI_ROOT", "assets/gui");
        if gui_root.is_none() {
            log::error!(
                "Frontend assets directory not found (looked for `assets/gui` up-tree from the \
                 executable); the webview will be blank. Set FOLDIT_GUI_ROOT or run \
                 `cargo xtask build-gui`."
            );
        }
        let game_assets_root = Self::resolve_root("FOLDIT_ASSETS_ROOT", "assets");
        if game_assets_root.is_none() {
            log::error!(
                "Foldit assets directory not found (looked for `assets/` up-tree from the \
                 executable); `/game-assets/` requests will 404. Set FOLDIT_ASSETS_ROOT."
            );
        }
        Self {
            gui_root,
            game_assets_root,
            plugins_root: crate::plugin_assets::resolve_plugins_root(),
            ui_entrypoints: foldit_core::locate_plugin_ui_entrypoints(),
        }
    }

    /// Resolve a request path to its body and MIME type. `None` is a 404 — no
    /// further detail leaks to the webview.
    pub fn resolve(&self, path: &str) -> Option<(Vec<u8>, String)> {
        for (prefix, root, modules) in [
            ("/plugins/", &self.plugins_root, Some(&self.ui_entrypoints)),
            ("/game-assets/", &self.game_assets_root, None),
        ] {
            if !path.starts_with(prefix) {
                continue;
            }
            let no_modules = std::collections::HashSet::new();
            let allowlist = modules.unwrap_or(&no_modules);
            return match crate::plugin_assets::serve(path, prefix, root.as_ref()?, allowlist) {
                crate::plugin_assets::AssetResponse::Ok { bytes, mime } => {
                    Some((bytes, mime.to_owned()))
                }
                crate::plugin_assets::AssetResponse::NotFound => None,
            };
        }

        let relative = match path.trim_start_matches('/') {
            "" => "index.html",
            rest => rest,
        };
        let file = self.gui_root.as_ref()?.join(relative);
        let bytes = std::fs::read(&file).ok()?;
        let mime = Self::mime_from_ext(file.extension().and_then(|e| e.to_str()).unwrap_or(""));
        Some((bytes, mime.to_owned()))
    }

    /// Resolve a directory that both the bundle and a dev release build place at
    /// `suffix` relative to *some* ancestor of the executable: the exe's own
    /// directory in a bundle (xtask `bundle()` stages it there), the repo root
    /// in a dev tree. `env_var` overrides both.
    fn resolve_root(env_var: &str, suffix: &str) -> Option<std::path::PathBuf> {
        if let Some(override_path) = std::env::var_os(env_var).map(std::path::PathBuf::from) {
            if override_path.is_dir() {
                return Some(override_path);
            }
        }
        let exe = std::env::current_exe().ok()?;
        let mut cursor = exe.parent()?.to_path_buf();
        loop {
            let candidate = cursor.join(suffix);
            if candidate.is_dir() {
                return Some(candidate);
            }
            if !cursor.pop() {
                return None;
            }
        }
    }

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
}
