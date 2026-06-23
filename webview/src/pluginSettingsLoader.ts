/**
 * Plugin settings-tab loader.
 *
 * Holds the catalog of plugin-declared settings tabs (queried once from
 * the backend at startup). The tabbed settings panel and the per-plugin
 * settings-button injection both read the published signal.
 */

import { createSignal } from 'solid-js';
import { request } from './transport';
import type { SettingsTabInfo } from './types';

/**
 * Backend-reported plugin settings tabs. Empty until the startup catalog
 * query resolves; stays empty when no plugin declares settings.
 */
const [pluginSettings, setPluginSettings] = createSignal<SettingsTabInfo[]>([]);
export { pluginSettings };

/**
 * Query the backend once for the settings-tab catalog and publish it.
 * Failures are non-fatal: a missing or erroring catalog just leaves the
 * signal empty (no settings buttons, no settings panels).
 */
export async function loadSettingsCatalog(): Promise<void> {
  try {
    const tabs = await request<SettingsTabInfo[]>('settings_catalog');
    setPluginSettings(tabs ?? []);
  } catch (e) {
    console.warn('[pluginSettingsLoader] settings_catalog query failed:', e);
  }
}
