# xtask Commands

`xtask` is the build-automation binary. Run it with `cargo xtask <command>`. It
pins the working directory to the workspace root, so the command works from any
subdirectory. The full command set (`xtask/src/main.rs`):

| Command | What it does |
| --- | --- |
| `setup-envs` | Install the plugin Python environments in `crates/foldit-runner` |
| `build-rosetta-interactive [--clean]` | Build Rosetta from the rosetta-interactive checkout; `--clean` wipes the cmake build dir first |
| `package [--formats <list>] [--skip-assembly]` | The desktop production path: assemble the staging payload, then build OS installers via cargo-packager |
| `build-molex` | Rebuild the molex Python extension from local source |
| `build-gui` | Build the front end (`bun run build`) and copy the dist into `assets/gui` |
| `build-rosetta-ui` | Build the Rosetta plugin's panel ES module (`bun run build` in the plugin's `ui/` dir) |
| `build-host [--debug]` | Build the backend host artifacts (`foldit-worker` + the python-host dylib) into `target/<profile>/` next to the desktop exe |
| `build-web [--debug]` | Build the `foldit-web` cdylib, run wasm-bindgen, emit JS glue to `webview/public/pkg/` (requires the nightly toolchain) |
| `package-web` | The web production path: build the wasm artifact and run `bun run build:web` to produce a static site |
| `transcribe-filters` | One-shot, idempotent transcription of legacy Rosetta puzzle filter/design-mask files into the curated level TOML |

## build-host

`build-host` matters for day-to-day desktop work. A plain `cargo run` rebuilds
neither the worker nor the python-host dylib: the worker is a workspace-excluded
binary, and the dylib is dlopened rather than linked. After pulling or changing
runner code, run `cargo xtask build-host` so a fresh worker does not sit next to
a stale dylib (which would fail to load on an ABI bump). It defaults to release;
pass `--debug` to match a debug app build.

## package

`cargo xtask package` is the desktop production path. It assembles a staging
payload under `target/staging` and invokes cargo-packager (which needs
`cargo install cargo-packager`) to produce installers: macOS `.app`/`.dmg`,
Windows NSIS/MSI, Linux deb/AppImage. The cargo-packager config is
`packager.json`. `--skip-assembly` packages the existing staging payload as-is
for fast iteration on packaging config. See
[Assets and Bundling](assets-bundling.md).

## pixi aliases

The root `pixi.toml` exposes thin aliases over a subset of these commands
(`setup-envs`, `build-rosetta-interactive`, `package`, `build-host`,
`build-web`). They are conveniences; the work is done by xtask either way.
