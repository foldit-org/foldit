# Plugin SDK and Protocol

**foldit-plugin-sdk** (the `crates/foldit-plugin-sdk` submodule) defines the
contract every plugin implements. It owns `plugin.proto`, the protocol types,
and the `Plugin` trait, and it exposes a cbindgen C-ABI (consumed by the Rosetta
C++ bridge) and pyo3 bindings, plus a `python/` layer of plugin-author
utilities.

The SDK is implemented and load-bearing, not a placeholder. `foldit-runner`
depends on it and re-exports its proto types, the `Plugin` trait, the assembly
payload, the C-ABI, and `PluginError`; `foldit-python-host` builds `PluginError`
values against it directly. (The SDK's own README still calls itself an early
skeleton; that text is stale relative to the code.)

## Manifest

Each plugin ships a `plugin.toml` next to its assets. The manifest declares:

- `id`, `name`, `kind` (`native` or `python`), and an `order`.
- A `[native]` or `[python]` block naming the binary or entry module.
- `[[buttons]]` -- user-facing action buttons. Each `op` matches a bridge action
  row; the entry also carries display text, an icon, an optional hotkey badge,
  and a tooltip. On the desktop build a hotkey press resolves to the owning op
  and dispatches (first registered binding wins if two plugins claim the same
  key); on the web build the badge renders but key dispatch is not yet wired.
- `[[panels]]` -- plugin-contributed UI panels. Each `entry` is a manifest-
  relative ES module path that the front end dynamically imports, deriving a
  launcher button from it. The runner resolves icon paths to absolute.

The host reads manifests in two places: `Orchestrator::discover_plugins` builds
the op/query/panel registry, and `foldit_core::locate_plugin_ui_entrypoints`
walks `<plugins_root>/*/plugin.toml` to collect each declared panel `entry` as
the `/plugins/<id>/<entry>` URL the release asset protocol is allowed to serve.

## Ops, queries, and panels at runtime

- **Ops** are dispatched actions (a minimize, a repack, a prediction). An op
  declares lock metadata, including whether it `creates_entities` (its output is
  a new entity to adopt rather than an edit of an existing lane).
- **Queries** are read-only requests for derived data. The structural-viz
  overlays drive off queries, and a query stays inert until a plugin registers
  it (`RunnerClient::supports_query` gates the at-rest trigger).
- **Panels** are plugin UI. The Rosetta plugin declares a `rama_map` panel; its
  panel module is built on demand by xtask and is not committed. The panel UI is
  currently a placeholder, and the manifest's hotkey badges render but are not
  yet wired to dispatch.

## ES module build

Plugin panel modules are built with `cargo xtask build-rosetta-ui` (a `bun run
build` in the plugin's `ui/` directory) and copied into the staging bundle by
`cargo xtask package`. See [xtask Commands](../tooling/xtask.md).
