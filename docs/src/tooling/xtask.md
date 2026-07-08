# xtask Commands

`xtask` is the build-automation binary. Run it with `cargo xtask <command>`. It
pins the working directory to the workspace root, so the command works from any
subdirectory. The full command set (`xtask/src/main.rs`):

| Command | What it does |
| --- | --- |
| `setup` | Fresh-clone bootstrap: `build-host` (release) then `setup-plugins` (all). One command to make `cargo run` work |
| `setup-plugins [id] [--from-source] [--clean]` | Walk `plugins/`, read each plugin's `plugin.build.toml`, and run the setup it declares (native binary, pixi env, panel UI). Optional `id` restricts to one plugin; default = all |
| `package [--formats <list>] [--skip-assembly]` | The desktop production path: assemble the staging payload, then build OS installers via cargo-packager |
| `build-molex` | Rebuild the molex Python extension into the plugin pixi environments |
| `build-gui` | Build the front end (`bun run build`) and copy the dist into `assets/gui` |
| `build-host [--debug]` | Build the backend host artifacts (`foldit-worker` + the python-host dylib) into `target/<profile>/` next to the desktop exe |
| `build-web [--debug]` | Build the `foldit-web` cdylib, run wasm-bindgen, emit JS glue to `webview/public/pkg/` (requires the nightly toolchain) |
| `package-web` | The web production path: build the wasm artifact and run `bun run build:web` to produce a static site |
| `transcribe-filters` | One-shot, idempotent transcription of legacy Rosetta puzzle filter/design-mask files into the curated level TOML |

## setup and setup-plugins

`cargo xtask setup` is the fresh-clone bootstrap: it builds the backend host
(release) and then provisions every plugin, so a clean checkout reaches a
runnable state in one command.

`setup-plugins` is the generic, per-plugin engine underneath it. It walks
`plugins/`, reads each plugin's `plugin.build.toml`, and runs whatever that
descriptor declares — dispatching by section rather than by hardcoded per-plugin
commands:

- `[native]` — runs the plugin's own build recipe (declared as
  `recipe = "..."`), invoked with a `FOLDIT_*` env contract
  (`FOLDIT_WORKSPACE_ROOT`, `FOLDIT_MOLEX_DIR`, `FOLDIT_PROTO_DIR`,
  `FOLDIT_ABI_INCLUDE_DIR`, `FOLDIT_TARGET_TRIPLE`, `FOLDIT_PLUGIN_DIR`,
  `FOLDIT_LOCAL_DIR`). The recipe installs its binary into the plugin's `local/`
  dir. **The build is skipped when a binary already resolves** (a committed
  `prebuilt/<target-triple>/` fallback or a prior `local/` build); pass
  `--from-source` (or `--clean`, which implies it) to force a rebuild. So the
  from-source native build is opt-in — the committed fallback is the default.
- `[python]` — always runs `pixi install --all` in the plugin dir (Python
  environments are solved locally, never vendored).
- `[ui]` — runs `bun install` + `bun run build` for the plugin's panel module.
  Also skipped when the built module is already present unless forced (the panel
  is vendored alongside the native binary).

xtask holds no plugin-specific build knowledge: the Rosetta cmake/molex recipe
lives in `plugins/rosetta/scripts/build.sh`, owned by the plugin repo.

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
(`setup`, `setup-plugins`, `package`, `build-host`, `build-web`). They are
conveniences; the work is done by xtask either way.
