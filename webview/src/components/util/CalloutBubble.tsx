/**
 * SolidJS callout bubble with SVG background and optional tail.
 */

import { Component, createSignal, createEffect, onMount } from 'solid-js';
import { TailCoords, getTailFillColor, getOutlineColor, getShadowFilter } from '../../utils/textBubbleUtils';
import { TailPathGenerator } from '../../utils/tailPathUtils';

export interface CalloutBubbleProps {
  children?: any;
  color: string;
  tailCoords?: TailCoords | null;
  showTail?: boolean;
  className?: string;
  style?: Record<string, any>;
  tailRadius?: number;
  tailWidth?: number;
}

export const CalloutBubble: Component<CalloutBubbleProps> = (props) => {
  let containerRef: HTMLDivElement | undefined;
  const [path, setPath] = createSignal<string>('');

  const fillColor = () => getTailFillColor(props.color);
  const outlineColor = () => getOutlineColor(props.color);

  const updatePath = () => {
    if (!containerRef) return;
    const bounds = containerRef.getBoundingClientRect();
    const newPath = TailPathGenerator.generateUnifiedPath(
      bounds,
      props.tailCoords || null,
      props.showTail || false,
      { tailWidth: props.tailWidth || 10 }
    );
    setPath(newPath);
  };

  onMount(() => {
    updatePath();
    // Update on resize
    const observer = new ResizeObserver(updatePath);
    if (containerRef) observer.observe(containerRef);
    return () => observer.disconnect();
  });

  // Update path when props change
  createEffect(() => {
    // Access reactive props to track them
    void props.tailCoords;
    void props.showTail;
    void props.color;
    updatePath();
  });

  return (
    <div
      ref={containerRef}
      class={`callout-bubble ${props.className || ''}`}
      style={{
        filter: getShadowFilter(props.color),
        ...(props.style || {})
      }}
    >
      {/* SVG background (unified rectangle + tail) */}
      <svg
        class="callout-bubble-background"
        style={{
          position: 'absolute',
          top: '-2000px',
          left: '-2000px',
          width: 'calc(100% + 4000px)',
          height: 'calc(100% + 4000px)',
          'pointer-events': 'none',
          'z-index': -1
        }}
      >
        <path
          d={path()}
          fill={fillColor()}
          stroke={outlineColor()}
          stroke-width="1"
        />
      </svg>

      {/* Content layer */}
      <div
        class="callout-bubble-content"
        style={{
          position: 'relative',
          background: 'transparent',
          'z-index': 1
        }}
      >
        {props.children}
      </div>
    </div>
  );
};
