import {
  dispatchOp,
  appCommand,
  setSelection,
  viewportInput,
  request,
  openSessionDialog,
} from './transport';
import type { RequestKind } from './transport';
import { subscribe as channelSubscribe, currentState } from './stateChannel';
import { BRIDGE_CONTRACT_VERSION } from './generated/wire';
import type {
  FrontendState,
  AppCommand,
  EntitySelection,
  ViewportInput,
} from './types';

/**
 * Framework-neutral host surface handed to a plugin panel. Carries the
 * read side (state subscription + snapshot) and the write side (the same
 * IPC commands the native chrome uses), plus the contract version a plugin
 * checks against at mount.
 */
export type PluginBridge = {
  /**
   * Subscribe to raw section-keyed state deltas. With `selector`, the
   * callback only fires when a delta touches one of the named sections.
   * Returns an unsubscribe fn.
   */
  subscribe(
    cb: (delta: Partial<FrontendState>) => void,
    selector?: (keyof FrontendState)[],
  ): () => void;
  /** Deep copy of the current accumulated state. Safe to mutate. */
  snapshot(): FrontendState;
  dispatchOp: typeof dispatchOp;
  appCommand: (command: AppCommand) => void;
  setSelection: (entries: EntitySelection[]) => void;
  viewportInput: (input: ViewportInput) => void;
  request: <T = unknown>(kind: RequestKind, payload?: object, timeoutMs?: number) => Promise<T>;
  openSessionDialog: () => void;
  readonly contractVersion: number;
};

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
 * A plugin panel's entry point: render into `shadow`, return a cleanup fn.
 * `panelId` lets one entry point serve several declared panels.
 */
export type MountPanel = (
  panelId: string,
  shadow: ShadowRoot,
  bridge: PluginBridge,
) => () => void;

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
