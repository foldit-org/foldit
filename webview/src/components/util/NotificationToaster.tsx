import { createEffect, createSignal, untrack, Show } from 'solid-js';
import { Toaster, toast } from 'solid-toast';
import type { Notification } from '../../generated/wire';
import { state } from '../../adapter';
import '../../styles/util/NotificationToaster.css';

// Shared toast body: compact font and rounded border so no level renders at
// the library's ambient ~16px default.
const BASE_STYLE = {
	'font-size': '13px',
	'border-radius': '10px',
};

// Muted, translucent palettes: readable but not a harsh solid-color alarm.
const ERROR_STYLE = {
	...BASE_STYLE,
	background: 'rgba(120, 32, 32, 0.92)',
	color: '#ffe3e3',
	border: '1px solid rgba(220, 90, 90, 0.5)',
};
const WARN_STYLE = {
	...BASE_STYLE,
	background: 'rgba(60, 45, 15, 0.94)',
	color: '#f7e6c4',
	border: '1px solid rgba(214, 158, 46, 0.4)',
};
const SUCCESS_STYLE = {
	...BASE_STYLE,
	background: 'rgba(20, 30, 26, 0.94)',
	color: '#e6f4ec',
	border: '1px solid rgba(52, 211, 153, 0.28)',
};

function fire(note: Notification): void {
	if (note.level === 'Error') {
		toast.error(note.text, { style: ERROR_STYLE });
	} else if (note.level === 'Warning') {
		toast(note.text, { style: WARN_STYLE });
	} else {
		// Info is currently the download-success channel, so it renders as a
		// success toast (green checkmark) to match the completed-download look.
		toast.success(note.text, { style: SUCCESS_STYLE });
	}
}

// Live download bar for one plugin. Reads the store leaf inside JSX so the bar
// tracks progress reactively without re-firing the toast.
function DownloadToast(props: { pluginId: string }) {
	const entry = () => state.actions.download_progress[props.pluginId];
	const frac = () => entry()?.fraction ?? 0;
	const known = () => entry()?.fraction != null;
	const stage = () => entry()?.stage;
	return (
		<div class="dl-toast">
			<div class="dl-toast__row">
				<span class="dl-toast__title">Downloading {props.pluginId} weights</span>
				<Show when={known()}>
					<span class="dl-toast__pct">{Math.round(frac() * 100)}%</span>
				</Show>
			</div>
			<div class="dl-bar">
				<Show when={known()} fallback={<div class="dl-bar__indeterminate" />}>
					<div class="dl-bar__fill" style={{ width: `${frac() * 100}%` }} />
				</Show>
			</div>
			<Show when={stage()}>
				<span class="dl-toast__pct">{stage()}</span>
			</Show>
		</div>
	);
}

// Live refine bar. Reads the store leaf inside JSX so the bar tracks progress
// reactively without re-firing the toast. Only one refine runs at a time, so
// there is no per-instance key.
function RefineToast() {
	const entry = () => state.actions.refine_progress;
	const frac = () => entry()?.fraction ?? 0;
	const known = () => entry()?.fraction != null;
	const label = () => entry()?.label ?? '';
	return (
		<div class="dl-toast">
			<div class="dl-toast__row">
				<span class="dl-toast__title">{label()}</span>
				<Show when={known()}>
					<span class="dl-toast__pct">{Math.round(frac() * 100)}%</span>
				</Show>
			</div>
			<div class="dl-bar">
				<Show when={known()} fallback={<div class="dl-bar__indeterminate" />}>
					<div class="dl-bar__fill" style={{ width: `${frac() * 100}%` }} />
				</Show>
			</div>
		</div>
	);
}

/**
 * Mounts the top-right toast container and turns backend notifications into
 * toasts. Notifications are the host's reusable, append-only channel: each
 * carries a session-monotonic id, and we toast only ids above the highest
 * already shown. The high-water mark is seeded from whatever is present at
 * first render, so a reload that replays the retained list does not re-toast
 * messages the user already saw; only genuinely new ids fire.
 */
export default function NotificationToaster() {
	const [highestShown, setHighestShown] = createSignal(
		state.notifications.reduce((max, n) => Math.max(max, n.id), 0),
	);

	createEffect(() => {
		const shown = highestShown();
		let advanced = shown;
		for (const note of state.notifications) {
			if (note.id <= shown) {
				continue;
			}
			fire(note);
			if (note.id > advanced) {
				advanced = note.id;
			}
		}
		if (advanced !== shown) {
			setHighestShown(advanced);
		}
	});

	// Live download-progress toasts, distinct from the discrete-notification
	// effect above. A custom toast is fired once per plugin that newly appears
	// in `download_progress`, keyed by a stable id; the toast body binds to the
	// store leaf so the bar animates in place without re-toasting each frame.
	// When a plugin drops out of the map (download ended, success or failure)
	// its toast is dismissed; the discrete success/error notification carries
	// the outcome.
	const [downloadingIds, setDownloadingIds] = createSignal<string[]>([]);
	createEffect(() => {
		const progress = state.actions.download_progress ?? {};
		const currentIds = Object.keys(progress);
		const prevIds = untrack(downloadingIds);
		for (const pluginId of currentIds) {
			if (!prevIds.includes(pluginId)) {
				toast.custom(() => <DownloadToast pluginId={pluginId} />, {
					id: `download-progress:${pluginId}`,
					duration: Infinity,
				});
			}
		}
		for (const prevId of prevIds) {
			if (!currentIds.includes(prevId)) {
				toast.dismiss(`download-progress:${prevId}`);
			}
		}
		setDownloadingIds(currentIds);
	});

	// Live refine-progress toast. Fires once when a refine starts (refine_progress
	// goes null -> Some) and dismisses when it ends (Some -> null); the toast body
	// binds to the store leaf so the bar animates in place without re-toasting.
	const [refineShowing, setRefineShowing] = createSignal(false);
	createEffect(() => {
		const active = state.actions.refine_progress != null;
		const wasShowing = untrack(refineShowing);
		if (active && !wasShowing) {
			toast.custom(() => <RefineToast />, {
				id: 'refine-progress',
				duration: Infinity,
			});
		} else if (!active && wasShowing) {
			toast.dismiss('refine-progress');
		}
		setRefineShowing(active);
	});

	return <Toaster position="top-right" toastOptions={{ style: { 'font-size': '13px' } }} />;
}
