# The Per-Frame Tick

`App::tick(dt, fx)` is the single drive loop. The host calls it once per frame
and passes a `&mut dyn HostEffects` sink for the per-frame outputs. The order
matters: several steps run before the change-event drain specifically so the
state they mutate lands in this frame's batch rather than the next. The walk
below follows `crates/foldit-core/src/app/mod.rs`.

1. **`advance_startup()`** -- step the non-blocking bring-up state machine. It
   runs first so a publish a startup step triggers (the structure parse, the
   post-`Init` adoption) is in this frame's batch. Inert once bring-up is done.
   See [History and Scoring](history-scoring.md) for the score side and
   [The Plugin Runner](../plugins/runner.md) for the warm/init phases.

2. **`apply_backend_updates()`** -- drain whatever plugin workers have streamed
   (op frames, completions) and apply them to the session.

3. **Drain queued dispatches** -- ops enqueued this frame by
   `on_dispatch_op` are taken and run through `handle_dispatch_op`.

4. **`scores.poll(...)`** -- apply this tick's async score replies before the
   drain, so their `ScoresChanged` lands in this batch.

5. **`runner_client.poll_query_results()`** -- collect async viz query replies;
   when any arrived, `viz.apply_replies(...)` decodes them into overlay payloads.

6. **`store.take_updates()`** -- drain the `SessionUpdate` stream once. Everything
   downstream reads this one `changes` slice. `has_geometry` is set when the
   batch carries a scene-mutating variant (see
   [The Change-Event Stream](session-updates.md)).

7. **At-rest viz refresh** -- when the engine exists, startup has settled, the
   batch has geometry, and no edit is pending, refresh held connections and step
   the structural overlays. A view-options toggle also reapplies options.

8. **`projectors.runner.consume(...)`** -- broadcast the batch to plugin workers
   (Full or Delta snapshot diffs), skipping tentative per-cycle edits. Only when
   the batch is non-empty and an orchestrator is installed.

9. **`projectors.render.consume(...)` then `viz.push(engine)`** -- republish
   scene geometry to viso and flush the overlay cache. The render projector
   consumes tentative edits (the streaming/live frames) that the runner
   projector skipped.

10. **`request_scores()`** -- when settled, geometry changed, and nothing is
    pending or previewing, ask the scoring plugin for a fresh breakdown.

11. **`update_engine(dt)` and `update_frame_visuals()`** -- advance viso (camera
    animation, mesh swaps) and the per-frame overlay visuals.

12. **Tail tip** -- runs after the engine update so the camera is settled this
    frame. Core projects the open segment panel's CA to a screen position
    (off-screen, closed, or no engine all resolve to `None`) and stages a change
    only when it moved; the debounce lives on the front end.

13. **`projectors.gui.consume(...)`** -- rebuild the dirty `GuiState` sections
    from the batch and the App-owned dirty flags. Returns whether the open
    segment auto-closed (target evicted), which closes the panel.

14. **Push to the host** -- `bridge::push::serialize_dirty` produces the JSON for
    just the dirty sections; `fx.push_state` ships it. The remaining
    `HostEffects` outputs (tail update, fullscreen flip, progress to persist) are
    drained and forwarded if set.

The function carries a `too_many_lines` allow with the reason that the frame
order has to stay readable in one place; splitting it would scatter the
sequencing that this list depends on.
