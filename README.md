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
- [pixi](https://pixi.sh), only for the plugin Python environments.

## Build and run

```bash
cargo build               # builds the default member, the `foldit` desktop binary
cargo run -- 1bfe         # load a structure by PDB id (or a file path)
cargo run                 # no argument: start at the menus
```

`cargo build` does not build the plugin worker or the Python host dylib. Build
those next to the desktop exe with:

```bash
cargo xtask build-host
```

`cargo run` rebuilds neither, so run `build-host` after changing runner code. See
`docs/src/getting-started/build-and-run.md`.

## Workspace

Root-owned members (in `crates/`, plus `xtask/`):

| Member | Role |
| --- | --- |
| `foldit-core` | Host-agnostic application logic: session, history, scoring, plugin client, projectors |
| `foldit-desktop` | The default binary `foldit`: winit + wry + wgpu shell |
| `foldit-web` | wasm32 entry: canvas-mounted, wasm-bindgen surface |
| `foldit-gui` | Wire and state types, the JS-to-Rust bridge (in-tree, not a submodule) |
| `foldit-runner/python-host` | The Python plugin host dylib (a member living inside the runner submodule) |
| `xtask` | Build and packaging automation |

Submodules (each in its own repo, excluded from the workspace): `foldit-runner`,
`molex`, `viso`, `foldit-plugin-sdk`. See
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

The plugin Python environments are managed by pixi inside `crates/foldit-runner`.
Install them with `cargo xtask setup-envs`.

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
