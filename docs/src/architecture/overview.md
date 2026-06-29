# Overview

Everything that is not window-system or transport-specific lives in one type:
`foldit_core::App`. A host constructs an `App`, attaches a render engine, feeds
it input, and calls `tick` once per frame. The host learns about state changes
only through the bytes the tick pushes back.

## What App owns

From `crates/foldit-core/src/app/mod.rs`, an `App` holds:

- **`store: Session`** -- the authoritative document for the loaded structure or
  puzzle: history, selection, focus, view options, transient previews, and the
  optional `Puzzle` add-on. See [The Change-Event Stream](session-updates.md).
- **`harness: EngineHarness`** -- the viso engine handle plus the keybinding
  table.
- **`runner_client: RunnerClient`** -- the orchestrator handle (the plugin
  client) and the native stream bookkeeping. See
  [The Plugin Runner](../plugins/runner.md).
- **`projectors: Projectors`** -- the three consumers that translate session
  changes into render, GUI, and plugin updates.
- **`scores: ScoreCoordinator`** -- score-term weights, the in-flight
  composition-score targets, and the score-stamping methods. See
  [History and Scoring](history-scoring.md).
- **`viz: Viz`** -- the derived overlay cache (connections plus the structural
  overlays: clashes, voids, exposed hydrophobics), driven from plugin queries.
- **`gui: GuiState`** -- the wire-shaped state mirror the front end renders.
- **`host: Box<dyn HostResources>`** -- the only path to the filesystem outside
  structure loading.
- **`bringup: BringupState`** -- the non-blocking startup state machine.

## The host seam

The same `App` runs in two shells. The split is two traits:

- **`HostResources`** (`host_resources.rs`) is what the App pulls from the
  host: read a file, find the view-preset directory, name the initial structure
  path. Desktop reads the real filesystem; web returns `None` for the
  path-based pieces and feeds structure bytes in by a separate flow.
- **`HostEffects`** (`host_effects.rs`) is what the App pushes to the host each
  tick: serialized dirty GUI sections, a segment-panel tail-tip change, a
  fullscreen flip, and a progress blob to persist. The desktop host applies
  these to its winit window and `~/.foldit/` data dir; the web host turns the
  state push into a JS callback and persists progress to OPFS, no-opping the
  desktop-only effects.

Neither trait mentions a window, a webview, or wasm. That is the portability
seam: `foldit-core` compiles for both the desktop target and `wasm32` without
conditioning on the shell.

## Data flow

```
        input (keys, mouse, commands, requests)
                        |
                        v
  front end  <----  foldit_core::App  ---->  viso engine (texture)
   (webview /        |   |   |   |
    canvas)          |   |   |   +--> RunnerClient --> plugin workers
        ^            |   |   |                          (rosetta, models)
        |            |   |   +------> ScoreCoordinator
        |            |   +----------> Viz overlays
        |            +--------------> Session (history, selection, focus)
        |                                  |
        |                                  v
        |                          SessionUpdate batch
        |                                  |
        |          +------------+----------+-----------+
        |          v            v                      v
        |   RenderProjector  RunnerProjector     GuiProjector
        |     (engine)        (plugin workers)    (GuiState)
        |                                              |
        +------------ HostEffects::push_state ---------+
```

Each frame, `App::tick` advances startup, applies plugin updates, drains queued
commands, polls scores and viz replies, then drains the `Session`'s
`SessionUpdate` batch once and routes it to the three projectors. The render
projector writes to the viso engine, the runner projector broadcasts to plugin
workers, and the GUI projector rebuilds the dirty `GuiState` sections, which are
serialized and handed to `HostEffects::push_state`. The ordered walk is in
[The Per-Frame Tick](tick-loop.md).
