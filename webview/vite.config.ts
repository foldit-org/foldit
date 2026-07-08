import { ProxyOptions, ViteDevServer, Plugin } from 'vite'
import type { Connect } from 'vite';
import { defineConfig } from 'vitest/config';
import solid from 'vite-plugin-solid'

import fs from 'fs';
import path from 'path';
import { IncomingMessage, ServerResponse } from 'http';

// FOLDIT_TARGET selects which transport implementation the app builds
// against. `wry` (default) is the desktop wry-IPC bridge; `wasm` is the
// wasm-bindgen bridge to foldit-web's cdylib.
const FOLDIT_TARGET = (process.env.FOLDIT_TARGET ?? 'wry') as 'wry' | 'wasm';
const transportImpl =
  FOLDIT_TARGET === 'wasm'
    ? path.resolve(__dirname, 'src/transport/wasm.ts')
    : path.resolve(__dirname, 'src/transport/wry.ts');

function brotliMiddleWarePlugin(): Plugin {
  return {
    name: 'vite-brotli-middleware',

    configureServer: (server: ViteDevServer) => {
      server.middlewares.use((req: IncomingMessage, res: ServerResponse, next: Connect.NextFunction) => {
        const url = req.url!;
        if (!url || (!url.endsWith(".wasm") && !url.endsWith(".data"))) {
          return next();
        }

        const brFile = path.join(server.config.root, 'public', url + '.br');
        if (!fs.existsSync(brFile)) {
          return next();
        }

        const contentType = url.endsWith('.wasm') ? 'application/wasm' : 'application/octet-stream';
        res.setHeader('Content-Type', contentType);
        res.setHeader('Content-Encoding', 'br');

        fs.createReadStream(brFile).pipe(res);
      });
    },

  }
}

// Static asset serving for the plugin tree. Mirrors the wry custom
// protocol branch in `foldit-desktop/src/plugin_assets.rs`: anything
// the desktop release build serves under `foldit:///plugins/...` the
// dev build must serve under `http://localhost:5173/plugins/...` so
// the frontend's `<img src=>` works in both modes.
//
// Scope is deliberately narrow. Executable surfaces (.js, .html, .wasm)
// are absent from the whitelist. `.mjs` is the one exception: plugins
// ship custom-panel UI modules. Unlike the Rust release path, which gates
// `.mjs` to the manifest-declared `[[panels]]` entrypoints, dev serves any
// `.mjs` under `/plugins/*` (no per-entry allowlist) — see the middleware
// below. Path-traversal defense is canonicalization plus a containment
// check against the canonicalized plugins root, identical to the Rust side.
const PLUGIN_ASSET_MIME: Record<string, string> = {
  '.svg': 'image/svg+xml',
  '.png': 'image/png',
  '.jpg': 'image/jpeg',
  '.jpeg': 'image/jpeg',
  '.webp': 'image/webp',
  '.gif': 'image/gif',
  '.css': 'text/css',
  '.woff2': 'font/woff2',
  '.ttf': 'font/ttf',
  '.mjs': 'application/javascript',
};

function resolvePluginsRoot(): string | null {
  const env = process.env.FOLDIT_PLUGINS_ROOT;
  if (env && fs.existsSync(env) && fs.statSync(env).isDirectory()) {
    try {
      return fs.realpathSync(env);
    } catch {
      return null;
    }
  }
  // Walk up from this config's directory looking for the in-tree
  // plugins root. Matches `foldit-core::locate_plugins_root`'s
  // workspace-checkout fallback.
  let cursor = __dirname;
  for (;;) {
    const candidate = path.join(cursor, 'plugins');
    if (fs.existsSync(candidate) && fs.statSync(candidate).isDirectory()) {
      try {
        return fs.realpathSync(candidate);
      } catch {
        return null;
      }
    }
    const parent = path.dirname(cursor);
    if (parent === cursor) return null;
    cursor = parent;
  }
}

// Resolve the repo-root `assets/` directory holding foldit-owned static
// assets (residue icons, etc.). Mirrors `resolvePluginsRoot`, honoring a
// FOLDIT_ASSETS_ROOT override consistent with the desktop release path.
function resolveAssetsRoot(): string | null {
  const env = process.env.FOLDIT_ASSETS_ROOT;
  if (env && fs.existsSync(env) && fs.statSync(env).isDirectory()) {
    try {
      return fs.realpathSync(env);
    } catch {
      return null;
    }
  }
  // Walk up from this config's directory looking for the in-tree assets root.
  let cursor = __dirname;
  for (;;) {
    const candidate = path.join(cursor, 'assets');
    if (fs.existsSync(candidate) && fs.statSync(candidate).isDirectory()) {
      try {
        return fs.realpathSync(candidate);
      } catch {
        return null;
      }
    }
    const parent = path.dirname(cursor);
    if (parent === cursor) return null;
    cursor = parent;
  }
}

// Strip `urlPrefix` from the request, resolve the remainder within `root`
// under a realpath containment check, and stream the file with a
// fail-closed PLUGIN_ASSET_MIME type. Shared by the plugin and game-asset
// routes so both inherit the identical traversal defense and served-type
// restriction; requests outside `urlPrefix` fall through to `next()`.
function serveContained(
  root: string,
  urlPrefix: string,
  req: IncomingMessage,
  res: ServerResponse,
  next: Connect.NextFunction
) {
  const url = req.url ?? '';
  if (!url.startsWith(urlPrefix)) return next();

  // Drop query string, then strip the route prefix.
  const qIdx = url.indexOf('?');
  const pathOnly = qIdx >= 0 ? url.slice(0, qIdx) : url;
  const rel = pathOnly.slice(urlPrefix.length);
  if (rel === '') {
    res.statusCode = 404;
    return res.end('Not Found');
  }

  const asset = path.join(root, rel);
  let canonical: string;
  try {
    canonical = fs.realpathSync(asset);
  } catch {
    res.statusCode = 404;
    return res.end('Not Found');
  }
  // Component-aware containment: the canonical path must equal root or
  // have root as a strict directory ancestor. Comparing with the path
  // separator appended defeats prefix-collision sibling directories
  // (`plugins-evil` vs `plugins`).
  const rootWithSep = root.endsWith(path.sep) ? root : root + path.sep;
  if (canonical !== root && !canonical.startsWith(rootWithSep)) {
    res.statusCode = 404;
    return res.end('Not Found');
  }

  let stat: fs.Stats;
  try {
    stat = fs.statSync(canonical);
  } catch {
    res.statusCode = 404;
    return res.end('Not Found');
  }
  if (!stat.isFile()) {
    res.statusCode = 404;
    return res.end('Not Found');
  }

  const ext = path.extname(canonical).toLowerCase();
  const mime = PLUGIN_ASSET_MIME[ext];
  if (!mime) {
    res.statusCode = 404;
    return res.end('Not Found');
  }
  // Dev intentionally has no per-entry allowlist: any `.mjs` whose ext is
  // in the MIME table is served. Release gates `.mjs` to the
  // manifest-declared `[[panels]]` entrypoints (see plugin_assets.rs).

  res.setHeader('Content-Type', mime);
  res.setHeader('Access-Control-Allow-Origin', '*');
  fs.createReadStream(canonical).pipe(res);
}

function pluginAssetsPlugin(): Plugin {
  const root = resolvePluginsRoot();
  return {
    name: 'foldit-plugin-assets',
    configureServer: (server: ViteDevServer) => {
      if (!root) {
        server.config.logger.warn(
          '[plugin-assets] no plugins root found (set FOLDIT_PLUGINS_ROOT ' +
            'or run from a workspace checkout); plugin icons will 404'
        );
        return;
      }
      server.config.logger.info(
        `[plugin-assets] serving /plugins/* from ${root}`
      );
      server.middlewares.use((req: IncomingMessage, res: ServerResponse, next: Connect.NextFunction) =>
        serveContained(root, '/plugins/', req, res, next)
      );
    },
  };
}

// Foldit-owned static assets under `/game-assets/*`, dev-mirror of the wry
// release branch in `foldit-desktop/src/webview_assets.rs`. Kept off
// `/assets/*` so it never shadows Vite's own emitted GUI bundle. Same
// canonicalize + containment defense as the plugin middleware.
function gameAssetsPlugin(): Plugin {
  const root = resolveAssetsRoot();
  return {
    name: 'foldit-game-assets',
    configureServer: (server: ViteDevServer) => {
      if (!root) {
        server.config.logger.warn(
          '[game-assets] no assets root found (set FOLDIT_ASSETS_ROOT ' +
            'or run from a workspace checkout); game assets will 404'
        );
        return;
      }
      server.config.logger.info(
        `[game-assets] serving /game-assets/* from ${root}`
      );
      server.middlewares.use((req: IncomingMessage, res: ServerResponse, next: Connect.NextFunction) =>
        serveContained(root, '/game-assets/', req, res, next)
      );
    },
  };
}

// Web entry rewrite: when FOLDIT_TARGET=wasm, the dev server should
// serve `index-web.html` at `/` instead of `index.html`. The two entries
// are deliberately separate — `main.tsx` skips the canvas mount and is
// for the wry build; `main-web.tsx` calls `mountCanvas` and provides the
// wasm import map. Without this rewrite, navigating to `/` loads the
// desktop entry, which still triggers wasm load (via the
// `transport-impl` alias) but has no import map for
// `wasi_snapshot_preview1` — the wasm-bindgen module then fails to load.
function webEntryRewritePlugin(target: 'wry' | 'wasm'): Plugin {
  return {
    name: 'vite-web-entry-rewrite',
    configureServer: (server: ViteDevServer) => {
      if (target !== 'wasm') return;
      server.middlewares.use((req: IncomingMessage, _res: ServerResponse, next: Connect.NextFunction) => {
        const url = req.url ?? '';
        // Match `/`, `/index.html`, and either with a query string.
        if (url === '/' || url === '/index.html' || url.startsWith('/?') || url.startsWith('/index.html?')) {
          const qIdx = url.indexOf('?');
          req.url = '/index-web.html' + (qIdx >= 0 ? url.slice(qIdx) : '');
        }
        next();
      });
    },
  };
}

// https://vitejs.dev/config/
export default defineConfig({
  base: './', // Use relative paths for CEF file:// protocol compatibility
  plugins: [solid(), brotliMiddleWarePlugin(), pluginAssetsPlugin(), gameAssetsPlugin(), webEntryRewritePlugin(FOLDIT_TARGET)],
  resolve: {
    alias: {
      'transport-impl': transportImpl,
    },
  },
  define: {
    __FOLDIT_TARGET__: JSON.stringify(FOLDIT_TARGET),
  },
  build: {
    rollupOptions: {
      input: FOLDIT_TARGET === 'wasm'
        ? path.resolve(__dirname, 'index-web.html')
        : path.resolve(__dirname, 'index.html'),
      // The wasm-bindgen output at /pkg/foldit_web.js is produced by
      // `cargo xtask build-web` (separate pipeline). Keep it external so
      // Rollup doesn't try to bundle it.
      external: FOLDIT_TARGET === 'wasm' ? ['/pkg/foldit_web.js'] : [],
    },
  },
  test: {
    globals: true,
    environment: 'jsdom',
    setupFiles: './vitest.setup.ts',
    include: ['test/**/*.{test,spec}.{js,ts,jsx,tsx}'],
  },
  server: {
    port: 5173,
    proxy: {
      '/api': {
        target: 'https://fold.it',
        changeOrigin: true,
        configure: (proxy) => {
          proxy.on('proxyRes', (proxyRes) => {
            delete proxyRes.headers['cross-origin-embedder-policy'];
            delete proxyRes.headers['cross-origin-opener-policy'];

            proxyRes.headers['access-control-allow-origin'] = '*';
            proxyRes.headers['access-control-allow-methods'] = 'GET, POST, PUT, DELETE, OPTIONS';
          });
        }
      } as ProxyOptions
    },
    headers: {
      // Required for SharedArrayBuffer (wasm-bindgen-rayon).
      'Cross-Origin-Embedder-Policy': 'require-corp',
      'Cross-Origin-Opener-Policy': 'same-origin',
    },
  },
});
