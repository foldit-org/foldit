import { createSignal, For, Show } from 'solid-js';

import { state, appCommand } from '../../adapter';

import SelectDropdown from '../util/SelectDropdown';
import SettingsPanel from '../util/SettingsPanel';
import EntityAppearanceBody from './EntityAppearanceBody';

import '../../styles/panels/ViewOptionsMenuPanel.css';

// Sentinel option value for the whole-scene ("Global") focus row. Entity rows
// use their numeric entity id stringified; this never collides with one.
const GLOBAL_FOCUS = '__global__';

export default function ViewOptionsMenuPanel() {
	const [savePresetName, setSavePresetName] = createSignal('');
	const [showSaveInput, setShowSaveInput] = createSignal(false);

	const handlePresetLoad = (_: string, presetName: string) => {
		if (presetName) {
			appCommand({ type: 'LoadViewPreset', name: presetName });
		}
	};

	const handlePresetSave = () => {
		const name = savePresetName().trim();
		if (name) {
			appCommand({ type: 'SaveViewPreset', name });
			setSavePresetName('');
			setShowSaveInput(false);
		}
	};

	const handleSaveKeyDown = (e: KeyboardEvent) => {
		if (e.key === 'Enter') {
			handlePresetSave();
		} else if (e.key === 'Escape') {
			setShowSaveInput(false);
			setSavePresetName('');
		}
	};

	// Two-way focus binding: the select VALUE is derived from the
	// backend-authoritative `scene.focused_entity` (null/absent => Global). A
	// change emits SetFocus; the next scene tick reflects it back. Focus is
	// never owned locally.
	const focusedId = () => state.scene.focused_entity;

	const focusValue = () => {
		const id = focusedId();
		return id == null ? GLOBAL_FOCUS : String(id);
	};

	const focusedEntity = () => {
		const id = focusedId();
		if (id == null) return undefined;
		return state.scene.entities.find((e) => e.entity_id === id);
	};

	const handleFocusChange = (e: Event) => {
		const raw = (e.target as HTMLSelectElement).value;
		const entity_id = raw === GLOBAL_FOCUS ? null : Number(raw);
		appCommand({ type: 'SetFocus', entity_id });
	};

	// A function (not a shared node) so each Show branch gets its own instance.
	const focusDropdown = () => (
		<div class="options-dropdown pb-2">
			<label>Focus</label>
			<select value={focusValue()} onChange={handleFocusChange}>
				<option value={GLOBAL_FOCUS}>Global</option>
				<For each={state.scene.entities}>
					{(entity) => (
						<option value={String(entity.entity_id)}>
							{entity.label || `Entity ${entity.entity_id}`}
						</option>
					)}
				</For>
			</select>
		</div>
	);

	const presetFooter = (
		<div class="space-y-2 pt-3 border-t border-white/10 mt-3">
			<Show when={state.view.available_presets.length > 0}>
				<SelectDropdown
					title="Presets"
					options={state.view.available_presets}
					value={state.view.active_preset ?? ''}
					option_name="presets"
					onChange={handlePresetLoad}
				/>
			</Show>

			<Show when={!showSaveInput()}>
				<button
					class="w-full py-1.5 px-3 text-sm bg-white/10 hover:bg-white/20 rounded transition-colors"
					onClick={() => setShowSaveInput(true)}
				>
					Save Current as Preset...
				</button>
			</Show>

			<Show when={showSaveInput()}>
				<div class="flex gap-2">
					<input
						type="text"
						placeholder="Preset name"
						value={savePresetName()}
						onInput={(e) => setSavePresetName(e.currentTarget.value)}
						onKeyDown={handleSaveKeyDown}
						onClick={(e) => e.currentTarget.focus()}
						ref={(el) => requestAnimationFrame(() => el.focus())}
						class="flex-1 px-2 py-1.5 text-sm bg-white/10 border border-white/20 rounded text-white placeholder-gray-500 focus:border-blue-500 focus:ring-1 focus:ring-blue-500 focus:outline-none transition-colors"
					/>
					<button
						class="px-3 py-1.5 text-sm bg-blue-600 hover:bg-blue-500 rounded transition-colors disabled:opacity-40"
						disabled={!savePresetName().trim()}
						onClick={handlePresetSave}
					>
						Save
					</button>
					<button
						class="px-2 py-1.5 text-sm bg-white/10 hover:bg-white/20 rounded transition-colors"
						onClick={() => { setShowSaveInput(false); setSavePresetName(''); }}
					>
						&times;
					</button>
				</div>
			</Show>
		</div>
	);

	return (
		<Show
			when={focusedEntity()}
			fallback={
				// Global focus: the existing whole-scene options path, unchanged.
				<SettingsPanel
					schema={state.view.options_schema}
					values={state.view.options}
					onUpdate={(opts) => appCommand({ type: 'SetViewOptions', options: opts })}
					header={focusDropdown()}
					footer={presetFooter}
				/>
			}
		>
			{(entity) => (
				<EntityAppearanceBody
					schema={state.view.appearance_schema}
					values={entity().appearance_values}
					hasOverrides={entity().has_overrides}
					onFieldUpdate={(field, value) =>
						appCommand({
							type: 'SetEntityAppearance',
							entity_id: entity().entity_id,
							field,
							value,
						})
					}
					onReset={() =>
						appCommand({ type: 'ClearEntityAppearance', entity_id: entity().entity_id })
					}
					header={focusDropdown()}
				/>
			)}
		</Show>
	);
}
