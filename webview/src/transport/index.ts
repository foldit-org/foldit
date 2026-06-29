/**
 * Transport barrel — picks between `wry` (desktop) and `wasm` (web) at
 * build time. Vite resolves the `transport-impl` alias differently per
 * `FOLDIT_TARGET` env var:
 *
 *   FOLDIT_TARGET=wry  pnpm build      → desktop bundle (default)
 *   FOLDIT_TARGET=wasm pnpm build      → web bundle
 *
 * Both impls expose the same surface (subscribe, viewportInput,
 * dispatchOp, appCommand, request). Components import from
 * `'../transport'`, never from `'../transport/wry'` or `'../transport/wasm'`
 * directly, so swapping targets is a config change, not a code change.
 */

// `transport-impl` is a Vite alias defined in vite.config.ts. The path it
// resolves to is determined by FOLDIT_TARGET.
// @ts-expect-error — the alias is provided at build time by Vite.
export * from 'transport-impl';
