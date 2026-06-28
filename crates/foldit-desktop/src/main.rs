//! Foldit-RS desktop binary.
//!
//! Thin entry: parse argv, set up logging, resolve the structure path,
//! construct `foldit_core::App`, hand it to the winit/wry event
//! loop in `window::run`. All host-agnostic state and dispatch logic
//! lives in `foldit-core`; this binary owns only the desktop shell.
//!
//! Controls:
//!   W - Wiggle (Rosetta minimize, toggle on/off)
//!   S - Shake (Rosetta repack sidechains, toggle on/off)
//!   P - Predict (`RoseTTAFold3` structure prediction)
//!   M - MPNN (design sequence for structure)
//!   I - Toggle water and ion visibility
//!   Q - Recenter camera on focused entity
//!   T - Toggle trajectory playback (load with --trajectory <path.dcd>)
//!   Tab - Cycle focus (Session -> Structure 1 -> ... -> Session)
//!   Backtick key - Reset focus to full scene
//!   Esc - Cancel operation / clear selection / clear bands
//!   Left-drag on residue - Pull
//!   Right-drag residue to residue - Create band
//!   Mouse - Rotate/zoom camera

mod host;
#[cfg(any(not(debug_assertions), test))]
mod plugin_assets;
mod tee_logger;
mod webview_assets;
mod window;

use foldit_core::App;

/// Platform file name of the python-host cdylib (the worker dlopens it by this
/// name). Mirrors `xtask`'s `python_host_lib_name`.
#[cfg(target_os = "macos")]
const PYTHON_HOST_DYLIB_NAME: &str = "libfoldit_python_host.dylib";
#[cfg(target_os = "windows")]
const PYTHON_HOST_DYLIB_NAME: &str = "foldit_python_host.dll";
#[cfg(target_os = "linux")]
const PYTHON_HOST_DYLIB_NAME: &str = "libfoldit_python_host.so";

/// Locate the packaged resource directory and point the resource resolvers at
/// it via their env overrides. Probes a small candidate list relative to the
/// executable (macOS `Contents/MacOS` -> `../Resources`; Linux `usr/bin` ->
/// `../lib/foldit` or `../share/foldit`; Windows / flat bundle -> next to the
/// exe), picking the first that actually contains `assets/gui`. Each override
/// is a LEAF path (the asset dir itself), matching the resolver contracts. A
/// pre-existing env value always wins, and a dev `cargo run` matches no
/// candidate, so this is a no-op outside a real bundle.
fn init_bundle_resource_paths() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let Some(exe_dir) = exe.parent() else {
        return;
    };

    let candidates = [
        exe_dir.join("../Resources"),
        exe_dir.join("../lib/foldit"),
        exe_dir.join("../share/foldit"),
        exe_dir.to_path_buf(),
    ];
    let Some(root) = candidates
        .into_iter()
        .find(|c| c.join("assets/gui").is_dir())
    else {
        return; // not a recognized bundle layout; keep default resolution
    };
    // Normalize away the `..` so logged/resolved paths are clean.
    let root = std::fs::canonicalize(&root).unwrap_or(root);

    let set_if_unset = |key: &str, path: std::path::PathBuf| {
        if std::env::var_os(key).is_none() {
            if let Some(s) = path.to_str() {
                std::env::set_var(key, s);
            }
        }
    };
    set_if_unset("FOLDIT_GUI_ROOT", root.join("assets/gui"));
    set_if_unset("FOLDIT_VIEW_PRESETS_DIR", root.join("assets/view_presets"));
    set_if_unset("FOLDIT_LEVELS_ROOT", root.join("assets/levels"));
    set_if_unset("FOLDIT_SCORING_DIR", root.join("assets/scoring"));
    set_if_unset("FOLDIT_PLUGINS_ROOT", root.join("plugins"));

    // The python-host dylib is found next to the worker exe by default; when it
    // ships as a resource instead, point the worker (which inherits this env)
    // at it explicitly.
    let dylib = root.join(PYTHON_HOST_DYLIB_NAME);
    if dylib.is_file() {
        set_if_unset("FOLDIT_PYTHON_HOST_DYLIB", dylib);
    }
}

fn main() {
    // When launched from a packaged bundle (macOS .app, Linux AppImage/deb,
    // Windows installer), the read-only assets and plugins live in a
    // platform-specific resource directory, not next to the executable. The
    // resolvers default to "next to the exe", so point their env overrides at
    // the resource root before anything reads them. Must run before threads
    // spawn (set_var is not thread-safe) and before the first resolver fires.
    init_bundle_resource_paths();

    let default_filter = "info,wgpu_hal::vulkan::instance=off,naga=warn";
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| default_filter.to_owned());
    let log_buffer = tee_logger::init(&filter);

    // Install signal handlers that kill ML worker process groups on
    // SIGINT/SIGTERM, preventing orphaned Python subprocesses.
    foldit_runner::install_cleanup_signal_handlers();

    log::info!("Foldit starting...");

    // A CLI argument names the structure to load on startup; without one
    // the App starts at the menus rather than auto-loading anything.
    let structure_path = std::env::args().nth(1).map(|input| {
        match foldit_core::puzzle::resolve_structure_path(&input) {
            Ok(path) => {
                log::info!("Loading structure from: {path}");
                path
            }
            Err(e) => {
                log::error!("{e}");
                std::process::exit(1);
            }
        }
    });
    if structure_path.is_none() {
        log::info!("No structure argument; starting at the menus.");
    }

    let host = Box::new(host::DesktopHost::new(structure_path));
    let app = App::new(host);
    window::run(app, log_buffer);
}
