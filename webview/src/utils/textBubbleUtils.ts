export type TailCoords = {
	startX: number;
	startY: number;
	endX: number;
	endY: number;
};

export type Point = {
	x: number;
	y: number;
};

/**
 * maps color strings to css variable names for tail fill colors
 */
export const getTailFillColor = (color: string): string => {
	if (color === 'green') return "rgba(155, 205, 75, 0.8)"; // Brightened Foldit green
	if (color === 'purple') return "rgba(126, 18, 148, 0.8)"; // Semi-transparent bright purple
	if (color === 'panel') return "rgba(23, 23, 23, 0.8)"; // Updated to match Panel's bg-neutral-900/80
	if (color === 'tooltip') return "rgba(0, 0, 0, 0.95)"; // Almost completely black
	return "rgba(23, 23, 23, 0.8)"; // Match Panel's bg-neutral-900/80
};

/**
 * gets outline color - opposite of background color
 */
export const getOutlineColor = (color: string): string => {
	if (color === 'tooltip') return "rgba(255, 255, 255, 0.05)"; // Extremely subtle border for tooltips
	return "rgba(255, 255, 255, 0.1)"; // Match Panel's ring-white/10 for all colors
};

/**
 * gets shadow filter - color depends on context
 */
export const getShadowFilter = (color?: string): string => {
	if (color === 'tooltip') {
		// Extremely subtle light shadow for dark tooltips
		return "drop-shadow(0 1px 2px rgba(255, 255, 255, 0.02))";
	}
	// Match Panel's sophisticated shadow system
	return "drop-shadow(0 10px 20px rgba(0, 0, 0, 0.5))";
};

/**
 * calculates target coordinates based on different target types
 * @param target - can be string (element id) or object (backend coordinates)
 * @returns point coordinates or null if target not found
 */
export const calculateTargetCoords = (target: any): Point | null => {
	// string target = GUI component
	if (typeof target === 'string') {
		const element = document.getElementById(target);
		if (!element) return null;

		const rect = element.getBoundingClientRect();
		const offset = target === "viewport"
			? { x: rect.width / 4, y: rect.height / 2 }
			: { x: rect.width / 2, y: rect.height / 2 };

		return { x: rect.left + offset.x, y: rect.top + offset.y };
	}

	// object target with domCoords flag = DOM coordinates (no Y flip needed)
	if (typeof target === 'object' && target.x !== undefined && target.y !== undefined && target.domCoords) {
		return { x: target.x, y: target.y };
	}

	// object target = backend coordinates
	if (typeof target === 'object' && target.x !== undefined && target.y !== undefined) {
		return { x: target.x, y: window.innerHeight - target.y };
	}

	return null;
};
