export type RamaPoint = {
	seqpos: number;
	phi: number;
	psi: number;
	aa_index: number;
	aa_name: string;
};

export type RamaColorGrid = number[][][];

export type BlueprintRes = {
	aa: string;
	secstruct: string;
	color: number[];
};

export type BlueprintBlock = {
	seqpos: number;
	length: number;
};

export type BlueprintData = {
	sequence: BlueprintRes[];
	blocks: BlueprintBlock[];
};

export type BuildingBlock = {
	abego: string;
	secstruct: string;
	length: number;
};

export type AlignmentRes = {
	position: number;
	residue: string;
	color: number[];
	is_matched: boolean;
	is_selected: boolean;
	is_highlighted: boolean;
};

export type AlignmentTemplate = {
	name: string;
	sequence: AlignmentRes[];
};

export type AlignmentData = {
	sequence: AlignmentRes[];
	templates: AlignmentTemplate[];
	thread_locked: boolean;
	thread_running: boolean;
	color_mode: number;
	primary_template: string;
	selection_start: number;
	selection_end: number;
	highlight_index: number;
};

export type BuildingBlockGallery = {
	blocks: BuildingBlock[];
};

export type RotamerInfo = {
	rotamer_index: number;
	chi_angles: number[];
	chi_string: string;
	preference: number;
};

export type RotamerResidue = {
	residue_index: number;
	residue_name: string;
	rotamers: RotamerInfo[];
};

export type RotamerData = {
	residues: RotamerResidue[];
};

export type ElectronDensityData = {
	radius: number;
	threshold: number;
	alpha: number;
	visualization: string;
	backfaceCulling: boolean;
	trim: number;
	color: {
		red: number;
		green: number;
		blue: number;
	};
	radiusDisplay: string;
	thresholdDisplay: string;
	selectionEnabled: boolean;
};

export type UndoHistoryNode = {
	index: number;
	score: number;
	identifier: number;
};

export type UndoHistoryData = {
	history: UndoHistoryNode[];
	currentIndex: number;
};
