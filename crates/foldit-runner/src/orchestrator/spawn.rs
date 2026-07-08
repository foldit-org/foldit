//! Plugin spawn descriptors and the orchestrator-side spawn primitive.
//!
//! [`PluginSpawnDescriptor`] describes how a plugin worker is launched.
//! [`spawn_plugin_worker`] actually does it: creates a socket, spawns the
//! worker binary with the 2-arg CLI (`<plugin_dir> <ipc_endpoint>`),
//! accepts the connection, and hands back a [`PluginClient`] paired with
//! the child process.
//!
//! pixi is a DEV/BUILD-time tool only: it creates the per-plugin conda
//! environments (`cargo xtask setup-envs`) and the bundler copies them
//! into the distribution. It is NEVER invoked at runtime; a shipped
//! Foldit has no pixi. Python plugins spawn the worker binary directly,
//! with the orchestrator pointing it at the plugin's env via `PYTHONHOME`
//! / `<env>/lib` on the loader path. The worker links Python through the
//! python-host dylib and boots the interpreter from that env. The worker
//! reads its plugin kind from `<plugin_dir>/plugin.toml`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use anyhow::{anyhow, Context, Result};

use super::client::{PluginClient, SocketPluginClient};
use super::manifest::{
    ButtonEntry, PanelEntry, PluginKind, PluginManifest, SettingsEntry,
};
use crate::ipc::{self, LocalSocketListener};

/// Stable identifier for a plugin. Matches `PluginRegistration.id` and is
/// used by the orchestrator to route ops back to their owning worker.
pub type PluginId = String;

/// Describes how a plugin worker should be spawned.
///
/// The orchestrator dispatches on the variant; one variant per host
/// runtime. Every variant carries the plugin directory because the
/// worker reads its kind + entry point from `<plugin_dir>/plugin.toml`
/// at startup.
#[derive(Debug, Clone)]
pub enum PluginSpawnDescriptor {
    /// Python plugin backed by a per-plugin conda environment. The
    /// orchestrator spawns the worker directly and activates the env
    /// itself (`PYTHONHOME` + `<env>/lib` on the loader path); no pixi at
    /// runtime. The env is created at dev/build time by pixi.
    Python {
        /// Plugin id (matches `PluginRegistration.id`).
        id: PluginId,
        /// Environment name. Selects the dev env directory
        /// (`.pixi/envs/<env>`); defaults to the plugin id per manifest
        /// resolution rules. Ignored in a bundle (the env ships at
        /// `<plugin_dir>/env`).
        env: String,
        /// Absolute path to the plugin's directory (contains
        /// `plugin.toml` + the installed Python package).
        plugin_dir: PathBuf,
        /// Human display name for the button group, from the manifest.
        /// `None` → the GUI titles the group from the id.
        name: Option<String>,
        /// Left-to-right sort key for the button group, from the manifest.
        order: Option<u32>,
        /// Whether this plugin fits against electron density; gates
        /// whether the host includes the density map in its init payload.
        uses_density: bool,
        /// Button declarations parsed from the manifest. Empty for
        /// plugins with no user-facing surface.
        buttons: Vec<ButtonEntry>,
        /// Custom-panel declarations parsed from the manifest. Empty for
        /// plugins that contribute no panels.
        panels: Vec<PanelEntry>,
        /// Settings-tab declarations parsed from the manifest. Empty for
        /// plugins that contribute no settings.
        settings: Vec<SettingsEntry>,
    },

    /// Native plugin: a shared library loaded by `foldit-worker`
    /// via `dlopen` + the C ABI vtable in [`crate::plugin::abi`]. The
    /// worker subprocess provides crash isolation. Spawned directly with
    /// no environment activation (native plugins carry their own deps).
    Native {
        /// Plugin id (matches `PluginRegistration.id`).
        id: PluginId,
        /// Absolute path to the plugin's directory (contains
        /// `plugin.toml` + the dylib named by `manifest.native.binary`).
        plugin_dir: PathBuf,
        /// Human display name for the button group, from the manifest.
        name: Option<String>,
        /// Left-to-right sort key for the button group, from the manifest.
        order: Option<u32>,
        /// Whether this plugin fits against electron density; gates
        /// whether the host includes the density map in its init payload.
        uses_density: bool,
        /// Button declarations parsed from the manifest.
        buttons: Vec<ButtonEntry>,
        /// Custom-panel declarations parsed from the manifest.
        panels: Vec<PanelEntry>,
        /// Settings-tab declarations parsed from the manifest.
        settings: Vec<SettingsEntry>,
    },

    /// Wasm plugin. Stubbed; the native host doesn't spawn wasm
    /// workers. The web build will provide its own spawn path.
    Wasm {
        /// Plugin id (matches `PluginRegistration.id`).
        id: PluginId,
        /// Absolute path to the plugin's directory (contains
        /// `plugin.toml` + the wasm module).
        plugin_dir: PathBuf,
        /// Human display name for the button group, from the manifest.
        name: Option<String>,
        /// Left-to-right sort key for the button group, from the manifest.
        order: Option<u32>,
        /// Whether this plugin fits against electron density; gates
        /// whether the host includes the density map in its init payload.
        uses_density: bool,
        /// Button declarations parsed from the manifest.
        buttons: Vec<ButtonEntry>,
        /// Custom-panel declarations parsed from the manifest.
        panels: Vec<PanelEntry>,
        /// Settings-tab declarations parsed from the manifest.
        settings: Vec<SettingsEntry>,
    },
}

impl PluginSpawnDescriptor {
    /// Plugin id, regardless of variant.
    #[must_use]
    pub fn id(&self) -> &str {
        match self {
            PluginSpawnDescriptor::Python { id, .. }
            | PluginSpawnDescriptor::Native { id, .. }
            | PluginSpawnDescriptor::Wasm { id, .. } => id,
        }
    }

    /// Plugin directory, regardless of variant.
    #[must_use]
    pub fn plugin_dir(&self) -> &Path {
        match self {
            PluginSpawnDescriptor::Python { plugin_dir, .. }
            | PluginSpawnDescriptor::Native { plugin_dir, .. }
            | PluginSpawnDescriptor::Wasm { plugin_dir, .. } => plugin_dir,
        }
    }

    /// Manifest-declared button entries, regardless of variant. Empty
    /// when the plugin contributes no user-facing buttons.
    #[must_use]
    pub fn buttons(&self) -> &[ButtonEntry] {
        match self {
            PluginSpawnDescriptor::Python { buttons, .. }
            | PluginSpawnDescriptor::Native { buttons, .. }
            | PluginSpawnDescriptor::Wasm { buttons, .. } => buttons.as_slice(),
        }
    }

    /// Manifest-declared custom panels, regardless of variant. Empty
    /// when the plugin contributes no panels.
    #[must_use]
    pub fn panels(&self) -> &[PanelEntry] {
        match self {
            PluginSpawnDescriptor::Python { panels, .. }
            | PluginSpawnDescriptor::Native { panels, .. }
            | PluginSpawnDescriptor::Wasm { panels, .. } => panels.as_slice(),
        }
    }

    /// Manifest-declared settings tabs, regardless of variant. Empty
    /// when the plugin contributes no settings.
    #[must_use]
    pub fn settings(&self) -> &[SettingsEntry] {
        match self {
            PluginSpawnDescriptor::Python { settings, .. }
            | PluginSpawnDescriptor::Native { settings, .. }
            | PluginSpawnDescriptor::Wasm { settings, .. } => {
                settings.as_slice()
            }
        }
    }

    /// Manifest-declared display name for the button group, regardless of
    /// variant. `None` when the manifest omits it.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        match self {
            PluginSpawnDescriptor::Python { name, .. }
            | PluginSpawnDescriptor::Native { name, .. }
            | PluginSpawnDescriptor::Wasm { name, .. } => name.as_deref(),
        }
    }

    /// Manifest-declared button-group sort key, regardless of variant.
    /// `None` when the manifest omits it.
    #[must_use]
    pub fn order(&self) -> Option<u32> {
        match self {
            PluginSpawnDescriptor::Python { order, .. }
            | PluginSpawnDescriptor::Native { order, .. }
            | PluginSpawnDescriptor::Wasm { order, .. } => *order,
        }
    }

    /// Whether this plugin fits against electron density, regardless of
    /// variant. The host reads it to decide whether to include the density
    /// map in the plugin's init payload.
    #[must_use]
    pub const fn uses_density(&self) -> bool {
        match self {
            PluginSpawnDescriptor::Python { uses_density, .. }
            | PluginSpawnDescriptor::Native { uses_density, .. }
            | PluginSpawnDescriptor::Wasm { uses_density, .. } => *uses_density,
        }
    }

    /// Build a descriptor for `plugin_dir` against the parsed manifest.
    #[must_use]
    pub fn from_manifest(
        plugin_dir: PathBuf,
        manifest: &PluginManifest,
    ) -> Self {
        let buttons = manifest.buttons.clone();
        let panels = manifest.panels.clone();
        let settings = manifest.settings.clone();
        let name = manifest.name.clone();
        let order = manifest.order;
        let uses_density = manifest.uses_density;
        match manifest.kind {
            PluginKind::Python => PluginSpawnDescriptor::Python {
                id: manifest.id.clone(),
                env: String::from(manifest.python_env()),
                plugin_dir,
                name,
                order,
                uses_density,
                buttons,
                panels,
                settings,
            },
            PluginKind::Native => PluginSpawnDescriptor::Native {
                id: manifest.id.clone(),
                plugin_dir,
                name,
                order,
                uses_density,
                buttons,
                panels,
                settings,
            },
            PluginKind::Wasm => PluginSpawnDescriptor::Wasm {
                id: manifest.id.clone(),
                plugin_dir,
                name,
                order,
                uses_density,
                buttons,
                panels,
                settings,
            },
        }
    }
}

/// Scan `plugins_root` for child directories containing `plugin.toml`.
///
/// Parses each manifest and builds a [`PluginSpawnDescriptor`] per
/// plugin. Directories without `plugin.toml` are silently skipped (lets
/// the user keep scratch dirs alongside real plugins). Manifests that
/// fail to parse are logged and skipped; discovery never aborts on a
/// single bad plugin.
///
/// The returned vector is sorted by plugin id for deterministic ordering
/// in tests and logs.
///
/// # Errors
///
/// Returns an error if `plugins_root` can't be read as a directory.
pub fn discover_plugins(
    plugins_root: &Path,
) -> Result<Vec<PluginSpawnDescriptor>> {
    let mut out = Vec::new();
    let read = fs::read_dir(plugins_root)
        .with_context(|| format!("read_dir {}", plugins_root.display()))?;
    for entry in read {
        let entry = entry.with_context(|| {
            format!("entry under {}", plugins_root.display())
        })?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let manifest_path = path.join("plugin.toml");
        if !manifest_path.exists() {
            continue;
        }
        let toml_src = match fs::read_to_string(&manifest_path) {
            Ok(s) => s,
            Err(e) => {
                log::warn!(
                    "discover_plugins: failed to read {}: {e}",
                    manifest_path.display()
                );
                continue;
            }
        };
        let manifest = match PluginManifest::parse(&toml_src) {
            Ok(m) => m,
            Err(e) => {
                log::warn!(
                    "discover_plugins: failed to parse {}: {e}",
                    manifest_path.display()
                );
                continue;
            }
        };
        log::info!(
            "discovered plugin {} (kind={:?}) at {}",
            manifest.id,
            manifest.kind,
            path.display()
        );
        out.push(PluginSpawnDescriptor::from_manifest(path, &manifest));
    }
    out.sort_by(|a, b| a.id().cmp(b.id()));
    Ok(out)
}

/// Spawn a plugin worker process and return its client.
///
/// Steps:
/// 1. Create an IPC listener with a unique socket name (derived from the plugin
///    id).
/// 2. Spawn `foldit-worker <plugin_dir> <socket>` directly. For Python plugins
///    the orchestrator first sets `PYTHONHOME` + the env's `lib/` on the loader
///    path so the worker boots the interpreter from the per-plugin env (no
///    pixi). Native plugins run with no environment activation.
/// 3. Register the worker's process group for signal-handler cleanup.
/// 4. Accept the connection and wrap it in a [`PluginClient`].
///
/// # Errors
///
/// Returns an error if the socket listener can't be created, the worker
/// process can't be spawned, or the worker doesn't connect back.
pub fn spawn_plugin_worker(
    worker_binary: &Path,
    descriptor: &PluginSpawnDescriptor,
) -> Result<(Option<Child>, Box<dyn PluginClient>)> {
    let plugin_id = String::from(descriptor.id());
    let pending = bind_and_spawn_worker(worker_binary, descriptor)?;
    log::info!("waiting for plugin worker {plugin_id} to connect...");
    let stream =
        ipc::accept_connection(&pending.listener).with_context(|| {
            format!("accept_connection failed for plugin {plugin_id}")
        })?;
    log::info!("plugin worker {plugin_id} connected");
    Ok((
        Some(pending.process),
        Box::new(SocketPluginClient::new(plugin_id, stream)),
    ))
}

/// A worker process whose connection has not been accepted yet.
///
/// Created by [`bind_and_spawn_worker`]: the listener is bound and the
/// child process is spawned, but the accept is deferred. The worker's
/// single boot-time connect is held in the OS backlog, so a later
/// [`try_accept`](PendingWorker::try_accept) completes it without a race
/// (the async warm path arms non-blocking accept first via
/// [`set_accept_nonblocking`](PendingWorker::set_accept_nonblocking)).
///
/// This type OWNS the listener and child across the gap between spawn and
/// accept. Dropping the listener unlinks the socket, so it must be kept
/// alive (here, retained in the orchestrator's pending-warm map) until the
/// accept succeeds.
pub struct PendingWorker {
    plugin_id: String,
    process: Child,
    listener: LocalSocketListener,
}

impl PendingWorker {
    /// Plugin id this pending worker belongs to.
    #[must_use]
    pub fn plugin_id(&self) -> &str {
        &self.plugin_id
    }

    /// Switch this worker's listener into non-blocking accept mode so
    /// [`Self::try_accept`] can poll per frame without blocking. Required
    /// before the async warm path retains and polls the worker; the
    /// blocking spawn path leaves it in the default blocking mode.
    ///
    /// # Errors
    ///
    /// Returns an error if the platform rejects the mode change.
    pub fn set_accept_nonblocking(&self) -> Result<()> {
        ipc::set_accept_nonblocking(&self.listener).with_context(|| {
            format!(
                "set_accept_nonblocking failed for plugin {}",
                self.plugin_id
            )
        })
    }

    /// Try to complete the worker's connection without blocking.
    /// Consumes `self`: on either non-terminal outcome the
    /// [`PendingWorker`] is handed back so the caller can retain it for
    /// the next poll. On success the listener is dropped (its job is done)
    /// and the bound process + client are returned.
    pub fn try_accept(mut self) -> AcceptOutcome {
        match ipc::try_accept_connection(&self.listener) {
            Ok(Some(stream)) => {
                log::info!("plugin worker {} connected", self.plugin_id);
                let client: Box<dyn PluginClient> = Box::new(
                    SocketPluginClient::new(self.plugin_id.as_str(), stream),
                );
                AcceptOutcome::Connected(self.process, client)
            }
            Ok(None) => {
                // Check whether the child process has already exited. If it
                // crashed before connecting (e.g. missing DLL, bad env), the
                // listener will never receive a connection and the warm
                // would hang forever.
                match self.process.try_wait() {
                    Ok(Some(status)) => AcceptOutcome::Failed(anyhow::anyhow!(
                        "plugin worker {} exited ({}) before connecting",
                        self.plugin_id,
                        status,
                    )),
                    _ => AcceptOutcome::Pending(self),
                }
            }
            Err(e) => AcceptOutcome::Failed(e),
        }
    }
}

/// Outcome of a non-blocking [`PendingWorker::try_accept`].
pub enum AcceptOutcome {
    /// The worker connected; carries its process and IPC client.
    Connected(Child, Box<dyn PluginClient>),
    /// The worker has not connected yet; carries the pending worker back
    /// so the caller retains it (keeping the listener alive) and polls
    /// again next frame.
    Pending(PendingWorker),
    /// The accept failed for a genuine reason (not "no connection
    /// waiting"); the pending worker is dropped with this outcome (its
    /// listener + child go with it).
    Failed(anyhow::Error),
}

/// Bind a listener and spawn the worker process WITHOUT accepting.
///
/// The returned [`PendingWorker`] owns the bound listener and the child;
/// the caller completes the connection later (the async warm path first
/// calls [`PendingWorker::set_accept_nonblocking`], then polls
/// [`PendingWorker::try_accept`] per frame).
///
/// Shares the per-kind command construction with [`spawn_plugin_worker`]:
/// the blocking path bind+spawns here and then accepts immediately, while
/// the async path retains the [`PendingWorker`] and accepts later.
///
/// # Errors
///
/// Returns an error if the socket listener can't be created, the worker
/// process can't be spawned, or (for Wasm) the variant is unsupported.
pub fn bind_and_spawn_worker(
    worker_binary: &Path,
    descriptor: &PluginSpawnDescriptor,
) -> Result<PendingWorker> {
    match descriptor {
        PluginSpawnDescriptor::Python {
            id,
            env,
            plugin_dir,
            ..
        } => bind_and_spawn_python(worker_binary, id, env, plugin_dir),
        PluginSpawnDescriptor::Native { id, plugin_dir, .. } => {
            bind_and_spawn_native(worker_binary, id, plugin_dir)
        }
        PluginSpawnDescriptor::Wasm { id, .. } => Err(anyhow!(
            "spawn_plugin_worker: Wasm variant not implemented for the native \
             host (plugin id {id})"
        )),
    }
}

/// Default `RUST_LOG` directive applied to spawned worker processes.
///
/// Plain `info` is too loud: any Rust crate the worker pulls in that uses
/// `tracing-subscriber` or `env_logger` would inherit it and dump its
/// internal telemetry into our log stream. Scoping `info` to our own
/// crates and defaulting everything else to `warn` muzzles third-party
/// noise without hiding plugin lifecycle events. Callers honour any
/// user-supplied `RUST_LOG` ahead of this default.
const WORKER_RUST_LOG_DEFAULT: &str =
    "warn,foldit_runner=info,foldit_python_host=info";

/// Walk up from `start` looking for the nearest directory that holds a
/// `pixi.toml` (and, in dev, its `.pixi/envs/`). Each plugin (and the dummy
/// fixture) owns its own `pixi.toml`, so from a plugin dir this returns that
/// plugin's own project root. Returns `None` if none is found before reaching
/// the filesystem root. Used by `resolve_plugin_env_dir` to locate the dev env
/// directory regardless of the caller's CWD.
fn find_pixi_root(start: &Path) -> Option<PathBuf> {
    let mut cursor = start;
    loop {
        if cursor.join("pixi.toml").is_file() {
            return Some(cursor.to_path_buf());
        }
        cursor = cursor.parent()?;
    }
}

/// Resolve the conda environment directory for a Python plugin. Two
/// layouts, checked in order:
///
/// - **bundle**: a self-contained `<plugin_dir>/env`. Shipped distributions
///   carry each plugin's env next to its `plugin.toml`; there is no pixi and no
///   `.pixi/` on the machine.
/// - **dev**: `<plugin_root>/.pixi/envs/<env_name>`, created by `cargo xtask
///   setup-envs` (`pixi install` per plugin). `plugin_root` is the nearest
///   ancestor holding a `pixi.toml`, found by walking up from `plugin_dir`;
///   each plugin (and the dummy fixture) owns its own pixi.toml + env.
///
/// # Errors
///
/// Returns an error if neither layout yields an existing directory.
fn resolve_plugin_env_dir(
    plugin_dir: &Path,
    env_name: &str,
) -> Result<PathBuf> {
    let bundled = plugin_dir.join("env");
    if bundled.is_dir() {
        return Ok(bundled);
    }
    if let Some(root) = find_pixi_root(plugin_dir) {
        let dev = root.join(".pixi").join("envs").join(env_name);
        if dev.is_dir() {
            return Ok(dev);
        }
    }
    Err(anyhow!(
        "could not resolve a Python environment for plugin at {} (env \
         `{env_name}`): checked bundle layout `{}` and dev layout \
         `<runner_root>/.pixi/envs/{env_name}`. Run `cargo xtask setup-envs` \
         to create the dev environments.",
        plugin_dir.display(),
        bundled.display(),
    ))
}

/// Resolve the per-spawn log file path for a plugin worker. Returns
/// `None` (and logs a warning) if the log directory can't be created;
/// callers omit the env var in that case, leaving the plugin without
/// file logging rather than failing the spawn. Path format:
/// `<dir>/<plugin_id>-<YYYYMMDDTHHMMSSmmmZ>.log`.
fn resolve_plugin_log_path(plugin_id: &str) -> Option<PathBuf> {
    let dir = std::env::var_os("FOLDIT_LOG_DIR")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".foldit/logs")))?;

    if let Err(err) = fs::create_dir_all(&dir) {
        log::warn!(
            "could not create plugin log dir {}: {err}; plugin {plugin_id} \
             will run without file logging",
            dir.display()
        );
        return None;
    }

    let ts = chrono::Utc::now().format("%Y%m%dT%H%M%S%3fZ").to_string();
    Some(dir.join(format!("{plugin_id}-{ts}.log")))
}

/// Activate a Python plugin's conda env on `cmd`, replacing what
/// `pixi run -e <env>` used to do. Sets `PYTHONHOME` (the embedded
/// interpreter reads it directly), `CONDA_PREFIX` (parity for libraries
/// that read it), and prepends the env's `lib/` to the platform loader
/// path so libpython resolves when the worker dlopens the python-host
/// dylib. That dlopen happens before any python-host code runs, so the var
/// must be set by us at spawn time (on macOS dyld reads it once at
/// startup); any inherited value is preserved.
fn apply_python_env(cmd: &mut Command, env_dir: &Path) {
    let _ = cmd.env("PYTHONHOME", env_dir);
    let _ = cmd.env("CONDA_PREFIX", env_dir);

    // Windows resolves DLL dependencies via PATH. python3xx.dll sits at the
    // env root and conda's native libs in <env>\Library\bin; both must be on
    // PATH so the worker can load foldit_python_host.dll (which imports
    // python3xx.dll) before any python-host code runs.
    #[cfg(target_os = "windows")]
    {
        let mut dll_path = std::ffi::OsString::from(env_dir);
        dll_path.push(";");
        dll_path.push(env_dir.join("Library").join("bin"));
        if let Some(existing) = std::env::var_os("PATH") {
            if !existing.is_empty() {
                dll_path.push(";");
                dll_path.push(existing);
            }
        }
        let _ = cmd.env("PATH", dll_path);
    }

    // macOS/Linux: prepend the env's lib/ to the loader path so libpython
    // (DT_NEEDED of the python-host dylib) resolves at dlopen time.
    #[cfg(not(target_os = "windows"))]
    {
        let lib_var = if cfg!(target_os = "macos") {
            "DYLD_LIBRARY_PATH"
        } else {
            "LD_LIBRARY_PATH"
        };
        let lib_dir = env_dir.join("lib");
        let lib_path = match std::env::var_os(lib_var) {
            Some(existing) if !existing.is_empty() => {
                let mut p = lib_dir.into_os_string();
                p.push(":");
                p.push(existing);
                p
            }
            _ => lib_dir.into_os_string(),
        };
        let _ = cmd.env(lib_var, lib_path);
    }
}

fn bind_and_spawn_python(
    worker_binary: &Path,
    plugin_id: &str,
    env_name: &str,
    plugin_dir: &Path,
) -> Result<PendingWorker> {
    let (listener, socket_name) = bind_listener(plugin_id)?;

    // Resolve the plugin's conda env (bundle: <plugin_dir>/env; dev:
    // <runner_root>/.pixi/envs/<env_name>). The worker links Python via
    // the python-host dylib and boots the interpreter from this env; pixi
    // is NOT involved at runtime.
    let env_dir = resolve_plugin_env_dir(plugin_dir, env_name)?;

    log::info!(
        "spawning Python plugin worker {} (env={}, env_dir={}, plugin_dir={}, \
         socket={})",
        plugin_id,
        env_name,
        env_dir.display(),
        plugin_dir.display(),
        socket_name
    );

    let mut cmd =
        base_worker_command(worker_binary, plugin_id, plugin_dir, &socket_name);

    // Activate the env ourselves (no pixi at runtime): `PYTHONHOME` +
    // `<env>/lib` on the loader path. See `apply_python_env`.
    apply_python_env(&mut cmd, &env_dir);

    let process = spawn_worker_command(cmd, plugin_id)?;
    Ok(PendingWorker {
        plugin_id: String::from(plugin_id),
        process,
        listener,
    })
}

/// Spawn `foldit-worker` directly (no pixi). The worker reads
/// `<plugin_dir>/plugin.toml` to find the dylib, dlopens it, and
/// connects to `<socket>`.
fn bind_and_spawn_native(
    worker_binary: &Path,
    plugin_id: &str,
    plugin_dir: &Path,
) -> Result<PendingWorker> {
    let (listener, socket_name) = bind_listener(plugin_id)?;

    log::info!(
        "spawning native plugin worker {} (plugin_dir={}, socket={})",
        plugin_id,
        plugin_dir.display(),
        socket_name
    );

    let cmd =
        base_worker_command(worker_binary, plugin_id, plugin_dir, &socket_name);
    let process = spawn_worker_command(cmd, plugin_id)?;
    Ok(PendingWorker {
        plugin_id: String::from(plugin_id),
        process,
        listener,
    })
}

/// Create the worker's IPC listener.
///
/// The listener is left in its default BLOCKING accept mode: the blocking
/// path ([`spawn_plugin_worker`]) accepts immediately via
/// `ipc::accept_connection`, and the async path opts into non-blocking
/// accept explicitly via [`PendingWorker::set_accept_nonblocking`].
fn bind_listener(plugin_id: &str) -> Result<(LocalSocketListener, String)> {
    ipc::create_listener(plugin_id).with_context(|| {
        format!("create_listener failed for plugin {plugin_id}")
    })
}

/// Build the base `foldit-worker` command shared by every plugin kind:
/// the two-arg CLI (`<plugin_dir> <socket>`), the scoped `RUST_LOG`
/// default, inherited stderr, an optional per-spawn log file, and (on
/// unix) its own process group for signal-handler cleanup. Per-kind
/// environment activation (Python env vars) is layered on by the caller.
fn base_worker_command(
    worker_binary: &Path,
    plugin_id: &str,
    plugin_dir: &Path,
    socket_name: &str,
) -> Command {
    let mut cmd = Command::new(worker_binary);
    let _ = cmd
        .arg(plugin_dir)
        // The socket name is the one the listener already bound; the caller
        // passes it in so the worker connects to the exact same endpoint.
        .arg(socket_name)
        .env(
            "RUST_LOG",
            std::env::var("RUST_LOG")
                .unwrap_or_else(|_| String::from(WORKER_RUST_LOG_DEFAULT)),
        )
        .stderr(Stdio::inherit());

    if let Some(log_path) = resolve_plugin_log_path(plugin_id) {
        log::info!("plugin worker {plugin_id} log -> {}", log_path.display());
        let _ = cmd.env("FOLDIT_PLUGIN_LOG_PATH", &log_path);
    }

    // Each worker in its own process group so signal handlers can kill
    // the whole tree (worker + any Python children).
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let _ = cmd.process_group(0);
    }

    cmd
}

/// Spawn a prepared worker command and register its process group for
/// signal-handler cleanup.
fn spawn_worker_command(mut cmd: Command, plugin_id: &str) -> Result<Child> {
    let process = cmd.spawn().with_context(|| {
        format!("failed to spawn plugin worker {plugin_id}")
    })?;
    super::cleanup::register_worker_pgid(process.id());
    Ok(process)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn skips_non_plugin_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("not_a_plugin")).unwrap();
        let real = tmp.path().join("real");
        fs::create_dir(&real).unwrap();
        fs::write(
            real.join("plugin.toml"),
            r#"
                id = "real-one"
                kind = "python"
            "#,
        )
        .unwrap();
        let out = discover_plugins(tmp.path()).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id(), "real-one");
    }

    #[test]
    fn skips_unparseable_manifests_without_aborting() {
        let tmp = tempfile::tempdir().unwrap();
        let bad = tmp.path().join("bad");
        let good = tmp.path().join("good");
        fs::create_dir(&bad).unwrap();
        fs::create_dir(&good).unwrap();
        fs::write(bad.join("plugin.toml"), "this is not toml ::: !!!").unwrap();
        fs::write(
            good.join("plugin.toml"),
            r#"
                id = "good-one"
                kind = "python"
            "#,
        )
        .unwrap();
        let out = discover_plugins(tmp.path()).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id(), "good-one");
    }
}
