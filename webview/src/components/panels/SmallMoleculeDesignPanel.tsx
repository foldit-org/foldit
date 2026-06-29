import { createSignal, createMemo, For } from 'solid-js';

// component imports
import Tabs from '../util/Tabs';
import ImageFromFS from '../util/ImageFromFS';

// style imports
import '../../styles/panels/SmallMoleculeDesignPanel.css';

const FragmentSelectionTab = () => {
	const [activeCategory, setActiveCategory] = createSignal('functional');

	// Fragment category buttons with icons
	const categories = [
		{
			id: 'functional',
			name: 'Functional Groups',
			icon: 'resources/images/128x128/functional_group_active.png'
		},
		{
			id: 'saturated',
			name: 'Saturated Cyclic',
			icon: 'resources/images/128x128/saturated_cyclic_inactive.png'
		},
		{
			id: 'unsaturated',
			name: 'Unsaturated Cyclic',
			icon: 'resources/images/128x128/unsaturated_cyclic_inactive.png'
		},
		{
			id: 'polycyclic',
			name: 'Polycyclic',
			icon: 'resources/images/128x128/polycyclic_inactive.png'
		}
	];

	// Fragment collections by category (from database files)
	const fragmentsByCategory = {
		functional: [
			{ name: 'Trifluoromethyl', image: 'resources/images/32x32/elements/trifluoromethyl/trifluoromethyl.png' },
			{ name: 'Carboxamide', image: 'resources/images/32x32/elements/carboxamide/carboxamide.png' },
			{ name: 'Carboxylic Acid', image: 'resources/images/32x32/elements/carboxylic acid/carboxylic acid.png' },
			{ name: 'Sulfate', image: 'resources/images/32x32/elements/sulfate/sulfate.png' },
			{ name: 'Sulfone', image: 'resources/images/32x32/elements/sulfone/sulfone.png' },
			{ name: 'Sulfoxide', image: 'resources/images/32x32/elements/sulfoxide/sulfoxide.png' },
			{ name: 'Nitrile', image: 'resources/images/32x32/elements/nitrile/nitrile.png' },
			{ name: 'Phosphite Ion', image: 'resources/images/32x32/elements/phosphite ion/phosphite ion.png' }
		],
		saturated: [
			{ name: 'Cyclopentane', image: 'resources/images/32x32/elements/cyclopentane/cyclopentane.png' },
			{ name: 'Piperazine', image: 'resources/images/32x32/elements/piperazine/piperazine.png' },
			{ name: 'Cyclopropane', image: 'resources/images/32x32/elements/cyclopropane/cyclopropane.png' },
			{ name: 'Cyclobutane', image: 'resources/images/32x32/elements/cyclobutane/cyclobutane.png' },
			{ name: 'Cyclohexane', image: 'resources/images/32x32/elements/cyclohexane/cyclohexane.png' },
			{ name: 'Cycloheptane', image: 'resources/images/32x32/elements/cycloheptane/cycloheptane.png' },
			{ name: 'Oxetane', image: 'resources/images/32x32/elements/oxetane/oxetane.png' },
			{ name: 'Tetrahydrofuran', image: 'resources/images/32x32/elements/tetrahydrofuran/tetrahydrofuran.png' },
			{ name: 'Piperidine', image: 'resources/images/32x32/elements/piperidine/piperidine.png' },
			{ name: 'Oxepane', image: 'resources/images/32x32/elements/oxepane/oxepane.png' },
			{ name: 'Morpholine', image: 'resources/images/32x32/elements/morpholine/morpholine.png' },
			{ name: 'Azepane', image: 'resources/images/32x32/elements/azepane/azepane.png' },
			{ name: 'Azetidine', image: 'resources/images/32x32/elements/azetidine/azetidine.png' },
			{ name: 'Pyrrolidine', image: 'resources/images/32x32/elements/pyrrolidine/pyrrolidine.png' },
			{ name: 'Diazepane', image: 'resources/images/32x32/elements/diazepane/diazepane.png' }
		],
		unsaturated: [
			{ name: 'Triazole', image: 'resources/images/32x32/elements/triazole/triazole.png' },
			{ name: 'Imidazole', image: 'resources/images/32x32/elements/imidazole/imidazole.png' },
			{ name: 'Benzene', image: 'resources/images/32x32/elements/benzene/benzene.png' },
			{ name: 'Furan', image: 'resources/images/32x32/elements/furan/furan.png' },
			{ name: 'Thiophene', image: 'resources/images/32x32/elements/thiophene/thiophene.png' },
			{ name: 'Pyrrole', image: 'resources/images/32x32/elements/pyrrole/pyrrole.png' },
			{ name: 'Pyrazole', image: 'resources/images/32x32/elements/pyrazole/pyrazole.png' },
			{ name: 'Pyrimidine', image: 'resources/images/32x32/elements/pyrimidine/pyrimidine.png' },
			{ name: 'Pyrazine', image: 'resources/images/32x32/elements/pyrazine/pyrazine.png' },
			{ name: 'Cyclopentadien', image: 'resources/images/32x32/elements/cyclopentadien/cyclopentadien.png' },
			{ name: 'Isoxazole', image: 'resources/images/32x32/elements/isoxazole/isoxazole.png' },
			{ name: 'Isothiazole', image: 'resources/images/32x32/elements/isothiazole/isothiazole.png' },
			{ name: 'Oxazole', image: 'resources/images/32x32/elements/oxazole/oxazole.png' },
			{ name: 'Pyridazine', image: 'resources/images/32x32/elements/pyridazine/pyridazine.png' },
			{ name: 'Thiazole', image: 'resources/images/32x32/elements/thiazole/thiazole.png' },
			{ name: 'Pyridine', image: 'resources/images/32x32/elements/pyridine/pyridine.png' }
		],
		polycyclic: [
			{ name: 'Quinoline', image: 'resources/images/32x32/elements/quinoline/quinoline.png' },
			{ name: 'Naphthalene', image: 'resources/images/32x32/elements/naphthalene/naphthalene.png' },
			{ name: 'Indole', image: 'resources/images/32x32/elements/indole/indole.png' },
			{ name: 'Benzimidazole', image: 'resources/images/32x32/elements/benzimidazole/benzimidazole.png' },
			{ name: 'Benzoxazole', image: 'resources/images/32x32/elements/benzoxazole/benzoxazole.png' },
			{ name: 'Benzisoxazole', image: 'resources/images/32x32/elements/benzisoxazole/benzisoxazole.png' },
			{ name: 'Quinazoline', image: 'resources/images/32x32/elements/quinazoline/quinazoline.png' },
			{ name: 'Quinoxaline', image: 'resources/images/32x32/elements/quinoxaline/quinoxaline.png' },
			{ name: 'Indazole', image: 'resources/images/32x32/elements/indazole/indazole.png' }
		]
	};

	const currentFragments = createMemo(() => 
		fragmentsByCategory[activeCategory() as keyof typeof fragmentsByCategory] || []
	);

	return (
		<div class="fragment-selection-tab">
			<div class="section">
				<h3>Fragment Selection</h3>
				<p class="stub-text">Select fragment categories and fragments to add to your molecule.</p>
				
				{/* Category Selection Buttons */}
				<div class="category-buttons">
					<For each={categories}>
						{(category) => (
							<button 
								class={`category-button ${activeCategory() === category.id ? 'active' : ''}`}
								data-tooltip={category.name}
								onClick={() => setActiveCategory(category.id)}
							>
								<ImageFromFS path={category.icon} />
							</button>
						)}
					</For>
				</div>

				{/* Fragment Grid */}
				<div class="fragment-section">
					<span class="section-label">Available Fragments</span>
					<div class="fragment-grid">
						<For each={currentFragments()}>
							{(fragment) => (
								<button 
									class="fragment-item" 
									data-tooltip={fragment.name}
								>
									<ImageFromFS path={fragment.image} />
								</button>
							)}
						</For>
					</div>
				</div>
			</div>
		</div>
	);
};

const AtomSelectionTab = () => {
	return (
		<div class="atom-selection-tab">
			<div class="section">
				<h3>Atom Selection</h3>
				<p class="stub-text">Atom selection and modification functionality will be implemented here.</p>

				<div class="atom-groups">
					<div class="atom-group">
						<span class="group-label">Common Elements</span>
						<div class="atom-grid">
							<button class="atom-button carbon disabled">C</button>
							<button class="atom-button nitrogen disabled">N</button>
							<button class="atom-button oxygen disabled">O</button>
							<button class="atom-button sulfur disabled">S</button>
							<button class="atom-button phosphorus disabled">P</button>
						</div>
					</div>

					<div class="atom-group">
						<span class="group-label">Halogens & Hydrogen</span>
						<div class="atom-grid">
							<button class="atom-button hydrogen disabled">H</button>
							<button class="atom-button fluorine disabled">F</button>
							<button class="atom-button chlorine disabled">Cl</button>
							<button class="atom-button bromine disabled">Br</button>
							<button class="atom-button iodine disabled">I</button>
						</div>
					</div>
				</div>

				<div class="bond-section">
					<span class="group-label">Bond Types</span>
					<div class="bond-grid">
						<button class="bond-button disabled">Single</button>
						<button class="bond-button disabled">Double</button>
						<button class="bond-button disabled">Triple</button>
					</div>
				</div>
			</div>
		</div>
	);
};

export default function SmallMoleculeDesignPanel() {
	const tabs = [
		{ name: 'fragment', label: 'Fragment Selection', children: <FragmentSelectionTab /> },
		{ name: 'atom', label: 'Atom Selection', children: <AtomSelectionTab /> }
	];

	return (
		<div class="small-molecule-design-content">
			<Tabs className="design-tabs" tabs={tabs} />

			<div class="cleanup-section">
				<span class="section-label">Cleanup Structure</span>
				<button class="cleanup-button disabled">Optimize</button>
			</div>
		</div>
	);
}
