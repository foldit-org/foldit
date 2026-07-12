# foldit-core

The brain of Foldit. Every piece of game state and behavior lives here, and
nothing that is specific to a window system or a network transport does. The
same core runs behind both the desktop app and the web build — each of those is
a thin shell that wraps one `App` from this crate.

If you are trying to understand "what does Foldit actually do when I click a
button," this is where you read.

## What it owns

The center of the crate is `App` (`src/app/`). It holds:

- **The session** (`src/session/`) — the authoritative document: the structure,
  the edit history, the current selection and focus, view options, live
  previews, and the optional puzzle. All state changes flow through it as a
  stream of `SessionUpdate` events.
- **The plugin client** (`src/runner_client/`) — how the core talks to the
  out-of-process scientific backends (Rosetta, design, crystallography, ML
  prediction) without knowing anything about their internals.
- **The score coordinator** (`src/scores.rs`) — turns raw plugin energies into
  the game score.
- **The projectors** (`src/*_projector.rs`) — three fan-out routes that take
  `SessionUpdate` changes and push them to the render engine (viso), the plugin
  workers, and the GUI state, respectively.
- **Puzzle loading** (`src/puzzle_toml.rs`, `src/puzzle_load.rs`) — parsing a
  `puzzle.toml` level and turning it into a playable session.

## How it talks to the outside world

The core makes almost no calls to the outside on its own. Instead the host (the
desktop or web shell) supplies capabilities through the `HostResources` trait
and receives per-frame outputs through `HostEffects`. The only exception is
structure loading, which reads files directly. This is what lets the exact same
logic compile for the desktop target and for `wasm32`.

## Building and testing

`foldit-core` is a library, not a runnable binary — you run it through
`foldit-desktop` or `foldit-web`. To check and test it in isolation:

```bash
cargo check -p foldit-core
cargo test  -p foldit-core
```

## Where to look next

The architecture walkthrough in the workspace book (`docs/`, under
"Architecture" and "The Crates → foldit-core") explains the per-frame tick, the
`SessionUpdate` stream, and the GUI bridge in depth. Start there if you want the
mental model before diving into the code.
