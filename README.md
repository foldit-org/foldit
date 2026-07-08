# Foldit

A protein-folding client. A player loads a structure, manipulates it (pull
residues, build bands, run minimizers and design tools), and is scored against a
Rosetta-style energy function. One body of host-agnostic logic drives a desktop
shell and a web (wasm) shell; the molecular backends run as out-of-process
plugins.

Full documentation is the mdbook under `docs/`. Build it with `mdbook build
docs` and open `docs/book/index.html`, or read the source under `docs/src/`.

## Prerequisites

- Rust, via [rustup](https://rustup.rs).
- [bun](https://bun.sh), for the GUI frontend under `webview/`.
- [pixi](https://pixi.sh), only for the plugin Python environments.

## Build and run

`cargo build`/`cargo run` builds only the `foldit` desktop binary. The backend
host (`foldit-worker` + the Python host dylib) and the plugins are built
out-of-band by `xtask`. The fresh-clone path:

```bash
cd webview && bun install && cd ..   # GUI frontend deps (once, and when they change)
cargo xtask setup                    # backend host (release) + every plugin
cargo run --release -- 1bfe          # load a structure by PDB id (or a file path)
cargo run --release                  # no argument: start at the menus
```

`cargo xtask setup` builds the host and provisions all plugins. The native
Rosetta plugin ships a **committed prebuilt binary + database** per platform, so
`setup` uses those directly — it does **not** compile Rosetta from source. It
does solve the Python plugins' pixi environments (see below), which is the slow
part. See `docs/src/getting-started/build-and-run.md`.

### Debug vs release

A **release** build (`cargo run --release`) serves the GUI from the prebuilt
`assets/gui` over the `foldit://` protocol, so it needs `cargo xtask build-gui`
first (the webview is blank otherwise), and the host artifacts in
`target/release/` (what `setup` produces).

A **debug** build (`cargo run`) instead spawns a Vite dev server (`bun run dev`
on `localhost:5173`) for the GUI — so bun + the installed `webview/` deps are
required, but `build-gui` is not (it would be ignored). The debug worker/dylib
must sit in `target/debug/`, so for debug iteration run `cargo xtask build-host
--debug` (and `cargo xtask setup-plugins` for plugins) rather than `setup`.
Skipping the GUI deps still launches the app, just with no GUI overlay ("running
without webview overlay" in the log).

### Native Rosetta plugin: vendored by default, build opt-in

The Rosetta plugin is native code. Its per-platform shared library and 251 MB
database are **committed into the plugin repo** (`plugins/rosetta/prebuilt/
<target-triple>/` and `assets/database/`), so a fresh clone runs without a
Rosetta build. The ~25 GB Rosetta source (`deps/rosetta-interactive`) is an
opt-in submodule (`update = none`), so a recursive clone does **not** pull it —
you only need it to build from source. To rebuild from source (needs a C++
toolchain):

```bash
cargo xtask setup-plugins rosetta --from-source
```

That fetches the `deps/rosetta-interactive` source on demand, runs the plugin's
own build recipe, and installs the result into `plugins/rosetta/local/`
(gitignored), which the runtime prefers over the committed `prebuilt/` fallback.
See `docs/src/plugins/`.

### xtask build commands

| Command | Output |
| --- | --- |
| `setup` | Fresh-clone bootstrap: `build-host` (release) + `setup-plugins` (all) |
| `setup-plugins [id] [--from-source] [--clean]` | Provision plugins from each `plugin.build.toml`: native binary (vendored unless `--from-source`), pixi env, panel UI |
| `build-host [--debug]` | `foldit-worker` + `libfoldit_python_host` dylib in `target/<profile>/` (defaults to release) |
| `build-gui` | main GUI → `assets/gui` (release path; debug uses the dev server instead) |
| `build-web [--debug]` | wasm cdylib + JS glue in `webview/public/pkg/` (needs nightly) |
| `build-molex` | rebuild the molex Python extension into the plugin pixi envs |
| `package` | OS installers in `dist/` |
| `package-web` | static web build |

## Workspace

Root-owned members (in `crates/`, plus `xtask/`):

| Member | Role |
| --- | --- |
| `foldit-core` | Host-agnostic application logic: session, history, scoring, plugin client, projectors |
| `foldit-desktop` | The default binary `foldit`: winit + wry + wgpu shell |
| `foldit-web` | wasm32 entry: canvas-mounted, wasm-bindgen surface |
| `foldit-gui` | Wire and state types, the JS-to-Rust bridge (in-tree, not a submodule) |
| `foldit-runner/python-host` | The Python plugin host dylib (a member under the in-tree `crates/foldit-runner`) |
| `xtask` | Build and packaging automation |

Submodules (each in its own repo, excluded from the workspace): `molex`, `viso`,
`foldit-plugin-sdk`, plus the plugin repos under `plugins/`. See
`docs/src/getting-started/workspace-layout.md`.

## Architecture

Both shells construct one `foldit_core::App`, attach a viso render engine, feed
it input, and call `tick` once per frame. The App owns the session, the
undo/redo history, the scoring coordinator, the plugin client, and three
projectors that turn session changes into render, GUI, and plugin updates. The
host boundary is two traits (`HostResources` for resource access in,
`HostEffects` for per-frame outputs out), which is what lets the same core run in
the wry/winit desktop shell and the wasm web shell. See
`docs/src/architecture/`.

## Backends and plugins

The molecular backends are not compiled into the client. They run as plugins
hosted by **foldit-runner**, which speaks one protocol to every plugin over an
interprocess socket plus iceoryx2 shared memory. The worker binary is
`foldit-worker`. Rosetta is itself a native plugin; the structure-prediction and
design models are Python plugins loaded through `foldit-python-host`. See
`docs/src/plugins/`.

The plugin Python environments are managed by pixi, one per plugin. They are
provisioned (`pixi install --all`) as part of `cargo xtask setup-plugins` (which
`setup` calls), alongside each plugin's native/UI build.

## Development

```bash
cargo build --release         # release build
cargo test --workspace        # tests
just check-all                # clippy + deny + machete + file-length gates
```

To work on viso or the plugin SDK locally, populate the submodule and keep its
`[patch]` block in `Cargo.toml` active; comment the block out to build against
the published version. See `docs/src/getting-started/submodules.md`.

For OS installers use `cargo xtask package` (output in `dist/`); for the static
web build use `cargo xtask package-web`. See `docs/src/tooling/`.
