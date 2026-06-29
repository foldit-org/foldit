# foldit-gui

The wire and state types shared by both shells. It holds `GuiState` and its
sections, the command and dispatch enums, the JS-to-Rust bridge, and the
serializable history wire types. It depends on neither viso nor a window system,
so both `foldit-core` and the shells can pull it in cheaply.

This crate is an in-tree root workspace member, not a submodule. (Its
`Cargo.toml` repository field points at this repo, and it is absent from
`.gitmodules`.)

## What it exposes

- **`GuiState`** and `DirtyFlags` -- the section-shaped state mirror and the
  bitset that tracks which sections changed. See
  [State and the GUI Bridge](../architecture/gui-bridge.md).
- **`AppPhase`** -- the top-level lifecycle phase the front end gates its root
  view on (`Initializing`, `Landing`, `LoadingSession`, `InSession`; plus a
  reserved `Downloading`).
- **`actions`** -- `AppCommand`, `OpDispatch`, `ViewportInput`: the inbound
  intent types.
- **`bridge`** -- `IpcMessage`, `RequestKind`, `RequestResult`, `Transport`, the
  JSON decoder, and `push::serialize_dirty`. Transport-agnostic; desktop and web
  share it.
- **`wire`** -- the serializable history and checkpoint types
  (`CheckpointInfo`, `HistorySection`, `HistoryCommand`, `HistoryLiveUpdate`,
  `FilterStatus`, and friends) plus `WireId`.

The state sections serialize to JSON for the front end; some authoritative
fields are kept off the wire (`#[serde(skip)]`) and their wire mirrors are
regenerated on mutation.

## Naming note

The JavaScript package under `webview/` is also named `foldit-gui`
(`webview/package.json`). It is the SolidJS front end and is unrelated to this
Rust crate beyond consuming the wire shapes this crate defines. See
[The Frontend](../tooling/frontend.md).
