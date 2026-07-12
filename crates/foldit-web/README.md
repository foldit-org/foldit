# foldit-web

The browser build. This crate is the WebAssembly entry point that lets Foldit
run in a web page: it mounts the viso render engine into a host-provided
`<canvas>`, owns the `requestAnimationFrame` loop, and exposes a `FolditApp`
object to JavaScript through `wasm-bindgen`.

Like [`foldit-desktop`](../foldit-desktop), it is a thin shell over
[`foldit-core`](../foldit-core) — all the game logic is shared. The only thing
that changes between the two is the delivery mechanism: where the desktop app
round-trips JSON through a webview IPC channel, the web build has JavaScript
call Rust functions directly via `wasm-bindgen`. The message envelope
(`foldit_gui::bridge`) is the same.

## The JavaScript lifecycle

```js
await init();                                        // panic hook + console logging
await initThreadPool(navigator.hardwareConcurrency); // rayon thread pool
const app = new FolditApp();
app.set_state_callback(json => onState(JSON.parse(json)));
app.set_response_callback((wishId, ok, payload) => __onResponse(wishId, ok, payload));
await app.start(canvas);                             // mounts viso, starts the rAF loop
```

## Building

The wasm build requires the nightly toolchain and produces the bundle under
`webview/public/pkg/`:

```bash
cargo xtask build-web
```

Outside of `wasm32` this crate still compiles as a thin rlib so that
workspace-wide `cargo check`/`clippy` cover it without a wasm target.

> Note: `cargo check -p foldit-web` checks the **host** target, not wasm. The
> real wasm build is the one produced by `cargo xtask build-web`.
