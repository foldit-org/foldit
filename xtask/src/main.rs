// foldit:allow-long-file -- CLI command dispatch; length-exempt.
#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "xtask is a CLI build tool; console output is its intended interface"
)]

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Command;

mod filters_transcribe;
use filters_transcribe::transcribe_filters;

/// Canonicalize to an absolute path, stripping Windows' `\\?\`
fn canonical_clean(path: impl AsRef<Path>) -> Result<std::path::PathBuf> {
    let canonical = std::fs::canonicalize(path)?;
    let s = canonical.to_string_lossy();
    let cleaned = if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        rest.to_owned()
    } else {
        return Ok(canonical);
    };
    Ok(std::path::PathBuf::from(cleaned))
}

/// Intermediate staging dir where `assemble` lays out the full payload
const STAGING: &str = "target/staging";

/// Platform-canonical file name for the python-host cdylib. The worker
/// dlopens this by filename next to its own executable.
const fn python_host_lib_name() -> &'static str {
    #[cfg(target_os = "windows")]
    {
        "foldit_python_host.dll"
    }
    #[cfg(target_os = "macos")]
    {
        "libfoldit_python_host.dylib"
    }
    #[cfg(target_os = "linux")]
    {
        "libfoldit_python_host.so"
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
    /// Fresh-clone bootstrap: build the backend host artifacts (release) then
    /// set up every plugin. One command to make `cargo run` work.
    Setup,
    /// Walk `plugins/`, read each plugin's `plugin.build.toml`, and run the
    /// setup work it declares: native recipe (installs the binary into
    /// `local/`), python env (`pixi install --all`), and/or ui bun build. An
    /// optional positional id restricts to one plugin; default = all.
    SetupPlugins {
        /// Restrict setup to the plugin with this id (its `plugins/<id>`
        /// directory). Omit to set up every plugin.
        id: Option<String>,
        /// Forwarded to native recipes (via `FOLDIT_RECIPE_CLEAN=1`) to wipe
        /// their build dir before building, so a configure step re-runs from
        /// scratch. Use after toolchain/compiler changes a cached build would
        /// otherwise reject. Implies `--from-source`.
        #[arg(long)]
        clean: bool,
        /// Force a from-source rebuild of the vendored artifacts (native
        /// binary, UI panel) even when a committed/built one is already
        /// present. Without it, present vendored artifacts are reused and only
        /// missing ones are built (e.g. a platform with no prebuilt binary yet).
        #[arg(long)]
        from_source: bool,
    },
    /// THE desktop prod path: assemble the staging payload then build OS
    /// installers via cargo-packager (macOS .app/.dmg, Windows NSIS/MSI, Linux
    /// deb/AppImage). Requires `cargo install cargo-packager`. Config lives in
    /// `packager.json`.
    Package {
        /// Comma-separated cargo-packager formats (e.g. "app,dmg"). Defaults
        /// to the current platform's defaults.
        #[arg(long)]
        formats: Option<String>,
        /// Skip the assembly step and package the existing staging payload
        /// as-is (fast iteration on packaging config).
        #[arg(long)]
        skip_assembly: bool,
    },
    /// Rebuild the molex Python extension from local source
    BuildMolex,
    /// Build the GUI (bun run build) and copy dist to assets/gui
    BuildGui,
    /// Build the backend host artifacts (foldit-worker + python-host dylib)
    /// into target/<profile>/ so they sit next to the foldit-desktop exe for
    /// `cargo run`. A plain `cargo run` rebuilds neither (the worker is a
    /// workspace-excluded bin; the dylib is dlopened, not linked), so an ABI
    /// bump leaves a stale dylib next to a fresh worker and plugins fail to
    /// load. Run this after pulling/changing runner code. Defaults to release.
    BuildHost {
        /// Build in debug mode (default: release)
        #[arg(long)]
        debug: bool,
    },
    /// Build the foldit-web cdylib + run wasm-bindgen, emit JS glue to
    /// `webview/public/pkg/`. Requires nightly toolchain.
    BuildWeb {
        /// Build in debug mode (default: release)
        #[arg(long)]
        debug: bool,
    },
    /// The web prod path: build the web wasm artifact AND run `bun run
    /// build:web` to produce a static site ready for deployment.
    PackageWeb,
    /// Transcribe the legacy rosetta `.ir_puzzle.filters` files into
    /// `[[puzzle.filter]]` blocks, and the pose-numbered `"can_design"` from
    /// each `.ir_puzzle.puzzle_setup` into per-chain `[[puzzle.design_mask]]`
    /// blocks (remapping rosetta pose numbering to per-chain PDB numbering via
    /// the structure's residue order), in the curated level assets. One-shot,
    /// idempotent: re-running replaces any existing arrays. Only touches levels
    /// where the curated `puzzle.toml` and the matching legacy source exist.
    TranscribeFilters,
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
        Commands::Setup => setup(),
        Commands::SetupPlugins {
            id,
            clean,
            from_source,
        } => setup_plugins(id.as_deref(), clean, from_source),
        Commands::Package {
            formats,
            skip_assembly,
        } => package(formats.as_deref(), skip_assembly),
        Commands::BuildMolex => build_molex(),
        Commands::BuildGui => build_gui(),
        Commands::BuildHost { debug } => build_host(debug),
        Commands::BuildWeb { debug } => build_web(debug),
        Commands::PackageWeb => package_web(),
        Commands::TranscribeFilters => transcribe_filters(),
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

    let pkg_dir = Path::new("webview/public/pkg");
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

    println!("✓ foldit-web wasm built; JS glue at webview/public/pkg/");
    Ok(())
}

fn package_web() -> Result<()> {
    build_web(false)?;

    let js_dir = Path::new("webview");
    println!("Running bun run build:web in {}...", js_dir.display());
    let status = Command::new("bun")
        .args(["run", "build:web"])
        .current_dir(js_dir)
        .status()?;
    if !status.success() {
        anyhow::bail!("bun run build:web failed");
    }

    println!("✓ web build ready at webview/dist/");
    Ok(())
}

/// Fresh-clone bootstrap: build the runtime host prerequisites once, then set
/// up every plugin.
fn setup() -> Result<()> {
    build_host(false)?;
    setup_plugins(None, false, false)?;
    println!(
        "Setup complete. To build the main GUI, run `bun install` in webview/ \
         then `cargo xtask build-gui`."
    );
    Ok(())
}

/// One plugin's declared build work, parsed from its `plugin.build.toml`.
/// Which sections are present determines what `setup-plugins` runs; a plugin
/// may declare several (rosetta has native + ui).
struct PluginBuildPlan {
    /// Plugin id (= its `plugins/<id>` directory name).
    id: String,
    /// Absolute-or-relative plugin directory.
    dir: PathBuf,
    /// `[native].recipe`: plugin-relative build script to run.
    native_recipe: Option<String>,
    /// `[python]` present: run `pixi install --all` in the plugin dir.
    python: bool,
    /// `[ui].dir`: plugin-relative dir to run `bun install` + `bun run build`.
    ui_dir: Option<String>,
}

/// `plugin.build.toml` schema. Read only by xtask; the runtime `plugin.toml`
/// is a separate file the runner owns.
#[derive(Deserialize)]
struct PluginBuildDescriptor {
    native: Option<NativeBuildSection>,
    python: Option<PythonBuildSection>,
    ui: Option<UiBuildSection>,
}

#[derive(Deserialize)]
struct NativeBuildSection {
    /// Plugin-relative path to the build recipe script.
    recipe: String,
}

/// Marker section: its mere presence means "provision a pixi env here".
#[derive(Deserialize)]
struct PythonBuildSection {}

#[derive(Deserialize)]
struct UiBuildSection {
    /// Plugin-relative directory holding the bun project.
    dir: String,
}

/// The sorted immediate subdirectories of a `plugins/` tree. Both the setup
/// planner (which reads each dir's `plugin.build.toml`) and the native stager
/// (which reads each dir's `plugin.toml`) iterate the same directory set and
/// differ only in the per-dir file they consult, so the walk lives here once.
/// A missing root yields an empty list rather than an error.
fn plugin_dirs(plugins_root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(plugins_root) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(std::result::Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    dirs
}

/// Walk `plugins_root`, read each plugin's `plugin.build.toml`, and classify
/// the declared build work. Plugin dirs without a `plugin.build.toml` are
/// skipped (they contribute no plan). `filter_id`, if given, restricts the
/// result to the plugin whose directory name matches. Pure w.r.t. build
/// tooling: it only reads the descriptors, so it is unit-testable without
/// running bun/pixi/recipes.
fn plan_setup_plugins(
    plugins_root: &Path,
    filter_id: Option<&str>,
) -> Result<Vec<PluginBuildPlan>> {
    let mut plans = Vec::new();
    for dir in plugin_dirs(plugins_root) {
        let Some(id) = dir.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if let Some(want) = filter_id {
            if id != want {
                continue;
            }
        }
        let descriptor_path = dir.join("plugin.build.toml");
        if !descriptor_path.is_file() {
            continue;
        }
        let text = std::fs::read_to_string(&descriptor_path)?;
        let descriptor: PluginBuildDescriptor = toml::from_str(&text)
            .map_err(|e| anyhow::anyhow!("parse {}: {e}", descriptor_path.display()))?;
        plans.push(PluginBuildPlan {
            id: id.to_owned(),
            dir: dir.clone(),
            native_recipe: descriptor.native.map(|n| n.recipe),
            python: descriptor.python.is_some(),
            ui_dir: descriptor.ui.map(|u| u.dir),
        });
    }
    Ok(plans)
}

/// Set up plugins by walking `plugins/` and running each plugin's declared
/// build work. `id` restricts to one plugin (default all); `clean` is
/// forwarded to native recipes via `FOLDIT_RECIPE_CLEAN`. The native binary
/// and UI panel are vendored (committed prebuilt fallback), so their
/// from-source build is skipped when the artifact is already present unless
/// `from_source` (or `clean`, which implies it) forces a rebuild. The python
/// env is never vendored and is always provisioned.
fn setup_plugins(id: Option<&str>, clean: bool, from_source: bool) -> Result<()> {
    let plans = plan_setup_plugins(Path::new("plugins"), id)?;
    if plans.is_empty() {
        match id {
            Some(want) => println!(
                "Warning: no plugin `{want}` with a plugin.build.toml under plugins/; \
                 nothing to do."
            ),
            None => println!("No plugins with a plugin.build.toml under plugins/."),
        }
        return Ok(());
    }

    for plan in plans {
        println!("Setting up plugin `{}`...", plan.id);
        if let Some(recipe) = &plan.native_recipe {
            let resolved = resolve_native_binary(&plan)?;
            if skip_vendored(resolved.is_some(), from_source, clean) {
                if let Some(path) = &resolved {
                    println!(
                        "  {}: using committed/local native binary ({}); \
                         pass --from-source to rebuild",
                        plan.id,
                        path.display()
                    );
                }
            } else {
                run_native_recipe(&plan, recipe, clean)?;
            }
        }
        if plan.python {
            run_python_setup(&plan.dir)?;
        }
        if let Some(ui_dir) = &plan.ui_dir {
            if skip_vendored(ui_panel_built(&plan.dir, ui_dir), from_source, clean) {
                println!(
                    "  {}: using committed panel module(s); pass --from-source to rebuild",
                    plan.id
                );
            } else {
                run_ui_build(&plan.dir, ui_dir)?;
            }
        }
    }
    println!("Plugin setup complete.");
    Ok(())
}

/// Whether to skip the from-source build of a vendored artifact: skip only when
/// it is already `present` and the user has not forced a rebuild (`--from-source`,
/// or `--clean` which implies it). A missing artifact is always built, even
/// unforced (e.g. a platform with no committed prebuilt yet).
const fn skip_vendored(present: bool, from_source: bool, clean: bool) -> bool {
    present && !(from_source || clean)
}

/// Resolve a plugin's loadable native binary via the runner's own resolver
/// (`local/` then `prebuilt/<triple>/`), returning its path if one exists for
/// the host triple. Reads the runtime `plugin.toml` for the decorated binary
/// name so xtask never re-encodes the `lib{name}.dylib` decoration or the
/// lookup precedence.
fn resolve_native_binary(plan: &PluginBuildPlan) -> Result<Option<PathBuf>> {
    let manifest_text = std::fs::read_to_string(plan.dir.join("plugin.toml"))?;
    let manifest = foldit_runner::orchestrator::manifest::PluginManifest::parse(&manifest_text)
        .map_err(|e| anyhow::anyhow!("parse {}/plugin.toml: {e}", plan.dir.display()))?;
    let name = manifest.native_binary_name();
    Ok(foldit_runner::plugin::resolve_native_binary_inner(
        &plan.dir,
        &plan.id,
        &name,
        foldit_runner::plugin::host_target_triple(),
        Path::exists,
    )
    .ok())
}

/// Whether a plugin's UI panel is already built: at least one `*.mjs` under its
/// `<ui_dir>/dist/`. The panel module is vendored (committed), so a fresh dev
/// without bun can still `cargo xtask setup` against it.
fn ui_panel_built(plugin_dir: &Path, ui_dir: &str) -> bool {
    let dist = plugin_dir.join(ui_dir).join("dist");
    let Ok(entries) = std::fs::read_dir(&dist) else {
        return false;
    };
    entries
        .filter_map(std::result::Result::ok)
        .any(|e| e.path().extension().and_then(|x| x.to_str()) == Some("mjs"))
}

/// Run a plugin's native build recipe under the well-known `FOLDIT_*` env
/// contract, then verify the recipe actually produced a loadable binary.
fn run_native_recipe(plan: &PluginBuildPlan, recipe: &str, clean: bool) -> Result<()> {
    let root = canonical_clean(".")?;
    let plugin_dir = canonical_clean(&plan.dir)?;

    // On Windows, prefer a `.ps1` sibling of the declared `.sh` recipe so
    // native tooling (PowerShell + MSVC/Zig) is used instead of requiring
    // Git Bash. Falls back to the declared recipe if no `.ps1` exists.
    let recipe_is_sh = Path::new(recipe)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("sh"));
    let recipe_path = if cfg!(target_os = "windows") && recipe_is_sh {
        let ps1_path = plugin_dir.join(Path::new(recipe).with_extension("ps1"));
        if ps1_path.is_file() {
            ps1_path
        } else {
            plugin_dir.join(recipe)
        }
    } else {
        plugin_dir.join(recipe)
    };

    if !recipe_path.is_file() {
        anyhow::bail!(
            "native recipe not found for `{}`: {}",
            plan.id,
            recipe_path.display()
        );
    }

    // Decorated shared-library name the runner expects (e.g. `lib{name}.dylib`),
    // handed to the recipe so it never re-derives the platform decoration, and
    // reused below to verify the recipe actually produced a loadable binary.
    let manifest_text = std::fs::read_to_string(plan.dir.join("plugin.toml"))?;
    let manifest = foldit_runner::orchestrator::manifest::PluginManifest::parse(&manifest_text)
        .map_err(|e| anyhow::anyhow!("parse {}/plugin.toml: {e}", plan.dir.display()))?;
    let binary_name = manifest.native_binary_name();

    println!("  Running native recipe {} ...", recipe_path.display());

    // `.ps1` scripts need an explicit PowerShell interpreter.
    let mut cmd = if recipe_path.extension().and_then(|e| e.to_str()) == Some("ps1") {
        let mut c = Command::new("powershell");
        c.args(["-ExecutionPolicy", "Bypass", "-NoProfile", "-File"]);
        c.arg(&recipe_path);
        c
    } else {
        Command::new(&recipe_path)
    };
    cmd.current_dir(&plugin_dir)
        .env("FOLDIT_WORKSPACE_ROOT", &root)
        .env("FOLDIT_MOLEX_DIR", root.join("crates/molex"))
        .env("FOLDIT_PROTO_DIR", root.join("crates/foldit-plugin-sdk/proto"))
        .env(
            "FOLDIT_ABI_INCLUDE_DIR",
            root.join("crates/foldit-plugin-sdk/include"),
        )
        .env("FOLDIT_TARGET_TRIPLE", foldit_runner::plugin::host_target_triple())
        .env("FOLDIT_NATIVE_BINARY_NAME", &binary_name)
        .env("FOLDIT_PLUGIN_DIR", &plugin_dir)
        .env("FOLDIT_LOCAL_DIR", plugin_dir.join("local"));
    if clean {
        cmd.env("FOLDIT_RECIPE_CLEAN", "1");
    }
    let status = cmd.status()?;
    if !status.success() {
        anyhow::bail!("native recipe for `{}` failed", plan.id);
    }

    // Verify the recipe left a binary the runner can actually load.
    if foldit_runner::plugin::resolve_native_binary_inner(
        &plan.dir,
        &plan.id,
        &binary_name,
        foldit_runner::plugin::host_target_triple(),
        Path::exists,
    )
    .is_err()
    {
        anyhow::bail!(
            "native recipe for `{}` ran but produced no loadable binary \
             (expected under local/ or prebuilt/)",
            plan.id
        );
    }
    Ok(())
}

/// Provision a python plugin's pixi env.
fn run_python_setup(plugin_dir: &Path) -> Result<()> {
    // `--all` (not bare `pixi install`, which only installs the `default`
    // env): each plugin's real env is NAMED after its id, so bare install
    // would materialize only the empty default and leave the named env
    // solved-but-unmaterialized.
    println!("  pixi install --all in {}", plugin_dir.display());
    let status = Command::new("pixi")
        .args(["install", "--all"])
        .current_dir(plugin_dir)
        .status()?;
    if !status.success() {
        anyhow::bail!(
            "Failed to install the plugin environment in {}",
            plugin_dir.display()
        );
    }
    Ok(())
}

/// Build a plugin's UI ES module (`bun install` + `bun run build`) in its
/// declared bun project dir. Output stays in that dir (e.g. `ui/dist/`).
fn run_ui_build(plugin_dir: &Path, ui_dir: &str) -> Result<()> {
    let dir = plugin_dir.join(ui_dir);
    println!("  Installing UI dependencies in {}...", dir.display());
    let status = Command::new("bun").arg("install").current_dir(&dir).status()?;
    if !status.success() {
        anyhow::bail!("Failed to install UI dependencies in {}", dir.display());
    }

    println!("  Building UI in {}...", dir.display());
    let status = Command::new("bun")
        .args(["run", "build"])
        .current_dir(&dir)
        .status()?;
    if !status.success() {
        anyhow::bail!("Failed to build UI in {}", dir.display());
    }
    Ok(())
}

/// Directories that own a pixi environment to provision: each plugin under the
/// top-level `plugins/` tree that carries a `pixi.toml` (native plugins like
/// rosetta have none and are skipped), plus the dummy round-trip test fixture.
/// Each env is named after its directory (= the plugin id).
fn plugin_env_dirs() -> Result<Vec<std::path::PathBuf>> {
    let mut dirs = Vec::new();
    if let Ok(entries) = std::fs::read_dir("plugins") {
        for entry in entries {
            let path = entry?.path();
            if path.join("pixi.toml").is_file() {
                dirs.push(path);
            }
        }
    }
    dirs.sort();
    let dummy =
        std::path::PathBuf::from("crates/foldit-runner/tests/fixtures/dummy");
    if dummy.join("pixi.toml").is_file() {
        dirs.push(dummy);
    }
    Ok(dirs)
}

/// Path to the conda-forge `CPython` 3.12 that foldit-python-host links libpython
/// against, provisioned by the build-only pixi env at `python-host/`. Installs
/// that env on first use, so building the host never depends on a system
/// python being present. Set as `PYO3_PYTHON` on the python-host cargo build.
fn python_host_build_python() -> Result<std::path::PathBuf> {
    let host_dir = std::path::Path::new("crates/foldit-runner/python-host");
    let py = if cfg!(target_os = "windows") {
        host_dir.join(".pixi/envs/default/python.exe")
    } else {
        host_dir.join(".pixi/envs/default/bin/python3.12")
    };
    if !py.is_file() {
        println!("  Provisioning the python-host build env (pixi install)...");
        let status =
            Command::new("pixi").arg("install").current_dir(host_dir).status()?;
        if !status.success() {
            anyhow::bail!("Failed to provision the python-host build env");
        }
    }
    // Absolute: pyo3-ffi's build script runs from its own cwd, so a relative
    // PYO3_PYTHON would not resolve.
    canonical_clean(&py)
}

fn build_molex() -> Result<()> {
    println!("Rebuilding the molex extension from local source...");
    // molex composes into each plugin env via that plugin's own pixi.toml.
    // `pixi reinstall molex` refreshes it from the resolved source. Each
    // plugin's env is named after its id (= the directory name).
    for dir in plugin_env_dirs()? {
        let env = dir
            .file_name()
            .and_then(|n| n.to_str())
            .with_context(|| {
                format!("plugin env dir has a non-UTF-8 name: {}", dir.display())
            })?;
        println!("  Reinstalling molex in {}", dir.display());
        let status = Command::new("pixi")
            .args(["reinstall", "-e", env, "molex"])
            .current_dir(&dir)
            .status()?;
        if !status.success() {
            anyhow::bail!("Failed to reinstall molex in {}", dir.display());
        }
    }
    println!("molex rebuilt and reinstalled in all plugin environments.");
    Ok(())
}

/// Assemble the full payload into the [`STAGING`] dir: build the host
/// artifacts, copy them flat with the python-host dylib, copy the native +
/// Python plugins, and mirror the read-only data assets. `package` runs this
/// (unless `--skip-assembly`) before handing `STAGING` to cargo-packager.
fn assemble() -> Result<()> {
    let exe_ext = if cfg!(windows) { ".exe" } else { "" };

    // 1. Build the host artifacts into target/release/. A plain
    //    `cargo build --release` won't produce the worker (foldit-runner is
    //    a workspace-excluded submodule) or the python-host dylib (it is
    //    dlopened, not linked), so build each explicitly.
    println!("Building release host artifacts (app, worker, python-host)...");
    let builds: [(&str, &[&str]); 3] = [
        (
            "foldit app",
            &["build", "--release", "-p", "foldit-desktop"],
        ),
        (
            "foldit-worker",
            &[
                "build",
                "--release",
                "-p",
                "foldit-runner",
                "--bin",
                "foldit-worker",
            ],
        ),
        (
            "foldit-python-host",
            &["build", "--release", "-p", "foldit-python-host"],
        ),
    ];
    for (desc, args) in builds {
        println!("  cargo {}", args.join(" "));
        let mut cmd = Command::new("cargo");
        cmd.args(args);
        if desc == "foldit-python-host" {
            cmd.env("PYO3_PYTHON", python_host_build_python()?);
        }
        let status = cmd.status()?;
        if !status.success() {
            anyhow::bail!("Failed to build {desc}");
        }
    }

    // 2. Fresh staging dir with host infra flat at the root: the app, the
    //    worker, and the python-host dylib (the worker dlopens it next to
    //    its own exe). No pixi, no .pixi — the payload is self-contained.
    let _ = std::fs::remove_dir_all(STAGING);
    std::fs::create_dir_all(STAGING)?;

    let host_files = [
        format!("foldit{exe_ext}"),
        format!("foldit-worker{exe_ext}"),
        python_host_lib_name().to_owned(),
    ];
    for name in &host_files {
        let src = format!("target/release/{name}");
        if Path::new(&src).exists() {
            std::fs::copy(&src, format!("{STAGING}/{name}"))?;
            println!("Staged {name}.");
        } else {
            anyhow::bail!("expected host artifact missing: {src}");
        }
    }

    // 3. Native plugins: stage each into <staging>/plugins/<id>.
    stage_native_plugins()?;

    // 4. Python plugins: TEMPORARILY DISABLED for a Rosetta-only demo build.
    //    To restore foundry/simplefold, re-add the bundler step in place of
    //    this skip: `python crates/foldit-runner/pixi_scripts/bundle_runtime.py
    //    --output <abs STAGING>` (canonicalize STAGING first; bail on non-zero
    //    exit). It discovers each plugin's env under `plugins/<id>/.pixi/envs`.
    println!("Skipping Python plugins (Rosetta-only demo build).");

    // 5. Read-only data assets, mirrored under <staging>/assets/ so the runtime
    //    resolvers find them. Each is reached either next to the exe (Windows /
    //    flat layout) or via a FOLDIT_*_ROOT env override the desktop shim sets
    //    when the assets live in a platform resource dir (macOS .app Resources):
    //      gui          -> `create_webview_release` / FOLDIT_GUI_ROOT
    //      view_presets -> `resolve_view_presets_dir` / FOLDIT_VIEW_PRESETS_DIR
    //      levels       -> `puzzle::levels_root` / FOLDIT_LEVELS_ROOT
    //      scoring      -> `scores::load_default_term_weights` / FOLDIT_SCORING_DIR
    //      puzzle_setup -> referenced by the above data.
    //    `models/` (the on-demand PDB-by-id download cache) is intentionally
    //    omitted; staged `levels` carry their own `structure.pdb`. The app icon
    //    is NOT here - it is build-time packaging input (`packaging/icon.png`).
    std::fs::create_dir_all(format!("{STAGING}/assets"))?;

    let asset_dirs = [
        "gui",
        "view_presets",
        "levels",
        "scoring",
        "puzzle_setup",
        "residue_icons",
    ];
    for name in asset_dirs {
        let src = format!("assets/{name}");
        let dst = format!("{STAGING}/assets/{name}");
        if Path::new(&src).exists() {
            println!("Staging {src} -> {dst} ...");
            copy_dir(&src, &dst)?;
        } else if name == "gui" {
            println!("Warning: Frontend assets not found at assets/gui");
            println!("  Run 'cargo xtask build-gui' first");
        } else {
            println!("Warning: asset dir not found at {src} (skipping)");
        }
    }

    println!("Staging payload ready at ./{STAGING}/");
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// OS installers via cargo-packager (.app/.dmg, NSIS/MSI, deb/AppImage)
// ─────────────────────────────────────────────────────────────────────────────

/// Build OS installers from the [`STAGING`] payload via `cargo packager`
/// (config in `packager.json`). Runs [`assemble`] first unless `skip_assembly`.
///
/// cargo-packager places the `binaries` (`foldit` + `foldit-worker`) adjacent
/// in the package and the `resources` (assets, plugins, python-host dylib) in
/// the platform resource dir; the launch shim in `foldit-desktop::main` points
/// the runtime resolvers there. See `docs/app_packaging.md`.
fn package(formats: Option<&str>, skip_assembly: bool) -> Result<()> {
    if skip_assembly {
        if !Path::new(&format!("{STAGING}/foldit")).exists() {
            anyhow::bail!(
                "{STAGING}/foldit not found - drop --skip-assembly so it gets \
                 built first"
            );
        }
    } else {
        assemble()?;
    }

    let have_packager = Command::new("cargo")
        .args(["packager", "--version"])
        .output()
        .is_ok_and(|o| o.status.success());
    if !have_packager {
        anyhow::bail!(
            "cargo-packager not found - install it with \
             `cargo install cargo-packager --locked`"
        );
    }

    // Pass the config's CONTENTS as the raw `-c` argument (it starts with
    // `{`, so cargo-packager parses it inline).
    let config = std::fs::read_to_string("packager.json")
        .map_err(|e| anyhow::anyhow!("read packager.json: {e}"))?;

    println!("Packaging via cargo-packager...");
    let mut args = vec!["packager".to_owned(), "-c".to_owned(), config.clone()];
    if let Some(formats) = formats {
        for fmt in formats.split(',') {
            args.push("--formats".to_owned());
            args.push(fmt.trim().to_owned());
        }
    }
    let status = Command::new("cargo").args(&args).status()?;
    if !status.success() {
        anyhow::bail!("cargo packager failed");
    }

    // cargo-packager only code-signs when a `signingIdentity` is configured.
    // For the default (ad-hoc) path, seal each produced .app so it has a valid
    // bundle signature
    #[cfg(target_os = "macos")]
    if !config.contains("\"signingIdentity\"") {
        adhoc_seal_apps("dist")?;
    }

    println!("Installers ready in ./dist/");
    Ok(())
}

/// Ad-hoc code-sign each `*.app` in `out_dir` (`codesign --sign -`). Used for
/// the default unsigned-distribution path
#[cfg(target_os = "macos")]
fn adhoc_seal_apps(out_dir: &str) -> Result<()> {
    let Ok(entries) = std::fs::read_dir(out_dir) else {
        return Ok(());
    };
    for path in entries.flatten().map(|e| e.path()) {
        if path.extension().and_then(|e| e.to_str()) == Some("app") {
            println!("Ad-hoc sealing {}...", path.display());
            let status = Command::new("codesign")
                .args(["--force", "--sign", "-"])
                .arg(&path)
                .status()?;
            if !status.success() {
                anyhow::bail!("ad-hoc codesign failed for {}", path.display());
            }
        }
    }
    Ok(())
}

/// Staged destination for a native plugin's shippable binary. The bundled
/// runtime resolves native binaries under `prebuilt/<triple>/<name>` (its
/// per-platform fallback branch), so stage there regardless of whether the
/// source binary came from `local/` or `prebuilt/`.
fn staged_native_binary_dest(staging: &str, id: &str, triple: &str, name: &str) -> PathBuf {
    let staging_plugin_dir = Path::new(staging).join("plugins").join(id);
    foldit_runner::plugin::prebuilt_binary_path(&staging_plugin_dir, triple, name)
}

/// Stage every native plugin under `plugins/` into `<staging>/plugins/<id>`,
/// mirroring the dev-tree layout the orchestrator's `discover_plugins` scans.
///
/// Per plugin, non-fatal: a plugin with no binary for the host triple is warned
/// about and skipped rather than failing the whole package. The shippable
/// binary is resolved with the runner's own resolver (`local/` then
/// `prebuilt/<triple>/`) so xtask never re-encodes the lookup precedence, and
/// is restaged under `prebuilt/<triple>/` where the runtime resolver expects it.
fn stage_native_plugins() -> Result<()> {
    let triple = foldit_runner::plugin::host_target_triple();

    for dir in plugin_dirs(Path::new("plugins")) {
        let Some(id) = dir.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let manifest_path = dir.join("plugin.toml");
        if !manifest_path.is_file() {
            continue;
        }
        let manifest_text = std::fs::read_to_string(&manifest_path)?;
        let manifest =
            foldit_runner::orchestrator::manifest::PluginManifest::parse(&manifest_text)
                .map_err(|e| anyhow::anyhow!("parse {}: {e}", manifest_path.display()))?;
        if manifest.kind != foldit_runner::orchestrator::manifest::PluginKind::Native {
            continue;
        }

        let name = manifest.native_binary_name();
        let Ok(binary_src) = foldit_runner::plugin::resolve_native_binary_inner(
            &dir,
            id,
            &name,
            triple,
            Path::exists,
        ) else {
            println!(
                "Warning: no native binary for plugin `{id}` (host triple \
                 {triple}); skipping. Build it from source with \
                 `cargo xtask setup-plugins {id}`, or add a prebuilt binary \
                 for this platform."
            );
            continue;
        };

        let plugin_dst = format!("{STAGING}/plugins/{id}");
        println!("Staging native plugin `{id}` -> {plugin_dst} ...");
        std::fs::create_dir_all(&plugin_dst)?;

        std::fs::copy(&manifest_path, format!("{plugin_dst}/plugin.toml"))?;

        let binary_dst = staged_native_binary_dest(STAGING, id, triple, &name);
        if let Some(parent) = binary_dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(&binary_src, &binary_dst)?;

        // assets/ holds runtime data (e.g. the compact rosetta database) plus
        // the button icons referenced by plugin.toml.
        let assets_src = dir.join("assets");
        if assets_src.is_dir() {
            copy_dir(&assets_src.to_string_lossy(), &format!("{plugin_dst}/assets"))?;
        }

        // Built panel ES modules (plugin.toml's [[panels]].entry). Built on
        // demand and gitignored, so they live outside assets/; copy each to the
        // same manifest-relative path the serve/allowlist resolves against.
        let ui_dist = dir.join("ui/dist");
        if ui_dist.is_dir() {
            stage_panel_modules(&ui_dist, &format!("{plugin_dst}/ui/dist"))?;
        }
    }
    Ok(())
}

/// Copy every `*.mjs` panel module directly under `src_dist` into `dst_dist`,
/// creating `dst_dist` lazily on the first module found.
fn stage_panel_modules(src_dist: &Path, dst_dist: &str) -> Result<()> {
    let Ok(entries) = std::fs::read_dir(src_dist) else {
        return Ok(());
    };
    let mut created = false;
    for entry in entries.filter_map(std::result::Result::ok) {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("mjs") {
            continue;
        }
        let Some(file_name) = path.file_name() else {
            continue;
        };
        if !created {
            std::fs::create_dir_all(dst_dist)?;
            created = true;
        }
        std::fs::copy(&path, Path::new(dst_dist).join(file_name))?;
    }
    Ok(())
}

/// Build the backend host artifacts into `target/<profile>/` so they sit
/// next to the foldit-desktop exe that `cargo run` produces.
fn build_host(debug: bool) -> Result<()> {
    let profile_flag: &[&str] = if debug { &[] } else { &["--release"] };
    let profile = if debug { "debug" } else { "release" };
    println!("Building backend host artifacts ({profile}): foldit-worker + python-host...");

    println!("  Building foldit-worker binary...");
    let mut worker_args = vec!["build", "-p", "foldit-runner", "--bin", "foldit-worker"];
    worker_args.extend_from_slice(profile_flag);
    let status = Command::new("cargo").args(&worker_args).status()?;
    if !status.success() {
        anyhow::bail!("Failed to build foldit-worker");
    }

    println!("  Building foldit-python-host dylib...");
    let mut host_args = vec!["build", "-p", "foldit-python-host"];
    host_args.extend_from_slice(profile_flag);
    let status = Command::new("cargo")
        .args(&host_args)
        .env("PYO3_PYTHON", python_host_build_python()?)
        .status()?;
    if !status.success() {
        anyhow::bail!("Failed to build foldit-python-host");
    }

    println!(
        "✓ Host artifacts refreshed in target/{profile}/ (next to the \
         `cargo run{}` foldit exe).",
        if debug { "" } else { " --release" }
    );
    Ok(())
}

fn build_gui() -> Result<()> {
    let gui_src_dir = "webview";

    println!("Installing GUI dependencies...");
    let install_status = Command::new("bun")
        .arg("install")
        .current_dir(gui_src_dir)
        .status()?;
    if !install_status.success() {
        anyhow::bail!("Failed to install GUI dependencies");
    }

    println!("Building GUI...");
    let status = Command::new("bun")
        .args(["run", "build"])
        .current_dir(gui_src_dir)
        .status()?;
    if !status.success() {
        anyhow::bail!("Failed to build GUI");
    }

    let dist_dir = format!("{gui_src_dir}/dist");
    let gui_dir = "assets/gui";

    if !Path::new(&dist_dir).exists() {
        anyhow::bail!("GUI dist directory not found at {dist_dir}");
    }

    // Remove old assets/gui if it exists.
    if Path::new(gui_dir).exists() {
        std::fs::remove_dir_all(gui_dir)?;
    }

    copy_dir(&dist_dir, gui_dir)?;
    println!("GUI built and copied to {gui_dir}");
    Ok(())
}

fn copy_dir(src: &str, dst: &str) -> Result<()> {
    #[cfg(unix)]
    {
        let status = Command::new("cp").args(["-r", src, dst]).status()?;
        if !status.success() {
            anyhow::bail!("Failed to copy {src} to {dst}");
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

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a test fixture that cannot build its temp tree should panic; \
              that panic IS the failing test"
)]
mod setup_plugins_tests {
    use super::*;

    /// Unique scratch dir under the OS temp dir; removed on drop.
    struct TempTree(PathBuf);
    impl TempTree {
        fn new(tag: &str) -> Self {
            let base = std::env::temp_dir().join(format!(
                "xtask_setup_plugins_{tag}_{}_{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&base).unwrap();
            Self(base)
        }
        fn write(&self, rel: &str, contents: &str) {
            let path = self.0.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }
    }
    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn fixture() -> TempTree {
        let t = TempTree::new("fixture");
        t.write(
            "plugins/rosetta/plugin.build.toml",
            "[native]\nrecipe = \"scripts/build.sh\"\n[ui]\ndir = \"ui\"\n",
        );
        t.write("plugins/foundry/plugin.build.toml", "[python]\n");
        t.write("plugins/simplefold/plugin.build.toml", "[python]\n");
        // No descriptor: must be skipped, not error.
        t.write("plugins/undeclared/plugin.toml", "id = \"undeclared\"\n");
        t
    }

    fn plan_for<'a>(plans: &'a [PluginBuildPlan], id: &str) -> &'a PluginBuildPlan {
        plans.iter().find(|p| p.id == id).expect("plan present")
    }

    #[test]
    fn classifies_each_section_and_skips_descriptorless() {
        let t = fixture();
        let plans = plan_setup_plugins(&t.0.join("plugins"), None).unwrap();

        let ids: Vec<&str> = plans.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["foundry", "rosetta", "simplefold"]);

        let rosetta = plan_for(&plans, "rosetta");
        assert_eq!(rosetta.native_recipe.as_deref(), Some("scripts/build.sh"));
        assert_eq!(rosetta.ui_dir.as_deref(), Some("ui"));
        assert!(!rosetta.python);

        let foundry = plan_for(&plans, "foundry");
        assert!(foundry.python);
        assert!(foundry.native_recipe.is_none());
        assert!(foundry.ui_dir.is_none());
    }

    #[test]
    fn filters_by_id() {
        let t = fixture();
        let plans =
            plan_setup_plugins(&t.0.join("plugins"), Some("rosetta")).unwrap();
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].id, "rosetta");
    }

    #[test]
    fn unknown_id_yields_empty_plan() {
        let t = fixture();
        let plans = plan_setup_plugins(&t.0.join("plugins"), Some("nope")).unwrap();
        assert!(plans.is_empty());
    }

    #[test]
    fn missing_plugins_root_is_empty_not_error() {
        let plans =
            plan_setup_plugins(Path::new("/definitely/not/a/real/dir"), None).unwrap();
        assert!(plans.is_empty());
    }

    #[test]
    fn vendored_artifact_skipped_only_when_present_and_unforced() {
        // Present + unforced: reuse the committed artifact.
        assert!(skip_vendored(true, false, false));
        // Present but forced (either flag): rebuild.
        assert!(!skip_vendored(true, true, false));
        assert!(!skip_vendored(true, false, true));
        // Absent: always build, forced or not.
        assert!(!skip_vendored(false, false, false));
        assert!(!skip_vendored(false, true, false));
        assert!(!skip_vendored(false, false, true));
    }

    #[test]
    fn staged_binary_lands_under_prebuilt_triple() {
        let dest = staged_native_binary_dest(
            "target/staging",
            "rosetta",
            "aarch64-apple-darwin",
            "librosetta_interactive.dylib",
        );
        assert_eq!(
            dest,
            Path::new(
                "target/staging/plugins/rosetta/prebuilt/aarch64-apple-darwin/librosetta_interactive.dylib"
            )
        );
    }
}

