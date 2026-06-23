import { subscribe as transportSubscribe } from './transport';
import type { FrontendState } from './types';

const initialHistory = {
  checkpoints: [],
  lanes: [],
  checkpoint_head: null,
  checkpoint_root: null,
  best: null,
  best_that_counts: null,
  topology_version: 0,
};

// The seed mirror. `app_state: 'initializing'` is the pre-session phase
// so the LoadingScreen renders before the first snapshot arrives.
export const initialState: FrontendState = {
  app_state: 'initializing',
  score: { value: 0, invalid: true, title: '' },
  puzzle: { mode: 'scientist', puzzle_id: 0, title: '', starting_score: 0, target_score: 0, complete: false },
  selection: { entries: [] },
  view: {
    options: {},
    options_schema: null,
    available_presets: [],
    active_preset: null,
  },
  ui: { text_bubble: null, fps: 0, log: '', selected_count: 0, hints_visible: true, fullscreen: false },
  actions: { available: [] },
  loading: { progress: null, puzzle_loaded: false },
  scene: { entities: [], focused_entity: null },
  history: initialHistory,
  history_live: null,
  panels: { open: [], positions: [] },
  progress: { entries: [] },
};

type Listener = (delta: Partial<FrontendState>) => void;

const listeners = new Set<Listener>();
let mirror: FrontendState = { ...initialState };
let unsubscribeTransport: (() => void) | null = null;

// Open the single transport channel the first time anyone listens. The
// first emit after `ready` is a full-state snapshot; subsequent emits are
// section-keyed deltas. Each delta is merged section-wise into the mirror
// (last-write-wins per section) and then fanned out raw to every listener.
function ensureTransport(): void {
  if (unsubscribeTransport) {
    return;
  }
  unsubscribeTransport = transportSubscribe((delta) => {
    mirror = { ...mirror, ...delta };
    for (const listener of listeners) {
      listener(delta);
    }
  });
}

/**
 * Register a listener for raw section-keyed deltas. Returns an unsubscribe
 * fn. The mirror is updated before the listener runs, so a listener that
 * reads `currentState()` sees the just-applied delta.
 */
export function subscribe(cb: Listener): () => void {
  listeners.add(cb);
  ensureTransport();
  return () => {
    listeners.delete(cb);
  };
}

/**
 * The live accumulated state mirror. Returns the live reference; callers
 * that need isolation deep-clone it themselves.
 */
export function currentState(): FrontendState {
  return mirror;
}
