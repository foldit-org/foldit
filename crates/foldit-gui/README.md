# foldit-gui

The shared vocabulary between the Rust core and the JavaScript front end. This
crate holds no game logic; it defines the **types that cross the boundary** —
the state the UI renders, the commands the UI sends back, and the serialization
that carries them.

The front end holds no state of its own. It is a pure declarative render of the
`FrontendState` this crate defines, and every user intent comes back as a typed
command. That contract lives here.

## The four modules

- **`state`** — `FrontendState` and its pieces: everything the UI draws (score,
  buttons, panels, selection, puzzle info). The Rust core builds these; the
  front end only reads them.
- **`actions`** — the inbound command types: `AppCommand` (menu and app-level
  intents), `OpDispatch` (a plugin op the user triggered), and `ViewportInput`
  (pointer and key events from the 3D view).
- **`wire`** — the serialized payloads and id types (`CheckpointId`,
  `EntitySnapshotId`, `FilterStatus`, and so on) that ride the transport.
- **`bridge`** — the transport-agnostic plumbing: `IpcMessage` (the inbound
  envelope), `RequestKind`, `RequestResult`, and the `Transport` trait. Both the
  desktop webview and the wasm build implement `Transport` over the same
  message types.

## Why it is its own crate

Keeping these types in one dependency-light crate means the core
(`foldit-core`), both shells (`foldit-desktop`, `foldit-web`), and the runner
can all agree on the wire format without depending on each other's internals.

```bash
cargo check -p foldit-gui
```

See [State and the GUI Bridge](../../docs/src/architecture/gui-bridge.md) in the
workspace book for how a click travels from the UI to a plugin op and back.
