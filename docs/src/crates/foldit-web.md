# foldit-web

The wasm32 entry crate. It mounts viso into a host-provided `<canvas>`, owns the
`requestAnimationFrame` loop, and exposes `foldit_core::App` to JavaScript
through `wasm-bindgen`. The same core that runs on desktop runs here; this crate
is the wasm shell around it.

Built as a `cdylib` for wasm and an `rlib` otherwise, so a workspace-wide
`cargo check` on the host succeeds without invoking wasm-bindgen. The wasm-only
body is gated with `#![cfg(target_arch = "wasm32")]`.

## FolditApp

`FolditApp` is the `#[wasm_bindgen]` handle. It wraps the `App` (in an
`Rc<RefCell<...>>` since wasm is single-threaded) plus two JS callbacks: one for
state-section pushes, one for resolving async requests. The JS lifecycle:

```ignore
await init();                                    // panic hook + console logging
await initThreadPool(navigator.hardwareConcurrency);  // rayon pool
const app = new FolditApp();
app.setStateCallback(json => onState(JSON.parse(json)));
app.setResponseCallback((wishId, ok, payload) => ...);
await app.start(canvas);                          // mounts viso, starts the rAF loop
```

`start` builds a wgpu `RenderContext` against the canvas, constructs a
`VisoEngine`, hands it to the App, and starts the render loop.

## Host implementation

The web `HostResources` returns `None` for the path-based pieces (no filesystem,
no path-based presets) and the host fetches structure bytes itself, feeding them
in through the orchestrator rather than by path. The `HostEffects` state push
becomes a JS callback; the desktop-only effects (fullscreen, tail) are no-ops on
web. High-score progress is persisted to the origin-private OPFS file system: a
startup task reads `progress.json` and the rAF loop merges it once via
`App::import_progress`.

## Same bridge as desktop

The IPC envelope, the `RequestKind` request shapes, and the dirty-section
serializer are `foldit_gui::bridge`, shared with the desktop transport. Only the
delivery mechanism changes: JS calls Rust functions directly through wasm-bindgen
instead of round-tripping JSON through a webview IPC shim. See
[State and the GUI Bridge](../architecture/gui-bridge.md).

## viso web feature

This crate deliberately does not enable viso's `web` feature (see the comment in
`Cargo.toml`). The wasm build is produced through xtask; see
[xtask Commands](../tooling/xtask.md) (`build-web` and `package-web`).
