/**
 * Build the webview-fetchable URL for a plugin asset. The backend ships
 * asset paths manifest-relative (relative to the owning plugin directory);
 * the desktop runtime serves a plugin's asset tree under `/plugins/<id>/...`.
 */
export function pluginAssetUrl(pluginId: string, relPath: string): string {
  return new URL(`/plugins/${pluginId}/${relPath}`, window.location.origin).href;
}

/**
 * Build the webview-fetchable URL for a foldit-owned static asset. The
 * desktop runtime (and the Vite dev server) serve the repo-root `assets/`
 * tree under `/game-assets/...`.
 */
export function gameAssetUrl(relPath: string): string {
  return new URL(`/game-assets/${relPath}`, window.location.origin).href;
}
