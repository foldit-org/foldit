# foldit-desktop

The default binary, named `foldit` (`default-run = "foldit"`). It runs
`foldit_core::App` inside a winit event loop with a wry webview for the UI and
wgpu for rendering. All game logic lives in the core; this crate is the desktop
shell.

## Entry point

`main.rs` is thin: it sets up logging, resolves the structure path from argv
(`foldit_core::puzzle::resolve_structure_path`), constructs the App with a
`DesktopHost`, and hands it to `window::run`. With no argument the App settles at
the menus (`Landing`). It also installs signal handlers that kill plugin worker
process groups on `SIGINT`/`SIGTERM` so Python subprocesses are not orphaned.

Before anything reads an asset, `init_bundle_resource_paths` probes a small
candidate list relative to the executable for a packaged resource directory
(macOS `Contents/Resources`, Linux `../lib/foldit` or `../share/foldit`, or next
to the exe) and, if it finds one containing `assets/gui`, points the resource
resolvers at it through their `FOLDIT_*_ROOT` environment overrides. A
pre-existing env value always wins, and a dev `cargo run` matches no candidate,
so this is a no-op outside a real bundle. See
[Assets and Bundling](../tooling/assets-bundling.md).

## DesktopHost

`host.rs` implements `foldit_core::HostResources`: real filesystem reads, a
view-preset directory resolved relative to the executable (with a dev fallback
that walks up to the repo-root `assets/view_presets`), and the bootstrap
structure path from argv.

## The event loop

`window.rs` holds `AppRunner`, which owns the `App` by value and implements
winit's `ApplicationHandler`. It owns the wry webview, the IPC receiver, frame
timing, and the dev-server wiring. It also runs the `HostEffects` side: a small
single-threaded tokio runtime persists the high-score progress map to
`~/.foldit/progress.json` off the event-loop thread, and the structure load is
deferred until the webview's loading screen is visible. State pushes from the
tick are forwarded into the webview.

## Worker artifacts

The desktop app spawns plugin workers and dlopens the Python host dylib at
runtime. `cargo run` builds neither; build them next to the exe with
`cargo xtask build-host`. The dylib file name is platform-specific
(`libfoldit_python_host.dylib`, `.so`, or `.dll`) and mirrors the name xtask
uses. See [Python and Native Plugins](../plugins/hosts.md).
