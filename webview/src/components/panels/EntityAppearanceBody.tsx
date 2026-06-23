/**
 * EntityAppearanceBody - per-entity appearance editor
 *
 * Renders the flat `DisplayOverrides` schema for a single focused entity.
 * Unlike SettingsPanel, the DisplayOverrides schema has no object-group
 * properties, so there is no section dropdown; the fields are grouped
 * TS-side into a few labeled sections. Each control edit emits ONE
 * `SetEntityAppearance` command for the leaf field (not a whole-object
 * snapshot), and a Reset button emits `ClearEntityAppearance`.
 *
 * Control rendering reuses `SchemaField` from SettingsPanel so the
 * slider/checkbox/select behavior stays identical to the global panel.
 */

import { Component, For, JSX, Show, createMemo } from 'solid-js';

import { SchemaField } from '../util/SettingsPanel';
import { controlKind } from '../util/settingsSchema';

// ---------------------------------------------------------------------------
// Props
// ---------------------------------------------------------------------------

export interface EntityAppearanceBodyProps {
	schema: any;
	values: any;
	/** Emit one per-field appearance edit (leaf field name + new value). */
	onFieldUpdate: (field: string, value: unknown) => void;
	/** Emit a clear-overrides for this entity. */
	onReset: () => void;
	/** Disable the Reset button when the entity has no overrides set. */
	hasOverrides: boolean;
	header?: JSX.Element;
	footer?: JSX.Element;
}

// ---------------------------------------------------------------------------
// Curation
// ---------------------------------------------------------------------------

// Palette fields are global-scope only; never shown in the per-entity body.
const HIDDEN_FIELDS = new Set([
	'palette_preset',
	'palette_mode',
]);

// TS-side grouping of the flat DisplayOverrides fields into labeled sections.
// Any schema field not listed here lands in a trailing "Other" section, so a
// newly added Rust field still renders rather than silently vanishing.
const SECTION_LAYOUT: { label: string; fields: string[] }[] = [
	{ label: 'Drawing', fields: ['drawing_mode', 'helix_style', 'sheet_style'] },
	{
		label: 'Coloring',
		fields: ['color_scheme', 'sidechain_color_mode', 'na_color_mode', 'lipid_mode'],
	},
	{ label: 'Sidechains', fields: ['show_sidechains'] },
	{ label: 'Surface', fields: ['surface_kind', 'surface_opacity', 'show_cavities'] },
	{ label: 'Visibility', fields: ['show_hydrogens'] },
];

// ---------------------------------------------------------------------------
// Component
// ---------------------------------------------------------------------------

const EntityAppearanceBody: Component<EntityAppearanceBodyProps> = (props) => {
	// Build the rendered sections from the schema's actual properties, honoring
	// the curated layout but skipping hidden/absent fields and sweeping any
	// unlisted schema field into a trailing "Other" group.
	const sections = createMemo(() => {
		const properties = props.schema?.properties;
		if (!properties) return [];

		const present = new Set(Object.keys(properties).filter((k) => !HIDDEN_FIELDS.has(k)));
		const placed = new Set<string>();

		const groups = SECTION_LAYOUT.map((section) => {
			const fields = section.fields.filter((f) => present.has(f));
			fields.forEach((f) => placed.add(f));
			return { label: section.label, fields };
		}).filter((g) => g.fields.length > 0);

		const leftover = [...present].filter((f) => !placed.has(f));
		if (leftover.length > 0) {
			groups.push({ label: 'Other', fields: leftover });
		}

		// Annotate each field with a row-break flag at control-kind transitions,
		// so a checkbox run never shares a half-width row with a taller select.
		return groups.map((g) => ({
			label: g.label,
			fields: g.fields.map((name, i, all) => {
				const kind = controlKind(properties[name], props.schema);
				const prevKind = i > 0 ? controlKind(properties[all[i - 1]], props.schema) : kind;
				return { name, breakBefore: i > 0 && kind !== prevKind };
			}),
		}));
	});

	return (
		<div class="text-white pt-2">
			<Show when={props.header}>
				{props.header}
			</Show>

			<Show
				when={props.schema?.properties && props.values != null}
				fallback={<p class="text-xs text-white/40 pt-3">Loading...</p>}
			>
				<For each={sections()}>
					{(section) => (
						<div class="pt-2">
							<div class="text-xs text-white/60 pb-1">{section.label}</div>
							<div class="grid grid-cols-2 gap-2 items-start">
								<For each={section.fields}>
									{(field) => (
										<>
											<Show when={field.breakBefore}>
												<div class="col-span-full h-0" />
											</Show>
											<SchemaField
												fieldKey={field.name}
												schema={props.schema.properties[field.name]}
												rootSchema={props.schema}
												path={[field.name]}
												value={() => props.values?.[field.name]}
												values={() => props.values}
												onUpdate={(_path, value) => props.onFieldUpdate(field.name, value)}
											/>
										</>
									)}
								</For>
							</div>
						</div>
					)}
				</For>

				<div class="pt-3 border-t border-white/10 mt-3">
					<button
						class="w-full py-1.5 px-3 text-sm bg-white/10 hover:bg-white/20 rounded transition-colors disabled:opacity-40"
						disabled={!props.hasOverrides}
						onClick={props.onReset}
					>
						Reset to Default
					</button>
				</div>
			</Show>

			<Show when={props.footer}>
				{props.footer}
			</Show>
		</div>
	);
};

export default EntityAppearanceBody;
