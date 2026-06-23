export type TextBubbleButton = {
	text: string;
	goto?: number | null;
};

export type TextBubbleType = {
	text: string;
	buttons: TextBubbleButton[];
	color?: string | null;
	image?: string | null;
	trigger_event?: number;
	target?: number | string | object;
	index?: number;
};

export type RisingTextMessage = {
	id: string;
	message: string;
	x: number;
	y: number;
	color: string;
	duration?: number;
	risePixels?: number;
	displayMode: 'mouse' | 'small' | 'huge';
	stackOffset?: number;
};
