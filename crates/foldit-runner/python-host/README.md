# foldit-python-host

The bridge that lets a Python plugin run inside Foldit. It is a `cdylib` (a
dynamically loaded library) that links libpython, so that libpython only enters
the process when a Python plugin is actually in use — a session with only native
plugins never loads it.

## How it is loaded

`foldit-worker` loads this dylib through `libloading`, but **only** when a
plugin's `plugin.toml` declares `kind = "python"`. The handoff is deliberately
minimal:

- `foldit_python_host_abi_version` — a C-ABI version probe the worker calls
  first, so it can reject a stale or mismatched dylib before trusting anything
  else.
- `foldit_python_host_create(plugin_dir)` — a Rust-ABI entry that re-reads the
  plugin's `plugin.toml`, initializes the Python interpreter, loads the plugin's
  Python module, and returns a `Box<dyn Plugin>` the worker then calls directly.
  No C-ABI vtable, no per-call marshaling.

`plugin_dir` lets the Python plugin find its own assets — for example model
weights under `<plugin_dir>/assets/weights/`.

## Why the Rust-ABI handoff is safe

Passing a `Box<dyn Plugin>` across a dylib boundary is normally unsound, but it
works here because the worker and this dylib are **co-built from one workspace**:
same rustc, same dependency versions, same flags, same (System) allocator. The
worker holds the dylib for the plugin's whole lifetime and drops the box before
unloading it.

This crate is a workspace member; its source lives in-tree under
`foldit-runner`. It is built alongside the worker by:

```bash
cargo xtask build-host
```
