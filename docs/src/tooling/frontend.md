# The Frontend

The UI lives under `webview/`. It is a SolidJS application built with Vite, with
bun as the package manager and runtime (xtask's `build-gui` runs `bun run
build`). Its npm package is named `foldit-gui`, which is unrelated to the Rust
crate of the same name beyond consuming the wire shapes that crate defines.

The same front end serves both shells. On desktop it runs inside the wry
webview; on web it talks to the wasm build. Its backend module abstracts the two
delivery channels, but the message shapes are identical: it receives
dirty-section pushes of `GuiState` and sends back commands, op dispatches, and
requests. See [State and the GUI Bridge](../architecture/gui-bridge.md).

## Scripts

From `webview/package.json`:

| Script | Purpose |
| --- | --- |
| `dev` | Vite dev server (desktop target) |
| `dev:web` | Vite dev server with `FOLDIT_TARGET=wasm` |
| `build` | Production build (consumed by `xtask build-gui` into `assets/gui`) |
| `build:web` | Production build for the wasm target (consumed by `xtask package-web`) |
| `test` / `test:watch` / `test:ui` | Vitest |
| `lint` | ESLint |
| `preview` | Vite preview of a build |

## Further reading

The front end has its own documentation in the `webview/` directory:

- `webview/README.md` -- overview and component layout.
- `webview/DEVELOPMENT.md` -- development workflow.
- `webview/AGENTS.md` -- conventions for working in the front end.
