/**
 * SolidJS tabs implementation
 */

import { Component, createSignal, createMemo, For } from 'solid-js';
import '../../styles/util/Tabs.css';

export interface TabData {
  name: string;
  label: string;
  children: any;
}

type TabsProps = {
  className?: string;
  tabs: TabData[];
};

const Tabs: Component<TabsProps> = (props) => {
  const [activeTab, setActiveTab] = createSignal(props.tabs?.[0]?.name ?? '');

  const activeIndex = createMemo(() =>
    props.tabs?.findIndex(tab => tab.name === activeTab()) ?? 0
  );

  const activeContent = createMemo(() => {
    const tab = props.tabs?.find(tab => tab.name === activeTab());
    return tab?.children;
  });

  return (
    <div class={props.className ?? ''}>
      <div class="tabs">
        <div class="container">
          <For each={props.tabs}>
            {(tab) => (
              <button
                onClick={() => setActiveTab(tab.name)}
                class={`tab ${activeTab() === tab.name ? 'text-black' : 'text-white'}`}
                style={{ width: `${100 / (props.tabs?.length || 1)}%` }}
              >
                {tab.label}
              </button>
            )}
          </For>
          <div
            class="overlay"
            style={{
              transform: `translateX(${activeIndex() * 100}%)`,
              width: `${100 / (props.tabs?.length || 1)}%`
            }}
          />
        </div>
      </div>
      <div class="tab-content">{activeContent()}</div>
    </div>
  );
};

export default Tabs;
