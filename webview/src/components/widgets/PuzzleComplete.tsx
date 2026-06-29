/**
 * Stack of two surfaces sharing the `puzzle.complete` latch from the
 * backend, rebuilt from the production bundle on play.fold.it:
 *
 *   1. Centered popup modal (`puzzleCompleteMenu` widget) — appears once
 *      `puzzle.complete` flips true. Shows current score, elapsed time,
 *      and a preview / status card for the next puzzle in the category.
 *   2. Draggable side widget (`puzzleCompleteWidget`) — Panel-based,
 *      manages its own visibility via the kebab → camelCase widget id.
 *      The modal's X dismisses the modal AND opens the widget so the
 *      action row stays accessible.
 */

import { Component, Show, createEffect, createMemo, createSignal } from 'solid-js';
import { RefreshCw, ArrowRight, List } from '../../utils/iconMapping';
import '../../styles/widgets/PuzzleComplete.css';
import '../../styles/popups/PopupMenu.css';
import { useUI, useBackendData, useFrontendState, useGameProgress } from '../../services/adapters';
import { usePuzzleProgress } from '../../hooks/usePuzzleProgress';
import { replayPuzzle, playNextPuzzle } from '../../services/PuzzleLoader';
import Button from '../util/Button';
import Panel from '../util/Panel';

// Vite asset glob — same pattern PuzzleMenuPopup uses for previews.
const puzzleImages: Record<string, any> = import.meta.glob(
  '../../assets/puzzle_previews/*.png',
  { eager: true, import: 'default' },
);
const previewSrc = (id: number) =>
  puzzleImages[`../../assets/puzzle_previews/${id}.png`];

/** Format an elapsed-millisecond value as `1h 23m 45s` (hour/minute hidden when 0). */
function formatElapsed(ms: number): string {
  if (!Number.isFinite(ms) || ms < 0) return '0s';
  const totalSec = Math.floor(ms / 1000);
  const hh = Math.floor(totalSec / 3600);
  const mm = Math.floor((totalSec % 3600) / 60);
  const ss = totalSec % 60;
  return (
    (hh > 0 ? `${hh}h ` : '') +
    (mm > 0 ? `${mm}m ` : '') +
    `${ss}s`
  );
}

/**
 * Three-up action row used in both the modal and the widget. Inside the
 * widget, "Puzzle Menu" only opens the puzzle menu; inside the modal it
 * also closes the modal+widget so the user isn't left with stacked UI.
 */
const PuzzleCompleteActions: Component<{ inWidget?: boolean }> = (props) => {
  const toggleWidget = useUI(state => state.toggleWidget);

  const handlePuzzleMenu = () => {
    if (!props.inWidget) {
      toggleWidget()('puzzleCompleteMenu');
      toggleWidget()('puzzleCompleteWidget');
    }
    toggleWidget()('puzzleMenu');
  };

  return (
    <ul class="puzzle-complete-widget-buttons">
      <li class="flex-1">
        <div class="flex w-full justify-center mb-2">
          <Button color="green" callback={replayPuzzle}>
            <div class="m-1"><RefreshCw size={24} /></div>
          </Button>
        </div>
        <div class="flex w-full justify-center text-xs">Replay</div>
      </li>
      <li class="flex-1">
        <div class="flex w-full justify-center mb-2">
          <Button color="green" callback={handlePuzzleMenu}>
            <div class="m-1"><List size={24} /></div>
          </Button>
        </div>
        <div class="flex w-full justify-center text-xs">Puzzle Menu</div>
      </li>
      <li class="flex-1">
        <div class="flex w-full justify-center mb-2">
          <Button color="green" callback={playNextPuzzle}>
            <div class="m-1"><ArrowRight size={24} /></div>
          </Button>
        </div>
        <div class="flex w-full justify-center text-xs">Next Puzzle</div>
      </li>
    </ul>
  );
};

const PuzzleCompleteModal: Component = () => {
  const toggleWidget = useUI(state => state.toggleWidget);
  const currentScore = useBackendData(state => state.currentScore);
  const puzzleData = useBackendData(state => state.puzzleData);
  const inGame = useFrontendState(state => state.inGame);
  const isPuzzleComplete = useGameProgress(state => state.isPuzzleComplete);
  const getHighScore = useGameProgress(state => state.getHighScore);
  const { getNextPuzzle } = usePuzzleProgress();

  // Snapshot the elapsed timer the moment the modal mounts so re-renders
  // don't tick the time forward — matches the legacy useRef(Date.now()) flow.
  const startedAt = Date.now();
  const elapsed = createMemo(() => {
    // Backend doesn't send a puzzleStartTime today; track it client-side
    // off the most recent puzzle id change (snapshot below).
    const start = puzzleStartTimes.get(puzzleData().puzzleId) ?? startedAt;
    return formatElapsed(startedAt - start);
  });

  const dismiss = () => {
    toggleWidget()('puzzleCompleteMenu');
    toggleWidget()('puzzleCompleteWidget');
  };

  const next = createMemo(() => getNextPuzzle());

  return (
    <div class={inGame() ? 'ingame-popup-menu' : 'landing-popup-menu'}>
      <div class="border-effect-wrapper w-1/2 max-w-[700px]">
        <div class="popup-menu-content puzzle-complete-menu w-full relative">
          <button
            class="absolute top-4 right-4 text-2xl opacity-60 hover:opacity-100"
            onClick={dismiss}
            aria-label="Dismiss"
          >
            ✕
          </button>

          <h2 class="my-8 text-center text-2xl font-light tracking-widest text-gray-100">
            Puzzle Complete
          </h2>

          <div class="w-full py-4 px-10">
            <div class="puzzle-complete-stat">
              <h6>Score:</h6>
              <div>{currentScore().toFixed(2)}</div>
            </div>
            <div class="puzzle-complete-stat">
              <h6>Time:</h6>
              <div>{elapsed()}</div>
            </div>
            <div class="puzzle-complete-stat">
              <h6>Next Up:</h6>
            </div>

            <Show when={next()}>
              {(np) => (
                <div class="flex flex-col justify-center p-4 w-full rounded-lg bg-[#555] bg-opacity-20">
                  <h6 class="font-semibold">{np().title}</h6>
                  <div class="flex flex-row">
                    <img
                      src={previewSrc(np().id)}
                      alt={`${np().id}.png`}
                      class="w-32 rounded shadow-sm"
                    />
                    <div class="flex flex-col ml-10 justify-center">
                      <div class="flex flex-row">
                        <Show
                          when={isPuzzleComplete()(np().id)}
                          fallback={<p class="text-yellow-400">Incomplete</p>}
                        >
                          <p class="text-gray-400 mr-2">High score:</p>
                          <p class="text-green-500 font-bold">
                            {getHighScore()(np().id)}
                          </p>
                        </Show>
                      </div>
                      <p><i>{np().description}</i></p>
                    </div>
                  </div>
                </div>
              )}
            </Show>

            <div class="mt-10 mb-2 mx-2">
              <PuzzleCompleteActions />
            </div>
          </div>
        </div>
      </div>
    </div>
  );
};

const PuzzleCompleteWidget: Component = () => (
  <Panel
    id="puzzle-complete-widget"
    title="Puzzle Complete"
    position={{ x: 20, y: window.innerHeight - 240 }}
    width={300}
  >
    <div class="flex items-center justify-between p-2">
      <PuzzleCompleteActions inWidget />
    </div>
  </Panel>
);

// Map of puzzle_id → Date.now() at the moment that puzzle was loaded.
// Filled by the side-effect below; consumed by the modal's elapsed-time memo.
const puzzleStartTimes = new Map<number, number>();

const PuzzleComplete: Component = () => {
  const puzzleCompleteMenu = useUI(state => state.puzzleCompleteMenu);
  const showWidget = useUI(state => state.showWidget);
  const hideWidget = useUI(state => state.hideWidget);
  const puzzleComplete = useBackendData(state => state.puzzleComplete);
  const puzzleData = useBackendData(state => state.puzzleData);

  // Track puzzle-load timestamps so the modal can show elapsed time. The
  // backend doesn't ship a puzzleStartTime today; deriving it on the
  // puzzle_id transition is good enough for the victory flow.
  const [lastPuzzleId, setLastPuzzleId] = createSignal(0);
  createEffect(() => {
    const id = puzzleData().puzzleId;
    if (id !== lastPuzzleId()) {
      puzzleStartTimes.set(id, Date.now());
      setLastPuzzleId(id);
    }
  });

  // Open the modal on the false→true edge of `puzzle.complete`. On false
  // (puzzle reset) clear both surfaces.
  let lastComplete = false;
  createEffect(() => {
    const now = puzzleComplete();
    if (now && !lastComplete) {
      setTimeout(() => {
        if (!puzzleCompleteMenu()) {
          showWidget()('puzzleCompleteMenu');
        }
      }, 3200);
    } else if (!now && lastComplete) {
      hideWidget()('puzzleCompleteMenu');
      hideWidget()('puzzleCompleteWidget');
    }
    lastComplete = now;
  });

  return (
    <>
      <Show when={puzzleCompleteMenu()}>
        <PuzzleCompleteModal />
      </Show>
      <PuzzleCompleteWidget />
    </>
  );
};

export default PuzzleComplete;
