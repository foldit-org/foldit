/**
 * History panel — single source of truth for the undo/redo UI.
 *
 * Renders `state.history.checkpoints` as nodes positioned by
 * (timestamp, score) with parent edges. The running tentative gets a
 * pulsing dot + dashed edge to its parent.
 *
 * Focus-aware filtering:
 *   - No focused entity: show every checkpoint (whole-assembly view).
 *   - Entity X focused: show only checkpoints with `entity == X` plus
 *     non-entity-targeted ones (root) for context.
 *
 * Click → JumpCheckpoint when nothing's focused (whole-state move). When
 * an entity is focused, click → LaneUndo on that entity, leaving other
 * entities' lane heads unchanged.
 *
 * Score axis (raw vs game) is local GUI state — toggling it doesn't
 * round-trip; both scores ride on every CheckpointInfo (G7).
 */

import { Component, createMemo, createSignal, For, Show } from 'solid-js';
import { ArrowLeft, ArrowRight, RotateCcw, Star, Trophy } from '../../utils/iconMapping';

import '../../styles/panels/HistoryPanel.css';

import Button from '../util/Button';

import { state, historyCommand } from '../../adapter';
import type { CheckpointInfo } from '../../types';

const NODE_RADIUS = 5;
const NODE_SPACING = 36; // min horizontal distance between adjacent nodes
const MIN_WIDTH = 460;   // panel-width fallback when there are few nodes
const VIEWBOX_HEIGHT = 100;
const PADDING = 12;

type ScoreAxis = 'raw' | 'game';

function pickScore(c: CheckpointInfo, axis: ScoreAxis): number | null {
  return axis === 'raw' ? c.raw_score : c.game_score;
}

interface CtxMenuState {
  visible: boolean;
  x: number;
  y: number;
  checkpoint: CheckpointInfo | null;
}

const HistoryPanel: Component = () => {
  const [axis, setAxis] = createSignal<ScoreAxis>('raw');
  const [hovered, setHovered] = createSignal<string | null>(null);
  const [ctx, setCtx] = createSignal<CtxMenuState>({
    visible: false,
    x: 0,
    y: 0,
    checkpoint: null,
  });

  const focused = () => state.scene.focused_entity;

  const visibleCheckpoints = createMemo(() => {
    const cps = state.history.checkpoints;
    if (!cps || cps.length === 0) return [] as CheckpointInfo[];
    const f = focused();
    const filtered =
      f === null
        ? cps
        : cps.filter((c) => c.entity === null || c.entity === f);
    return [...filtered].sort((a, b) => (a.timestamp_ms ?? 0) - (b.timestamp_ms ?? 0));
  });

  const layout = createMemo(() => {
    const cps = visibleCheckpoints();
    if (cps.length === 0) return null;

    const scored = cps
      .map((c) => pickScore(c, axis()))
      .filter((s): s is number => s !== null);
    const minScore = scored.length > 0 ? Math.min(...scored) : 0;
    const maxScore = scored.length > 0 ? Math.max(...scored) : 1;
    const range = maxScore - minScore;
    const effectiveHeight = VIEWBOX_HEIGHT - 2 * PADDING;

    const yScale = (score: number | null): number => {
      if (score === null || range === 0) return PADDING + effectiveHeight / 2;
      return PADDING + effectiveHeight - ((score - minScore) / range) * effectiveHeight;
    };
    const naturalWidth = 2 * PADDING + cps.length * NODE_SPACING;
    const width = Math.max(MIN_WIDTH, naturalWidth);
    const xScale = (i: number): number => PADDING + (i + 0.5) * NODE_SPACING;

    const positions = new Map<string, { x: number; y: number }>();
    cps.forEach((c, i) => {
      positions.set(c.id, { x: xScale(i), y: yScale(pickScore(c, axis())) });
    });

    return { cps, positions, width };
  });

  const onCheckpointClick = (c: CheckpointInfo) => {
    const f = focused();
    if (f !== null) {
      // Lane-undo: move only the focused entity's lane to its snapshot
      // at this checkpoint. Other entities keep their state.
      const target = c.entity_heads[f];
      if (typeof target === 'string') {
        historyCommand({ kind: 'LaneUndo', entity: f, target });
      }
    } else {
      historyCommand({ kind: 'JumpCheckpoint', id: c.id });
    }
  };

  const onContextMenu = (e: MouseEvent, c: CheckpointInfo) => {
    e.preventDefault();
    setCtx({ visible: true, x: e.clientX, y: e.clientY, checkpoint: c });
  };

  const closeMenu = () => setCtx({ visible: false, x: 0, y: 0, checkpoint: null });

  const jumpToRoot = () => {
    if (state.history.checkpoint_root) {
      historyCommand({ kind: 'JumpCheckpoint', id: state.history.checkpoint_root });
    }
  };
  const jumpToBest = () => {
    if (state.history.best) {
      historyCommand({ kind: 'JumpCheckpoint', id: state.history.best });
    }
  };
  const jumpToBestThatCounts = () => {
    if (state.history.best_that_counts) {
      historyCommand({ kind: 'JumpCheckpoint', id: state.history.best_that_counts });
    }
  };

  const numHorizontalLines = 5;

  return (
    <>
      <div class="flex flex-row items-center justify-between text-xs mb-1">
        <span class="text-gray-400">
          {focused() !== null ? `Entity ${focused()}` : 'All entities'}
        </span>
        <div class="flex flex-row gap-2">
          <button
            class={`px-2 py-1 rounded ${axis() === 'raw' ? 'bg-blue-600' : 'bg-gray-700'}`}
            onClick={() => setAxis('raw')}
          >
            Raw
          </button>
          <button
            class={`px-2 py-1 rounded ${axis() === 'game' ? 'bg-blue-600' : 'bg-gray-700'}`}
            onClick={() => setAxis('game')}
          >
            Game
          </button>
        </div>
      </div>

      <div
        class="history-graph flex-grow mt-2 min-h-0 overflow-x-auto overflow-y-hidden"
        onMouseLeave={() => setHovered(null)}
      >
        <Show
          when={layout()}
          fallback={<p class="text-gray-400 text-center py-4">No history yet — make a move.</p>}
        >
          {(data) => (
            <svg width={data().width} height={VIEWBOX_HEIGHT} viewBox={`0 0 ${data().width} ${VIEWBOX_HEIGHT}`}>
              {/* gridlines */}
              <For each={Array.from({ length: numHorizontalLines })}>
                {(_, i) => {
                  const y =
                    PADDING +
                    (i() * (VIEWBOX_HEIGHT - 2 * PADDING)) / (numHorizontalLines - 1);
                  return (
                    <line
                      x1="0"
                      y1={y}
                      x2={data().width}
                      y2={y}
                      stroke="#333"
                      stroke-width="0.5"
                      stroke-dasharray="2 2"
                    />
                  );
                }}
              </For>

              {/* parent → child edges. Inline accessors so Solid
                  re-evaluates positions and tentative styling on every
                  state change — closure-body `const`s freeze cosmetics
                  at first-render time. */}
              <For each={data().cps}>
                {(node) => {
                  const from = () =>
                    node.parent ? data().positions.get(node.parent) : undefined;
                  const to = () => data().positions.get(node.id);
                  return (
                    <Show when={node.parent && from() && to()}>
                      <line
                        x1={from()!.x}
                        y1={from()!.y}
                        x2={to()!.x}
                        y2={to()!.y}
                        stroke="#4b5563"
                        stroke-width="1"
                        stroke-dasharray={node.tentative ? '3 2' : undefined}
                      />
                    </Show>
                  );
                }}
              </For>

              {/* nodes */}
              <For each={data().cps}>
                {(node) => {
                  const p = () => data().positions.get(node.id);
                  const fill = () => {
                    if (node.pinned) return '#a78bfa';
                    if (node.id === state.history.checkpoint_head) return '#60a5fa';
                    if (node.id === state.history.best_that_counts) return '#facc15';
                    if (node.id === state.history.best) return '#fb923c';
                    return '#a1a1aa';
                  };
                  return (
                    <Show when={p()}>
                      {(pos) => (
                        <circle
                          cx={pos().x}
                          cy={pos().y}
                          r={hovered() === node.id ? NODE_RADIUS + 1.5 : NODE_RADIUS}
                          fill={fill()}
                          fill-opacity={node.tentative ? 0.5 : 1.0}
                          stroke={node.tentative ? '#fbbf24' : '#1f2937'}
                          stroke-width="1"
                          stroke-dasharray={node.tentative ? '2 2' : undefined}
                          class={`cursor-pointer ${node.tentative ? 'tentative-pulse' : ''}`}
                          onClick={() => onCheckpointClick(node)}
                          onContextMenu={(e) => onContextMenu(e, node)}
                          onMouseEnter={() => setHovered(node.id)}
                          data-tooltip={`${node.tentative ? '(in progress) ' : ''}${node.label}${
                            pickScore(node, axis()) !== null
                              ? ` — ${pickScore(node, axis())!.toFixed(2)}`
                              : ''
                          } [${node.kind}]`}
                        />
                      )}
                    </Show>
                  );
                }}
              </For>
            </svg>
          )}
        </Show>
      </div>

      <div class="flex flex-row justify-between mt-4 flex-shrink-0">
        <div class="history-button">
          <Button color="green" callback={() => historyCommand({ kind: 'Undo' })}>
            <div class="p-1"><ArrowLeft size={24} /></div>
          </Button>
          <p class="text-xs">Undo</p>
        </div>
        <div class="history-button">
          <Button color="green" callback={() => historyCommand({ kind: 'Redo', branch: null })}>
            <div class="p-1"><ArrowRight size={24} /></div>
          </Button>
          <p class="text-xs">Redo</p>
        </div>
        <div class="history-button">
          <Button color="green" callback={jumpToRoot}>
            <div class="p-1"><RotateCcw size={24} /></div>
          </Button>
          <p class="text-xs">Reset</p>
        </div>
        <div class="history-button">
          <Button color="green" callback={jumpToBest}>
            <div class="p-1"><Star size={24} /></div>
          </Button>
          <p class="text-xs">Best</p>
        </div>
        <div class="history-button">
          <Button color="green" callback={jumpToBestThatCounts}>
            <div class="p-1"><Trophy size={24} /></div>
          </Button>
          <p class="text-xs">Credit Best</p>
        </div>
      </div>

      <Show when={ctx().visible && ctx().checkpoint}>
        <CheckpointContextMenu
          x={ctx().x}
          y={ctx().y}
          checkpoint={ctx().checkpoint!}
          onClose={closeMenu}
        />
      </Show>
    </>
  );
};

const CheckpointContextMenu: Component<{
  x: number;
  y: number;
  checkpoint: CheckpointInfo;
  onClose: () => void;
}> = (props) => {
  const c = () => props.checkpoint;

  const onJump = () => {
    historyCommand({ kind: 'JumpCheckpoint', id: c().id });
    props.onClose();
  };

  const onPin = () => {
    historyCommand({
      kind: c().pinned ? 'UnpinCheckpoint' : 'PinCheckpoint',
      id: c().id,
    });
    props.onClose();
  };

  const onToggleExclude = () => {
    historyCommand({
      kind: 'SetExcludeFromBest',
      id: c().id,
      exclude: !c().exclude_from_best,
    });
    props.onClose();
  };

  const onAbort = () => {
    historyCommand({ kind: 'AbortAction' });
    props.onClose();
  };

  return (
    <div
      class="checkpoint-context-menu fixed z-50 bg-gray-800 text-white shadow-lg rounded p-1"
      style={{ left: `${props.x}px`, top: `${props.y}px` }}
      onClick={(e) => e.stopPropagation()}
      onContextMenu={(e) => e.preventDefault()}
    >
      <button class="block w-full text-left px-3 py-1 hover:bg-gray-700" onClick={onJump}>
        Jump to here (whole assembly)
      </button>
      <button class="block w-full text-left px-3 py-1 hover:bg-gray-700" onClick={onPin}>
        {c().pinned ? 'Unpin' : 'Pin as best'}
      </button>
      <button class="block w-full text-left px-3 py-1 hover:bg-gray-700" onClick={onToggleExclude}>
        {c().exclude_from_best ? 'Include in best' : 'Exclude from best'}
      </button>
      <Show when={c().tentative}>
        <button
          class="block w-full text-left px-3 py-1 hover:bg-gray-700 text-yellow-300"
          onClick={onAbort}
        >
          Abort tentative
        </button>
      </Show>
      <button
        class="block w-full text-left px-3 py-1 hover:bg-gray-700 text-gray-400"
        onClick={props.onClose}
      >
        Cancel
      </button>
    </div>
  );
};

export default HistoryPanel;
