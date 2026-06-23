/**
 * ToolStatusWidget Component
 * 
 * SolidJS tool status widget component.
 */

import { Component, createSignal, createMemo, createEffect, onCleanup, Show } from 'solid-js';
import { useBackendData } from '../../services/adapters';

// Simple CSS spinner component
const SpinnerCircularFixed: Component<{ color?: string; size?: number }> = (props) => {
  const { color = 'white', size = 30 } = props;
  
  return (
    <div
      style={{
        width: `${size}px`,
        height: `${size}px`,
        border: `2px solid ${color}`,
        'border-top': '2px solid transparent',
        'border-radius': '50%',
        animation: 'spin 1s linear infinite'
      }}
    />
  );
};

const ToolStatusWidget: Component = () => {
  const actions = useBackendData(state => state.actions);

  const [elapsed, setElapsed] = createSignal(0);
  const [iterations, setIterations] = createSignal(0);

  const hasActiveAction = createMemo(() =>
    actions().some(action => action.active)
  );

  createEffect(() => {
    if (!hasActiveAction()) return;

    const startTime = Date.now();
    const interval = setInterval(() => {
      const now = Date.now();
      setElapsed((now - startTime) / 1000);
      setIterations((prev: number) => prev + 1);
    }, 100);

    onCleanup(() => clearInterval(interval));
  });

  const glowStyles = `
    @keyframes glowPulse {
      0%, 100% { filter: drop-shadow(0 0 1px #f5f5f5); }
      50% { filter: drop-shadow(0 0 3px #f5f5f5); }
    }
    .spinner-glow { animation: glowPulse 2s ease-in-out infinite; }
  `;

  return (
    <Show when={hasActiveAction()}>
      <div class="fixed bottom-2 left-4 z-20 w-24 h-28 flex flex-col items-center justify-center text-white pointer-events-auto select-none">
        <div class="relative w-24 h-24 flex items-center justify-center">
          <div class="absolute spinner-glow">
            <SpinnerCircularFixed
              size={96}
              color="#f5f5f5"
            />
          </div>
        </div>
        <span class="text-[11px]">
          {elapsed().toFixed(0)}s ({iterations()})
        </span>
        <style>{glowStyles}</style>
      </div>
    </Show>
  );
};

export default ToolStatusWidget;
