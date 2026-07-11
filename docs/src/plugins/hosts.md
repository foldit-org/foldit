# Python and Native Plugins

A plugin's `plugin.toml` declares its `kind`. The worker loads the two kinds
differently, and only the Python path pulls libpython into the process.

## Native plugins

`kind = "native"` plugins are shared libraries that export the plugin vtable.
The worker dlopens the library directly. The native plugins are `rosetta`,
`design`, and `xtal`. Rosetta is the most involved: its
manifest names a `binary` basename that
`PluginManifest::native_binary_name()` resolves to the platform-canonical file
name (`librosetta_interactive.dylib`, `librosetta_interactive.so`, or
`rosetta_interactive.dll`). The Rosetta plugin shim exports
`foldit_plugin_vtable` from the existing `librosetta_interactive` dylib; there is
no separate plugin dylib.

## Python plugins

`kind = "python"` plugins (the `foundry` and `simplefold` plugins
under `plugins/`) run through **foldit-python-host**, a
`cdylib` that links libpython. The worker dlopens it only when it is about to
host a Python plugin, so libpython joins the process lazily; a session that uses
only native plugins never loads it.

`foldit-python-host` is a root workspace member; its source lives in-tree
under `foldit-runner` (`crates/foldit-runner/python-host`). It
depends on `foldit-runner` for the `Plugin` trait, the proto and orchestrator
types, and the C-ABI struct definitions, and on `foldit-plugin-sdk` for the
`PluginError` value type its `impl Plugin for PyPlugin` builds.

## Locating the dylib

The dylib's platform file name is a constant in `foldit-desktop`'s `main.rs`
(`libfoldit_python_host.dylib` / `.so` / `.dll`), mirroring the name xtask
builds. The worker finds it next to its own executable by default. When the
dylib ships as a packaged resource instead, the desktop binary sets
`FOLDIT_PYTHON_HOST_DYLIB` to its absolute path (inherited by the worker) during
bundle-resource resolution. See [foldit-desktop](../crates/foldit-desktop.md).

The Python environments themselves are managed by pixi, one per plugin (each
plugin's own `pixi.toml`), not by the root pixi project; they are provisioned by
`cargo xtask setup-plugins`. See [xtask Commands](../tooling/xtask.md).
