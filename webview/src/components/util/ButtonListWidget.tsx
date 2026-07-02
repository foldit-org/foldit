import { Component, For, Show, type JSX } from 'solid-js';
import "../../styles/util/ButtonListWidget.css";

// Types

export interface ButtonListItem {
  id: string;
  icon?: string;
  iconNode?: JSX.Element;
  content?: any;
  color?: string;
  hotkey?: string;
  disabled?: boolean;
  tooltip?: string;
  // Download-progress fraction (0..1). When set, a thin fill bar renders
  // along the bottom of the button; absent leaves the button unchanged.
  progress?: number;
  onClick: () => void;
}

interface ButtonListWidgetProps {
  items: ButtonListItem[];
  className?: string;
}

const ButtonListWidget: Component<ButtonListWidgetProps> = (props) => {
  // Don't destructure `props` - it breaks reactivity for live `items`.
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
                <img src={item.icon!} draggable={false} />
              </Show>
              <Show when={item.iconNode}>{item.iconNode}</Show>
              <Show when={item.content}>
                <div class="button-list-content">{item.content}</div>
              </Show>
              <Show when={item.hotkey}>
                <div class="button-list-hotkey">{item.hotkey}</div>
              </Show>
              <Show when={item.progress !== undefined}>
                <div
                  class="button-list-progress"
                  style={{ width: `${Math.max(0, Math.min(1, item.progress!)) * 100}%` }}
                />
              </Show>
            </button>
          );
        }}
      </For>
    </div>
  );
};

export default ButtonListWidget;
