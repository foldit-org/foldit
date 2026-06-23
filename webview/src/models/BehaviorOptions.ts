export enum WigglePower {
	LOW = 0,
	MEDIUM = 1,
	HIGH = 2,
	AUTO = 3,
}

export interface BehaviorOptions {
	clashing: number;
	sidechain: number;
	backbone: number;
	wigglePower: WigglePower;
	enableCutBands: boolean;
}

export type ViewOptions = {
	"view_options/sidechain_mode"?: string;
	"view_options/render_style"?: string;
	"view_options/show_hydrogens"?: string;

	"view_options/show_outlines"?: boolean;
	"view_options/light_background"?: boolean;
	"view_options/working_pulse_style"?: number;
	"view_options/show_grid"?: boolean;
	"view_options/orthographic"?: boolean;

	"view_options/show_voids"?: boolean;
	"view_options/show_exposed"?: boolean;
	"view_options/show_clashes"?: boolean;
	"view_options/show_backbone_issues"?: boolean;
	"view_options/show_sidechains_with_issues"?: boolean;
	"view_options/show_bondable_atoms"?: boolean;

	"view_options/show_hbonds"?: boolean;
	"view_options/show_helix_hbonds"?: boolean;
	"view_options/show_other_hbonds"?: boolean;
	"view_options/show_sidechain_hbonds"?: boolean;
	"view_options/show_non_protein_hbonds"?: boolean;

	"view_options/sym_chain_visible"?: boolean;
	"view_options/sym_chain_colors"?: boolean;
};
