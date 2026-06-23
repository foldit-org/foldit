import { TailCoords } from './textBubbleUtils';

/* utility class for generating unified SVG paths with integrated tails */
export class TailPathGenerator {
	static createSimpleRectanglePath(
		width: number,
		height: number,
		offsetX: number,
		offsetY: number,
		radius: number
	): string {
		let path = `M ${radius + offsetX} ${offsetY}`;
		path += `L ${width - radius + offsetX} ${offsetY}`;
		path += `Q ${width + offsetX} ${offsetY} ${width + offsetX} ${radius + offsetY}`;
		path += `L ${width + offsetX} ${height - radius + offsetY}`;
		path += `Q ${width + offsetX} ${height + offsetY} ${width - radius + offsetX} ${height + offsetY}`;
		path += `L ${radius + offsetX} ${height + offsetY}`;
		path += `Q ${offsetX} ${height + offsetY} ${offsetX} ${height - radius + offsetY}`;
		path += `L ${offsetX} ${radius + offsetY}`;
		path += `Q ${offsetX} ${offsetY} ${radius + offsetX} ${offsetY} Z`;
		return path;
	}

	static calculateAttachmentPoint(
		width: number,
		height: number,
		offsetX: number,
		offsetY: number,
		relativeTargetX: number,
		relativeTargetY: number,
		cornerPadding: number
	): { side: 'top' | 'right' | 'bottom' | 'left', x: number, y: number } {
		const bubbleCenterX = (width / 2) + offsetX;
		const bubbleCenterY = (height / 2) + offsetY;
		const dx = relativeTargetX - bubbleCenterX;
		const dy = relativeTargetY - bubbleCenterY;

		const length = Math.sqrt(dx * dx + dy * dy);
		if (length === 0) return { side: 'top', x: bubbleCenterX, y: offsetY };

		const dirX = dx / length;
		const dirY = dy / length;

		const intersections = [];

		// top edge (y = offsetY)
		if (dirY < 0) {
			const t = (offsetY - bubbleCenterY) / dirY;
			const x = bubbleCenterX + t * dirX;
			if (x >= offsetX && x <= width + offsetX) {
				intersections.push({ side: 'top', distance: t, x, y: offsetY });
			}
		}

		// bottom edge (y = height + offsetY)
		if (dirY > 0) {
			const t = (height + offsetY - bubbleCenterY) / dirY;
			const x = bubbleCenterX + t * dirX;
			if (x >= offsetX && x <= width + offsetX) {
				intersections.push({ side: 'bottom', distance: t, x, y: height + offsetY });
			}
		}

		// left edge (x = offsetX)
		if (dirX < 0) {
			const t = (offsetX - bubbleCenterX) / dirX;
			const y = bubbleCenterY + t * dirY;
			if (y >= offsetY && y <= height + offsetY) {
				intersections.push({ side: 'left', distance: t, x: offsetX, y });
			}
		}

		// right edge (x = width + offsetX)
		if (dirX > 0) {
			const t = (width + offsetX - bubbleCenterX) / dirX;
			const y = bubbleCenterY + t * dirY;
			if (y >= offsetY && y <= height + offsetY) {
				intersections.push({ side: 'right', distance: t, x: width + offsetX, y });
			}
		}

		// find closest intersection
		let attachSide: 'top' | 'right' | 'bottom' | 'left';
		let attachX: number, attachY: number;

		if (intersections.length > 0) {
			const closest = intersections.reduce((min, curr) =>
				curr.distance < min.distance ? curr : min
			);
			attachSide = closest.side as 'top' | 'right' | 'bottom' | 'left';
			attachX = closest.x;
			attachY = closest.y;
		} else {
			attachSide = Math.abs(dx) > Math.abs(dy) ? (dx > 0 ? 'right' : 'left') : (dy > 0 ? 'bottom' : 'top');
			attachX = attachSide === 'left' ? offsetX : attachSide === 'right' ? width + offsetX : bubbleCenterX;
			attachY = attachSide === 'top' ? offsetY : attachSide === 'bottom' ? height + offsetY : bubbleCenterY;
		}

		// corner padding to keep tail away from corners
		if (attachSide === 'top' || attachSide === 'bottom') {
			attachX = Math.max(offsetX + cornerPadding, Math.min(width + offsetX - cornerPadding, attachX));
		} else {
			attachY = Math.max(offsetY + cornerPadding, Math.min(height + offsetY - cornerPadding, attachY));
		}

		return { side: attachSide, x: attachX, y: attachY };
	}

	/* checks if target coordinates are inside the bubble bounds (don't want tail in this case) */
	static isTargetInsideBubble(
		relativeTargetX: number,
		relativeTargetY: number,
		width: number,
		height: number,
		offsetX: number,
		offsetY: number
	): boolean {
		return relativeTargetX >= offsetX && relativeTargetX <= width + offsetX &&
			relativeTargetY >= offsetY && relativeTargetY <= height + offsetY;
	}

	/* build unified SVG path with (tail + rectangle) */
	static buildUnifiedPathWithTail(
		width: number,
		height: number,
		offsetX: number,
		offsetY: number,
		radius: number,
		attachSide: 'top' | 'right' | 'bottom' | 'left',
		attachX: number,
		attachY: number,
		relativeTargetX: number,
		relativeTargetY: number,
		tailWidth: number
	): string {
		let path = `M ${radius + offsetX} ${offsetY}`;

		if (attachSide === 'top') {
			path += `L ${attachX - tailWidth} ${offsetY}`;
			path += `L ${relativeTargetX} ${relativeTargetY}`;
			path += `L ${attachX + tailWidth} ${offsetY}`;
		}
		path += `L ${width - radius + offsetX} ${offsetY}`;
		path += `Q ${width + offsetX} ${offsetY} ${width + offsetX} ${radius + offsetY}`;

		if (attachSide === 'right') {
			path += `L ${width + offsetX} ${attachY - tailWidth}`;
			path += `L ${relativeTargetX} ${relativeTargetY}`;
			path += `L ${width + offsetX} ${attachY + tailWidth}`;
		}
		path += `L ${width + offsetX} ${height - radius + offsetY}`;
		path += `Q ${width + offsetX} ${height + offsetY} ${width - radius + offsetX} ${height + offsetY}`;

		if (attachSide === 'bottom') {
			path += `L ${attachX + tailWidth} ${height + offsetY}`;
			path += `L ${relativeTargetX} ${relativeTargetY}`;
			path += `L ${attachX - tailWidth} ${height + offsetY}`;
		}
		path += `L ${radius + offsetX} ${height + offsetY}`;
		path += `Q ${offsetX} ${height + offsetY} ${offsetX} ${height - radius + offsetY}`;

		if (attachSide === 'left') {
			path += `L ${offsetX} ${attachY + tailWidth}`;
			path += `L ${relativeTargetX} ${relativeTargetY}`;
			path += `L ${offsetX} ${attachY - tailWidth}`;
		}
		path += `L ${offsetX} ${radius + offsetY}`;
		path += `Q ${offsetX} ${offsetY} ${radius + offsetX} ${offsetY} Z`;

		return path;
	}

	// main entry point
	static generateUnifiedPath(
		containerBounds: DOMRect,
		tailCoords: TailCoords | null,
		showTail: boolean,
		config: {
			offsetX?: number;
			offsetY?: number;
			radius?: number;
			tailWidth?: number;
			cornerPadding?: number;
		} = {}
	): string {
		const { width, height } = containerBounds;
		const {
			offsetX = 2000,
			offsetY = 2000,
			radius = 8,
			tailWidth = 10,
			cornerPadding = 15
		} = config;

		if (!showTail || !tailCoords) {
			return this.createSimpleRectanglePath(width, height, offsetX, offsetY, radius);
		}

		const { startX, startY } = tailCoords;
		const relativeTargetX = (startX - containerBounds.x) + offsetX;
		const relativeTargetY = (startY - containerBounds.y) + offsetY;

		if (this.isTargetInsideBubble(relativeTargetX, relativeTargetY, width, height, offsetX, offsetY)) {
			return this.createSimpleRectanglePath(width, height, offsetX, offsetY, radius);
		}

		const attachment = this.calculateAttachmentPoint(
			width, height, offsetX, offsetY,
			relativeTargetX, relativeTargetY, cornerPadding
		);

		return this.buildUnifiedPathWithTail(
			width, height, offsetX, offsetY, radius,
			attachment.side, attachment.x, attachment.y,
			relativeTargetX, relativeTargetY, tailWidth
		);
	}
}
