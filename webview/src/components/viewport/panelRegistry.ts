/**
 * Panel registry — single source of truth for first-party panel chrome.
 *
 * Each descriptor carries the Panel-level props (id, title, width, position)
 * plus a pure body component (no <Panel> wrapper). PanelsLayer maps over the
 * signal so a later plugin-loader session can append via setPanels.
 */

import { Component, createSignal, createRoot, createEffect, onMount, onCleanup } from 'solid-js';

import { state } from '../../adapter';
import HistoryPanel from '../panels/HistoryPanel';
import DebugInfoPanel from '../panels/DebugInfoPanel';
import AlignmentPanel from '../panels/AlignmentPanel';
import SmallMoleculeDesignPanel from '../panels/SmallMoleculeDesignPanel';
import ViewOptionsMenuPanel from '../panels/ViewOptionsMenuPanel';
import { pluginPanels, mountPluginPanel } from '../../pluginPanelLoader';
import { pluginSettings } from '../../pluginSettingsLoader';
import { makeSettingsPanelBody } from '../panels/PluginSettingsPanel';
import type { PanelInfo } from '../../types';

// Descriptor id for a plugin's settings panel. Pass through
// `panelVisibilityKey` to get the `state.panels.open` key the launcher
// toggles.
export function settingsPanelId(pluginId: string): string {
	return `plugin-settings-${pluginId}`;
}

export type PanelDescriptor = {
	id: string;
	title: () => string;
	width: () => number;
	position: () => { x: number; y: number };
	Body: Component;
	triggerId?: string;
};

const DEBUG_MIN_WIDTH = 300;
const DEBUG_MAX_WIDTH = 420;

function debugInfoWidth(): number {
	const currentLog = state.ui.log ?? '';
	if (!currentLog) return DEBUG_MIN_WIDTH;
	const lines = currentLog.split('\n');
	const longestLineLength = Math.max(...lines.map((line: string) => line.length), 0);
	const calculatedWidth = longestLineLength * 6.5 + 56;
	return Math.min(Math.max(calculatedWidth, DEBUG_MIN_WIDTH), DEBUG_MAX_WIDTH);
}

const FIRST_PARTY: PanelDescriptor[] = [
	{
		id: 'history-panel',
		title: () => 'History',
		width: () => 500,
		position: () => ({ x: 80, y: window.innerHeight / 2 - 160 }),
		Body: HistoryPanel,
		triggerId: 'history-panel-button',
	},
	{
		id: 'debug-info-panel',
		title: () => 'Debug Info',
		width: () => debugInfoWidth(),
		position: () => ({ x: 500, y: 100 }),
		Body: DebugInfoPanel,
		triggerId: 'debug-info',
	},
	{
		id: 'alignment-panel',
		title: () => 'Alignment',
		width: () => 600,
		position: () => ({ x: 100, y: 200 }),
		Body: AlignmentPanel,
	},
	{
		id: 'small-molecule-design-panel',
		title: () => 'Small-Molecule Design',
		width: () => 400,
		position: () => ({ x: 50, y: 50 }),
		Body: SmallMoleculeDesignPanel,
	},
	{
		id: 'view-options-menu-panel',
		title: () => 'Appearance',
		width: () => 440,
		position: () => ({ x: 80, y: 60 }),
		Body: ViewOptionsMenuPanel,
		triggerId: 'view-options-menu-panel-button',
	},
];

export const [panels, setPanels] = createSignal<PanelDescriptor[]>(FIRST_PARTY);

/**
 * A plugin panel's body: a bare host element the plugin loader mounts its
 * bundle into on mount and tears down on cleanup. The loader caches the
 * imported module, so reopening reuses it; teardown runs per mount because
 * the Panel's `Show` unmounts the body when the panel closes.
 */
function makePluginPanelBody(panel: PanelInfo): Component {
	return () => {
		const host = document.createElement('div');
		// A box-generating display: `contents` produced no box, so the mounted
		// shadow content collapsed. The plugin owns its min-height; the host
		// just needs to be a real box that spans the panel width.
		host.style.display = 'block';
		host.style.width = '100%';
		onMount(() => {
			let live = true;
			let teardown: (() => void) | undefined;
			void mountPluginPanel(host, panel).then((t) => {
				if (live) teardown = t;
				else t();
			}).catch((err) => {
				console.error(`[panelRegistry] plugin panel '${panel.id}' failed to mount:`, err);
			});
			onCleanup(() => {
				live = false;
				teardown?.();
			});
		});
		return host;
	};
}

function pluginDescriptor(panel: PanelInfo): PanelDescriptor {
	return {
		id: panel.id,
		title: () => panel.title,
		width: () => panel.width,
		position: () => ({ x: panel.position_x ?? 100, y: panel.position_y ?? 100 }),
		Body: makePluginPanelBody(panel),
	};
}

function settingsDescriptor(pluginId: string): PanelDescriptor {
	return {
		id: settingsPanelId(pluginId),
		title: () => 'Settings',
		width: () => 400,
		position: () => ({ x: 100, y: 100 }),
		Body: makeSettingsPanelBody(pluginId),
	};
}

// Append plugin-declared panels and per-plugin settings panels to the
// first-party set as the catalog signals populate. Detached root so the
// effect outlives any component.
createRoot(() => {
	createEffect(() => {
		const settingsPlugins = [...new Set(pluginSettings().map((t) => t.plugin_id))];
		setPanels([
			...FIRST_PARTY,
			...pluginPanels().map(pluginDescriptor),
			...settingsPlugins.map(settingsDescriptor),
		]);
	});
});
