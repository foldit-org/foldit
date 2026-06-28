/**
 * Wasm transport — talks to the Rust `foldit-web` cdylib loaded via
 * wasm-bindgen. Same surface as `transport/wry.ts`; the build-time barrel
 * (`transport/index.ts`) selects between them based on FOLDIT_TARGET.
 */

import type { FrontendState, ViewportInput, AppCommand, EntitySelection } from '../types';
import type { DispatchableOp, RequestKind } from '@foldit/plugin-bridge';

// The wasm-pack output lives at /pkg/foldit_web.js relative to the served
// bundle root (xtask build-web emits there). The dynamic import keeps the
// wasm bytes off the critical path and avoids a top-level await everywhere
// transport is touched.
type FolditAppCtor = new () => FolditApp;
interface FolditApp {
  setStateCallback(cb: (json: string) => void): void;
  setResponseCallback(cb: (wishId: string, ok: boolean, payload: string) => void): void;
  start(canvas: HTMLCanvasElement): Promise<void>;
  viewportInput(json: string): void;
  dispatchOp(json: string): void;
  appCommand(json: string): void;
  setSelection(json: string): void;
  request(kind: string, payload: string): Promise<string>;
}

interface WasmModule {
  default: (input?: unknown) => Promise<unknown>;
  init: () => void;
  initThreadPool: (threads: number) => Promise<void>;
  FolditApp: FolditAppCtor;
}

let appPromise: Promise<FolditApp> | null = null;

// Pull every readable property off whatever the wasm threw. C++
// exceptions surface as `WebAssembly.Exception` with no `.message`; pure
// wasm traps surface as `RuntimeError` (e.g. "unreachable") with one;
// JS `throw new Error()` from imports surfaces as a regular Error. Log
// all of them comprehensively because we don't know which kind it is
// until we read the object.
function describeWasmError(e: unknown): string {
  if (e === null || e === undefined) return String(e);
  const lines: string[] = [];
  const ctor = (e as { constructor?: { name?: string } }).constructor?.name ?? typeof e;
  lines.push(`type: ${ctor}`);
  for (const key of ['name', 'message', 'code', 'stack']) {
    const v = (e as Record<string, unknown>)[key];
    if (v !== undefined) lines.push(`${key}: ${typeof v === 'string' ? v : JSON.stringify(v)}`);
  }
  // Enumerate own properties (Exception arg slots are sometimes here).
  try {
    const own = Object.getOwnPropertyNames(e as object);
    if (own.length) lines.push(`own props: ${own.join(', ')}`);
  } catch { /* ignore */ }
  // Try toString — some browsers format wasm exceptions usefully.
  try {
    const ts = String(e);
    if (ts !== '[object Object]') lines.push(`toString: ${ts}`);
  } catch { /* ignore */ }
  return lines.join('\n');
}

async function loadApp(): Promise<FolditApp> {
  if (appPromise) return appPromise;
  appPromise = (async () => {
    // The wasm-bindgen output lives under /public/pkg/. Vite's
    // import-analysis rejects static `import('/pkg/...')` because
    // /public files must be referenced via <script src> or `?url`,
    // not as ESM modules. We dodge the check by computing the URL at
    // runtime so the analyzer can't trace it back to /public.
    const url = new URL('/pkg/foldit_web.js', window.location.origin).href;
    // @ts-ignore — wasm-bindgen-emitted module, no .d.ts in the public folder
    const mod: WasmModule = await import(/* @vite-ignore */ url);

    let stage = 'mod.default()';
    try {
      await mod.default();
      stage = 'initThreadPool';
      await mod.initThreadPool(navigator.hardwareConcurrency ?? 4);
      stage = 'mod.init()';
      mod.init();
      stage = 'new FolditApp()';
      return new mod.FolditApp();
    } catch (e) {
      console.error(`[transport/wasm] failed at stage '${stage}':\n${describeWasmError(e)}`, e);
      throw e;
    }
  })();
  return appPromise;
}

// Subscribe (state push)

export function subscribe(callback: (sections: Partial<FrontendState>) => void): () => void {
  let cancelled = false;
  loadApp().then(app => {
    if (cancelled) return;
    app.setStateCallback((json: string) => {
      try {
        callback(JSON.parse(json));
      } catch (e) {
        console.warn('[transport/wasm] state JSON parse failed:', e);
      }
    });
    app.setResponseCallback((wishId: string, ok: boolean, payload: string) => {
      const entry = pending.get(wishId);
      if (!entry) return;
      pending.delete(wishId);
      clearTimeout(entry.timer);
      try {
        const parsed = payload ? JSON.parse(payload) : null;
        ok ? entry.resolve(parsed) : entry.reject(new Error(typeof parsed === 'string' ? parsed : payload));
      } catch {
        ok ? entry.resolve(payload) : entry.reject(new Error(payload));
      }
    });
  });
  return () => {
    cancelled = true;
  };
}

// Outbound IPC

export function viewportInput(input: ViewportInput): void {
  loadApp().then(app => app.viewportInput(JSON.stringify(input)));
}

/** Dispatch a plugin op by op-id (catalog-driven button click).
 *  `focused_entity_id` is omittable here; a missing key deserializes to
 *  `None` on the Rust side, so click-to-fire buttons post `{ op_id }`. */
export function dispatchOp(op: DispatchableOp): void {
  loadApp().then(app => app.dispatchOp(JSON.stringify(op)));
}

/** Send a native GUI / chrome command (history nav, bubble advance, view options, load). */
export function appCommand(command: AppCommand): void {
  loadApp().then(app => app.appCommand(JSON.stringify(command)));
}

/** Desktop-only; the web build has no native file picker. */
export function openSessionDialog(): void {
  console.warn('Load Session is desktop-only');
}

/**
 * Panel-originated selection mutation. Replaces the backend
 * `App.selection` wholesale with `entries`; pass `[]` to clear.
 * Pointer-pick selection (viso click expansion) flows through
 * `viewportInput` and does not use this path.
 */
export function setSelection(entries: EntitySelection[]): void {
  loadApp().then(app => app.setSelection(JSON.stringify(entries)));
}

// Async request channel

const pending = new Map<string, { resolve: (v: unknown) => void; reject: (e: Error) => void; timer: ReturnType<typeof setTimeout> }>();
let nextWishId = 0;

export function request<T = unknown>(kind: RequestKind, payload: object = {}, timeoutMs = 30000): Promise<T> {
  // The Rust-side `FolditApp::request` returns a Promise<JsValue> that
  // resolves with the JSON result string. We wire through it directly —
  // the resolver Map is unused on the wasm path because there's no need
  // for wishId correlation; but we keep it for symmetry with the wry side
  // in case set_response_callback fires unrelated responses.
  void pending;
  void nextWishId;

  const wishId = `${Date.now()}_${++nextWishId}`;
  return new Promise<T>((resolve, reject) => {
    const timer = setTimeout(() => {
      pending.delete(wishId);
      reject(new Error(`request '${kind}' timed out after ${timeoutMs}ms`));
    }, timeoutMs);
    pending.set(wishId, { resolve: resolve as (v: unknown) => void, reject, timer });

    loadApp()
      .then(app => app.request(kind, JSON.stringify(payload)))
      .then(jsonString => {
        const entry = pending.get(wishId);
        if (!entry) return;
        pending.delete(wishId);
        clearTimeout(entry.timer);
        try {
          entry.resolve(jsonString ? JSON.parse(jsonString) : null);
        } catch {
          entry.resolve(jsonString);
        }
      })
      .catch(err => {
        const entry = pending.get(wishId);
        if (!entry) return;
        pending.delete(wishId);
        clearTimeout(entry.timer);
        entry.reject(err instanceof Error ? err : new Error(String(err)));
      });
  });
}

/** Web-only: hand the canvas element to the engine. Called once at boot. */
export async function mountCanvas(canvas: HTMLCanvasElement): Promise<void> {
  const app = await loadApp();
  await app.start(canvas);
}
