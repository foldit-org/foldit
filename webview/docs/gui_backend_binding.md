# Foldit GUI/Backend Binding Architecture

This document describes the 2-way data binding and communication mechanism between the Foldit GUI (TypeScript) and the C++ Backend (Rosetta Interactive). The system is designed to support two distinct runtime environments: **Web (Emscripten/Wasm)** and **Native (WebView)**.

## Overview

The communication architecture relies on a **bridge pattern** where the GUI sends requests via a global `Module` object and the backend sends updates via a global `CustomEvent` bus. All data payloads are serialized as JSON strings.

### Key Components

1.  **`src/services/backend.ts`**: The singleton service providing the API for GUI-to-backend communication. It abstracts the environment differences.
2.  **`src/services/EventManager.ts`**: The listener service that handles unsolicited events from the backend and updates the GUI state.
3.  **`src/services/WishingWell.ts`**: Manages asynchronous request/response cycles (promises) for backend queries.
4.  **`src/store/BackendDataStore.ts`**: A Zustand store acting as the single source of truth for data received from the backend.
5.  **`src/services/WasmManager.ts`**: (Web-only) Handles Emscripten module initialization and filesystem management.
6.  **`window.Module`**: The global bridge object. In Web, this is the Emscripten module. In Native, this is a mock object injected by the C++ host.

---

## 1. GUI to Backend (G -> B)

The GUI initiates actions (e.g., "fold", "undo", "get score") by calling methods on the `backend` service.

### Interface
The `backend` service exposes two primary methods:

*   **`callBackend(event, payload)`**: Fire-and-forget. Used for actions where no immediate return value is needed (e.g., input events, UI toggles).
*   **`requestFromBackend(type, payload)`**: Returns a `Promise`. Used via `WishingWell.makeWish()` to get data back (e.g., "getPuzzleData").

### Implementation Details

The `backend` service detects the environment (`EMSCRIPTEN` or `WEBVIEW`) but uses a unified implementation that relies on `window.Module.FolditBackend`.

```typescript
// src/services/backend.ts
window.Module.FolditBackend!.call_backend(event, JSON.stringify(payload));
```

#### A. Web (Emscripten) Path
1.  `App.tsx` detects `BackendEnvironment.EMSCRIPTEN` and calls `initWasm()`.
2.  `initWasm` initializes the Emscripten `Module`.
3.  Emscripten binds the C++ function `call_backend` to `Module.FolditBackend.call_backend`.
4.  Calls execute directly into Wasm memory.

#### B. Native (WebView) Path
1.  The C++ host application (e.g., Qt/WebView) injects a JavaScript shim *before* React loads.
2.  This shim creates `window.Module` and `window.Module.FolditBackend`.
3.  `App.tsx` detects `BackendEnvironment.WEBVIEW` (via `window.isWebview`) and **skips** `initWasm()`.
4.  When `backend.ts` calls `call_backend`, the injected shim intercepts the call.
5.  The shim passes the JSON string to the native C++ layer (e.g., via `window.chrome.webview.postMessage`, `QWebChannel`, or `window.external`).

---

## 2. Backend to GUI (B -> G)

The backend sends data to the GUI either as a direct response to a request or as an unsolicited update (e.g., "score changed").

### Mechanism: Global Event Bus
The backend executes JavaScript code to dispatch a `CustomEvent` on the `document` (or `window`) object.

```javascript
// Conceptually executed by C++
const event = new CustomEvent('FolditEvent', { detail: { type: 'ScoreUpdate', data: { ... } } });
document.dispatchEvent(event);
```

### Listeners

#### A. `WishingWell.ts` (Request/Response)
*   Listens for `wishGranted` (success) and `wishDenied` (failure) events.
*   Uses a unique `wish_id` to match the response to the pending Promise created by the GUI request.

#### B. `EventManager.ts` (Unsolicited Updates)
*   Listens for specific event types (e.g., `puzzleLoaded`, `updateScore`, `loginStatus`).
*   **Action**: Parses the JSON payload and calls setters on the **`BackendDataStore`**.

### State Updates
1.  `BackendDataStore` updates its internal state (Zustand).
2.  React components subscribed to the store (via `useBackendDataStore`) re-render automatically.

---

## Summary Flow

### Web (Wasm)
1.  **Init**: `initWasm` loads `foldit.wasm`.
2.  **G->B**: `backend.ts` -> `Module.FolditBackend.call_backend` -> **Wasm Heap**.
3.  **B->G**: C++ (Emscripten) -> `emscripten_run_script` -> `document.dispatchEvent`.

### Native (WebView)
1.  **Init**: C++ Host injects `window.Module` & `window.isWebview`. `initWasm` is skipped.
2.  **G->B**: `backend.ts` -> `Module.FolditBackend` (Shim) -> **Native Bridge** (IPC).
3.  **B->G**: C++ Native -> `webview->eval()` -> `document.dispatchEvent`.
