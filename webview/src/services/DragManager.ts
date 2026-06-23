/**
 * DragManager - Framework-agnostic drag functionality
 * 
 * Replaces react-draggable with a simple, vanilla JS implementation.
 * 
 * Usage:
 *   const drag = dragManager.create(element, {
 *     handle: '.header',           // CSS selector for drag handle (optional)
 *     cancel: '.buttons',          // CSS selector for elements that shouldn't trigger drag (optional)
 *     initialPosition: { x, y },   // Starting position (optional)
 *     onDragStart: (pos) => {},    // Callback when drag starts (optional)
 *     onDrag: (pos) => {},         // Callback during drag (optional)
 *     onDragEnd: (pos) => {},      // Callback when drag ends (optional)
 *     bounds: 'viewport',          // Constrain to viewport (optional) - 'viewport' | 'parent' | null
 *   });
 *   
 *   drag.setPosition({ x: 100, y: 200 });  // Programmatically set position
 *   drag.getPosition();                     // Get current position
 *   drag.destroy();                         // Clean up listeners
 */

export type Position = { x: number; y: number };

export type DragOptions = {
	handle?: string;
	cancel?: string;
	initialPosition?: Position;
	onDragStart?: (position: Position) => void;
	onDrag?: (position: Position) => void;
	onDragEnd?: (position: Position) => void;
	bounds?: 'viewport' | 'parent' | null;
};

export type DragInstance = {
	setPosition: (position: Position) => void;
	getPosition: () => Position;
	destroy: () => void;
};

class DragController implements DragInstance {
	private element: HTMLElement;
	private options: DragOptions;
	private position: Position;
	private isDragging = false;
	private dragOffset: Position = { x: 0, y: 0 };

	// Bound handlers for cleanup
	private handleMouseDown: (e: MouseEvent) => void;
	private handleMouseMove: (e: MouseEvent) => void;
	private handleMouseUp: (e: MouseEvent) => void;
	private handleTouchStart: (e: TouchEvent) => void;
	private handleTouchMove: (e: TouchEvent) => void;
	private handleTouchEnd: (e: TouchEvent) => void;

	constructor(element: HTMLElement, options: DragOptions = {}) {
		this.element = element;
		this.options = options;
		this.position = options.initialPosition || { x: 0, y: 0 };

		// Apply initial position
		this.applyPosition();

		// Ensure element is positioned
		const computedStyle = window.getComputedStyle(element);
		if (computedStyle.position === 'static') {
			element.style.position = 'absolute';
		}

		// Bind handlers
		this.handleMouseDown = this.onMouseDown.bind(this);
		this.handleMouseMove = this.onMouseMove.bind(this);
		this.handleMouseUp = this.onMouseUp.bind(this);
		this.handleTouchStart = this.onTouchStart.bind(this);
		this.handleTouchMove = this.onTouchMove.bind(this);
		this.handleTouchEnd = this.onTouchEnd.bind(this);

		// Always attach to the element itself; onMouseDown/onTouchStart
		// check whether the event originated inside the handle selector.
		this.element.addEventListener('mousedown', this.handleMouseDown);
		this.element.addEventListener('touchstart', this.handleTouchStart, { passive: false });
	}

	/** Check if `target` is inside the configured handle selector. */
	private isInHandle(target: EventTarget | null): boolean {
		if (!this.options.handle) return true; // no handle = whole element
		if (!target) return false;
		const handle = this.element.querySelector(this.options.handle);
		return !!handle && handle.contains(target as Node);
	}

	private shouldCancel(target: EventTarget | null): boolean {
		if (!this.options.cancel || !target) return false;

		const cancelElements = this.element.querySelectorAll(this.options.cancel);
		for (const cancelEl of cancelElements) {
			if (cancelEl.contains(target as Node)) return true;
		}
		return false;
	}

	private onMouseDown(e: MouseEvent) {
		if (e.button !== 0) return; // Only left click
		if (!this.isInHandle(e.target)) return;
		if (this.shouldCancel(e.target)) return;

		this.startDrag(e.clientX, e.clientY);

		document.addEventListener('mousemove', this.handleMouseMove);
		document.addEventListener('mouseup', this.handleMouseUp);

		e.preventDefault();
	}

	private onMouseMove(e: MouseEvent) {
		if (!this.isDragging) return;
		this.updateDrag(e.clientX, e.clientY);
	}

	private onMouseUp(_e: MouseEvent) {
		this.endDrag();
		document.removeEventListener('mousemove', this.handleMouseMove);
		document.removeEventListener('mouseup', this.handleMouseUp);
	}

	private onTouchStart(e: TouchEvent) {
		if (e.touches.length !== 1) return;
		if (!this.isInHandle(e.target)) return;
		if (this.shouldCancel(e.target)) return;

		const touch = e.touches[0];
		this.startDrag(touch.clientX, touch.clientY);

		document.addEventListener('touchmove', this.handleTouchMove, { passive: false });
		document.addEventListener('touchend', this.handleTouchEnd);

		e.preventDefault();
	}

	private onTouchMove(e: TouchEvent) {
		if (!this.isDragging || e.touches.length !== 1) return;
		const touch = e.touches[0];
		this.updateDrag(touch.clientX, touch.clientY);
		e.preventDefault();
	}

	private onTouchEnd(_e: TouchEvent) {
		this.endDrag();
		document.removeEventListener('touchmove', this.handleTouchMove);
		document.removeEventListener('touchend', this.handleTouchEnd);
	}

	private startDrag(clientX: number, clientY: number) {
		this.isDragging = true;
		this.dragOffset = {
			x: clientX - this.position.x,
			y: clientY - this.position.y,
		};

		this.element.style.userSelect = 'none';
		this.options.onDragStart?.(this.position);
	}

	private updateDrag(clientX: number, clientY: number) {
		let newX = clientX - this.dragOffset.x;
		let newY = clientY - this.dragOffset.y;

		// Apply bounds
		if (this.options.bounds === 'viewport') {
			const rect = this.element.getBoundingClientRect();
			const maxX = window.innerWidth - rect.width;
			const maxY = window.innerHeight - rect.height;
			newX = Math.max(0, Math.min(newX, maxX));
			newY = Math.max(0, Math.min(newY, maxY));
		} else if (this.options.bounds === 'parent' && this.element.parentElement) {
			const parentRect = this.element.parentElement.getBoundingClientRect();
			const rect = this.element.getBoundingClientRect();
			const maxX = parentRect.width - rect.width;
			const maxY = parentRect.height - rect.height;
			newX = Math.max(0, Math.min(newX, maxX));
			newY = Math.max(0, Math.min(newY, maxY));
		}

		this.position = { x: newX, y: newY };
		this.applyPosition();
		this.options.onDrag?.(this.position);
	}

	private endDrag() {
		if (!this.isDragging) return;
		
		this.isDragging = false;
		this.element.style.userSelect = '';
		this.options.onDragEnd?.(this.position);
	}

	private applyPosition() {
		this.element.style.transform = `translate(${this.position.x}px, ${this.position.y}px)`;
	}

	setPosition(position: Position) {
		this.position = position;
		this.applyPosition();
	}

	getPosition(): Position {
		return { ...this.position };
	}

	destroy() {
		this.element.removeEventListener('mousedown', this.handleMouseDown);
		this.element.removeEventListener('touchstart', this.handleTouchStart);
		document.removeEventListener('mousemove', this.handleMouseMove);
		document.removeEventListener('mouseup', this.handleMouseUp);
		document.removeEventListener('touchmove', this.handleTouchMove);
		document.removeEventListener('touchend', this.handleTouchEnd);
	}
}

/**
 * DragManager singleton - creates and manages drag instances
 */
class DragManager {
	private static instance: DragManager | null = null;
	private instances = new WeakMap<HTMLElement, DragController>();

	static getInstance(): DragManager {
		if (!DragManager.instance) {
			DragManager.instance = new DragManager();
		}
		return DragManager.instance;
	}

	/**
	 * Create a draggable instance for an element
	 */
	create(element: HTMLElement, options: DragOptions = {}): DragInstance {
		// Clean up existing instance if any
		this.destroy(element);

		const controller = new DragController(element, options);
		this.instances.set(element, controller);
		return controller;
	}

	/**
	 * Destroy drag instance for an element
	 */
	destroy(element: HTMLElement) {
		const existing = this.instances.get(element);
		if (existing) {
			existing.destroy();
			this.instances.delete(element);
		}
	}

	/**
	 * Get existing drag instance for an element
	 */
	get(element: HTMLElement): DragInstance | undefined {
		return this.instances.get(element);
	}
}

export const dragManager = DragManager.getInstance();
