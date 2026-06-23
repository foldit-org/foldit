/**
 * Web entry point. Mounts the SolidJS UI overlay AND hands the canvas to
 * the wasm-bound `FolditApp::start`. The transport barrel is already
 * pointing at `transport/wasm.ts` (Vite alias resolves at build time when
 * FOLDIT_TARGET=wasm).
 */

import { render } from 'solid-js/web';
import App from './App';
import { mountCanvas } from './transport/wasm';
import './index.css';

render(() => <App />, document.getElementById('root')!);

const canvas = document.getElementById('foldit-canvas') as HTMLCanvasElement | null;
if (!canvas) {
  throw new Error('foldit-web: <canvas id="foldit-canvas"> not found in index-web.html');
}

mountCanvas(canvas).catch(err => {
  console.error('[foldit-web] mountCanvas failed:', err);
});
