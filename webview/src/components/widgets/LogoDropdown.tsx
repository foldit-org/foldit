import { Component, createMemo, For } from 'solid-js';
import '../../styles/widgets/LogoDropdown.css';
import logo_simple from '../../assets/logo_simple.svg';
import { List, Save, LifeBuoy, Activity, Minimize, Maximize, LogOut } from '../../utils/iconMapping';
import { useUI, useFrontendState } from '../../services/adapters';
import { appCommand } from '../../adapter';

/**
 * Enter or leave OS fullscreen. Must run synchronously inside the click
 * gesture: the DOM fullscreen API is gesture-gated, so the call lives here
 * (not in a reactive effect). The desktop host applies the same change to
 * the native winit window off the SetFullscreen command the caller dispatches.
 */
function applyDomFullscreen(enter: boolean) {
  if (enter) {
    const el = document.getElementById('app-container');
    if (!el) return;
    if (el.requestFullscreen) el.requestFullscreen();
    else if ((el as any).webkitRequestFullscreen) (el as any).webkitRequestFullscreen(); // Safari
    else if ((el as any).msRequestFullscreen) (el as any).msRequestFullscreen(); // IE/Edge
    navigator.keyboard?.lock?.(['Escape']);
  } else if (document.fullscreenElement) {
    document.exitFullscreen?.();
    navigator.keyboard?.unlock?.();
  }
}

const iconComponents: Record<string, any> = {
  'list': List,
  'save': Save,
  'life-buoy': LifeBuoy,
  'activity': Activity,
  'minimize': Minimize,
  'maximize': Maximize,
  'log-out': LogOut,
};

const LogoDropdown: Component = () => {
  const logoDropdown = useUI(state => state.logoDropdown);
  const toggleWidget = useUI(state => state.toggleWidget);
  const toggleHints = useUI(state => state.toggleHints);
  const setAppScreen = useFrontendState(state => state.setAppScreen);
  const fullscreen = useFrontendState(state => state.fullscreen);

  const menuItems = createMemo(() => [
    {
      id: 'puzzle-menu',
      iconName: 'list',
      label: 'Puzzle Menu',
      onClick: () => toggleWidget()('puzzleMenu')
    },
    {
      id: 'load-save',
      iconName: 'save',
      label: 'Load/Save',
      onClick: () => toggleWidget()('loadSavePanel')
    },
    {
      id: 'hints',
      iconName: 'life-buoy',
      label: 'Show/Hide Hints',
      onClick: () => toggleHints()()
    },
    {
      id: 'debug-info',
      iconName: 'activity',
      label: 'Debug Info',
      onClick: () => toggleWidget()('debugInfoPanel')
    },
    {
      id: 'fullscreen',
      iconName: fullscreen() ? 'minimize' : 'maximize',
      label: fullscreen() ? 'Exit Fullscreen' : 'Enter Fullscreen',
      onClick: () => {
        const next = !fullscreen();
        applyDomFullscreen(next);
        appCommand({ type: 'SetFullscreen', value: next });
      }
    },
    {
      id: 'exit',
      iconName: 'log-out',
      label: 'Exit',
      onClick: () => setAppScreen()('LANDING')
    }
  ]);

  return (
    <div
      class="z-20 absolute top-5 left-5 pointer-events-auto"
      id="logo-dropdown"
    >
      <img
        class={`logo-dropdown-button pointer-events-auto ${logoDropdown() && "bg-black bg-opacity-80"}`}
        src={logo_simple}
        onClick={() => toggleWidget()('logoDropdown')}
      />
      <div
        class={`logo-dropdown-contents ${logoDropdown() ? 'logo-dropdown-visible' : 'logo-dropdown-hidden'}`}
      >
        <For each={menuItems()}>
          {(item) => {
            const IconComponent = iconComponents[item.iconName];
            return (
              <button onClick={item.onClick}>
                {IconComponent && <IconComponent size={24} />}
                <p>{item.label}</p>
              </button>
            );
          }}
        </For>
      </div>
    </div>
  );
};

export default LogoDropdown;
