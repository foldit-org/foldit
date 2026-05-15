//! Foldit-RS desktop binary.
//!
//! Thin entry: parse argv, set up logging, resolve the structure path,
//! construct `foldit_core::app::App`, hand it to the winit/wry event
//! loop in `window::run`. All host-agnostic state and dispatch logic
//! lives in `foldit-core`; this binary owns only the desktop shell.
//!
//! Controls:
//!   W - Wiggle (Rosetta minimize, toggle on/off)
//!   S - Shake (Rosetta repack sidechains, toggle on/off)
//!   P - Predict (RoseTTAFold3 structure prediction)
//!   M - MPNN (design sequence for structure)
//!   I - Toggle water and ion visibility
//!   Q - Recenter camera on focused entity
//!   T - Toggle trajectory playback (load with --trajectory <path.dcd>)
//!   Tab - Cycle focus (Session -> Structure 1 -> ... -> Session)
//!   ` (backtick) - Reset focus to full scene
//!   Esc - Cancel operation / clear selection / clear bands
//!   Left-drag on residue - Pull
//!   Right-drag residue to residue - Create band
//!   Mouse - Rotate/zoom camera

#[cfg(any(not(debug_assertions), test))]
mod plugin_assets;
mod tee_logger;
mod window;

use foldit_core::app::App;

fn main() {
    let default_filter = "info,wgpu_hal::vulkan::instance=off,naga=warn";
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| default_filter.to_string());
    let log_buffer = tee_logger::init(&filter);

    let input = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "1bfe".to_string());

    // Install signal handlers that kill ML worker process groups on
    // SIGINT/SIGTERM, preventing orphaned Python subprocesses.
    foldit_runner::install_cleanup_signal_handlers();

    log::info!("Foldit starting...");

    let pdb_path = match foldit_core::puzzle::resolve_structure_path(&input) {
        Ok(path) => path,
        Err(e) => {
            log::error!("{}", e);
            std::process::exit(1);
        }
    };

    log::info!("Loading structure from: {}", pdb_path);

    let app = App::new(pdb_path);
    window::run(app, foldit_gui::FrontendState::new(), log_buffer);
}
