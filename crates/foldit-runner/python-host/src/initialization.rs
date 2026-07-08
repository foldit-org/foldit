//! Python runtime initialization
//!
//! This module is only used when embedding Python (worker binary).
//! When built as a Python extension module, Python is already running.

use std::env;

use anyhow::Result;
use log;

use crate::library;

/// Initialize Python with the given configuration
///
/// This must be called before any `PyO3` operations. It:
/// 1. Registers the `foldit_runner` extension module with Python's inittab
/// 2. Sets up platform-specific DLL/library search paths
/// 3. Initializes the Python interpreter
/// 4. Adds configured paths to sys.path
///
/// NOTE: This function should only be called when embedding Python (worker
/// binary). When this library is loaded as a Python extension, Python is
/// already initialized.
///
/// # Errors
///
/// Returns `Err` if the library search paths cannot be configured, the
/// interpreter cannot import `sys`, or a configured path is not valid UTF-8.
pub fn initialize_python(
    config: &crate::python_config::PythonConfig,
) -> Result<()> {
    log::info!("Initializing Python with config: {config:?}");

    // Platform-specific: Add Python DLL/library directories to search path
    library::add_library_search_paths(
        &config.python_home,
        &config.python_paths,
    )?;

    // Set PYTHONHOME for PyO3
    env::set_var("PYTHONHOME", &config.python_home);

    // Set PYTHONEXECUTABLE for multiprocessing child processes
    if let Some(python_executable) =
        library::find_python_executable(&config.python_home)
    {
        log::info!(
            "Setting PYTHONEXECUTABLE to {}",
            python_executable.display()
        );
        env::set_var("PYTHONEXECUTABLE", python_executable);
    } else {
        log::warn!(
            "No Python executable found in {}, multiprocessing may fail",
            config.python_home.display()
        );
    }

    // Initialize Python interpreter. The legacy `foldit_runner` pymodule
    // registration was removed with the unified plugin migration — plugins
    // import from `foldit_plugin_sdk`, not from a foldit-runner-provided
    // pymodule.
    pyo3::Python::initialize();

    configure_sys_path(config)
}

/// Add the configured paths to `sys.path` and point `sys.executable` at the
/// bundled interpreter so multiprocessing children re-launch the right Python.
///
/// # Errors
///
/// Returns `Err` if `sys` cannot be imported or a configured path is not valid
/// UTF-8.
fn configure_sys_path(config: &crate::python_config::PythonConfig) -> Result<()> {
    // Add user paths to sys.path
    pyo3::Python::attach(|py| {
        use pyo3::types::{PyAnyMethods, PyListMethods};

        let sys = py
            .import("sys")
            .map_err(|e| anyhow::anyhow!("Failed to import sys: {e}"))?;

        // Set sys.executable to correct Python interpreter for multiprocessing
        if let Some(python_executable) =
            library::find_python_executable(&config.python_home)
        {
            let executable_str =
                python_executable.to_str().ok_or_else(|| {
                    anyhow::anyhow!("Python executable path is not valid UTF-8")
                })?;
            sys.setattr("executable", executable_str).map_err(|e| {
                anyhow::anyhow!("Failed to set sys.executable: {e}")
            })?;
            log::info!("Set sys.executable to {executable_str}");
        }
        let sys_path = sys
            .getattr("path")
            .map_err(|e| anyhow::anyhow!("Failed to get sys.path: {e}"))?;
        let path_list = sys_path
            .cast::<pyo3::types::PyList>()
            .map_err(|e| anyhow::anyhow!("sys.path is not a list: {e}"))?;

        for python_path in &config.python_paths {
            if python_path.exists() {
                let path_str = python_path.to_str().ok_or_else(|| {
                    anyhow::anyhow!("sys.path entry is not valid UTF-8")
                })?;
                path_list.insert(0, path_str).map_err(|e| {
                    anyhow::anyhow!("Failed to insert into sys.path: {e}")
                })?;
                log::info!("Added to sys.path: {}", python_path.display());
            } else {
                log::debug!(
                    "Path does not exist, skipping: {}",
                    python_path.display()
                );
            }
        }

        // Log Python version
        let version: String = sys
            .getattr("version")
            .map_err(|e| anyhow::anyhow!("Failed to get Python version: {e}"))?
            .extract()
            .map_err(|e| {
                anyhow::anyhow!("Failed to extract version string: {e}")
            })?;
        log::info!("Python initialized: {version}");

        Ok::<(), anyhow::Error>(())
    })?;

    Ok(())
}
