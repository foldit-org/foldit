//! Worker binary location utilities

use std::path::PathBuf;

use anyhow::Result;
use log;

/// Get the worker binary name with platform-specific extension
#[must_use]
pub fn worker_binary_name() -> String {
    if std::env::consts::EXE_EXTENSION.is_empty() {
        String::from("foldit-worker")
    } else {
        format!("foldit-worker.{}", std::env::consts::EXE_EXTENSION)
    }
}

/// Find the worker binary path.
///
/// Search logic:
/// 1. If `binary_dir` is provided, look there (FFI mode)
/// 2. Otherwise, look next to the current executable (server/bundle mode)
///
/// # Errors
///
/// Returns an error if the worker binary isn't found in either location,
/// or if `current_exe()` returns a path with no parent directory.
pub fn find_worker_binary(binary_dir: Option<&str>) -> Result<PathBuf> {
    let binary_name = worker_binary_name();
    log::debug!("Worker binary name: {binary_name}");

    // 1. Check provided binary directory (FFI mode or explicit override)
    if let Some(dir) = binary_dir {
        let dir_path = PathBuf::from(dir);
        let path = dir_path.join(&binary_name);
        if path.exists() {
            log::info!(
                "Found worker binary at provided directory: {}",
                path.display()
            );
            return Ok(path);
        }
        log::warn!(
            "Worker binary not found at provided directory: {}",
            dir_path.display()
        );
    }

    // 2. Check same directory as current executable (server/bundle mode).
    let exe_dir = std::env::current_exe()?
        .parent()
        .ok_or_else(|| {
            anyhow::anyhow!("current exe path has no parent directory")
        })?
        .to_path_buf();
    let path = exe_dir.join(&binary_name);
    if path.exists() {
        log::info!(
            "Found worker binary next to executable: {}",
            path.display()
        );
        return Ok(path);
    }

    // 3. Under `cargo test`, the running exe is a test binary inside
    // `target/<profile>/deps/`; cargo puts production binaries one
    // directory up at `target/<profile>/foldit-worker`. Try there
    // before giving up.
    if exe_dir.file_name().and_then(|n| n.to_str()) == Some("deps") {
        if let Some(parent) = exe_dir.parent() {
            let cargo_test_path = parent.join(&binary_name);
            if cargo_test_path.exists() {
                log::info!(
                    "Found worker binary at cargo-test parent dir: {}",
                    cargo_test_path.display()
                );
                return Ok(cargo_test_path);
            }
        }
    }

    log::error!(
        "Worker binary '{}' not found. Expected at: {}",
        binary_name,
        path.display()
    );
    anyhow::bail!(
        "Worker binary '{binary_name}' not found. Please ensure foldit-worker \
         is built and located next to the executable.",
    )
}
