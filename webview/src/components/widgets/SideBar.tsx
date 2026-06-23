/**
 * SideBar Component
 * 
 * SolidJS sidebar component.
 */

import { Component, createMemo, For } from 'solid-js';
import { RotateCcw, Eye, Activity } from '../../utils/iconMapping';
import '../../styles/widgets/SideBar.css';
import { useUI } from '../../services/adapters';
import Button from "../util/Button";

const iconComponents: Record<string, any> = {
  'undo': RotateCcw,
  'eye': Eye,
  'activity': Activity,
};

const SideBar: Component = () => {
  const toggleWidget = useUI(state => state.toggleWidget);

  const buttons = createMemo(() => [
    {
      id: 'history-panel-button',
      tooltip: 'History',
      iconName: 'undo',
      callback: () => toggleWidget()('historyPanel')
    },
    {
      id: 'view-options-menu-panel-button',
      tooltip: 'View Options',
      iconName: 'eye',
      callback: () => toggleWidget()('viewOptionsMenuPanel')
    },
    {
      id: 'foundry-tools-panel-button',
      tooltip: 'Foundry Tools',
      iconName: 'activity',
      callback: () => toggleWidget()('foundryToolsPanel')
    }
  ]);

  return (
    <div class="absolute z-20 top-1/2 -translate-y-1/2 left-2 flex flex-col space-y-5 text-white p-2 rounded pointer-events-auto font-bold bg-black bg-opacity-50 rounded-md">
      <For each={buttons()}>
        {(button) => {
          const IconComponent = iconComponents[button.iconName];
          return (
            <Button
              id={button.id}
              color="transparent"
              shape="pill"
              tooltip={button.tooltip}
              callback={button.callback}
            >
              {IconComponent && <IconComponent size={40} />}
            </Button>
          );
        }}
      </For>
    </div>
  );
};

export default SideBar;
