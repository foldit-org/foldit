//! Platform-specific Python functionality

// Only the Linux/macOS paths touch LD_LIBRARY_PATH / DYLD_LIBRARY_PATH;
// on Windows the DLL search path is configured via the WinAPI below.
#[cfg(unix)]
use std::env;
use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::{ffi::OsStr, os::windows::ffi::OsStrExt};

use anyhow::Result;
use log;

/// Add Python library directories to platform-specific search paths
///
/// This must be called before initializing the Python interpreter.
///
/// # Errors
///
/// Returns `Err` if the platform-specific search-path configuration fails
/// (e.g. a Windows `AddDllDirectory` call cannot be completed).
pub fn add_library_search_paths(
    python_home: &Path,
    python_paths: &[PathBuf],
) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        add_linux_library_paths(python_home, python_paths)?;
    }

    #[cfg(target_os = "macos")]
    {
        add_macos_library_paths(python_home, python_paths)?;
    }

    #[cfg(windows)]
    {
        add_windows_dll_directories(python_home, python_paths)?;
    }

    Ok(())
}

/// Linux-specific library path configuration
#[cfg(target_os = "linux")]
#[allow(
    clippy::unnecessary_wraps,
    reason = "Result return mirrors the macOS/Windows variants so the \
              cross-platform caller stays uniform, even though this path is \
              currently infallible"
)]
fn add_linux_library_paths(
    python_home: &Path,
    _python_paths: &[PathBuf],
) -> Result<()> {
    let mut lib_dirs = vec![
        python_home.join("lib"),
        python_home.to_path_buf(),
        python_home.parent().unwrap_or(python_home).to_path_buf(), /* bundle root */
    ];

    // Add torch/lib - find the python3.XX directory dynamically.
    // read_dir errors (incl. a missing dir) skip the block.
    let lib_dir = python_home.join("lib");
    if let Ok(entries) = std::fs::read_dir(&lib_dir) {
        for entry in entries.filter_map(|e| e.ok()) {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if !(name_str.starts_with("python3") && entry.path().is_dir()) {
                continue;
            }
            let torch_lib =
                entry.path().join("site-packages").join("torch").join("lib");
            if torch_lib.exists() {
                lib_dirs.push(torch_lib);
            }
            break;
        }
    }

    let existing = env::var("LD_LIBRARY_PATH").unwrap_or_default();
    let mut new_paths: Vec<String> = Vec::new();

    for dir in lib_dirs {
        if dir.exists() {
            new_paths.push(dir.to_string_lossy().to_string());
            log::info!("Adding to LD_LIBRARY_PATH: {}", dir.display());
        }
    }

    if !new_paths.is_empty() {
        let new_ld_path = if existing.is_empty() {
            new_paths.join(":")
        } else {
            format!("{}:{}", new_paths.join(":"), existing)
        };

        env::set_var("LD_LIBRARY_PATH", &new_ld_path);
        log::info!("Updated LD_LIBRARY_PATH");
    }

    Ok(())
}

/// macOS-specific library path configuration
#[cfg(target_os = "macos")]
#[allow(
    clippy::unnecessary_wraps,
    reason = "Result return mirrors the Linux/Windows variants so the \
              cross-platform caller stays uniform, even though this path is \
              currently infallible"
)]
fn add_macos_library_paths(
    python_home: &Path,
    _python_paths: &[PathBuf],
) -> Result<()> {
    let mut lib_dirs = vec![
        python_home.join("lib"),
        python_home.to_path_buf(),
        python_home.parent().unwrap_or(python_home).to_path_buf(), /* bundle root */
    ];

    // Add torch/lib - find the python3.XX directory dynamically.
    // read_dir errors (incl. a missing dir) skip the block.
    let lib_dir = python_home.join("lib");
    if let Ok(entries) = std::fs::read_dir(&lib_dir) {
        for entry in entries.filter_map(std::result::Result::ok) {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if !(name_str.starts_with("python3") && entry.path().is_dir()) {
                continue;
            }
            let torch_lib =
                entry.path().join("site-packages").join("torch").join("lib");
            if torch_lib.exists() {
                lib_dirs.push(torch_lib);
            }
            break;
        }
    }

    let existing = env::var("DYLD_LIBRARY_PATH").unwrap_or_default();
    let mut new_paths: Vec<String> = Vec::new();

    for dir in lib_dirs {
        if dir.exists() {
            new_paths.push(dir.to_string_lossy().to_string());
            log::info!("Adding to DYLD_LIBRARY_PATH: {}", dir.display());
        }
    }

    if !new_paths.is_empty() {
        let new_dyld_path = if existing.is_empty() {
            new_paths.join(":")
        } else {
            format!("{}:{}", new_paths.join(":"), existing)
        };

        env::set_var("DYLD_LIBRARY_PATH", &new_dyld_path);
        log::info!("Updated DYLD_LIBRARY_PATH");
    }

    Ok(())
}

/// Windows-specific DLL path configuration
#[cfg(windows)]
#[allow(
    clippy::unnecessary_wraps,
    reason = "Result return mirrors the macOS/Linux variants so the \
              cross-platform caller stays uniform, even though this path is \
              currently infallible"
)]
fn add_windows_dll_directories(
    python_home: &Path,
    _python_paths: &[PathBuf],
) -> Result<()> {
    fn to_wide_string(path: &Path) -> Vec<u16> {
        OsStr::new(path)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    // Ensure AddDllDirectory paths are included in LoadLibrary search order.
    // Without this call, AddDllDirectory paths may not be searched on all
    // Windows versions.
    const LOAD_LIBRARY_SEARCH_DEFAULT_DIRS: u32 = 0x00001000;
    unsafe {
        windows_sys::Win32::System::LibraryLoader::SetDefaultDllDirectories(
            LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
        );
    }
    log::info!(
        "SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_DEFAULT_DIRS) called"
    );

    // Build list of directories to add
    let mut dll_dirs = vec![
        python_home.to_path_buf(), // env dir (contains Lib/ and DLLs/)
        python_home.parent().unwrap_or(python_home).to_path_buf(), /* env parent (sibling libs) */
        python_home.join("Library").join("bin"), // conda Library/bin
    ];

    // Add torch/lib from ml-envs/Lib/torch/lib
    let torch_lib = python_home.join("Lib").join("torch").join("lib");
    if torch_lib.exists() {
        dll_dirs.push(torch_lib);
    }

    // Add .libs directories for packages like numpy, scipy, pandas, rdkit
    // These contain binary dependencies (e.g., OpenBLAS for numpy)
    let lib_dir = python_home.join("Lib");
    if lib_dir.exists() {
        if let Ok(entries) = std::fs::read_dir(&lib_dir) {
            for entry in entries.filter_map(|e| e.ok()) {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if name_str.ends_with(".libs") && entry.path().is_dir() {
                    dll_dirs.push(entry.path());
                }
            }
        }
    }

    for dir in dll_dirs {
        if dir.exists() {
            let wide_path = to_wide_string(&dir);

            unsafe {
                let cookie =
                    windows_sys::Win32::System::LibraryLoader::AddDllDirectory(
                        wide_path.as_ptr(),
                    );

                if cookie.is_null() {
                    log::warn!(
                        "Failed to add DLL directory: {}",
                        dir.display()
                    );
                } else {
                    log::info!("Added DLL search directory: {}", dir.display());
                }
            }
        }
    }

    Ok(())
}

/// Check if a directory looks like a valid Python home
#[must_use]
pub fn is_valid_python_home(path: &Path) -> bool {
    #[cfg(windows)]
    {
        path.join("python312.dll").exists()
    }

    #[cfg(target_os = "linux")]
    {
        path.join("lib").join("libpython3.12.so").exists()
            || path.join("libpython3.12.so").exists()
    }

    #[cfg(target_os = "macos")]
    {
        path.join("lib").join("libpython3.12.dylib").exists()
            || path.join("libpython3.12.dylib").exists()
    }

    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        false
    }
}

/// Find Python executable path given Python home directory
///
/// Returns `Some(path)` if a Python executable is found, `None` otherwise.
#[must_use]
pub fn find_python_executable(python_home: &Path) -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        let exe_path = python_home.join("python.exe");
        if exe_path.is_file() {
            return Some(exe_path);
        }
        // In pixi environments on Windows, python.exe is in the Scripts
        // subdirectory
        let scripts_exe = python_home.join("Scripts").join("python.exe");
        if scripts_exe.is_file() {
            return Some(scripts_exe);
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let bin_python = python_home.join("bin").join("python");
        if bin_python.is_file() {
            return Some(bin_python);
        }
        // Fallback: python in same directory (embedded)
        let python = python_home.join("python");
        if python.is_file() {
            return Some(python);
        }
    }
    None
}
