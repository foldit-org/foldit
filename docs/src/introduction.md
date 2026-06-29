# Introduction

Foldit is a protein-folding client. A player loads a structure, manipulates it
(pull residues, build bands, run minimizers and design tools), and is scored
against a Rosetta-style energy function. This repository holds the client; the
heavy molecular work runs in out-of-process plugins.

The codebase is split so that one body of logic drives several front ends:

- **`foldit-core`** is the host-agnostic application. It owns the session,
  the undo/redo history, the scoring coordinator, the plugin client, and the
  projectors that turn state changes into render, GUI, and plugin updates. It
  touches no window system and (outside structure loading) no filesystem.
- **`foldit-desktop`** is the default binary, named `foldit`. It runs the core
  inside a winit event loop with a wry webview for the UI.
- **`foldit-web`** is the wasm32 build. It mounts the same core in a browser
  canvas and exposes it to JavaScript through `wasm-bindgen`.
- **`foldit-gui`** holds the wire and state types shared by both shells: the
  `GuiState` sections, the command/dispatch enums, and the JS-to-Rust bridge.

Both shells construct one `foldit_core::App` and drive it the same way: feed it
input, call `tick` once per frame, and forward the serialized state it pushes
back to the front end. The desktop and web paths differ only in how bytes are
delivered.

Two larger subsystems live in their own repositories and are pulled in as git
submodules:

- **viso** is the GPU render engine (wgpu). It takes a molecule assembly and
  produces a 2D texture. See the [viso book](https://github.com/foldit-org/viso) (mdbook source under `crates/viso/docs/`).
- **molex** is the molecule model and structure-format codec. See the
  [molex book](https://github.com/foldit-org/molex) (mdbook source under `crates/molex/docs/`).

The molecular backends (Rosetta, and the structure-prediction and design
models) are not built into the client. They run as plugins hosted by
**foldit-runner**, which speaks one protocol to every plugin regardless of
language. Rosetta is itself a plugin; there is no in-process Rosetta executor.

## Where to start

- New to the repo: [Build and Run](getting-started/build-and-run.md), then
  [Workspace Layout](getting-started/workspace-layout.md).
- Understanding the runtime: [Architecture Overview](architecture/overview.md)
  and [The Per-Frame Tick](architecture/tick-loop.md).
- Working on a backend: [The Plugin Runner](plugins/runner.md).
