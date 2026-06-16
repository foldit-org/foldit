// xtask is a developer-facing CLI / build tool: writing build progress and
// results straight to stdout / stderr is its primary job, not a debug leak.
#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "xtask is a CLI build tool; console output is its intended interface"
)]

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::Path;
use std::process::Command;

fn rosetta_interactive_path() -> String {
    "crates/foldit-runner/plugins/rosetta/deps/rosetta-interactive".to_owned()
}

/// Canonicalize to an absolute path, stripping Windows' `\\?\` verbatim
/// prefix. `std::fs::canonicalize` emits verbatim paths on Windows, which
/// some build tools (e.g. protoc) reject as invalid filenames. On non-Windows
/// the prefix is absent, so this just canonicalizes.
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

const fn rosetta_lib_name() -> &'static str {
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

/// Intermediate staging dir where `assemble` lays out the full payload (host
/// binaries + python-host dylib + plugins + assets) that `cargo packager` then
/// wraps into the installers in `dist/`. Lives under `target/` so it is already
/// git-ignored and clearly a build artifact, not a deliverable.
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
    /// Install the plugin Python environments in crates/foldit-runner
    SetupEnvs,
    /// Build Rosetta from ~/rosetta-interactive
    BuildRosettaInteractive {
        /// Wipe the rosetta-interactive cmake build dir before building, so
        /// the configure step re-runs from scratch (forwarded to build.sh
        /// --clean / build.ps1 -Clean). Use after toolchain/compiler changes
        /// that a cached CMakeCache.txt would otherwise reject.
        #[arg(long)]
        clean: bool,
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
    /// `crates/foldit-gui/js/public/pkg/`. Requires nightly toolchain.
    BuildWeb {
        /// Build in debug mode (default: release)
        #[arg(long)]
        debug: bool,
    },
    /// The web prod path: build the web wasm artifact AND run `bun run
    /// build:web` to produce a static site ready for deployment.
    PackageWeb,
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
        Commands::SetupEnvs => setup_envs(),
        Commands::BuildRosettaInteractive { clean } => build_rosetta_interactive(clean),
        Commands::Package {
            formats,
            skip_assembly,
        } => package(formats.as_deref(), skip_assembly),
        Commands::BuildMolex => build_molex(),
        Commands::BuildGui => build_gui(),
        Commands::BuildHost { debug } => build_host(debug),
        Commands::BuildWeb { debug } => build_web(debug),
        Commands::PackageWeb => package_web(),
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

fn package_web() -> Result<()> {
    build_web(false)?;

    let js_dir = Path::new("crates/foldit-gui/js");
    println!("Running bun run build:web in {}...", js_dir.display());
    let status = Command::new("bun")
        .args(["run", "build:web"])
        .current_dir(js_dir)
        .status()?;
    if !status.success() {
        anyhow::bail!("bun run build:web failed");
    }

    println!("✓ web build ready at crates/foldit-gui/js/dist/");
    Ok(())
}

fn setup_envs() -> Result<()> {
    println!("Installing plugin Python environments (pixi install --all)...");
    let status = Command::new("pixi")
        .args(["install", "--all"])
        .current_dir("crates/foldit-runner")
        .status()?;
    if !status.success() {
        anyhow::bail!("Failed to install plugin environments");
    }
    // molex is installed editable into every plugin env via pixi (the
    // `molex-local` feature in pixi.toml). To recompile the molex
    // extension after editing its Rust source, run
    // `cargo xtask build-molex` separately.

    // libpython is brought into the process via foldit-python-host's
    // build.rs (resolves via $CONDA_PREFIX or .pixi/envs/<env>/lib);
    // no host-binary link path, no copy to assets/libs/.

    // Build the worker binary + the python-host dylib so they sit next to
    // the main executable. Without the worker, plugins can't spawn; without
    // the dylib, Python plugins fail to load (the worker dlopens it by
    // filename next to itself). Scope to -p so the build doesn't drag
    // foldit's viso/wgpu dep graph into backend-only artifacts.
    println!("Building foldit-worker binary...");
    let status = Command::new("cargo")
        .args(["build", "-p", "foldit-runner", "--bin", "foldit-worker"])
        .status()?;
    if !status.success() {
        anyhow::bail!("Failed to build foldit-worker");
    }

    println!("Building foldit-python-host dylib...");
    let status = Command::new("cargo")
        .args(["build", "-p", "foldit-python-host"])
        .status()?;
    if !status.success() {
        anyhow::bail!("Failed to build foldit-python-host");
    }

    println!("Plugin environments setup complete.");
    Ok(())
}

fn build_molex() -> Result<()> {
    println!("Rebuilding the molex extension from local source...");
    // molex is an editable maturin path dep composed into every plugin
    // env (pixi.toml's `molex-local` feature). `pixi reinstall <pkg>`
    // re-runs maturin to recompile the extension from the local crate.
    for env in ["dummy", "foundry", "esmfold", "simplefold"] {
        println!("  Reinstalling molex into the {env} environment...");
        let status = Command::new("pixi")
            .args(["reinstall", "-e", env, "molex"])
            .current_dir("crates/foldit-runner")
            .status()?;
        if !status.success() {
            anyhow::bail!("Failed to reinstall molex in the {env} environment");
        }
    }
    println!("molex rebuilt and reinstalled in all plugin environments.");
    Ok(())
}

fn build_rosetta_interactive(clean: bool) -> Result<()> {
    let rosetta_path = rosetta_interactive_path();
    let cmake_dir = format!("{rosetta_path}/source/cmake_4");

    if !Path::new(&cmake_dir).exists() {
        anyhow::bail!(
            "Rosetta cmake directory not found at {cmake_dir}. \
             Make sure the rosetta-interactive submodule is initialized."
        );
    }

    // Build the molex static lib first; the bridge dylib links against
    // it for ASSEM01 IO and Assembly walks. Invoke via
    // --manifest-path so cargo treats this as a standalone build,
    // bypassing the workspace resolver. crates/molex is workspace-
    // excluded; the workspace itself pins published molex ^0.5.1 for
    // foldit-core, which does not resolve against the in-tree 0.4.x.
    // Running outside the workspace sidesteps that conflict.
    // rustc's Windows host target is MSVC, but the Rosetta C++ is linked by
    // zig in MinGW/GNU mode (toolchains/zig.cmake). An MSVC-ABI molex can't
    // link into a MinGW binary (mismatched CRT / unwinding / RTTI), so on
    // Windows build molex for the GNU target to match the link; other
    // platforms build for the host. A GNU-ABI staticlib is named libmolex.a.
    #[cfg(target_os = "windows")]
    let molex_target = Some("x86_64-pc-windows-gnu");
    #[cfg(not(target_os = "windows"))]
    let molex_target: Option<&str> = None;

    println!("Building molex static library (release, c-api feature)...");
    let mut molex_args = vec![
        "build",
        "--manifest-path",
        "crates/molex/Cargo.toml",
        "--release",
        "--features",
        "c-api",
    ];
    if let Some(target) = molex_target {
        molex_args.push("--target");
        molex_args.push(target);
    }
    let status = Command::new("cargo").args(&molex_args).status()?;
    if !status.success() {
        anyhow::bail!("Failed to build molex static library");
    }

    // `cargo build --target <t>` nests artifacts under target/<t>/release.
    let molex_release_dir = molex_target.map_or_else(
        || "crates/molex/target/release".to_owned(),
        |target| format!("crates/molex/target/{target}/release"),
    );
    let molex_include = canonical_clean("crates/molex/include")?;
    let molex_lib = canonical_clean(format!("{molex_release_dir}/libmolex.a"))?;
    let proto_dir = canonical_clean("crates/foldit-runner/proto")?;

    // Delegate the cmake configure + build (and the make.py /
    // make_database.py preprocessing) to rosetta-interactive's own
    // build.sh. Molex paths flow through env vars that build.sh
    // appends to its CMAKE_ARGS. This avoids drift between the
    // canonical script and a parallel xtask reimplementation; any
    // future build-flag change in build.sh propagates automatically.
    // build.sh (Unix) and build.ps1 (Windows) are the rosetta-interactive
    // repo's own scripts; both read the molex/proto paths from these env
    // vars and append them as -D defines to their cmake invocation.
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = Command::new("powershell.exe");
        c.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File", "build.ps1"]);
        if clean {
            c.arg("-Clean");
        }
        c
    };
    #[cfg(not(target_os = "windows"))]
    let mut cmd = {
        let mut c = Command::new("./build.sh");
        if clean {
            c.arg("--clean");
        }
        c
    };

    let build_script = if cfg!(target_os = "windows") {
        "build.ps1"
    } else {
        "build.sh"
    };
    println!("Running {build_script} in {rosetta_path}...");
    let status = cmd
        .env("MOLEX_INCLUDE_DIR", molex_include.as_os_str())
        .env("MOLEX_STATIC_LIB", molex_lib.as_os_str())
        .env("FOLDIT_PROTO_DIR", proto_dir.as_os_str())
        .current_dir(&rosetta_path)
        .status()?;
    if !status.success() {
        anyhow::bail!("rosetta-interactive {build_script} failed");
    }

    // Copy the dylib into the plugin directory. That's the single
    // sink: `Orchestrator::discover_plugins` + `spawn_native_worker`
    // dlopen it from there. No host-binary link path; the foldit-
    // plugin-vtable contract is the only surface.
    let lib_src = format!("{}/release/bin/{}", cmake_dir, rosetta_lib_name());
    if !Path::new(&lib_src).exists() {
        anyhow::bail!("Built library not found at {lib_src}");
    }

    let plugin_dir = "crates/foldit-runner/plugins/rosetta";
    let lib_dst_plugin = format!("{}/{}", plugin_dir, rosetta_lib_name());
    std::fs::create_dir_all(plugin_dir)?;
    std::fs::copy(&lib_src, &lib_dst_plugin)?;
    println!("Copied {lib_src} -> {lib_dst_plugin}");

    // Copy compact database into the plugin's own assets dir, alongside
    // the dylib. Bridge `find_rosetta_database` walks up from the plugin
    // dir looking for `assets/database/`, so this is exactly where it
    // wants to find it.
    let db_src = format!("{cmake_dir}/cmp-database/database");
    if Path::new(&db_src).exists() {
        let db_dst = format!("{plugin_dir}/assets/database");
        if Path::new(&db_dst).exists() {
            std::fs::remove_dir_all(&db_dst)?;
        }
        std::fs::create_dir_all(format!("{plugin_dir}/assets"))?;
        copy_dir(&db_src, &db_dst)?;
        println!("Copied compact database -> {db_dst}");
    } else {
        println!("Warning: Compact database not found at {db_src}");
        println!(
            "  Run 'python3 make_database.py' in {}/source/cmake_4/ first",
            rosetta_interactive_path()
        );
    }

    println!("Rosetta build complete.");
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
        let status = Command::new("cargo").args(args).status()?;
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

    // 3. Native plugins: copy the Rosetta plugin into <staging>/plugins/rosetta.
    copy_rosetta_plugin()?;

    // 4. Python plugins: TEMPORARILY DISABLED for a Rosetta-only demo build.
    //    To restore foundry/esmfold/simplefold, re-add the pixi bundler step in
    //    place of this skip: `pixi run bundle --output <abs STAGING>` run in
    //    crates/foldit-runner (canonicalize STAGING first; bail on non-zero exit).
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

    let asset_dirs = ["gui", "view_presets", "levels", "scoring", "puzzle_setup"];
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
    // `{`, so cargo-packager parses it inline). This skips the tree-walking
    // `**/packager.toml` glob discovery and keeps relative paths resolving from
    // the workspace root (no chdir to a manifest/config dir).
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
    // bundle signature (Apple Silicon also requires signed code to run at all).
    // A real `signingIdentity` means cargo-packager already signed properly, so
    // leave it untouched rather than clobber it with an ad-hoc signature.
    #[cfg(target_os = "macos")]
    if !config.contains("\"signingIdentity\"") {
        adhoc_seal_apps("dist")?;
    }

    println!("Installers ready in ./dist/");
    Ok(())
}

/// Ad-hoc code-sign each `*.app` in `out_dir` (`codesign --sign -`). Used for
/// the default unsigned-distribution path; sealing works because the bundled
/// conda envs live under `Contents/Resources/` (codesign hashes them as
/// resources rather than trying to sign them as nested code).
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

/// Copy the Rosetta native plugin into `<staging>/plugins/rosetta`, mirroring
/// the dev-tree layout the orchestrator's `discover_plugins` scans
/// (`plugins/<id>/{plugin.toml, <dylib>, assets/}`). The plugin dir is the
/// source of truth, written by `build-rosetta-interactive`. We deliberately
/// do NOT copy `deps/` (the rosetta-interactive C++ source submodule + cmake
/// build tree). Missing artifacts warn rather than fail, so a dev without
/// Rosetta built can still stage the Python plugins.
fn copy_rosetta_plugin() -> Result<()> {
    let rosetta_plugin_src = "crates/foldit-runner/plugins/rosetta";
    let rosetta_lib = rosetta_lib_name();
    let lib_src = format!("{rosetta_plugin_src}/{rosetta_lib}");

    if !Path::new(&lib_src).exists() {
        println!("Warning: Rosetta plugin dylib not found at {lib_src}");
        println!("  Run 'cargo xtask build-rosetta-interactive' first");
        return Ok(());
    }

    let rosetta_plugin_dst = format!("{STAGING}/plugins/rosetta");
    println!("Copying Rosetta plugin -> {rosetta_plugin_dst} ...");
    std::fs::create_dir_all(&rosetta_plugin_dst)?;

    std::fs::copy(
        format!("{rosetta_plugin_src}/plugin.toml"),
        format!("{rosetta_plugin_dst}/plugin.toml"),
    )?;
    std::fs::copy(&lib_src, format!("{rosetta_plugin_dst}/{rosetta_lib}"))?;

    // assets/ holds both the compact rosetta database (runtime) and the
    // button icons referenced by plugin.toml.
    let assets_src = format!("{rosetta_plugin_src}/assets");
    if Path::new(&assets_src).exists() {
        copy_dir(&assets_src, &format!("{rosetta_plugin_dst}/assets"))?;
    } else {
        println!(
            "Warning: Rosetta assets not found at {assets_src} (icons + database)"
        );
        println!("  Run 'cargo xtask build-rosetta-interactive' first");
    }
    Ok(())
}

/// Build the backend host artifacts into `target/<profile>/` so they sit
/// next to the foldit-desktop exe that `cargo run` produces. `cargo run`
/// builds only foldit-desktop and its linked deps; the worker is a separate
/// (workspace-excluded) bin and the python-host is a cdylib that is dlopened
/// rather than linked, so neither is rebuilt and both go stale silently. The
/// classic symptom is a plugin ABI-version mismatch: a fresh worker (built
/// from current source) dlopens a months-old python-host dylib reporting an
/// older ABI, and plugin load fails. This refreshes both in one shot.
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
    let status = Command::new("cargo").args(&host_args).status()?;
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
    let gui_src_dir = "crates/foldit-gui/js";

    println!("Installing GUI dependencies...");
    // `bun` is a single native executable on every platform, so Command
    // resolves it directly (no cmd /c shim dance like pnpm needed on Windows).
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

    // Remove old assets/gui if it exists. We deliberately do NOT
    // create_dir_all(gui_dir) afterwards: `copy_dir` shells out to
    // `cp -r dist assets/gui`, and `cp` nests the source *inside* the
    // destination when the destination already exists (yielding
    // assets/gui/dist/index.html, which the release webview can't find
    // -- it serves assets/gui/index.html). With gui_dir absent, `cp -r`
    // creates it as a copy of dist's contents, which is what we want.
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
