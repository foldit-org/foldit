# The Change-Event Stream

`Session` mutations do not call into the render engine, the plugin workers, or
the GUI directly. Every observable mutation pushes one `SessionUpdate` onto an
internal queue. Once per frame the tick drains that queue and hands the batch to
each projector, which decides independently how to react.

## SessionUpdate

`SessionUpdate` (in `crates/foldit-core/src/session/change.rs`) is signal-only:
it names *what changed*, never the payload. Carrying payloads would duplicate
state the projectors already read from `Session` and invite drift, so each
projector re-derives what it needs from `Session` when it sees a relevant
variant.

The variants:

| Variant | Meaning |
| --- | --- |
| `Edit { tentative }` | A structural/coordinate edit. `tentative` marks per-cycle live edits (a pull-drag, a mid-action plugin frame); the plugin broadcaster and persistent projectors skip those, the render projector consumes them. |
| `HeadMoved` | The history head moved (undo / redo / jump / commit / reset). |
| `PreviewAdded` / `PreviewUpdated` / `PreviewDiscarded` | A transient overlay entity was added, had its geometry updated in place (a streaming frame), or was dropped. |
| `ScoresChanged` | A head / edit / checkpoint score value changed. |
| `SelectionChanged` | The residue selection changed. |
| `FocusChanged` | Session focus changed (Tab-cycle or reset). |
| `BubbleChanged` | The active tutorial bubble cursor advanced or stepped back. |
| `PuzzleChanged` | A puzzle loaded, or a free-form load dropped the objective. |
| `ViewOptionsChanged` | A render option toggled, or a preset applied. |
| `EntityAppearanceChanged` | A per-entity ambient appearance override changed. |
| `CurationChanged` | A history curation flag changed (pin / unpin / exclude-from-best). |

`SessionUpdate::is_geometry()` is true for the coordinate-mutating variants
(`Edit`, `HeadMoved`, and the three `Preview` variants). The tick uses it as the
single key for "the scene, scores, and overlays are now stale": an assembly
republish, an at-rest viz refresh, and a score request all gate on it.

## The consumer contract

Each projector implements `SessionUpdateConsumer`:

```rust
pub trait SessionUpdateConsumer {
    type Sources<'a>;
    type Sink;
    type Out;
    fn consume(
        &mut self,
        updates: &[SessionUpdate],
        sources: Self::Sources<'_>,
        sink: &mut Self::Sink,
    ) -> Self::Out;
}
```

A consumer reads the drained batch plus borrowed `Sources` (the `Session`, and
whatever else it needs, such as the engine or the score state) and writes its
one `Sink`. The three implementations:

- **`RenderProjector`** -- `Sink` is the viso engine; it republishes geometry
  and stamps connections.
- **`RunnerProjector`** -- `Sink` is the orchestrator; it broadcasts Full/Delta
  snapshots to plugin workers.
- **`GuiProjector`** -- `Sink` is `GuiState`; it rebuilds the dirty sections. See
  [State and the GUI Bridge](gui-bridge.md).

Because the batch is signal-only and the projectors are independent, adding a new
observer means implementing the trait and adding one drain call to the tick; the
mutation sites do not change.

## What Session owns

`Session` (`crates/foldit-core/src/session/mod.rs`) is the authoritative document
over the whole scene. It owns the two-layer `History`, the transient preview
entities (presence in the `transient` map *is* the preview signal), per-entity
display names, the residue selection, per-entity appearance overrides, the
session focus, the title, the optional `Puzzle`, and the pending `SessionUpdate`
queue. History and the puzzle add-on are covered in
[History and Scoring](history-scoring.md).
