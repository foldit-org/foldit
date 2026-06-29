# History and Scoring

## Two-layer history

`History` (`crates/foldit-core/src/history/`) is two cooperating DAGs:

1. **Entity timelines (lanes).** Each `EntityId` has its own `EntityHistory`, a
   slotmap of `EntitySnapshot`s linked parent-to-children. A snapshot owns the
   entity payload as an `Arc<MoleculeEntity>`.
2. **Checkpoint graph.** A slotmap of `Checkpoint`s, also linked
   parent-to-children. Each checkpoint carries an
   `IndexMap<EntityId, EntitySnapshotId>` (which lane snapshot each entity was at)
   plus per-checkpoint state: scores and filter status. It does not own snapshot
   payloads; it points into the lanes.

The cross-DAG invariant: for every entity in the checkpoint head, the lane head
is either that committed snapshot or a tentative snapshot whose parent is it. An
in-flight action's open tentative sits one step past the committed head on its
lane. This is asserted under `debug_assertions` at the tail of every
DAG-bearing event.

Every checkpoint-bearing operation (commit, undo, redo, jump, reset) funnels
through one private `History::record`; the public methods are thin shims that
build a `HistoryEvent` and delegate. Per-cycle byte mutation (the streaming
`action_update`) and curation (pin, unpin, exclude-from-best, budget) change
neither DAG topology and bypass the record root.

`History` enforces only the structural action lock: one open tentative per lane,
and a frozen committed head (no navigation or immediate-commit mutation) while
any action is in flight. Multi-client locking, where the runner is server-side
and clients are remote, is the orchestrator's `EntityLockTable`, not this
module's concern.

`HeadMoved` (or `Edit`) is emitted on each of these so the projectors rebuild.
The history wire form is pushed to the front end as the `HISTORY` section, with a
lighter `HISTORY_LIVE` patch for in-flight tentative score/label updates that
should not reproject the whole graph.

## Scoring

`ScoreCoordinator` (`crates/foldit-core/src/app/score_coordinator.rs`) owns the
score-term weight map, the term-name alignment key, the labeled met-filter raw
bonus, and the in-flight composition-score targets. The score types it weights
are in `crates/foldit-core/src/scores.rs`.

### Raw versus game score

A plugin score report carries RAW (unweighted) per-term energies, both
whole-pose and per-residue, plus the term-name list. Core does the weighting: it
multiplies each raw term by the coordinator's weight map to get the weighted
total (`ScoreReport::weighted_total`) and the per-residue scalars used for
coloring.

The displayed **game** score is the weighted raw total run through
`rosetta_raw_to_game`:

```rust
((-raw + 800.0) * 10.0).max(0.0)
```

so lower energy reads as a higher game score, floored at zero. For free-form
("scientist") sessions the raw Rosetta number is what is surfaced; for
tutorial/campaign/science puzzles the game framing and a target are shown
(`ScoringMode`).

Two raw bonuses fold into the game total before the conversion: the loaded
puzzle's met-filter bonus (summed across filters, held by the coordinator) and
the per-report `bonus_breakdown` the plugin forwards. Both are in raw energy with
the same sign convention as the terms.

### Stamping

Two paths land scores on the session:

- **Whole-assembly poll** (`apply_score_reports`): weights the chosen report and
  stamps it onto the sole open edit if one is pending
  (`sole_pending_request_id`), otherwise onto the committed head.
- **Composition scores per checkpoint**: a committed checkpoint's score arrives
  asynchronously and is matched to its checkpoint through a
  `request_id -> CheckpointId` join held in `score_targets`. The reply is
  weighted, the bonuses added, the result stamped with
  `set_checkpoint_scores`, and the spent target removed.

Either way the RAW per-term breakdown is retained as a `StoredBreakdown`
(whole-pose and per-residue, aligned to the session's `term_names`), so the
per-residue segment panel and per-term displays can be rebuilt without
re-querying. A `debug_assert` checks breakdown/term-name alignment.

The tick requests a fresh score only when startup has settled, the batch carried
geometry, and nothing is pending or previewing. See
[The Per-Frame Tick](tick-loop.md).
