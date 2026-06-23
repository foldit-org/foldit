import { createSignal, createMemo, onMount, onCleanup, Show } from "solid-js";
import { CalloutBubble } from "../util/CalloutBubble";
import "../../styles/viewport/Tooltip.css";

const TOOLTIP_DELAY = 1000;
const MOUSE_MOVE_THRESHOLD = 5;

export default function GlobalTooltipSystem() {
	const [tooltip, setTooltip] = createSignal<{
		content: string;
		target: HTMLElement | null;
		bounds: DOMRect | null;
		mouseX: number;
		mouseY: number;
	} | null>(null);

	let timeoutRef: number | null = null;
	let lastMousePos = { x: 0, y: 0 };
	let tooltipRef: HTMLDivElement | undefined;

	const tailCoords = createMemo(() => {
		const currentTooltip = tooltip();
		if (!currentTooltip?.bounds) return null;

		return {
			startX: currentTooltip.bounds.left + currentTooltip.bounds.width / 2,
			startY: currentTooltip.bounds.top + currentTooltip.bounds.height / 2,
		};
	});

	const showTail = createMemo(() => !!tooltip()?.target);

	const findTooltipTarget = (el: HTMLElement | null) => {
		while (el && el !== document.body) {
			const content = el.getAttribute("data-tooltip");

			if (content && content.trim()) return { content, target: el };
			el = el.parentElement;
		}

		return null;
	};

	onMount(() => {
		const handleMouseMove = (e: MouseEvent) => {
			const dx = Math.abs(e.clientX - lastMousePos.x);
			const dy = Math.abs(e.clientY - lastMousePos.y);
			if (dx < MOUSE_MOVE_THRESHOLD && dy < MOUSE_MOVE_THRESHOLD) return;

			lastMousePos = { x: e.clientX, y: e.clientY };

			if (timeoutRef) {
				clearTimeout(timeoutRef);
				timeoutRef = null;
			}
			setTooltip(null);

			const hovered = document.elementFromPoint(e.clientX, e.clientY) as HTMLElement | null;
			const data = findTooltipTarget(hovered);

			timeoutRef = window.setTimeout(() => {
				if (data) {
					setTooltip({
						content: data.content,
						target: data.target,
						bounds: data.target.getBoundingClientRect(),
						mouseX: e.clientX,
						mouseY: e.clientY
					});
				}
			}, TOOLTIP_DELAY);
		};

		const handleMouseLeave = () => {
			if (timeoutRef) clearTimeout(timeoutRef);
			setTooltip(null);
		};

		document.addEventListener("mousemove", handleMouseMove);
		document.addEventListener("mouseleave", handleMouseLeave);

		onCleanup(() => {
			document.removeEventListener("mousemove", handleMouseMove);
			document.removeEventListener("mouseleave", handleMouseLeave);
			if (timeoutRef) clearTimeout(timeoutRef);
		});
	});


	// `Show` keeps this reactive: the component body runs once, so a
	// bare `tooltip()` read + early return would latch on the initial
	// `null` and never re-render. Keyed so the body re-runs whenever
	// the tooltip object changes identity (new hover target), and
	// unmounts when the signal goes back to null.
	return (
		<Show when={tooltip()} keyed>
			{(currentTooltip) => (
				<div
					style={{
						// `fixed` so left/top are viewport-relative,
						// matching getBoundingClientRect's frame.
						position: "fixed",
						// Sits directly above the item: left edges
						// aligned, lifted clear of its top (8px gap)
						// via the -100% Y translate.
						left: currentTooltip.bounds ? `${currentTooltip.bounds.left}px` : "0px",
						top: currentTooltip.bounds ? `${currentTooltip.bounds.top}px` : "0px",
						transform: "translateY(calc(-100% - 8px))",
						// Above the sidebar (z-20) so button tooltips
						// aren't occluded, but below popup menus
						// (`.popup-menu` is z-50, e.g. the puzzle
						// menu) so a tooltip never covers a modal.
						"z-index": 30
					}}
				>
					<CalloutBubble
						color="tooltip"
						tailCoords={tailCoords()}
						showTail={showTail()}
						className="tooltip pointer-events-none"
						tailWidth={6}
						style={{
							// Override `.tooltip`'s `position:absolute`
							// (from @apply absolute): absolute takes the
							// bubble out of the fixed wrapper's flow, so
							// the wrapper collapses to 0 height and its
							// `translateY(-100%)` no longer lifts the
							// tooltip a full bubble-height above the item.
							position: 'relative',
							"border-radius": '6px',
							"font-size": '14px',
							"max-width": '20rem',
							"font-family": "'Noto Sans', sans-serif"
						}}
					>
						<div ref={tooltipRef} class="tooltip-content">
							{currentTooltip.content}
						</div>
					</CalloutBubble>
				</div>
			)}
		</Show>
	);
}


