/**
 * ButtonListWidget Component
 * 
 * SolidJS button list widget component.
 */

import { Component, For, Show } from 'solid-js';
import "../../styles/util/ButtonListWidget.css";
import ImageFromFS from "./ImageFromFS";

// ============================================================================
// Types
// ============================================================================

export interface ButtonListItem {
  id: string;
  icon?: string;
  content?: any;
  color?: string;
  hotkey?: string;
  disabled?: boolean;
  tooltip?: string;
  onClick: () => void;
}

interface ButtonListWidgetProps {
  items: ButtonListItem[];
  className?: string;
}

// ============================================================================
// ButtonListWidget Component
// ============================================================================

const ButtonListWidget: Component<ButtonListWidgetProps> = (props) => {
  // Don't destructure `props` — it breaks reactivity for live `items`.
  return (
    <div class={`button-list-widget ${props.className ?? ''}`}>
      <For each={props.items}>
        {(item) => {
          return (
            <button
              id={item.id}
              class={`button-list-item ${item.color || 'black'} ${item.disabled ? 'disabled' : ''}`}
              onClick={item.onClick}
              disabled={item.disabled}
              data-tooltip={item.tooltip}
            >
              <Show when={item.icon}>
                <ImageFromFS path={item.icon!} />
              </Show>
              <Show when={item.content}>
                <div class="button-list-content">{item.content}</div>
              </Show>
              <Show when={item.hotkey}>
                <div class="button-list-hotkey">{item.hotkey}</div>
              </Show>
            </button>
          );
        }}
      </For>
    </div>
  );
};

export default ButtonListWidget;
