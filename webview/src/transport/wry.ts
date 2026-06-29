import type { FrontendState, ViewportInput, AppCommand, EntitySelection, ParamValue } from '../types';
import type { DispatchableOp, RequestKind } from '@foldit/plugin-bridge';

declare global {
  interface Window {
    ipc: { postMessage: (msg: string) => void };
    __onStateUpdate?: (sections: Partial<FrontendState>) => void;
    __onResponse?: (wishId: string, ok: boolean, payload: unknown) => void;
  }
}

// Pending JS-side requests, keyed by wish_id. Resolved/rejected by
// window.__onResponse, which the Rust side calls via evaluate_script.
const pending = new Map<string, { resolve: (v: unknown) => void; reject: (e: Error) => void; timer: ReturnType<typeof setTimeout> }>();
let nextWishId = 0;

function ensureResponseHandler() {
  if (window.__onResponse) return;
  window.__onResponse = (wishId, ok, payload) => {
    const entry = pending.get(wishId);
    if (!entry) return;
    pending.delete(wishId);
    clearTimeout(entry.timer);
    if (ok) entry.resolve(payload);
    else entry.reject(new Error(typeof payload === 'string' ? payload : JSON.stringify(payload)));
  };
}

/**
 * Subscribe to state updates from the Rust backend and tell it we're ready
 * to receive them. The first emit after `ready` is a full-state snapshot
 * (backend marks everything dirty on receiving the `ready` IPC); subsequent
 * emits are partial deltas. Single channel — no Promise / microtask race.
 */
export function subscribe(callback: (sections: Partial<FrontendState>) => void): () => void {
  window.__onStateUpdate = callback;
  ensureResponseHandler();
  window.ipc.postMessage(JSON.stringify({ cmd: 'ready' }));
  return () => {
    window.__onStateUpdate = undefined;
  };
}

export function viewportInput(input: ViewportInput): void {
  window.ipc.postMessage(JSON.stringify({ cmd: 'viewport_input', data: input }));
}

/** Dispatch a plugin op by op-id (catalog-driven button click).
 *  `focused_entity_id` is omittable here; a missing key deserializes to
 *  `None` on the Rust side, so click-to-fire buttons post `{ op_id }`. */
export function dispatchOp(op: DispatchableOp): void {
  window.ipc.postMessage(JSON.stringify({ cmd: 'dispatch_op', data: op }));
}

/**
 * Start a streaming op via the async request channel and resolve with the
 * stream's `request_id`. Rejects (with the backend message) when the op is
 * not a stream or fails to start.
 */
export function startStream(op: DispatchableOp): Promise<number> {
  return request<{ request_id: number }>('start_stream', { op }).then((r) => r.request_id);
}

/**
 * Fire-and-forget frame update to a live stream. Drop-tolerant — a lost
 * update is just a stale frame — so this posts without awaiting a reply.
 */
export function updateStream(rid: number, params: Record<string, ParamValue>): void {
  window.ipc.postMessage(JSON.stringify({ cmd: 'update_stream', data: { request_id: rid, params } }));
}

/**
 * Cancel (= commit) a live stream via the async request channel. Awaited so
 * a dropped cancel cannot strand an open stream; resolves once the host has
 * acknowledged the cancel.
 */
export function cancelStream(rid: number): Promise<void> {
  return request('cancel_stream', { request_id: rid }).then(() => undefined);
}

/** Send a native GUI / chrome command (history nav, bubble advance, view options, load). */
export function appCommand(command: AppCommand): void {
  window.ipc.postMessage(JSON.stringify({ cmd: 'app_command', data: command }));
}

/** Desktop-only: ask the host to open the native "Load Session" file picker. */
export function openSessionDialog(): void {
  window.ipc.postMessage(JSON.stringify({ cmd: 'open_session_dialog' }));
}

/**
 * Panel-originated selection mutation. Replaces the backend
 * `App.selection` wholesale with the supplied per-entity entries; pass
 * `[]` to clear. Pointer-pick selection (viso click expansion) flows
 * through `viewportInput` and does not use this path.
 */
export function setSelection(entries: EntitySelection[]): void {
  window.ipc.postMessage(JSON.stringify({ cmd: 'set_selection', data: { entries } }));
}

/**
 * Round-trip an async request to the backend. Resolves with the JSON payload
 * on success, rejects with the backend-provided message on failure. 30 s
 * timeout — matches the legacy WishingWell behavior so callers don't hang.
 */
export function request<T = unknown>(kind: RequestKind, payload: object = {}, timeoutMs = 30000): Promise<T> {
  ensureResponseHandler();
  const wishId = `${Date.now()}_${++nextWishId}`;
  return new Promise<T>((resolve, reject) => {
    const timer = setTimeout(() => {
      pending.delete(wishId);
      reject(new Error(`request '${kind}' timed out after ${timeoutMs}ms`));
    }, timeoutMs);
    pending.set(wishId, { resolve: resolve as (v: unknown) => void, reject, timer });
    window.ipc.postMessage(JSON.stringify({ cmd: 'request', wish_id: wishId, kind, payload }));
  });
}
