use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::Path;
use std::process::Command;

fn rosetta_interactive_path() -> String {
    "crates/foldit-runner/external/rosetta-interactive".to_string()
}

fn rosetta_lib_name() -> &'static str {
    #[cfg(target_os = "windows")]
    { "rosetta_interactive.dll" }
    #[cfg(target_os = "macos")]
    { "librosetta_interactive.dylib" }
    #[cfg(target_os = "linux")]
    { "librosetta_interactive.so" }
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
    /// Copy Rosetta resources to bundle (lib + database)
    SetupRosettaInteractive,
    /// Create distribution bundle
    Bundle {
        #[arg(long)]
        cpu_only: bool,
    },
    /// Download ML model weights
    DownloadModels,
    /// Rebuild molex Python wheel from local source
    BuildMolex,
    /// Build the frontend (pnpm build) and copy dist to assets/gui
    BuildFrontend,
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
        Commands::SetupRosettaInteractive => setup_rosetta_interactive(),
        Commands::Bundle { cpu_only } => bundle(cpu_only),
        Commands::DownloadModels => download_models(),
        Commands::BuildMolex => build_molex(),
        Commands::BuildFrontend => build_frontend(),
    }
}

fn python_lib_name() -> &'static str {
    #[cfg(target_os = "windows")]
    { "python312.dll" }
    #[cfg(target_os = "macos")]
    { "libpython3.12.dylib" }
    #[cfg(target_os = "linux")]
    { "libpython3.12.so" }
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

    // Copy Python shared library to assets/libs/ (mirrors the Rosetta pattern).
    // All pixi envs share the same Python 3.12 runtime, so we just need to find
    // any installed env (GPU or CPU variant).
    let pixi_envs_dir = Path::new("crates/foldit-runner/.pixi/envs");
    let candidate_envs = ["foundry", "foundry-cpu", "simplefold", "simplefold-cpu"];
    let lib_name = python_lib_name();

    let python_lib_src = candidate_envs.iter().find_map(|env_name| {
        let env_path = pixi_envs_dir.join(env_name);
        #[cfg(target_os = "windows")]
        let lib_path = env_path.join(lib_name);
        #[cfg(not(target_os = "windows"))]
        let lib_path = env_path.join("lib").join(lib_name);
        if lib_path.exists() { Some(lib_path) } else { None }
    });

    if let Some(python_lib_src) = python_lib_src {
        std::fs::create_dir_all("assets/libs")?;
        let python_lib_dst = format!("assets/libs/{}", lib_name);
        std::fs::copy(&python_lib_src, &python_lib_dst)?;
        println!("Copied {} -> {}", python_lib_src.display(), python_lib_dst);
    } else {
        println!(
            "Warning: Python library ({}) not found in any pixi env (expected after pixi setup)",
            lib_name
        );
    }

    // On Windows, also copy the import library if present (for future link-time use)
    #[cfg(target_os = "windows")]
    {
        let import_lib_src = candidate_envs.iter().find_map(|env_name| {
            let path = pixi_envs_dir.join(env_name).join("libs").join("python312.lib");
            if path.exists() { Some(path) } else { None }
        });
        if let Some(import_lib_src) = import_lib_src {
            let import_lib_dst = "assets/libs/python312.lib";
            std::fs::copy(&import_lib_src, import_lib_dst)?;
            println!("Copied {} -> {}", import_lib_src.display(), import_lib_dst);
        }
    }

    // Build the worker binary so it's next to the main executable.
    // Without this, MLClient::new() fails at runtime because
    // find_worker_binary() can't locate foldit-runner-worker.
    // Scope to -p foldit-runner so the build doesn't drag foldit-rs's
    // viso/wgpu dep graph into a backend-only binary.
    println!("Building foldit-runner-worker binary...");
    let status = Command::new("cargo")
        .args(["build", "-p", "foldit-runner", "--bin", "foldit-runner-worker"])
        .status()?;
    if !status.success() {
        anyhow::bail!("Failed to build foldit-runner-worker");
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
    let release_dir = format!("{}/release", cmake_dir);

    if !Path::new(&cmake_dir).exists() {
        anyhow::bail!(
            "Rosetta cmake directory not found at {}. \
             Make sure ~/rosetta-interactive is set up correctly.",
            cmake_dir
        );
    }

    // Create release directory if needed
    std::fs::create_dir_all(&release_dir)?;

    // Run cmake configure if needed
    let cache_file = format!("{}/CMakeCache.txt", release_dir);
    if !Path::new(&cache_file).exists() {
        println!("Configuring Rosetta cmake build...");
        let status = Command::new("cmake")
            .args(["-G", "Ninja", "-DCMAKE_BUILD_TYPE=Release", ".."])
            .current_dir(&release_dir)
            .status()?;
        if !status.success() {
            anyhow::bail!("Failed to configure Rosetta cmake build");
        }
    }

    // Build
    println!("Building Rosetta (this may take a while)...");
    let status = Command::new("ninja")
        .current_dir(&release_dir)
        .status()?;
    if !status.success() {
        anyhow::bail!("Failed to build Rosetta");
    }

    // Copy library into assets/libs/
    let lib_src = format!("{}/release/bin/{}", cmake_dir, rosetta_lib_name());
    let lib_dst = format!("assets/libs/{}", rosetta_lib_name());
    if Path::new(&lib_src).exists() {
        std::fs::create_dir_all("assets/libs")?;
        std::fs::copy(&lib_src, &lib_dst)?;
        println!("Copied {} -> {}", lib_src, lib_dst);
    } else {
        anyhow::bail!("Built library not found at {}", lib_src);
    }

    // On Windows, also copy the import library (.lib) needed for linking at build time
    #[cfg(target_os = "windows")]
    {
        let import_lib_src = format!("{}/release/bin/rosetta_interactive.lib", cmake_dir);
        let import_lib_dst = "assets/libs/rosetta_interactive.lib";
        if Path::new(&import_lib_src).exists() {
            std::fs::copy(&import_lib_src, import_lib_dst)?;
            println!("Copied {} -> {}", import_lib_src, import_lib_dst);
        } else {
            anyhow::bail!(
                "Import library not found at {}. \
                 Check that the CMake build produced a .lib file.",
                import_lib_src
            );
        }
    }

    // Copy compact database to assets/database (generated by make_database.py)
    let db_src = format!("{}/cmp-database/database", cmake_dir);
    if Path::new(&db_src).exists() {
        let db_dst = "assets/database";
        if Path::new(db_dst).exists() {
            std::fs::remove_dir_all(db_dst)?;
        }
        std::fs::create_dir_all("assets")?;
        copy_dir(&db_src, db_dst)?;
        println!("Copied compact database -> {}", db_dst);
    } else {
        println!(
            "Warning: Compact database not found at {}",
            db_src
        );
        println!(
            "  Run 'python3 make_database.py' in {}/source/cmake_4/ first",
            rosetta_interactive_path()
        );
    }

    println!("Rosetta build complete.");
    Ok(())
}

fn setup_rosetta_interactive() -> Result<()> {
    let rosetta_path = rosetta_interactive_path();

    // Source paths
    let lib_src = format!("{}/source/cmake_4/release/bin/{}", rosetta_path, rosetta_lib_name());
    let db_src = "assets/database";

    // Destination paths
    let lib_dst = format!("bundle/{}", rosetta_lib_name());
    let db_dst = "bundle/rosetta_database";

    // Check source exists
    if !Path::new(&lib_src).exists() {
        anyhow::bail!(
            "Rosetta library not found at {}. \
             Run 'cargo xtask build-rosetta-interactive' first.",
            lib_src
        );
    }

    if !Path::new(db_src).exists() {
        anyhow::bail!(
            "Rosetta database not found at {}. \
             Run 'cargo xtask build-rosetta-interactive' first (requires make_database.py to have been run).",
            db_src
        );
    }

    // Create bundle directory if needed
    std::fs::create_dir_all("bundle")?;

    // Copy library
    println!("Copying Rosetta library...");
    std::fs::copy(&lib_src, &lib_dst)?;
    println!("  {} -> {}", lib_src, lib_dst);

    // Remove old database if exists
    if Path::new(db_dst).exists() {
        std::fs::remove_dir_all(db_dst)?;
    }

    // Copy database
    println!("Copying Rosetta database...");
    copy_dir(db_src, db_dst)?;
    println!("  {} -> {}", db_src, db_dst);

    println!("Rosetta interactive setup complete.");
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

    let worker_name = format!("foldit-runner-worker{}", exe_ext);
    let worker_src = format!("target/release/{}", worker_name);
    if std::path::Path::new(&worker_src).exists() {
        std::fs::copy(&worker_src, format!("bundle/{}", worker_name))?;
        println!("Copied {} to bundle.", worker_name);
    }

    let app_name = format!("foldit-rs{}", exe_ext);
    let app_src = format!("target/release/{}", app_name);
    if std::path::Path::new(&app_src).exists() {
        std::fs::copy(&app_src, format!("bundle/{}", app_name))?;
        println!("Copied {} to bundle.", app_name);
    }

    // 5. Copy Rosetta resources
    let lib_src = format!("assets/libs/{}", rosetta_lib_name());

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
        println!("  Run 'cargo xtask build-frontend' first");
    }

    println!("Bundle ready at ./bundle/");
    Ok(())
}

fn build_frontend() -> Result<()> {
    let frontend_dir = "crates/foldit-frontend/js";
    println!("Building frontend...");

    #[cfg(windows)]
    let status = Command::new("cmd")
        .args(["/c", "pnpm", "build"])
        .current_dir(frontend_dir)
        .status()?;
    #[cfg(unix)]
    let status = Command::new("pnpm")
        .arg("build")
        .current_dir(frontend_dir)
        .status()?;

    if !status.success() {
        anyhow::bail!("Failed to build frontend");
    }

    let dist_dir = format!("{}/dist", frontend_dir);
    let gui_dir = "assets/gui";

    if !Path::new(&dist_dir).exists() {
        anyhow::bail!("Frontend dist directory not found at {}", dist_dir);
    }

    // Remove old assets/gui if it exists
    if Path::new(gui_dir).exists() {
        std::fs::remove_dir_all(gui_dir)?;
    }
    std::fs::create_dir_all(gui_dir)?;

    copy_dir(&dist_dir, gui_dir)?;
    println!("Frontend built and copied to {}", gui_dir);
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
