// build.rs for foldit-python-host
//
// libpython linkage lives here, NOT in foldit-runner. This cdylib is
// what brings libpython into the process when it's dlopened — the
// `foldit-worker` binary is pyo3-free.
//
// Resolution order for libpython:
//   1. `$CONDA_PREFIX/lib` if it contains libpython3.12.{dylib,so} — set
//      automatically by `pixi run -e <env> cargo build`.
//   2. Scan `.pixi/envs/*/lib` for the first env with libpython3.12. Lets
//      `cargo build` outside `pixi run` still work as long as at least one env
//      is materialized.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-env-changed=CONDA_PREFIX");

    if cfg!(target_arch = "wasm32") {
        return Ok(());
    }

    if cfg!(target_os = "macos") || cfg!(target_os = "linux") {
        link_libpython()?;
    }

    Ok(())
}

/// Resolve libpython and emit the cargo link directives. Split out of
/// `main` so each half stays readable; only called on macOS / Linux.
fn link_libpython() -> Result<(), Box<dyn std::error::Error>> {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")?;
    // python-host lives at crates/foldit-runner/python-host/.
    // Project root is two levels up.
    let project_root = std::path::Path::new(&manifest_dir)
        .parent()
        .ok_or("python-host manifest dir has no parent")?;

    let libpython_filename = if cfg!(target_os = "macos") {
        "libpython3.12.dylib"
    } else {
        "libpython3.12.so"
    };

    let resolved_pixi_lib = std::env::var_os("CONDA_PREFIX")
        .map(std::path::PathBuf::from)
        .map(|p| p.join("lib"))
        .filter(|lib| lib.join(libpython_filename).exists())
        .or_else(|| {
            let envs_dir = project_root.join(".pixi").join("envs");
            if !envs_dir.is_dir() {
                return None;
            }
            std::fs::read_dir(&envs_dir)
                .ok()?
                .filter_map(std::result::Result::ok)
                .find_map(|e| {
                    let lib = e.path().join("lib");
                    if lib.join(libpython_filename).exists() {
                        Some(lib)
                    } else {
                        None
                    }
                })
        });

    let Some(pixi_lib_path) = resolved_pixi_lib else {
        println!(
            "cargo:warning=foldit-python-host: no pixi env with \
             {libpython_filename} found. Set CONDA_PREFIX (e.g. run via `pixi \
             run -e <env> cargo build`) or materialize a plugin env first."
        );
        return Ok(());
    };

    let absolute_path = pixi_lib_path.to_string_lossy().to_string();

    println!("cargo:rustc-link-arg=-Wl,-rpath,{absolute_path}");
    println!("cargo:rustc-link-search={absolute_path}");

    let python_config_path =
        pixi_lib_path
            .join("python3.12")
            .join(if cfg!(target_os = "macos") {
                "config-3.12-darwin"
            } else {
                "config-3.12-linux"
            });
    if python_config_path.exists() {
        println!(
            "cargo:rustc-link-search={}",
            python_config_path.to_string_lossy()
        );
    }

    println!("cargo:rustc-link-lib=python3.12");

    if cfg!(target_os = "linux") {
        println!("cargo:rustc-link-lib=dl");
    }

    Ok(())
}
