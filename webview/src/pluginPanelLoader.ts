/**
 * Plugin custom-panel loader.
 *
 * Holds the catalog of plugin-declared panels (queried once from the
 * backend at startup), and lazily imports + mounts a panel's ES-module
 * bundle on first open. Bundles are cached by URL so reopening a panel
 * reuses the already-imported module; only the per-mount teardown runs
 * each close.
 */

import { createSignal } from 'solid-js';
import { request } from './transport';
import { createBridge, mountPluginPanel as bridgeMountPanel, type MountPanel } from './bridge';
import { pluginAssetUrl } from './pluginAssets';
import type { PanelInfo } from './types';

/** Shape a plugin bundle must export to be mountable. */
type PluginModule = {
  mountPanel: MountPanel;
  BRIDGE_CONTRACT_VERSION: number;
};

/**
 * Backend-reported plugin panels. Empty until the startup catalog query
 * resolves; stays empty when no plugin declares panels. Both the panel
 * registry (chrome) and the action-button surface (launchers) read it.
 */
const [pluginPanels, setPluginPanels] = createSignal<PanelInfo[]>([]);
export { pluginPanels };

/**
 * Query the backend once for the plugin-panel catalog and publish it.
 * Failures are non-fatal: a missing or erroring catalog just leaves the
 * signal empty (no launchers, no registered panels).
 */
export async function loadPluginPanelCatalog(): Promise<void> {
  try {
    const panels = await request<PanelInfo[]>('panels_catalog');
    setPluginPanels(panels ?? []);
  } catch (e) {
    console.warn('[pluginPanelLoader] panels_catalog query failed:', e);
  }
}

// Bundle import cache keyed by absolute module URL. A second open of any
// panel served by the same entry reuses the in-flight or settled import.
const moduleCache = new Map<string, Promise<PluginModule>>();

// Live teardown per mounted panel id, so closing a panel runs its cleanup
// even though the cached module survives for the next open.
const teardowns = new Map<string, () => void>();

function importBundle(pluginId: string, entry: string): Promise<PluginModule> {
  // The `@vite-ignore` below keeps Vite's import analyzer from trying to
  // trace the runtime-built `/plugins/...` URL at build time (same dodge
  // the wasm transport uses for its /pkg/ module).
  const url = pluginAssetUrl(pluginId, entry);
  let mod = moduleCache.get(url);
  if (!mod) {
    mod = import(/* @vite-ignore */ url) as Promise<PluginModule>;
    moduleCache.set(url, mod);
  }
  return mod;
}

/**
 * Import (cached) and mount a plugin panel into `host`. Stores the
 * resulting teardown under the panel id; a prior mount of the same id is
 * torn down first. Returns the teardown for the caller to run on unmount.
 */
export async function mountPluginPanel(host: HTMLElement, panel: PanelInfo): Promise<() => void> {
  teardowns.get(panel.id)?.();
  teardowns.delete(panel.id);

  const mod = await importBundle(panel.plugin_id, panel.entry);
  const teardown = bridgeMountPanel(
    host,
    panel.id,
    mod.BRIDGE_CONTRACT_VERSION,
    mod.mountPanel,
    createBridge(),
  );
  teardowns.set(panel.id, teardown);
  return () => {
    teardown();
    teardowns.delete(panel.id);
  };
}
