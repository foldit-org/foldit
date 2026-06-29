# foldit-core

The host-agnostic application. `foldit-core` owns all game logic and state and
compiles for both the desktop target and `wasm32`. Outside structure loading it
makes no `std::fs` calls of its own; the host provides resource access through
`HostResources`.

The public surface (`crates/foldit-core/src/lib.rs`) is small: `App`,
`TailUpdate`, `HostResources`, `HostEffects`, the `puzzle` (structure_io)
module, and the native-only plugin-locator helpers. Everything else is
`pub(crate)`.

## Module map

| Module | Owns |
| --- | --- |
| `app` | `App` and the per-frame `tick`; command/dispatch handling, startup, preview, score coordination, view options |
| `session` | `Session`: the authoritative document (history, selection, focus, view options, transient previews, the optional `Puzzle`) and the `SessionUpdate` stream |
| `history` | The two-layer undo/redo model: per-entity lanes plus the checkpoint graph |
| `runner_client` | `RunnerClient`: the orchestrator handle and native stream bookkeeping; the kick/poll bring-up pairs |
| `runner_projector` | Broadcasts session changes to plugin workers as Full/Delta snapshots |
| `render_projector` | Republishes geometry and connections to the viso engine |
| `gui_projector` | Rebuilds the dirty `GuiState` sections (and the segment panel) |
| `scores` | Core-owned score types: RAW per-term energies, weighting, the raw-to-game conversion |
| `viz` | Plugin-sourced overlay decoders and the `Viz` overlay cache (native only) |
| `host_resources` / `host_effects` | The two host-boundary traits |
| `puzzle_toml` / `puzzle_load` / `puzzle_setup` | Puzzle manifest parsing, asset loading, and setup (filters, constraints, design gating) |
| `structure_io` (re-exported as `puzzle`) | Structure-path resolution and format loading; the one place core reads files |
| `wire_params` | Conversion between wire `ParamValue` and the orchestrator's native form |

## Key types

- **`App`** -- the single owner; see [Architecture Overview](../architecture/overview.md).
- **`Session`** and **`SessionUpdate`** -- see [The Change-Event Stream](../architecture/session-updates.md).
- **`HostResources`** / **`HostEffects`** -- the portability seam; see
  [Architecture Overview](../architecture/overview.md).

## Cross-platform notes

The scoring path is reachable on wasm, so the score types, their conversion, and
the weighting methods build on every target; only the file-IO weight loader and
the structural-viz overlays are native-gated (`#[cfg(not(target_arch = "wasm32"))]`).
`structure_io` resolves an existing file path directly and downloads a PDB id
from RCSB on native; on wasm the web entry crate fetches structure bytes and
feeds them in instead.
