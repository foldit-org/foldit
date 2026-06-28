import {
  dispatchOp,
  appCommand,
  setSelection,
  viewportInput,
  request,
  openSessionDialog,
} from './transport';
import { subscribe as channelSubscribe, currentState } from './stateChannel';
import { BRIDGE_CONTRACT_VERSION } from './generated/wire';
import type { FrontendState } from './types';
import type { MountPanel, PluginBridge } from '@foldit/plugin-bridge';

export type { MountPanel, PluginBridge } from '@foldit/plugin-bridge';

export function createBridge(): PluginBridge {
  return {
    subscribe(cb, selector) {
      if (!selector) {
        return channelSubscribe(cb);
      }
      return channelSubscribe((delta) => {
        if (Object.keys(delta).some((k) => selector.includes(k as keyof FrontendState))) {
          cb(delta);
        }
      });
    },
    snapshot() {
      return structuredClone(currentState());
    },
    dispatchOp,
    appCommand,
    setSelection,
    viewportInput,
    request,
    openSessionDialog,
    contractVersion: BRIDGE_CONTRACT_VERSION,
  };
}

/**
 * Mount a plugin panel into `host` behind a shadow root and return a
 * teardown that runs the plugin's cleanup and clears the shadow content.
 * Refuses when the plugin's declared contract version does not match the
 * bridge's.
 */
export function mountPluginPanel(
  host: HTMLElement,
  panelId: string,
  pluginVersion: number,
  mountPanel: MountPanel,
  bridge: PluginBridge,
): () => void {
  if (pluginVersion !== bridge.contractVersion) {
    throw new Error(
      `plugin panel '${panelId}' targets wire contract v${pluginVersion}, host is v${bridge.contractVersion}`,
    );
  }
  const shadow = host.attachShadow({ mode: 'open' });
  const cleanup = mountPanel(panelId, shadow, bridge);
  return () => {
    cleanup();
    shadow.replaceChildren();
  };
}
