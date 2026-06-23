import { createSignal } from 'solid-js';
import { createStore, produce, reconcile } from 'solid-js/store';
import { viewportInput, dispatchOp, appCommand } from './transport';
import { subscribe, initialState } from './stateChannel';
import type { FrontendState, HistoryCommand, HistoryLiveUpdate } from './types';

declare global {
  interface Window {
    __onTailUpdate?: (x: number | null, y?: number) => void;
  }
}

const [state, setState] = createStore<FrontendState>(initialState);

/**
 * Apply a `HistoryLiveUpdate` to the store: find the matching checkpoint
 * by id and patch its score/label/filter fields in place. A miss is
 * survivable (the topology push is presumably in flight or about to
 * arrive); we just no-op rather than ballooning state with a phantom
 * checkpoint.
 */
function applyHistoryLive(update: HistoryLiveUpdate): void {
  const idx = state.history.checkpoints.findIndex((c) => c.id === update.checkpoint_id);
  if (idx < 0) {
    return;
  }
  setState(
    'history',
    'checkpoints',
    idx,
    produce((c) => {
      c.raw_score = update.raw_score;
      c.game_score = update.game_score;
      c.label = update.label;
      c.filter_status = update.filter_status;
    }),
  );
}

// Single channel from Rust. The first emit after we post `ready` is a
// full-state snapshot (backend marks all dirty); subsequent emits are
// partial deltas. We initialize to `app_state: 'initializing'` (a
// pre-session phase) so the LoadingScreen renders immediately on JS
// startup, before the snapshot arrives.
//
// Two-channel history: `history` is the full reproject (rare, on
// topology bump); `history_live` is a small per-cycle patch to the
// running tentative checkpoint. We apply the live patch *first* (so
// the patched fields don't get overwritten by reconciling a stale
// `history` blob from the same delta), then reconcile the rest.
subscribe((sections) => {
  const { history_live, ...rest } = sections;
  if (rest.history !== undefined || Object.keys(rest).length > 0) {
    setState(reconcile({ ...state, ...rest }));
  }
  if (history_live) {
    applyHistoryLive(history_live);
    // Clear the live field locally so we don't keep re-applying on
    // unrelated future deltas. The backend resends on the next live
    // tick.
    setState('history_live', null);
  }
});

/** Send a typed history navigation command to the backend. */
function historyCommand(cmd: HistoryCommand): void {
  appCommand({ type: 'History', cmd });
}

/**
 * Close the segment-info panel by clearing the backend `App.open_segment`
 * source of truth. Rides the existing `app_command` envelope; a
 * frontend-only hide would desync (the backend would keep producing
 * `segment_info` and the live tail pushes).
 */
function closeSegment(): void {
  appCommand({ type: 'CloseSegment' });
}

// Live screen position of the targeted residue's tail tip, pushed by the
// host every frame the segment panel is open. `null` when the tip is
// off-screen (the panel keeps its body but hides the tail). The host calls
// `window.__onTailUpdate(x, y)` for an on-screen tip and `(null)` otherwise.
const [segmentTailTip, setSegmentTailTip] = createSignal<{ x: number; y: number } | null>(null);
if (typeof window !== 'undefined') {
  window.__onTailUpdate = (x, y) => {
    // The tip arrives in physical pixels (the cursor path forwards input
    // scaled by devicePixelRatio); divide back to CSS pixels for the DOM.
    const dpr = window.devicePixelRatio || 1;
    setSegmentTailTip(x === null || y === undefined ? null : { x: x / dpr, y: y / dpr });
  };
}

export { state, viewportInput, dispatchOp, appCommand, historyCommand, closeSegment, segmentTailTip };
