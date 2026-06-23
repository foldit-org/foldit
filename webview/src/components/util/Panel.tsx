/**
 * Panel Component
 * 
 * SolidJS panel component with drag support.
 */

import { Component, createMemo, createEffect, Show } from 'solid-js';
import { X } from '../../utils/iconMapping';
import '../../styles/util/Panel.css';
import { CalloutBubble } from './CalloutBubble';
import { createDraggable } from '../../services/adapters/drag-solid';
import { useUI, isWidgetVisible } from '../../services/adapters';
import { panelVisibilityKey } from '../../hooks/state';
import { calculateTargetCoords } from '../../utils/textBubbleUtils';

export type PanelProps = {
  id: string; // kebab-case
  title: string;
  width: number;
  backButton?: boolean;
  backCallback?: () => void;
  onClose?: () => void;
  position: { x: number; y: number };
  children: any;
  tailTarget?: any; // Optional tail target - when provided, switches to SVG rendering mode
  triggerId?: string; // Optional element id to snap next to on open
};

const Panel: Component<PanelProps> = (props) => {
  const {
    id,
    title,
    width,
    position,
    backButton = false,
    backCallback,
    onClose,
    children,
    tailTarget,
    triggerId,
  } = props;

  const toggleWidget = useUI(state => state.toggleWidget);

  const panelName = panelVisibilityKey(id);
  const isVisible = createMemo(() => isWidgetVisible(panelName));

  const handleClose = () => {
    if (onClose) onClose();
    toggleWidget()(panelName);
  };

  // Set to svg mode if we need to render panel with a tail
  const useSvgMode = !!tailTarget;

  // Calculate tail coordinates for SVG mode
  const tailCoords = createMemo(() => {
    if (!useSvgMode || !tailTarget) return null;

    const targetCoords = calculateTargetCoords(tailTarget);
    if (!targetCoords) return null;

    return {
      startX: targetCoords.x,
      startY: targetCoords.y,
      endX: 0, // Not used by TailPathGenerator
      endY: 0  // Not used by TailPathGenerator
    };
  });

  // Setup draggable behavior
  const { setRef, setPosition } = createDraggable({
    handle: '.header',
    initialPosition: position,
  });

  // Snap next to the trigger element each time the panel becomes visible
  createEffect(() => {
    if (isVisible() && triggerId) {
      const trigger = document.getElementById(triggerId);
      if (trigger) {
        const rect = trigger.getBoundingClientRect();
        setPosition({ x: rect.right + 10, y: rect.top });
      }
    }
  });

  // Main panel content
  const panelContent = (
    <>
      <button class="exit" onClick={handleClose}>
        <X size={20} />
      </button>
      
      <Show when={backButton}>
        <button class="back" onClick={backCallback}>←</button>
      </Show>
      
      <div class={`header ${backButton ? 'pl-10 pr-4' : 'px-4'}`}>
        {title}
      </div>
      
      <div class="body">
        {children}
      </div>
    </>
  );

  return (
    <Show when={isVisible()}>
      {useSvgMode ? (
        <div
          id={id}
          ref={setRef}
          style={{
            width: `min(${width}px, calc(100vw - 6rem))`,
            position: 'absolute',
            left: '0px',
            top: '0px',
            'z-index': '10',
            'backdrop-filter': 'blur(12px)',
            'border-radius': '16px'
          }}
        >
          <CalloutBubble
            color="panel"
            tailCoords={tailCoords()}
            showTail={useSvgMode}
            className={`panel-container-svg panel-container-visible`}
          >
            {panelContent}
          </CalloutBubble>
        </div>
      ) : (
        <div
          id={id}
          ref={setRef}
          class="panel-container panel-container-visible"
          style={{
            width: `min(${width}px, calc(100vw - 6rem))`,
            position: 'absolute',
            left: '0px',
            top: '0px',
            'z-index': '10'
          }}
        >
          {panelContent}
        </div>
      )}
    </Show>
  );
};

export default Panel;
