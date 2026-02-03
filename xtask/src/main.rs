use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::Path;
use std::process::Command;

fn rosetta_interactive_path() -> String {
    format!("{}/rosetta-interactive", std::env::var("HOME").unwrap_or_default())
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
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::SetupMl => setup_ml(),
        Commands::BuildRosettaInteractive => build_rosetta_interactive(),
        Commands::SetupRosettaInteractive => setup_rosetta_interactive(),
        Commands::Bundle { cpu_only } => bundle(cpu_only),
        Commands::DownloadModels => download_models(),
    }
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
    println!("ML environments setup complete.");
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

    println!("Rosetta build complete.");
    Ok(())
}

fn setup_rosetta_interactive() -> Result<()> {
    let rosetta_path = rosetta_interactive_path();

    // Source paths
    let lib_src = format!("{}/source/cmake_4/release/bin/librosetta_interactive.dylib", rosetta_path);
    let db_src = format!("{}/database", rosetta_path);

    // Destination paths
    let lib_dst = "bundle/librosetta_interactive.dylib";
    let db_dst = "bundle/rosetta_database";

    // Check source exists
    if !Path::new(&lib_src).exists() {
        anyhow::bail!(
            "Rosetta library not found at {}. \
             Run 'cargo xtask build-rosetta-interactive' first, or build manually with ./build.sh in ~/rosetta-interactive.",
            lib_src
        );
    }

    if !Path::new(&db_src).exists() {
        anyhow::bail!(
            "Rosetta database not found at {}. \
             Make sure ~/rosetta-interactive/database exists.",
            db_src
        );
    }

    // Create bundle directory if needed
    std::fs::create_dir_all("bundle")?;

    // Copy library
    println!("Copying Rosetta library...");
    std::fs::copy(&lib_src, lib_dst)?;
    println!("  {} -> {}", lib_src, lib_dst);

    // Remove old database if exists
    if Path::new(db_dst).exists() {
        std::fs::remove_dir_all(db_dst)?;
    }

    // Copy database
    println!("Copying Rosetta database...");
    copy_dir(&db_src, db_dst)?;
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
    if std::path::Path::new("target/release/foldit-ml-worker").exists() {
        std::fs::copy("target/release/foldit-ml-worker", "bundle/foldit-ml-worker")?;
        println!("Copied foldit-ml-worker to bundle.");
    }

    if std::path::Path::new("target/release/foldit-rs").exists() {
        std::fs::copy("target/release/foldit-rs", "bundle/foldit-rs")?;
        println!("Copied foldit-rs to bundle.");
    }

    // 5. Copy Rosetta resources from ~/rosetta-interactive
    let rosetta_path = rosetta_interactive_path();
    let lib_src = format!("{}/source/cmake_4/release/bin/librosetta_interactive.dylib", rosetta_path);
    let db_src = format!("{}/database", rosetta_path);

    if Path::new(&lib_src).exists() {
        println!("Copying Rosetta library...");
        std::fs::copy(&lib_src, "bundle/librosetta_interactive.dylib")?;
    } else {
        println!("Warning: Rosetta library not found at {}", lib_src);
        println!("  Run 'cargo xtask build-rosetta-interactive' or './build.sh' in ~/rosetta-interactive");
    }

    if Path::new(&db_src).exists() {
        println!("Copying Rosetta database...");
        copy_dir(&db_src, "bundle/rosetta_database")?;
    } else {
        println!("Warning: Rosetta database not found at {}", db_src);
    }

    println!("Bundle ready at ./bundle/");
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
        let status = Command::new("xcopy")
            .args([src, dst, "/E", "/I"])
            .status()?;
        if !status.success() {
            anyhow::bail!("Failed to copy {} to {}", src, dst);
        }
    }
    Ok(())
}
