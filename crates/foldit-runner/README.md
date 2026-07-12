# foldit-runner

The plugin host. Foldit's scientific engines — Rosetta, the ML structure
predictors and designers, the crystallography backend — are **not** compiled
into the app. They run as separate processes, and this crate is what launches
them, talks to them, and keeps them in sync with the structure the player is
editing.

Everything speaks one protocol (defined in
[`foldit-plugin-sdk`](../foldit-plugin-sdk)'s `plugin.proto`), regardless of
whether a given plugin is written in Rust, C++, or Python.

## The pieces

- **The orchestrator** (`src/orchestrator/`) owns the canonical molecular
  Assembly and the session state. When the core dispatches an op, the
  orchestrator routes `Invoke` / `StartStream` / `Query` to the plugin that owns
  it, applies the result back into canonical state, and broadcasts the change to
  the other plugins.
- **`foldit-worker`** is the binary this crate produces (`src/bin/`). Each
  plugin runs inside a worker process; the worker loads the plugin and relays
  the protocol over a local socket.
- **The plugin loader** (`src/plugin/`) resolves and loads a plugin per its
  `plugin.toml`: a native plugin is a dylib loaded through the C ABI; a Python
  plugin is handed to [`foldit-python-host`](python-host).

## No Python in the link graph

This crate deliberately has no pyo3 or libpython in its dependencies. Python
plugin hosting lives entirely in the sibling `foldit-python-host` cdylib, which
`foldit-worker` loads only when a plugin manifest declares `kind = "python"`.
That way a session using only native plugins never pulls libpython into the
process.

## Building

`foldit-worker` and the Python host are not built by a plain `cargo build`
(nothing in the desktop app's dependency graph pulls them in). Build them with:

```bash
cargo xtask build-host      # foldit-worker + foldit-python-host next to the app exe
cargo xtask setup           # the above, plus set up every plugin
```

See [The Plugin Runner](../../docs/src/plugins/runner.md) and
[Python and Native Plugins](../../docs/src/plugins/hosts.md) in the workspace
book.

> Build note: run its quality gate from this directory (`just check-all`);
> checking `-p foldit-runner` from the workspace root does not resolve the same
> way.
