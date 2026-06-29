import type { UserConfig } from 'vite';
import solid from 'vite-plugin-solid';

export interface PluginViteConfigOptions {
  /** Path to the panel's entry module (the file exporting `mountPanel`). */
  entry: string;
  /** Output file name, matching the manifest panel `entry` (e.g. `panel.mjs`). */
  outFile: string;
}

/**
 * Build a Vite config for a single-file plugin-panel ES library.
 *
 * Emits one `.mjs` with `solid-js` bundled in (plugin reactivity is
 * self-contained behind the callback bridge and its own shadow root, so it
 * never tracks inside the host's reactive graph). The solid Vite plugin is
 * enabled so the panel's JSX compiles to the same reactive runtime the host
 * uses.
 */
export function createPluginViteConfig(options: PluginViteConfigOptions): UserConfig {
  const { entry, outFile } = options;
  return {
    plugins: [solid()],
    build: {
      lib: {
        entry,
        formats: ['es'],
        fileName: () => outFile,
      },
    },
  };
}
