/**
 * Build the webview-fetchable URL for a plugin asset. The backend ships
 * asset paths manifest-relative (relative to the owning plugin directory);
 * the desktop runtime serves a plugin's asset tree under `/plugins/<id>/...`.
 */
export function pluginAssetUrl(pluginId: string, relPath: string): string {
  return new URL(`/plugins/${pluginId}/${relPath}`, window.location.origin).href;
}
