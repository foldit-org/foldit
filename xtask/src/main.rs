use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::Path;
use std::process::Command;

fn rosetta_interactive_path() -> String {
    "crates/foldit-runner/plugins/rosetta/deps/rosetta-interactive".to_string()
}

fn rosetta_lib_name() -> &'static str {
    #[cfg(target_os = "windows")]
    {
        "rosetta_interactive.dll"
    }
    #[cfg(target_os = "macos")]
    {
        "librosetta_interactive.dylib"
    }
    #[cfg(target_os = "linux")]
    {
        "librosetta_interactive.so"
    }
}

#[derive(Parser)]
#[command(name = "xtask")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Install Python environments in crates/foldit-runner
    SetupMl,
    /// Build Rosetta from ~/rosetta-interactive
    BuildRosettaInteractive,
    /// Create distribution bundle
    Bundle {
        #[arg(long)]
        cpu_only: bool,
    },
    /// Download ML model weights
    DownloadModels,
    /// Rebuild molex Python wheel from local source
    BuildMolex,
    /// Build the GUI (pnpm build) and copy dist to assets/gui
    BuildGui,
    /// Build the foldit-web cdylib + run wasm-bindgen, emit JS glue to
    /// `crates/foldit-gui/js/public/pkg/`. Requires nightly toolchain.
    BuildWeb {
        /// Build in debug mode (default: release)
        #[arg(long)]
        debug: bool,
    },
    /// Build the web wasm artifact AND run `pnpm build:web` to produce a
    /// static `dist/` ready for deployment.
    BundleWeb,
}

fn main() -> Result<()> {
    // Pin CWD to the workspace root so all relative paths resolve the
    // same way regardless of where the user invoked `cargo xtask` from.
    // The xtask crate lives at <workspace>/xtask, so the manifest dir's
    // parent is the workspace root.
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or_else(|| anyhow::anyhow!("xtask CARGO_MANIFEST_DIR has no parent"))?;
    std::env::set_current_dir(workspace_root)?;

    let cli = Cli::parse();

    match cli.command {
        Commands::SetupMl => setup_ml(),
        Commands::BuildRosettaInteractive => build_rosetta_interactive(),
        Commands::Bundle { cpu_only } => bundle(cpu_only),
        Commands::DownloadModels => download_models(),
        Commands::BuildMolex => build_molex(),
        Commands::BuildGui => build_gui(),
        Commands::BuildWeb { debug } => build_web(debug),
        Commands::BundleWeb => bundle_web(),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Web build: foldit-web cdylib → wasm-bindgen JS glue → public/pkg/
// ─────────────────────────────────────────────────────────────────────────────

/// `RUSTFLAGS` for the multithreaded wasm build. atomics + bulk-memory +
/// shared-memory are required for `wasm-bindgen-rayon` (which viso's
/// pipeline processor depends on). Cribbed from viso's xtask, which has
/// the same shape working in production on its GitHub Pages deploy.
const WASM_RUSTFLAGS: &str = "\
    -C target-feature=+atomics,+bulk-memory,+mutable-globals \
    -C link-arg=--shared-memory \
    -C link-arg=--import-memory \
    -C link-arg=--max-memory=1073741824 \
    -C link-arg=--export=__wasm_init_tls \
    -C link-arg=--export=__tls_size \
    -C link-arg=--export=__tls_align \
    -C link-arg=--export=__tls_base \
    -C link-arg=--export=__heap_base \
    -C link-arg=--export=__data_end";

fn build_web(debug: bool) -> Result<()> {
    let profile = if debug { "debug" } else { "release" };
    println!("Building foldit-web for wasm32-unknown-unknown ({profile})...");

    let mut args = vec![
        "+nightly",
        "build",
        "--target",
        "wasm32-unknown-unknown",
        "-p",
        "foldit-web",
        "-Z",
        "build-std=panic_abort,std,alloc",
    ];
    if !debug {
        args.push("--release");
    }

    let status = Command::new("cargo")
        .args(&args)
        .env("RUSTFLAGS", WASM_RUSTFLAGS)
        .status()?;
    if !status.success() {
        anyhow::bail!(
            "cargo build failed for foldit-web (wasm32). \
            Hint: nightly toolchain required (rustup toolchain install nightly)."
        );
    }

    let wasm_path = Path::new("target/wasm32-unknown-unknown")
        .join(profile)
        .join("foldit_web.wasm");
    if !wasm_path.exists() {
        anyhow::bail!("expected wasm artifact missing: {}", wasm_path.display());
    }

    let pkg_dir = Path::new("crates/foldit-gui/js/public/pkg");
    std::fs::create_dir_all(pkg_dir)?;

    println!("Running wasm-bindgen → {}", pkg_dir.display());
    let status = Command::new("wasm-bindgen")
        .args(["--target", "web", "--out-dir"])
        .arg(pkg_dir)
        .arg(&wasm_path)
        .status()
        .map_err(|e| {
            anyhow::anyhow!(
                "wasm-bindgen failed to invoke ({e}). Install: cargo install wasm-bindgen-cli"
            )
        })?;
    if !status.success() {
        anyhow::bail!("wasm-bindgen step failed");
    }

    println!("✓ foldit-web wasm built; JS glue at crates/foldit-gui/js/public/pkg/");
    Ok(())
}

fn bundle_web() -> Result<()> {
    build_web(false)?;

    let js_dir = Path::new("crates/foldit-gui/js");
    println!("Running pnpm build:web in {}...", js_dir.display());
    let status = Command::new("pnpm")
        .args(["build:web"])
        .current_dir(js_dir)
        .status()?;
    if !status.success() {
        anyhow::bail!("pnpm build:web failed");
    }

    println!("✓ web bundle ready at crates/foldit-gui/js/dist/");
    Ok(())
}

fn setup_ml() -> Result<()> {
    println!("Setting up ML environments (foundry + simplefold)...");
    let status = Command::new("pixi")
        .args(["run", "setup"])
        .current_dir("crates/foldit-runner")
        .status()?;
    if !status.success() {
        anyhow::bail!("Failed to setup ML environments");
    }
    // Note: molex is installed from PyPI by pixi (see pixi.toml's
    // `molex = ">=0.3.0"`). To rebuild from the local submodule, run
    // `cargo xtask build-molex` separately. Don't force a local
    // rebuild here — it's only needed when actively developing molex.

    // libpython is brought into the process via foldit-python-host's
    // build.rs (resolves via $CONDA_PREFIX or .pixi/envs/<env>/lib);
    // no host-binary link path, no copy to assets/libs/.

    // Build the worker binary so it's next to the main executable.
    // Without this, MLClient::new() fails at runtime because
    // find_worker_binary() can't locate foldit-worker.
    // Scope to -p foldit-runner so the build doesn't drag foldit's
    // viso/wgpu dep graph into a backend-only binary.
    println!("Building foldit-worker binary...");
    let status = Command::new("cargo")
        .args(["build", "-p", "foldit-runner", "--bin", "foldit-worker"])
        .status()?;
    if !status.success() {
        anyhow::bail!("Failed to build foldit-worker");
    }

    println!("ML environments setup complete.");
    Ok(())
}

fn build_molex() -> Result<()> {
    println!("Rebuilding molex wheel from local source...");
    for env in ["foundry", "simplefold"] {
        println!("  Installing into {} environment...", env);
        let status = Command::new("pixi")
            .args(["run", "--environment", env, "build-molex"])
            .current_dir("crates/foldit-runner")
            .status()?;
        if !status.success() {
            anyhow::bail!("Failed to install molex wheel in {} environment", env);
        }
    }
    println!("molex wheel rebuilt and installed in all environments.");
    Ok(())
}

fn build_rosetta_interactive() -> Result<()> {
    let rosetta_path = rosetta_interactive_path();
    let cmake_dir = format!("{}/source/cmake_4", rosetta_path);

    if !Path::new(&cmake_dir).exists() {
        anyhow::bail!(
            "Rosetta cmake directory not found at {}. \
             Make sure the rosetta-interactive submodule is initialized.",
            cmake_dir
        );
    }

    // Build the molex static lib first; the bridge dylib links against
    // it for ASSEM01 IO and Assembly walks. Invoke via
    // --manifest-path so cargo treats this as a standalone build,
    // bypassing the workspace resolver. crates/molex is workspace-
    // excluded; the workspace itself pins published molex ^0.3.0 for
    // foldit-core, which does not resolve against the in-tree 0.4.x.
    // Running outside the workspace sidesteps that conflict.
    println!("Building molex static library (release, c-api feature)...");
    let status = Command::new("cargo")
        .args([
            "build",
            "--manifest-path",
            "crates/molex/Cargo.toml",
            "--release",
            "--features",
            "c-api",
        ])
        .status()?;
    if !status.success() {
        anyhow::bail!("Failed to build molex static library");
    }
    let molex_include = std::fs::canonicalize("crates/molex/include")?;
    let molex_lib = std::fs::canonicalize("crates/molex/target/release/libmolex.a")?;
    let proto_dir = std::fs::canonicalize("crates/foldit-runner/proto")?;

    // Delegate the cmake configure + build (and the make.py /
    // make_database.py preprocessing) to rosetta-interactive's own
    // build.sh. Molex paths flow through env vars that build.sh
    // appends to its CMAKE_ARGS. This avoids drift between the
    // canonical script and a parallel xtask reimplementation; any
    // future build-flag change in build.sh propagates automatically.
    println!("Running build.sh in {}...", rosetta_path);
    let status = Command::new("./build.sh")
        .env("MOLEX_INCLUDE_DIR", molex_include.as_os_str())
        .env("MOLEX_STATIC_LIB", molex_lib.as_os_str())
        .env("FOLDIT_PROTO_DIR", proto_dir.as_os_str())
        .current_dir(&rosetta_path)
        .status()?;
    if !status.success() {
        anyhow::bail!("rosetta-interactive build.sh failed");
    }

    // Copy the dylib into the plugin directory. That's the single
    // sink: `Orchestrator::discover_plugins` + `spawn_native_worker`
    // dlopen it from there. No host-binary link path; the foldit-
    // plugin-vtable contract is the only surface.
    let lib_src = format!("{}/release/bin/{}", cmake_dir, rosetta_lib_name());
    if !Path::new(&lib_src).exists() {
        anyhow::bail!("Built library not found at {}", lib_src);
    }

    let plugin_dir = "crates/foldit-runner/plugins/rosetta";
    let lib_dst_plugin = format!("{}/{}", plugin_dir, rosetta_lib_name());
    std::fs::create_dir_all(plugin_dir)?;
    std::fs::copy(&lib_src, &lib_dst_plugin)?;
    println!("Copied {} -> {}", lib_src, lib_dst_plugin);

    // Copy compact database into the plugin's own assets dir, alongside
    // the dylib. Bridge `find_rosetta_database` walks up from the plugin
    // dir looking for `assets/database/`, so this is exactly where it
    // wants to find it.
    let db_src = format!("{}/cmp-database/database", cmake_dir);
    if Path::new(&db_src).exists() {
        let db_dst = format!("{}/assets/database", plugin_dir);
        if Path::new(&db_dst).exists() {
            std::fs::remove_dir_all(&db_dst)?;
        }
        std::fs::create_dir_all(format!("{}/assets", plugin_dir))?;
        copy_dir(&db_src, &db_dst)?;
        println!("Copied compact database -> {}", db_dst);
    } else {
        println!("Warning: Compact database not found at {}", db_src);
        println!(
            "  Run 'python3 make_database.py' in {}/source/cmake_4/ first",
            rosetta_interactive_path()
        );
    }

    println!("Rosetta build complete.");
    Ok(())
}

fn bundle(cpu_only: bool) -> Result<()> {
    // 1. Build release binaries
    println!("Building release binaries...");
    let status = Command::new("cargo")
        .args(["build", "--release"])
        .status()?;
    if !status.success() {
        anyhow::bail!("Failed to build release binaries");
    }

    // 2. Create ML bundle
    println!("Creating ML bundle...");
    let bundle_task = if cpu_only { "bundle" } else { "bundle-gpu" };
    let status = Command::new("pixi")
        .args(["run", "--environment", "foundry", bundle_task])
        .current_dir("crates/foldit-runner")
        .status()?;
    if !status.success() {
        anyhow::bail!("Failed to create ML bundle");
    }

    // 3. Copy ML bundle to root bundle directory
    println!("Assembling final bundle...");
    let _ = std::fs::remove_dir_all("bundle");
    copy_dir("crates/foldit-runner/bundle", "bundle")?;

    // 4. Copy Rust binaries
    let exe_ext = if cfg!(windows) { ".exe" } else { "" };

    let worker_name = format!("foldit-worker{}", exe_ext);
    let worker_src = format!("target/release/{}", worker_name);
    if std::path::Path::new(&worker_src).exists() {
        std::fs::copy(&worker_src, format!("bundle/{}", worker_name))?;
        println!("Copied {} to bundle.", worker_name);
    }

    let app_name = format!("foldit{}", exe_ext);
    let app_src = format!("target/release/{}", app_name);
    if std::path::Path::new(&app_src).exists() {
        std::fs::copy(&app_src, format!("bundle/{}", app_name))?;
        println!("Copied {} to bundle.", app_name);
    }

    // 5. Copy Rosetta resources. Source of truth is the plugin
    // directory, written by `build-rosetta-interactive`.
    let lib_src = format!(
        "crates/foldit-runner/plugins/rosetta/{}",
        rosetta_lib_name()
    );

    if Path::new(&lib_src).exists() {
        println!("Copying Rosetta library...");
        std::fs::copy(&lib_src, format!("bundle/{}", rosetta_lib_name()))?;
    } else {
        println!("Warning: Rosetta library not found at {}", lib_src);
        println!("  Run 'cargo xtask build-rosetta-interactive' first");
    }

    if Path::new("assets/database").exists() {
        println!("Copying Rosetta database...");
        copy_dir("assets/database", "bundle/rosetta_database")?;
    } else {
        println!("Warning: Rosetta database not found at assets/database");
        println!("  Run 'cargo xtask build-rosetta-interactive' first");
    }

    // 6. Copy frontend assets to bundle
    if Path::new("assets/gui").exists() {
        println!("Copying frontend assets...");
        copy_dir("assets/gui", "bundle/gui")?;
    } else {
        println!("Warning: Frontend assets not found at assets/gui");
        println!("  Run 'cargo xtask build-gui' first");
    }

    println!("Bundle ready at ./bundle/");
    Ok(())
}

fn build_gui() -> Result<()> {
    let gui_src_dir = "crates/foldit-gui/js";

    println!("Installing GUI dependencies...");
    #[cfg(windows)]
    let install_status = Command::new("cmd")
        .args(["/c", "pnpm", "install", "--frozen-lockfile"])
        .current_dir(gui_src_dir)
        .status()?;
    #[cfg(unix)]
    let install_status = Command::new("pnpm")
        .args(["install", "--frozen-lockfile"])
        .current_dir(gui_src_dir)
        .status()?;

    if !install_status.success() {
        anyhow::bail!("Failed to install GUI dependencies");
    }

    println!("Building GUI...");
    #[cfg(windows)]
    let status = Command::new("cmd")
        .args(["/c", "pnpm", "build"])
        .current_dir(gui_src_dir)
        .status()?;
    #[cfg(unix)]
    let status = Command::new("pnpm")
        .arg("build")
        .current_dir(gui_src_dir)
        .status()?;

    if !status.success() {
        anyhow::bail!("Failed to build GUI");
    }

    let dist_dir = format!("{}/dist", gui_src_dir);
    let gui_dir = "assets/gui";

    if !Path::new(&dist_dir).exists() {
        anyhow::bail!("GUI dist directory not found at {}", dist_dir);
    }

    // Remove old assets/gui if it exists
    if Path::new(gui_dir).exists() {
        std::fs::remove_dir_all(gui_dir)?;
    }
    std::fs::create_dir_all(gui_dir)?;

    copy_dir(&dist_dir, gui_dir)?;
    println!("GUI built and copied to {}", gui_dir);
    Ok(())
}

fn download_models() -> Result<()> {
    let status = Command::new("pixi")
        .args(["run", "--environment", "foundry", "download-foundry"])
        .current_dir("crates/foldit-runner")
        .status()?;
    if !status.success() {
        anyhow::bail!("Failed to download models");
    }
    Ok(())
}

fn copy_dir(src: &str, dst: &str) -> Result<()> {
    #[cfg(unix)]
    {
        let status = Command::new("cp").args(["-r", src, dst]).status()?;
        if !status.success() {
            anyhow::bail!("Failed to copy {} to {}", src, dst);
        }
    }
    #[cfg(windows)]
    {
        // robocopy exit codes < 8 indicate success (0=no change, 1=files copied, etc.)
        let status = Command::new("robocopy")
            .args([src, dst, "/E", "/NFL", "/NDL", "/NJH", "/NJS", "/NP"])
            .status()?;
        match status.code() {
            Some(code) if code < 8 => {}
            _ => anyhow::bail!("Failed to copy {} to {}", src, dst),
        }
    }
    Ok(())
}
