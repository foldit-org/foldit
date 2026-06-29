/**
 * ActionButtonWidget - the per-plugin button surface.
 *
 * Renders one titled box per plugin (`plugin_id`) holding that plugin's
 * buttons of two kinds: ACTION buttons that dispatch an op, and TOGGLE
 * buttons that open a plugin panel. Action buttons come from the backend's
 * `actions.available` (`Orchestrator::ops_catalog` joins each manifest's
 * `[[buttons]]` with its bridge-registered ops); a click dispatches
 * `{ op_id }`. Launcher toggles come from the plugin-panel catalog; a
 * click opens the panel via the panel-visibility toggle. A plugin with
 * only panels still gets a box.
 */

import { Component, createMemo, For, Show } from 'solid-js';
import "../../styles/widgets/ActionButtonWidget.css";
import { useBackendData, useUI } from '../../services/adapters';
import { dispatchOp } from '../../adapter';
import { pluginPanels } from '../../pluginPanelLoader';
import { pluginSettings } from '../../pluginSettingsLoader';
import { settingsPanelId } from '../viewport/panelRegistry';
import { panelVisibilityKey } from '../../hooks/state';
import { Icons } from '../../utils/iconMapping';
import type { ActionInfo, PanelInfo, PluginGroupInfo, SettingsTabInfo } from '../../types';
import ButtonListWidget, { type ButtonListItem } from "../util/ButtonListWidget";
import { pluginAssetUrl } from '../../pluginAssets';

/**
 * Prettify a winit `KeyCode` debug string for the corner badge. The
 * manifest stores the raw spelling ("KeyW", "Backquote", …) because
 * the core-side resolver matches against winit key strings; only the
 * displayed glyph is friendly. Routing never goes through this value.
 */
function formatHotkey(code: string): string {
  if (/^Key[A-Z]$/.test(code)) return code.slice(3);
  if (/^Digit[0-9]$/.test(code)) return code.slice(5);
  if (/^Numpad[0-9]$/.test(code)) return code.slice(6);
  const named: Record<string, string> = {
    Backquote: '`', Minus: '-', Equal: '=', Slash: '/', Backslash: '\\',
    Escape: 'Esc', Space: 'Space', Enter: '↵', Tab: 'Tab',
    ArrowUp: '↑', ArrowDown: '↓', ArrowLeft: '←', ArrowRight: '→',
  };
  return named[code] ?? code;
}

/**
 * Title-case a raw `plugin_id` for the group header ("foldit-rosetta" →
 * "Foldit Rosetta"). Fallback title for plugins whose manifest declares
 * no explicit `name`.
 */
function prettifyPluginId(id: string): string {
  return id
    .replace(/[-_]+/g, ' ')
    .split(' ')
    .filter((w) => w.length > 0)
    .map((w) => w.charAt(0).toUpperCase() + w.slice(1))
    .join(' ');
}

interface ActionGroup {
  pluginId: string;
  title: string;
  order: number;
  items: ButtonListItem[];
}

const ActionButtonWidget: Component = () => {
  const actions = useBackendData(state => state.actions);
  const groupMeta = useBackendData(state => state.actionGroups);
  const toggleWidget = useUI(state => state.toggleWidget);

  // Bucket each plugin's buttons -- action buttons then panel launchers --
  // by plugin, then title + order each box from the parallel `groups`
  // side-table. Title is the manifest-declared name, falling back to a
  // prettified id when absent. Boxes sort by their explicit `order`
  // ascending, with unordered ones last and plugin id breaking ties. A
  // plugin contributing only panels still gets a box.
  const groups = createMemo<ActionGroup[]>(() => {
    const byPluginActions = new Map<string, ButtonListItem[]>();
    const byPluginLaunchers = new Map<string, ButtonListItem[]>();

    for (const action of actions() as ActionInfo[]) {
      const item: ButtonListItem = {
        id: `action-${action.op_id}`,
        content: action.icon_path
          ? <img src={pluginAssetUrl(action.plugin_id, action.icon_path)} alt={action.display} />
          : action.display,
        // Hotkey + tooltip ride on the plugin manifest's [[buttons]]
        // entries (ButtonEntry -> CatalogEntry -> ActionInfo). Badge text
        // is the friendly glyph; the raw winit string stays in the
        // catalog for the core-side resolver. Tooltip falls back to display.
        hotkey: action.hotkey ? formatHotkey(action.hotkey) : undefined,
        disabled: !action.enabled,
        tooltip: action.tooltip ?? action.display,
        onClick: () => dispatchOp({ op_id: action.op_id }),
      };

      let bucket = byPluginActions.get(action.plugin_id);
      if (!bucket) {
        bucket = [];
        byPluginActions.set(action.plugin_id, bucket);
      }
      bucket.push(item);
    }

    for (const panel of pluginPanels() as PanelInfo[]) {
      // A launcher is a toggle, not an op: no hotkey, never disabled.
      // Opening goes through the same panel-visibility toggle the native
      // chrome uses, keyed by the panel's visibility key.
      const item: ButtonListItem = {
        id: `launcher-${panel.id}`,
        content: panel.icon_path
          ? <img src={pluginAssetUrl(panel.plugin_id, panel.icon_path)} alt={panel.title} />
          : panel.title,
        tooltip: panel.tooltip ?? panel.title,
        onClick: () => toggleWidget()(panelVisibilityKey(panel.id)),
      };

      let bucket = byPluginLaunchers.get(panel.plugin_id);
      if (!bucket) {
        bucket = [];
        byPluginLaunchers.set(panel.plugin_id, bucket);
      }
      bucket.push(item);
    }

    const meta = new Map(
      (groupMeta() as PluginGroupInfo[]).map((g) => [g.plugin_id, g]),
    );

    // Plugins that declare at least one settings tab get one auto-injected
    // settings toggle (host glyph, no plugin asset) opening their settings
    // panel via the per-plugin visibility key.
    const settingsPluginIds = new Set(
      (pluginSettings() as SettingsTabInfo[]).map((t) => t.plugin_id),
    );

    const pluginIds = new Set([
      ...byPluginActions.keys(),
      ...byPluginLaunchers.keys(),
      ...settingsPluginIds,
    ]);

    return [...pluginIds]
      .map((pluginId) => {
        const group = meta.get(pluginId);
        const settingsButton: ButtonListItem[] = settingsPluginIds.has(pluginId)
          ? [{
              id: `settings-${pluginId}`,
              content: <Icons.Settings />,
              tooltip: 'Settings',
              onClick: () => toggleWidget()(panelVisibilityKey(settingsPanelId(pluginId))),
            }]
          : [];
        return {
          pluginId,
          title: group?.name ?? prettifyPluginId(pluginId),
          order: group?.order ?? Number.MAX_SAFE_INTEGER,
          items: [
            ...(byPluginActions.get(pluginId) ?? []),
            ...(byPluginLaunchers.get(pluginId) ?? []),
            ...settingsButton,
          ],
        };
      })
      .sort((a, b) => a.order - b.order || a.pluginId.localeCompare(b.pluginId));
  });

  return (
    <Show when={actions().length > 0 || pluginPanels().length > 0 || pluginSettings().length > 0}>
      <div class="action-button-widget pointer-events-auto">
        <For each={groups()}>
          {(group) => (
            <div class="action-group">
              <div class="action-group-title">{group.title}</div>
              <ButtonListWidget items={group.items} />
            </div>
          )}
        </For>
      </div>
    </Show>
  );
};

export default ActionButtonWidget;
