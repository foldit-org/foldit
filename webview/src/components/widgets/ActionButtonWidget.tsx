/**
 * ActionButtonWidget - the per-plugin button surface.
 *
 * Renders one titled box per plugin (`plugin_id`) holding that plugin's
 * buttons of two kinds: ACTION buttons that dispatch an op, and TOGGLE
 * buttons that open a plugin panel. Action buttons come from the backend's
 * `actions.available` (`Orchestrator::ops_catalog` joins each manifest's
 * `[[buttons]]` with its bridge-registered ops); a click dispatches
 * `{ op_id }`. An action carrying a non-empty `options` list instead opens
 * a button-list picker, each entry dispatching its own full op
 * (`op_id` + `params`). Launcher toggles come from the plugin-panel
 * catalog; a click opens the panel via the panel-visibility toggle. A
 * plugin with only panels still gets a box.
 */

import { Component, createMemo, For, Show } from 'solid-js';
import "../../styles/widgets/ActionButtonWidget.css";
import { useBackendData, useUI } from '../../services/adapters';
import { state as backendState, dispatchOp, appCommand } from '../../adapter';
import { pluginPanels } from '../../pluginPanelLoader';
import { pluginSettings } from '../../pluginSettingsLoader';
import { settingsPanelId } from '../viewport/panelRegistry';
import { panelVisibilityKey } from '../../hooks/state';
import { Icons, resolveIcon } from '../../utils/iconMapping';
import type { ActionInfo, PanelInfo, PluginGroupInfo, SettingsTabInfo } from '../../types';
import ButtonListWidget, { type ButtonListItem } from "../util/ButtonListWidget";

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

/**
 * Resolve a button's visual content from its `icon_path`. `builtin:` glyphs
 * render inline, `game:` and plugin-relative paths fetch an image, and an
 * empty (or unknown-builtin) token falls back to the label text.
 */
function iconContent(iconPath: string, pluginId: string, fallback: string) {
  const icon = resolveIcon(iconPath, pluginId);
  if (icon.kind === 'component') return <icon.Icon />;
  if (icon.kind === 'url') return <img src={icon.url} alt={fallback} />;
  return fallback;
}

interface ActionPicker {
  opId: string;
  // One ButtonListItem array per picker row, one row per distinct option
  // `color` in first-appearance order. The mutate picker yields two rows,
  // hydrophobic (orange) then polar (blue).
  rows: ButtonListItem[][];
}

interface ActionGroup {
  pluginId: string;
  title: string;
  order: number;
  items: ButtonListItem[];
  pickers: ActionPicker[];
}

const ActionButtonWidget: Component = () => {
  const actions = useBackendData(state => state.actions);
  const groupMeta = useBackendData(state => state.actionGroups);
  const toggleWidget = useUI(state => state.toggleWidget);

  // Which options-carrying action has its picker open is backend-owned
  // (`actions.open_picker`, one op_id or null): the toggle button and the
  // button's hotkey both flip it through `SetActionPickerOpen`, so a click and
  // a keypress share one source of truth. The `groups` memo does not read it,
  // so toggling never rebuilds the button items.
  const openPicker = () => backendState.actions.open_picker;

  // Bucket each plugin's buttons -- action buttons then panel launchers --
  // by plugin, then title + order each box from the parallel `groups`
  // side-table. Title is the manifest-declared name, falling back to a
  // prettified id when absent. Boxes sort by their explicit `order`
  // ascending, with unordered ones last and plugin id breaking ties. A
  // plugin contributing only panels still gets a box.
  const groups = createMemo<ActionGroup[]>(() => {
    const byPluginActions = new Map<string, ButtonListItem[]>();
    const byPluginLaunchers = new Map<string, ButtonListItem[]>();
    const byPluginPickers = new Map<string, ActionPicker[]>();

    for (const action of actions() as ActionInfo[]) {
      // An action with options is a toggle that opens a picker; an action
      // without options dispatches its op directly. Both gate on the host's
      // `enabled` so the button disables live with the selection lock.
      const hasOptions = action.options.length > 0;
      // A built-in glyph is a JSX component, not a URL, so it renders through
      // `iconNode` (flex-centered) rather than the bottom-left `content` label
      // slot. Sized to sit at the same visual weight as the plugin-asset PNGs
      // (which get a 5px inset); `.button-list-item svg` adds a matching inset.
      // Plugin-asset and text buttons keep using `content`.
      const resolvedIcon = resolveIcon(action.icon_path, action.plugin_id);
      const BuiltinIcon = resolvedIcon.kind === 'component' ? resolvedIcon.Icon : undefined;
      // Live weights download surfaces on the host-injected download button
      // (icon-only); its plugin's fraction drives a fill bar and the stage
      // label rides the tooltip. Other ops render their progress as a toast.
      const download =
        action.op_id === 'download_weights'
          ? backendState.actions.op_progress?.find(
              (e) => e.op_id === 'download_weights' && e.plugin_id === action.plugin_id,
            )
          : undefined;
      const item: ButtonListItem = {
        id: `action-${action.op_id}`,
        iconNode: BuiltinIcon ? <BuiltinIcon size={32} color="white" /> : undefined,
        content: BuiltinIcon
          ? undefined
          : iconContent(action.icon_path, action.plugin_id, action.display),
        // Hotkey + tooltip ride on the plugin manifest's [[buttons]]
        // entries (ButtonEntry -> CatalogEntry -> ActionInfo). Badge text
        // is the friendly glyph; the raw winit string stays in the
        // catalog for the core-side resolver. Tooltip falls back to display.
        hotkey: action.hotkey ? formatHotkey(action.hotkey) : undefined,
        disabled: !action.enabled,
        tooltip: (download?.label || action.tooltip) ?? action.display,
        progress: download?.fraction ?? undefined,
        onClick: hasOptions
          ? () =>
              appCommand({
                type: 'SetActionPickerOpen',
                op_id:
                  backendState.actions.open_picker === action.op_id
                    ? null
                    : action.op_id,
              })
          : () =>
              dispatchOp({
                op_id: action.op_id,
                // Carry the action's source plugin so the host can route
                // ops that several plugins register under one shared op-id
                // (e.g. weight download) to this plugin rather than the
                // flat registry's arbitrary last-writer owner. Benign extra
                // param for ops that don't need it.
                params: { plugin_id: { String: action.plugin_id } },
              }),
      };

      // Is this action running in the currently-focused context? Grounded in
      // the lock list: a `running` entry matches when it's global or locks the
      // focused entity. If so, the button turns into a cancel "x" on a red
      // background - clickable (overriding the lock's `!enabled`) and
      // cancelling just that instance (its request_id, or the refine flag for
      // the native refine). Every other button greys out via
      // `disabled: !action.enabled`; a free focused entity is greenfield.
      const focused = backendState.actions.focused_entity_id;
      const runningMatch = backendState.actions.running.find(
        (r) =>
          r.op_id === action.op_id &&
          (r.global || (focused != null && r.entities.includes(focused))),
      );
      if (runningMatch) {
        item.iconNode = <Icons.X size={32} color="white" />;
        item.content = undefined;
        item.disabled = false;
        item.progress = undefined;
        item.color = 'cancel';
        item.tooltip = 'Cancel';
        item.onClick = () =>
          appCommand({ type: 'CancelAction', request_id: runningMatch.request_id });
      }

      let bucket = byPluginActions.get(action.plugin_id);
      if (!bucket) {
        bucket = [];
        byPluginActions.set(action.plugin_id, bucket);
      }
      bucket.push(item);

      if (hasOptions) {
        // Each option is a self-contained dispatch (op_id + params). Picking
        // one fires it and closes the picker. Option icons take the same
        // `builtin:` / `game:` / plugin-relative tokens as button icons.
        const toItem = (option: (typeof action.options)[number]): ButtonListItem => {
          const optionIcon = resolveIcon(option.icon, action.plugin_id);
          return {
            id: `${option.op_id}:${option.label}`,
            content: option.label,
            color: option.color,
            icon: optionIcon.kind === 'url' ? optionIcon.url : undefined,
            iconNode:
              optionIcon.kind === 'component'
                ? <optionIcon.Icon size={24} color="white" />
                : undefined,
            hotkey: option.hotkey ? formatHotkey(option.hotkey) : undefined,
            onClick: () => {
              dispatchOp({ op_id: option.op_id, params: option.params });
              appCommand({ type: 'SetActionPickerOpen', op_id: null });
            },
          };
        };

        // One picker row per distinct `color` token, in the order colors first
        // appear. Mutate yields hydrophobic (`orange`) then polar (`blue`).
        const rowsByColor = new Map<string, ButtonListItem[]>();
        for (const option of action.options) {
          let row = rowsByColor.get(option.color);
          if (!row) {
            row = [];
            rowsByColor.set(option.color, row);
          }
          row.push(toItem(option));
        }

        let pickers = byPluginPickers.get(action.plugin_id);
        if (!pickers) {
          pickers = [];
          byPluginPickers.set(action.plugin_id, pickers);
        }
        pickers.push({ opId: action.op_id, rows: [...rowsByColor.values()] });
      }
    }

    for (const panel of pluginPanels() as PanelInfo[]) {
      // A launcher is a toggle, not an op: no hotkey, never disabled.
      // Opening goes through the same panel-visibility toggle the native
      // chrome uses, keyed by the panel's visibility key.
      const item: ButtonListItem = {
        id: `launcher-${panel.id}`,
        content: iconContent(panel.icon_path, panel.plugin_id, panel.title),
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
          pickers: byPluginPickers.get(pluginId) ?? [],
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
              <For each={group.pickers}>
                {(picker) => (
                  <Show when={openPicker() === picker.opId}>
                    <For each={picker.rows}>
                      {(row) => <ButtonListWidget items={row} />}
                    </For>
                  </Show>
                )}
              </For>
            </div>
          )}
        </For>
      </div>
    </Show>
  );
};

export default ActionButtonWidget;
