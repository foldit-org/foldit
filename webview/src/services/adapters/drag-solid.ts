/**
 * SolidJS directive and hook for DragManager
 * 
 * Usage (directive):
 *   <div use:draggable={{ handle: '.header', onDrag: (pos) => setPos(pos) }}>
 *     ...
 *   </div>
 * 
 * Usage (hook):
 *   const { setRef, position } = createDraggable({
 *     handle: '.header',
 *     initialPosition: { x: 100, y: 100 },
 *   });
 *   
 *   return <div ref={setRef}>...</div>;
 */

import { createSignal, onCleanup, Accessor } from 'solid-js';
import { dragManager, DragOptions, Position, DragInstance } from '../DragManager';

export type CreateDraggableOptions = Omit<DragOptions, 'initialPosition'> & {
	initialPosition?: Position;
};

export type CreateDraggableReturn = {
	setRef: (el: HTMLElement) => void;
	position: Accessor<Position>;
	setPosition: (position: Position) => void;
};

/**
 * Creates a draggable instance for use with SolidJS
 */
export function createDraggable(options: CreateDraggableOptions = {}): CreateDraggableReturn {
	const [position, setPositionSignal] = createSignal<Position>(
		options.initialPosition || { x: 0, y: 0 }
	);
	
	let dragInstance: DragInstance | null = null;
	let element: HTMLElement | null = null;

	const setRef = (el: HTMLElement) => {
		// Clean up previous instance
		if (element) {
			dragManager.destroy(element);
		}

		element = el;
		if (!el) return;

		const handleDrag = (pos: Position) => {
			setPositionSignal(pos);
			options.onDrag?.(pos);
		};

		dragInstance = dragManager.create(el, {
			...options,
			initialPosition: options.initialPosition,
			onDragStart: options.onDragStart,
			onDrag: handleDrag,
			onDragEnd: options.onDragEnd,
		});

		onCleanup(() => {
			if (element) {
				dragManager.destroy(element);
			}
			dragInstance = null;
			element = null;
		});
	};

	const setPosition = (newPosition: Position) => {
		setPositionSignal(newPosition);
		dragInstance?.setPosition(newPosition);
	};

	return { setRef, position, setPosition };
}

/**
 * SolidJS directive for making elements draggable
 * 
 * Usage:
 *   <div use:draggable={{ handle: '.header' }}>...</div>
 */
export function draggable(el: HTMLElement, options: Accessor<DragOptions>) {
	const opts = options();
	
	const instance = dragManager.create(el, opts);

	onCleanup(() => {
		instance.destroy();
	});
}

// Type declaration for the directive
declare module 'solid-js' {
	namespace JSX {
		interface Directives {
			draggable: DragOptions;
		}
	}
}
