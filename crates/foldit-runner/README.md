# foldit-runner

Out-of-process plugin runner and orchestrator for Foldit. It hosts
language-agnostic plugins (Python and native) behind a single
proto-based protocol (`proto::plugin`), so the main application can
dispatch structure edits, streaming operations, and queries to a plugin
without linking that plugin's runtime into its own process.

The crate ships one library (`foldit_runner`) and one binary
(`foldit-worker`).

## Architecture

The `Orchestrator` (in `src/orchestrator/`) is the coordination point.
It owns the entity-lock table, the plugin worker pool, the op/query
registry, and the in-flight pending-operation maps (async invokes,
streams, score/query requests). It never links a plugin's runtime
itself.

Each plugin runs in its own `foldit-worker` subprocess:

```
┌──────────────────────────────────────────────┐
│  Host application                            │
│                                              │
│  ┌──────────────┐                            │
│  │ Orchestrator │  spawns one worker/plugin  │
│  └──────┬───────┘                            │
└─────────┼────────────────────────────────────┘
          │  local socket (length-prefixed proto messages)
   ┌──────┴───────┬──────────────┐
   ▼              ▼              ▼
┌────────┐   ┌────────┐   ┌────────┐
│ worker │   │ worker │   │ worker │
│ python │   │ native │   │  ...   │
└────────┘   └────────┘   └────────┘
```

- A **Python** plugin worker loads the `foldit-python-host` cdylib via
  `libloading` and boots a Python interpreter from the plugin's
  environment. libpython only enters a process when that dylib is
  dlopened; `foldit-runner` itself has no pyo3 / libpython in its link
  graph.
- A **native** plugin worker dlopens the plugin's shared library and
  dispatches through the C-ABI vtable in `src/plugin/abi.rs`.

Communication between the orchestrator and each worker is entirely over
a local (Unix domain / named-pipe) socket, carrying length-prefixed
proto messages. Framing and socket helpers live in `src/ipc/sockets.rs`
and `src/ipc/messaging.rs`. A shared-memory transport for large binary
payloads exists under `src/ipc/iceoryx/` but is unwired and
forward-looking; no live path uses it (see that module's docs).

The `foldit-plugin-sdk` crate owns `plugin.proto` and the protocol types
(the `Plugin` trait, `DispatchContext`, `ParamValue`, `PluginError`, the
C-ABI, proto<->native decode). `foldit-runner` re-exports that surface so
internal `crate::proto::plugin::*` paths resolve against one source of
truth.

## Plugin manifest model

There is no hardcoded plugin list. The orchestrator's `discover_plugins`
scans a plugins root for `*/plugin.toml` manifests and builds a spawn
descriptor per plugin. A worker is spawned lazily, on first
registration, not at discovery time. The worker reads its own kind and
entry point from `<plugin_dir>/plugin.toml` at startup.

A minimal manifest:

```toml
id = "dummy"
kind = "python"

[python]
entry = "dummy"
```

Manifests also declare the plugin's user-facing surface — buttons,
panels, and settings tabs — which the orchestrator joins with the
registered ops to build the GUI action catalog.

## Building

The worker binary is a normal cargo target:

```bash
cargo build --bin foldit-worker
```

As a workspace member, `cargo build` from the monorepo root builds
`foldit-worker` alongside the main application. The host locates the
worker binary at runtime (next to the main executable, on `PATH`, or in
`target/<profile>/`).

The worker is invoked with two arguments — the plugin directory and the
IPC endpoint — which the orchestrator supplies automatically when it
spawns a worker:

```bash
foldit-worker <plugin_dir> <ipc_endpoint>
```

## Development

`pixi` manages the per-plugin Python environments at dev/build time only;
a shipped Foldit has no pixi and never invokes it at runtime. Python
plugin workers are spawned directly, with the orchestrator pointing the
interpreter at the plugin's environment via `PYTHONHOME` and the loader
path.

The `justfile` wraps the check suite (mirrors CI):

```bash
just check        # fmt-check + clippy + test + doc
just clippy
just test         # co-builds foldit-worker + foldit-python-host, then tests
just check-all    # check + machete + deny + file-lengths
```
