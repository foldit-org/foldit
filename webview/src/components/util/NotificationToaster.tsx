import { createEffect, createSignal, untrack, Show, type JSX } from 'solid-js';
import { Toaster, toast } from 'solid-toast';
import type { Notification } from '../../generated/wire';
import { state, appCommand } from '../../adapter';
import { Icons } from '../../utils/iconMapping';
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

// Weight downloads are the one stream that surfaces on a plugin's button rather
// than as a cancellable action, so they are addressed by plugin id.
const DOWNLOAD_OP = 'download_weights';

// A stream's live progress entry, or `undefined` before it reports anything.
function progressEntry(requestId: number) {
	return (state.actions.op_progress ?? []).find((e) => e.request_id === requestId);
}

function downloadEntry(pluginId: string) {
	return (state.actions.op_progress ?? []).find(
		(e) => e.op_id === DOWNLOAD_OP && e.plugin_id === pluginId,
	);
}

// One unified progress toast body for every running thing (plugin action,
// download). All accessors are read inside JSX so the bar / title
// track backend state reactively without re-firing the toast. A determinate
// `fraction` (0..1) draws a fill bar; `null` draws the indeterminate sweep. An
// `onCancel` renders the top-left X that cancels the action and dismisses the
// toast; omit it for things that cannot be cancelled.
function ProgressToast(props: {
	title: () => string;
	fraction: () => number | null;
	stage?: () => string | undefined;
	onCancel?: () => void;
}) {
	const known = () => props.fraction() != null;
	const frac = () => props.fraction() ?? 0;
	return (
		<div class="dl-toast">
			<div class="dl-toast__row">
				<Show when={props.onCancel}>
					<button
						class="dl-toast__cancel"
						title="Cancel"
						onClick={() => props.onCancel?.()}
					>
						<Icons.X size={13} />
					</button>
				</Show>
				<span class="dl-toast__title">{props.title()}</span>
				<Show when={known()}>
					<span class="dl-toast__pct">{Math.round(frac() * 100)}%</span>
				</Show>
			</div>
			<div class="dl-bar">
				<Show when={known()} fallback={<div class="dl-bar__indeterminate" />}>
					<div class="dl-bar__fill" style={{ width: `${frac() * 100}%` }} />
				</Show>
			</div>
			<Show when={props.stage?.()}>
				<span class="dl-toast__pct">{props.stage?.()}</span>
			</Show>
		</div>
	);
}

// Live download bar for one plugin. No cancel affordance: a weight download is
// a background asset fetch, not an action (and ESC deliberately leaves it
// alone), so it simply shows progress until it completes.
function DownloadToast(props: { pluginId: string }) {
	const entry = () => downloadEntry(props.pluginId);
	return (
		<ProgressToast
			title={() => `Downloading ${props.pluginId} weights`}
			fraction={() => entry()?.fraction ?? null}
			stage={() => entry()?.label ?? undefined}
		/>
	);
}

// Live toast for one running plugin-action instance, keyed by its request-id
// (so two concurrent streams of the same op get two toasts). An op that reports
// a fraction (B-factor refine, RFdiffusion3) draws a determinate bar; one that
// reports none (wiggle, shake) draws the indeterminate sweep. The X cancels
// just this instance and dismisses the toast.
function ActionToast(props: { requestId: number }) {
	const running = () => state.actions.running.find((r) => r.request_id === props.requestId);
	const entry = () => progressEntry(props.requestId);
	return (
		<ProgressToast
			title={() => running()?.display ?? ''}
			fraction={() => entry()?.fraction ?? null}
			stage={() => entry()?.label ?? undefined}
			onCancel={() => {
				appCommand({ type: 'CancelAction', request_id: props.requestId });
				toast.dismiss(`action-progress:${props.requestId}`);
			}}
		/>
	);
}

// Keep a set of persistent (`duration: Infinity`) toasts in sync with a live
// id set: fire one per newly-appeared id and dismiss one per id that dropped
// out, keyed `<idPrefix>:<id>`. Backs both the download-progress toasts and
// the per-action toasts, which differ only in their source ids and rendered
// body. Call once per source during component setup.
function reconcilePersistentToasts(
	ids: () => string[],
	idPrefix: string,
	render: (id: string) => JSX.Element,
): void {
	const [shown, setShown] = createSignal<string[]>([]);
	createEffect(() => {
		const current = ids();
		const prev = untrack(shown);
		for (const id of current) {
			if (!prev.includes(id)) {
				toast.custom(() => render(id), { id: `${idPrefix}:${id}`, duration: Infinity });
			}
		}
		for (const id of prev) {
			if (!current.includes(id)) {
				toast.dismiss(`${idPrefix}:${id}`);
			}
		}
		setShown(current);
	});
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

	// Live download-progress toasts: one per plugin with a download stream in
	// flight, dismissed when the download ends (the discrete success/error
	// notification carries the outcome).
	reconcilePersistentToasts(
		() =>
			(state.actions.op_progress ?? [])
				.filter((e) => e.op_id === DOWNLOAD_OP)
				.map((e) => e.plugin_id),
		'download-progress',
		(pluginId) => <DownloadToast pluginId={pluginId} />,
	);

	// Live per-action toasts: one per running action instance, keyed by
	// request-id. Downloads have their own toast above and are already excluded
	// from `running`.
	reconcilePersistentToasts(
		() =>
			state.actions.running
				.filter((r) => r.request_id != null)
				.map((r) => String(r.request_id)),
		'action-progress',
		(idStr) => <ActionToast requestId={Number(idStr)} />,
	);

	return <Toaster position="top-right" toastOptions={{ style: { 'font-size': '13px' } }} />;
}
