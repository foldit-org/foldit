import { createEffect, createMemo, createSignal, For, Show } from "solid-js";
import { X } from "../../utils/iconMapping";
import { useBackendData } from "../../services/adapters";
import { state, closeSegment, segmentTailTip, appCommand } from "../../adapter";
import { CalloutBubble } from "../util/CalloutBubble";
import { TailCoords } from "../../utils/textBubbleUtils";
import "../../styles/util/Panel.css";

// Backend key for this panel's resting position in `state.panels.positions`.
const SEGMENT_PANEL_ID = "segment";

const PANEL_WIDTH = 240;
// Offset of the body's top-left from the anchor, placing it to the
// top-right of the targeted residue.
const BODY_OFFSET = { x: 24, y: -PANEL_WIDTH };

/**
 * Per-residue segment-info panel. Visibility is backend-driven: it renders
 * iff `segment_info` is present (Tab-toggle and stale-target auto-close null
 * it out on the backend, hiding the panel). The body is placed to the
 * top-right of the open-time anchor, then stays fixed and draggable; only
 * the tail tip tracks the residue live via the `__onTailUpdate` channel.
 */
function SegmentInfoPanel() {
	const segmentInfo = useBackendData(state => state.segmentInfo);

	// The resting body position is backend-owned (`state.panels.positions`),
	// so it survives reloads and can be driven from either side. The live
	// drag gesture stays ephemeral here: `dragDelta` gives instant local
	// feedback during a drag and is committed to the backend on release.
	const backendPos = createMemo(() => {
		const entry = state.panels?.positions?.find(p => p.panel === SEGMENT_PANEL_ID);
		if (!entry || entry.x == null || entry.y == null) return null;
		return { x: entry.x, y: entry.y };
	});

	const [dragDelta, setDragDelta] = createSignal({ x: 0, y: 0 });
	// The optimistic local resting position is what we render. It seeds from
	// the open-time anchor, is updated immediately on every local commit
	// (drag release), and is reconciled below to absorb authoritative backend
	// changes. Rendering from this rather than `backendPos` avoids a one-frame
	// snap-back when a commit's `SetPanelPosition` round-trip lags behind the
	// local update.
	const [openBody, setOpenBody] = createSignal<{ x: number; y: number } | null>(null);

	// Adopt the authoritative backend position whenever it actually changes.
	// On our own commits the backend echoes the value we already set, so this
	// is a no-op; it only moves the panel for genuine external changes. The
	// effect tracks `backendPos` alone (`openBody` is written but not read
	// here), so writing `openBody` cannot re-trigger it.
	createEffect(() => {
		const bp = backendPos();
		if (bp) setOpenBody(bp);
	});

	let dragOrigin: { px: number; py: number; dx: number; dy: number } | null = null;

	// On open, derive the body's anchored position from the segment's
	// anchor (CA screen position) and reset any prior drag. A null anchor
	// (atom off-screen at open) falls back to viewport-centre placement.
	// The derived position is also committed to the backend as the resting
	// position for this open/re-target.
	createMemo(() => {
		const info = segmentInfo();
		if (!info) {
			setOpenBody(null);
			return;
		}
		// The anchor comes from the backend in physical pixels (the cursor
		// path forwards input scaled by devicePixelRatio), so divide it back
		// to CSS pixels for DOM placement. The viewport-centre fallbacks are
		// already CSS pixels and are not scaled.
		const dpr = window.devicePixelRatio || 1;
		const anchor = info.anchor;
		const ax = anchor?.[0] != null ? anchor[0] / dpr : window.innerWidth / 2;
		const ay = anchor?.[1] != null ? anchor[1] / dpr : window.innerHeight / 2;
		const x = ax + BODY_OFFSET.x;
		const y = ay + BODY_OFFSET.y;
		setOpenBody({ x, y });
		setDragDelta({ x: 0, y: 0 });
		appCommand({ type: "SetPanelPosition", panel: SEGMENT_PANEL_ID, x, y });
	});

	const bodyPos = createMemo(() => {
		const base = openBody();
		if (!base) return { x: 0, y: 0 };
		return { x: base.x + dragDelta().x, y: base.y + dragDelta().y };
	});

	const onHeaderPointerDown = (e: PointerEvent) => {
		dragOrigin = { px: e.clientX, py: e.clientY, dx: dragDelta().x, dy: dragDelta().y };
		window.addEventListener("pointermove", onPointerMove);
		window.addEventListener("pointerup", onPointerUp);
	};

	const onPointerMove = (e: PointerEvent) => {
		if (!dragOrigin) return;
		setDragDelta({
			x: dragOrigin.dx + (e.clientX - dragOrigin.px),
			y: dragOrigin.dy + (e.clientY - dragOrigin.py),
		});
	};

	const onPointerUp = () => {
		dragOrigin = null;
		window.removeEventListener("pointermove", onPointerMove);
		window.removeEventListener("pointerup", onPointerUp);
		// Commit the dragged-to position as the new resting position. We adopt
		// it into `openBody` and zero `dragDelta`, so `bodyPos = openBody + 0`
		// stays exactly where the body was released regardless of whether the
		// backend round-trip has landed yet, avoiding a one-frame snap.
		const pos = bodyPos();
		setOpenBody(pos);
		setDragDelta({ x: 0, y: 0 });
		appCommand({ type: "SetPanelPosition", panel: SEGMENT_PANEL_ID, x: pos.x, y: pos.y });
	};

	// The tail points at the residue's live screen position; the body-side
	// attachment is derived from the bubble bounds by the path generator, so
	// only the target tip is supplied here.
	const tailCoords = createMemo<TailCoords | null>(() => {
		const tip = segmentTailTip();
		if (!tip) return null;
		// The tail geometry depends on the body bounds, which move when the
		// panel is dragged; CalloutBubble reads those bounds imperatively, so
		// re-fire this memo on a body-position change to force a recompute.
		void bodyPos();
		return { startX: tip.x, startY: tip.y } as TailCoords;
	});

	return (
		<Show when={segmentInfo()}>
			{(info) => (
				<div
					class="panel-container-svg panel-container-visible"
					style={{
						width: `min(${PANEL_WIDTH}px, calc(100vw - 6rem))`,
						position: "absolute",
						left: `${bodyPos().x}px`,
						top: `${bodyPos().y}px`,
						"z-index": "10",
					}}
				>
					<CalloutBubble
						color="panel"
						tailCoords={tailCoords()}
						showTail={segmentTailTip() != null}
						className="rounded-2xl"
						style={{ "backdrop-filter": "blur(12px)" }}
					>
						<button class="exit" onClick={() => closeSegment()}>
							<X size={20} />
						</button>

						<div
							class="header px-4"
							style={{ cursor: "grab" }}
							onPointerDown={onHeaderPointerDown}
						>
							{`${info().aa_three} ${info().residue_number}`}
						</div>

						<div class="body">
							<div class="text-sm space-y-1">
								<div class="flex justify-between">
									<span class="text-white/60">Residue</span>
									<span>{info().chain ? `${info().chain}${info().residue_number}` : info().residue_number}</span>
								</div>
								<div class="flex justify-between">
									<span class="text-white/60">Amino acid</span>
									<span>{info().aa_three} ({info().aa_one})</span>
								</div>
								<div class="flex justify-between">
									<span class="text-white/60">Secondary structure</span>
									<span>{info().ss_label}</span>
								</div>
							</div>

							<Show when={info().term_names.length > 0}>
								<table class="w-full text-sm mt-2">
									<tbody>
										<For each={info().term_names}>
											{(name, i) => (
												<tr>
													<td class="text-white/60 pr-2">{name}</td>
													<td class="text-right tabular-nums">{formatEnergy(info().term_values[i()])}</td>
												</tr>
											)}
										</For>
										<tr class="border-t border-white/10">
											<td class="pr-2 font-semibold">Total</td>
											<td class="text-right tabular-nums font-semibold">{formatEnergy(info().weighted)}</td>
										</tr>
									</tbody>
								</table>
							</Show>
						</div>
					</CalloutBubble>
				</div>
			)}
		</Show>
	);
}

function formatEnergy(value: number | null | undefined): string {
	return value === null || value === undefined ? "-" : value.toFixed(2);
}

export default SegmentInfoPanel;
