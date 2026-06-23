/**
 * PluginSettingsPanel - tabbed, write-only plugin settings.
 *
 * For one plugin, renders the generic Tabs chrome with one tab per
 * `SettingsTabInfo`. Each tab fetches its JSON-schema asset by URL, seeds
 * a form from the schema defaults plus a host last-written store, and
 * renders the schema with `SchemaField` per leaf. Each field edit
 * dispatches the tab's `on_update_op` carrying that one field as a typed
 * `ParamValue`, and records the value in the last-written store. There is
 * no backend read-back; the form seeds from defaults + last-written only.
 */

import { Component, For, createMemo, createResource, createSignal } from 'solid-js';

import { dispatchOp } from '../../adapter';
import { pluginAssetUrl } from '../../pluginAssets';
import { pluginSettings } from '../../pluginSettingsLoader';
import Tabs, { type TabData } from '../util/Tabs';
import { SchemaField } from '../util/SettingsPanel';
import { controlKind, resolveFieldSchema } from '../util/settingsSchema';
import type { ParamValue, SettingsTabInfo } from '../../types';

// Host last-written values, keyed by `${plugin_id}::${on_update_op}::${field}`.
// Survives panel close/reopen so a tab reseeds from the last edit, not the
// schema default. Never read back from the backend (write-only path).
const lastWritten = new Map<string, unknown>();

function storeKey(tab: SettingsTabInfo, field: string): string {
	return `${tab.plugin_id}::${tab.on_update_op}::${field}`;
}

// Pull the top-level field defaults out of a JSON schema's `properties`.
function schemaDefaults(schema: any): Record<string, unknown> {
	const properties = schema?.properties;
	if (!properties) return {};
	const out: Record<string, unknown> = {};
	for (const [key, fieldSchema] of Object.entries(properties)) {
		const resolved = resolveFieldSchema(fieldSchema, schema);
		if (resolved.default !== undefined) out[key] = resolved.default;
	}
	return out;
}

// Wrap a leaf value in its externally-tagged `ParamValue` variant, chosen
// from the field's resolved schema type.
function toParamValue(value: unknown, fieldSchema: any, rootSchema: any): ParamValue {
	const resolved = resolveFieldSchema(fieldSchema, rootSchema);
	if (resolved.type === 'boolean') return { Bool: Boolean(value) } as ParamValue;
	if (resolved.type === 'integer') return { Int: Number(value) } as ParamValue;
	if (resolved.type === 'number') return { Float: Number(value) } as ParamValue;
	return { String: String(value) } as ParamValue;
}

function SettingsTabBody(props: { tab: SettingsTabInfo }) {
	const url = () => pluginAssetUrl(props.tab.plugin_id, props.tab.schema_asset_path);

	const [schema] = createResource(url, async (u) => {
		const res = await fetch(u);
		if (!res.ok) throw new Error(`settings schema ${u}: ${res.status}`);
		return res.json();
	});

	// Form seed: schema defaults overlaid with the host last-written store.
	const [values, setValues] = createSignal<Record<string, unknown>>({});

	const seed = createMemo(() => {
		const s = schema();
		if (!s) return;
		const defaults = schemaDefaults(s);
		const seeded: Record<string, unknown> = { ...defaults };
		for (const field of Object.keys(s.properties ?? {})) {
			const key = storeKey(props.tab, field);
			if (lastWritten.has(key)) seeded[field] = lastWritten.get(key);
		}
		setValues(seeded);
	});

	const fields = createMemo(() => {
		seed();
		const s = schema();
		if (!s?.properties) return [];
		return Object.keys(s.properties).filter(
			(name) => controlKind(s.properties[name], s) !== 'none',
		);
	});

	const onUpdate = (field: string, value: unknown) => {
		const s = schema();
		if (!s) return;
		setValues((prev) => ({ ...prev, [field]: value }));
		lastWritten.set(storeKey(props.tab, field), value);
		dispatchOp({
			op_id: props.tab.on_update_op,
			params: { [field]: toParamValue(value, s.properties[field], s) },
		});
	};

	return (
		<div class="text-white pt-2">
			<div class="grid grid-cols-2 gap-2 items-start">
				<For each={fields()}>
					{(field) => (
						<SchemaField
							fieldKey={field}
							schema={schema()!.properties[field]}
							rootSchema={schema()!}
							path={[field]}
							value={() => values()[field]}
							values={() => values()}
							onUpdate={(_path, value) => onUpdate(field, value)}
						/>
					)}
				</For>
			</div>
		</div>
	);
}

const PluginSettingsPanel: Component<{ pluginId: string }> = (props) => {
	const tabs = createMemo<TabData[]>(() =>
		pluginSettings()
			.filter((t) => t.plugin_id === props.pluginId)
			.map((tab) => ({
				name: tab.on_update_op,
				label: tab.name,
				children: <SettingsTabBody tab={tab} />,
			})),
	);

	return <Tabs tabs={tabs()} />;
};

export default PluginSettingsPanel;

/** A bare body component (no Panel wrapper) for the panel registry. */
export function makeSettingsPanelBody(pluginId: string): Component {
	return () => <PluginSettingsPanel pluginId={pluginId} />;
}
