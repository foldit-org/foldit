# State and the GUI Bridge

The front end never holds the authoritative game state. Core owns it in
`GuiState` and pushes the parts that changed; the front end renders what it
receives and sends intents back. Desktop and web use the same message shapes;
only the byte-delivery channel differs.

## GuiState and DirtyFlags

`GuiState` (in `crates/foldit-gui/src/lib.rs`) is a struct of named sections:
`app_state`, `score`, `puzzle`, `selection`, `view`, `ui`, `actions`,
`loading`, `scene`, `history`, `panels`, `progress`, plus the per-residue
`segment_info`. Some fields are authoritative source-of-truth maps that are not
serialized (marked `#[serde(skip)]`); their wire mirrors are regenerated on each
mutation. For example `panels_open` and `panels_positions` are the truth, and
the `panels` section is rebuilt from them.

A `DirtyFlags` bitset tracks which sections changed since the last push:
`SCORE`, `SELECTION`, `VIEW`, `UI`, `LOADING`, `ACTIONS`, `SCENE`, `PUZZLE`,
`APP_STATE`, `HISTORY`, `HISTORY_LIVE`, `TEXT_BUBBLE`, `SEGMENT`, `PANELS`,
`PROGRESS`. The GUI projector and the App raise bits; at the end of the tick
`bridge::push::serialize_dirty` emits only the dirty sections as a partial JSON
object and clears the flags. The front end merges that partial into its local
copy. An unchanged section is never serialized.

`app_state` is the `AppPhase` lifecycle gate the front end keys its top-level
view off: `Initializing` and `LoadingSession` show the loading screen, `Landing`
is the menus with no session, and `InSession` is live play. (`Downloading` is
defined but never entered; there is no App-orchestrated download boundary
today.)

## The bridge

`foldit_gui::bridge` (`crates/foldit-gui/src/bridge/`) is transport-agnostic.
Both shells share its four submodules:

- **`message`** -- `IpcMessage` (the inbound envelope) and `RequestKind`.
- **`decode`** -- parse inbound JSON into typed commands.
- **`push`** -- `serialize_dirty`, the dirty-section serializer.
- **`transport`** -- the `Transport` trait and `RequestResult`; the per-platform
  delivery channel.

Inbound traffic is one of: fire-and-forget commands (`AppCommand`,
`ViewportInput`), op dispatches (`OpDispatch`, queued and drained on the next
tick), and synchronous requests.

## The request round-trip

`App::handle_request(kind, payload) -> RequestResult` resolves a request
synchronously and returns `Ok(json)` or `Err(message)`. `RequestKind`:

| Kind | Returns |
| --- | --- |
| `ReadResourceFile` | base64-wrapped file bytes, read through `HostResources::read_file` |
| `PanelsCatalog` | the plugin-contributed panel list (empty on wasm) |
| `SettingsCatalog` | the plugin settings tabs (empty on wasm) |
| `PluginQuery` | base64-wrapped opaque plugin reply bytes for a query id, run against the live focus, selection, and designable set (rejected on wasm) |

Binary replies are wrapped as `{ "encoding": "base64", "content": ... }`, the
envelope the JS request path expects.

## How the two shells deliver bytes

The message shapes above are identical across shells. What differs:

- **Desktop** (`crates/foldit-desktop/src/window.rs`): the wry webview posts IPC
  messages that arrive on an `mpsc` channel; the event loop decodes them and
  calls into `App`. State pushes are forwarded to the webview.
- **Web** (`crates/foldit-web/src/lib.rs`): JS calls the `FolditApp`
  `wasm-bindgen` methods directly. State pushes invoke a registered JS callback
  with the JSON string; async requests resolve through a second callback. No
  JSON round-trips through a `window.ipc` shim.

See [foldit-desktop](../crates/foldit-desktop.md) and
[foldit-web](../crates/foldit-web.md) for the shell-specific lifecycles, and
[The Frontend](../tooling/frontend.md) for the JavaScript side.
