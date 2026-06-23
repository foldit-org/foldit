import { onMount, onCleanup } from 'solid-js';
import { viewportInput } from '../adapter';

/**
 * Unified viewport event forwarding to Rust backend via Tauri IPC.
 *
 * Captures mouse, keyboard, and scroll events on the window, detects whether
 * they fall on a GUI element, and if not, forwards them as ViewportInput
 * commands to the Rust render engine.
 */
export function useInputForwarding() {
	// Whether the current mouse-down started on a GUI element.
	// Once set on mousedown, preserved until mouseup — dragging across
	// GUI elements won't interrupt viewport capture.
	let guiCaptured = false;
	let mouseButtonDown = false;

	let lastMouseMoveCall = 0;
	let lastWheelCall = 0;
	const MOUSE_THROTTLE_MS = 16; // ~60fps
	// Previous forwarded pointer position; drives the relative dx/dy in
	// PointerMove, so it must hold the *last forwarded* point, not the
	// live cursor. Updated only on the forwarding (non-GUI) paths.
	let lastMouseX = 0;
	let lastMouseY = 0;
	// Live cursor position, updated on every mousemove regardless of where
	// the cursor is. The key gate reads this to decide whether the cursor
	// is currently over a GUI element.
	let cursorX = 0;
	let cursorY = 0;

	onMount(() => {
		const isOverGuiElement = (x: number, y: number): boolean => {
			const elementsAtPoint = document.elementsFromPoint(x, y);

			for (const element of elementsAtPoint) {
				const el = element as HTMLElement;
				const classList = el.classList;
				const className = typeof el.className === 'string' ? el.className : '';
				const id = el.id || '';

				if (
					classList.contains('panel-container') ||
					classList.contains('panel-container-visible') ||
					classList.contains('pointer-events-auto') ||
					className.includes('-widget') ||
					className.includes('widget-') ||
					className.includes('popup') ||
					className.includes('menu') ||
					className.includes('-bar') ||
					id.includes('-bar') ||
					el.tagName === 'BUTTON' ||
					el.tagName === 'INPUT' ||
					el.tagName === 'SELECT' ||
					el.tagName === 'TEXTAREA' ||
					el.tagName === 'A' ||
					el.tagName === 'svg' ||
					el.tagName === 'SVG'
				) {
					return true;
				}
			}

			return false;
		};

		const handleMouseDown = (e: MouseEvent) => {
			// Prevent native drag (images) and text selection on mousedown.
			// This is critical on macOS where WKWebView allows native image drag
			// and selection even with user-select:none, which swallows click/move events.
			// Skip for form inputs so they can still receive focus.
			const tag = (e.target as HTMLElement)?.tagName;
			if (tag !== 'INPUT' && tag !== 'TEXTAREA' && tag !== 'SELECT') {
				e.preventDefault();
			}

			mouseButtonDown = true;
			guiCaptured = isOverGuiElement(e.clientX, e.clientY);

			if (!guiCaptured) {
				lastMouseX = e.clientX;
				lastMouseY = e.clientY;

				// Convert CSS pixels to physical pixels for the render engine
				const dpr = window.devicePixelRatio || 1;
				viewportInput({
					kind: 'PointerDown',
					x: e.clientX * dpr,
					y: e.clientY * dpr,
					button: e.button,
					shift: e.shiftKey,
					ctrl: e.ctrlKey || e.metaKey,
					alt: e.altKey,
				});
			}
		};

		const handleMouseUp = (e: MouseEvent) => {
			if (!guiCaptured) {
				const dpr = window.devicePixelRatio || 1;
				viewportInput({
					kind: 'PointerUp',
					x: e.clientX * dpr,
					y: e.clientY * dpr,
					button: e.button,
					shift: e.shiftKey,
					ctrl: e.ctrlKey || e.metaKey,
					alt: e.altKey,
				});
			}
			mouseButtonDown = false;
			guiCaptured = false;
		};

		const handleMouseMove = (e: MouseEvent) => {
			// Track the live cursor unconditionally so the key gate always
			// has the current position, even while the cursor is over GUI.
			cursorX = e.clientX;
			cursorY = e.clientY;

			// During a drag, preserve the capture decision from mousedown.
			// Only re-evaluate when no button is pressed (hover).
			const overGui = mouseButtonDown ? guiCaptured : isOverGuiElement(e.clientX, e.clientY);

			if (!overGui) {
				const now = Date.now();
				if (now - lastMouseMoveCall > MOUSE_THROTTLE_MS) {
					lastMouseMoveCall = now;

					const dx = e.clientX - lastMouseX;
					const dy = e.clientY - lastMouseY;
					lastMouseX = e.clientX;
					lastMouseY = e.clientY;

					// Convert CSS pixels to physical pixels for the render engine
					const dpr = window.devicePixelRatio || 1;
					viewportInput({
						kind: 'PointerMove',
						x: e.clientX * dpr,
						y: e.clientY * dpr,
						dx: dx * dpr,
						dy: dy * dpr,
						shift: e.shiftKey,
						ctrl: e.ctrlKey || e.metaKey,
						alt: e.altKey,
					});
				}
			}
		};

		const handleWheel = (e: WheelEvent) => {
			if (isOverGuiElement(e.clientX, e.clientY)) return;

			const now = Date.now();
			if (now - lastWheelCall > MOUSE_THROTTLE_MS) {
				lastWheelCall = now;

				const delta = e.deltaY > 0 ? -1 : e.deltaY < 0 ? 1 : 0;

				viewportInput({
					kind: 'Scroll',
					delta,
				});
			}
		};

		const handleKeyDown = (e: KeyboardEvent) => {
			// Gate on cursor hover, mirroring mouse/wheel: when the cursor is
			// over a GUI element, the focused control / browser handles the
			// key (typing into inputs, etc) and we do NOT forward to the
			// engine. Only forward view hotkeys when the cursor is over the
			// 3D view. This decouples key routing from document.activeElement,
			// so a panel control retaining focus no longer swallows hotkeys.
			if (isOverGuiElement(cursorX, cursorY)) return;

			// Action hotkeys (W/S/P/M/D etc) used to map to a fixed
			// integer action-id table. With the catalog-driven dispatch
			// path, that table no longer exists -- per-plugin hotkey
			// declarations need to ride on the manifest. Until then,
			// keys flow straight to the engine.

			// Forward all other keys to Rust backend
			viewportInput({
				kind: 'Key',
				code: e.code,
				pressed: true,
			});

			e.preventDefault();
		};

		const handleKeyUp = (e: KeyboardEvent) => {
			// Forward key-UP unconditionally (not hover-gated). A key pressed
			// over the view and released after the cursor moved over a panel
			// would otherwise be dropped by a hover gate, leaving the engine
			// thinking the key is still held. The engine ignores a release
			// for a key it never saw pressed (its Key arm only acts when
			// `pressed`), so an unmatched release is a safe no-op. Only
			// preventDefault over the view, so the browser still sees key-up
			// for typing in inputs when the cursor is over GUI.
			viewportInput({
				kind: 'Key',
				code: e.code,
				pressed: false,
			});

			if (!isOverGuiElement(cursorX, cursorY)) {
				e.preventDefault();
			}
		};

		const handleContextMenu = (e: MouseEvent) => {
			e.preventDefault();
			return false;
		};

		// Resize is handled natively by WindowEvent::Resized (physical pixels).
		// No need to forward from JS (which only knows CSS pixels).

		window.addEventListener('mousedown', handleMouseDown);
		window.addEventListener('mouseup', handleMouseUp);
		window.addEventListener('mousemove', handleMouseMove);
		window.addEventListener('wheel', handleWheel, { passive: false });
		window.addEventListener('keydown', handleKeyDown);
		window.addEventListener('keyup', handleKeyUp);
		window.addEventListener('contextmenu', handleContextMenu);

		onCleanup(() => {
			window.removeEventListener('mousedown', handleMouseDown);
			window.removeEventListener('mouseup', handleMouseUp);
			window.removeEventListener('mousemove', handleMouseMove);
			window.removeEventListener('wheel', handleWheel);
			window.removeEventListener('keydown', handleKeyDown);
			window.removeEventListener('keyup', handleKeyUp);
			window.removeEventListener('contextmenu', handleContextMenu);
		});
	});
}
