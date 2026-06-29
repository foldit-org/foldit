# AGENTS.md - Foldit GUI Development Guide

This document provides context and instructions for AI agents working on the Foldit GUI codebase.

---

## Project Overview

Foldit is a scientific puzzle game where players fold proteins. This GUI interfaces with a WebAssembly (WASM) backend that handles the protein simulation. The UI is built with **SolidJS and TypeScript** for optimal performance and developer experience.

### Architecture Principles

1. **SolidJS-First** - All components and logic built using SolidJS primitives and patterns
2. **TypeScript Safety** - Comprehensive type coverage for all components, hooks, and services
3. **Service Layer** - Stateful operations go through singleton services (StateManager, DragManager, EventManager)
4. **Signal-Based Reactivity** - Leverage SolidJS signals for fine-grained reactivity
5. **Component Co-location** - Related logic lives alongside components in normal directories

---

## Directory Structure

```
src/
├── components/           # SolidJS UI components
│   ├── landing/          # Landing/menu screens
│   ├── panels/           # Draggable tool panels
│   ├── popups/           # Modal dialogs
│   ├── util/             # Reusable UI primitives
│   ├── viewport/         # Game viewport overlays
│   └── widgets/          # Persistent UI widgets
│
├── hooks/                # Custom SolidJS hooks and utilities
│   ├── primitives/       # Low-level reactive primitives
│   ├── ui/               # UI-focused hooks (drag, resize, etc.)
│   └── state/            # State management hooks
│
├── data/                 # Static data (puzzles, tips, configs)
├── models/               # TypeScript type definitions
│
├── services/             # Framework-agnostic services
│   ├── adapters/         # SolidJS-specific adapters for services
│   ├── StateManager.ts   # Centralized state access
│   ├── DragManager.ts    # Drag-and-drop functionality
│   ├── EventManager.ts   # WASM event handling
│   ├── Backend.ts        # WASM/Webview communication
│   └── WishingWell.ts    # Async request/response handling
│
├── store/                # SolidJS stores (signals, contexts)
├── styles/               # CSS stylesheets
├── test-utils/           # Testing utilities
└── utils/                # Pure utility functions
```

---

## Key Services

### StateManager (`services/StateManager.ts`)

Singleton that wraps Zustand stores for framework-agnostic access.

```typescript
import { stateManager } from './services/StateManager';

// Get current state (non-reactive)
const ui = stateManager.getState('ui');

// Get store for subscriptions
const store = stateManager.getStore('backendData');
store.subscribe((state) => console.log(state));
```

### Framework Adapters (`services/adapters/`)

Use these instead of direct store imports in components:

```typescript
// In SolidJS components
import { useUI, useBackendData } from '../services/adapters';

const puzzleMenu = useUI(state => state.puzzleMenu);
const score = useBackendData(state => state.currentScore);
```

### DragManager (`services/DragManager.ts`)

Framework-agnostic drag implementation. Use via SolidJS hooks:

```typescript
import { useDraggable } from '../hooks/ui/useDraggable';

const { setRef, position, setPosition } = useDraggable({
  handle: '.header',
  initialPosition: { x: 100, y: 100 },
});

// Usage: <div ref={setRef}>...</div>
```

### EventManager (`services/EventManager.ts`)

Handles events from WASM backend. Framework-agnostic singleton.

### Backend (`services/Backend.ts`)

Communication layer with WASM/Webview. Detects environment automatically.

---

## State Stores

| Store | Purpose |
|-------|---------|
| `UIStore` | UI visibility, text bubbles, language |
| `BackendDataStore` | Game data from WASM (scores, selections, puzzle) |
| `FrontendStateStore` | App state (screen, game mode, loading) |
| `GameProgressStore` | User progress (completion, high scores) |
| `BackendOptionsStore` | View/behavior options synced with backend |

---

## Coding Standards

### Component Guidelines

- Components should be small and focused
- Extract complex logic into hooks
- Use adapter hooks (`useUI`, `useBackendData`) not direct store imports
- Prefer composition over prop drilling

### Hook Guidelines

- Prefix with `use`
- Single responsibility
- For new hooks, prefer framework-agnostic pattern (see `docs/FRAMEWORK_AGNOSTIC_ABSTRACTION_PLAN.md`)

### Naming Conventions

### Naming Conventions

| Type | Convention | Example |
|------|------------|---------|
| Components | PascalCase | `PuzzleMenu.tsx` |
| Hooks | camelCase with `use` prefix | `useDraggable.ts` |
| Services | PascalCase | `StateManager.ts` |
| Utilities | camelCase | `formatScore.ts` |
| Types/Interfaces | PascalCase | `PuzzleData` |
| Constants | SCREAMING_SNAKE_CASE | `MAX_SCORE` |

### File Organization

- One component per file
- Co-locate tests with source (`Component.tsx` + `Component.test.tsx`)
- Group by feature in `components/`, not by type

---

## Testing

### Running Tests

```bash
pnpm test              # Run all tests
pnpm test:watch        # Watch mode
pnpm test:coverage     # With coverage report
```

### Test Patterns

- Use `@testing-library/solid` for component tests
- Mock services using `src/test-utils/`
- Test behavior, not implementation

---

## Build & Development

```bash
pnpm dev               # Start dev server
pnpm build             # Production build
pnpm tsc --noEmit      # Type check only
pnpm lint              # Run ESLint
```

### Environment

- **Build Tool**: Vite 5.x
- **Framework**: SolidJS 1.x + TypeScript 5.x
- **State**: Zustand 5.x
- **Styling**: Tailwind CSS + CSS modules
- **Testing**: Vitest + Testing Library

---

## Backend Communication

### WASM Events

Events from WASM are dispatched as CustomEvents on `window`:

```typescript
// EventManager listens for these
window.dispatchEvent(new CustomEvent('cycleHasEnded', { detail: data }));
```

### Backend Requests

Use `Backend` class for WASM calls:

```typescript
import { Backend } from '../services/Backend';

// Fire-and-forget
Backend.fireEvent(BackendEvent.Undo);

// Request with response
const result = await Backend.request(BackendRequest.GetScore);
```

---

## SolidJS Development Patterns

### Custom Hooks

Create SolidJS hooks for reusable reactive logic:

```typescript
// src/hooks/state/useCounter.ts
import { createSignal } from 'solid-js';

export function useCounter(initial: number = 0) {
  const [count, setCount] = createSignal(initial);
  
  const increment = () => setCount(count() + 1);
  const decrement = () => setCount(count() - 1);
  const reset = () => setCount(initial);
  
  return { count, increment, decrement, reset };
}
```

### SolidJS Components

Components use standard SolidJS patterns with JSX and signals:

```typescript
// src/components/util/Button.tsx
import { Component } from 'solid-js';
import '../../styles/util/buttons.css';

export interface ButtonProps {
  children?: any;
  color?: 'green' | 'gray';
  disabled?: boolean;
  callback: () => void;
}

const Button: Component<ButtonProps> = (props) => {
  const {
    children,
    color = 'green',
    disabled = false,
    callback
  } = props;

  return (
    <button
      class={`button-${color} ${disabled ? 'pointer-events-none' : ''}`}
      disabled={disabled}
      onMouseUp={callback}
    >
      {children}
    </button>
  );
};

export default Button;
```

#### Component with State and Effects

```typescript
// src/components/util/Counter.tsx
import { Component, createSignal, createEffect } from 'solid-js';

interface CounterProps {
  initial?: number;
}

const Counter: Component<CounterProps> = (props) => {
  const [count, setCount] = createSignal(props.initial ?? 0);

  createEffect(() => {
    console.log('Count changed:', count());
  });

  return (
    <div class="counter">
      <span>Count: {count()}</span>
      <button onClick={() => setCount(count() + 1)}>+</button>
      <button onClick={() => setCount(count() - 1)}>-</button>
    </div>
  );
};

export default Counter;
```

### Key SolidJS Patterns

1. **Signal-based State**: Use `createSignal` for reactive state
2. **Fine-grained Reactivity**: SolidJS only updates what changes
3. **JSX with Signals**: Access signal values with `signal()` in JSX
4. **Effects**: Use `createEffect` for side effects
5. **Computed Values**: Use `createMemo` for derived state
6. **Props**: SolidJS props are reactive by default

---

## Common Tasks

### Adding a New Panel

1. Create component in `src/components/panels/`
2. Add visibility state to `UIStore`
3. Add toggle in relevant menu/toolbar
4. Use `Panel` wrapper from `components/util/`

### Adding a New SolidJS Component

Create components using standard SolidJS patterns:

```typescript
// src/components/util/MyComponent.tsx
import { Component, createSignal } from 'solid-js';

export interface MyComponentProps {
  title: string;
  onAction: () => void;
}

const MyComponent: Component<MyComponentProps> = (props) => {
  const [active, setActive] = createSignal(false);

  return (
    <div class="my-component">
      <h2>{props.title}</h2>
      <button
        class={active() ? 'active' : ''}
        onClick={() => {
          setActive(!active());
          props.onAction();
        }}
      >
        Click me
      </button>
    </div>
  );
};

export default MyComponent;
```

### Adding a New Store Selector

1. Add to appropriate store in `src/store/`
2. Access via adapter: `useUI(state => state.newField)`

### Adding a New Service

1. Create in `src/services/`
2. Keep framework-agnostic (no React imports)
3. Create adapter hook if needed for React integration

---

## Troubleshooting

### WASM Not Loading

- Check browser console for CORS errors
- Ensure dev server is running with correct headers
- Verify `.wasm` files are served with correct MIME type

### State Not Updating

- Verify using adapter hooks, not direct store access
- Check selector is returning new reference (use `shallow` for objects)
- Confirm component is mounted when state changes

### Drag Not Working

- Ensure element has `position: absolute` or `position: fixed`
- Check handle selector matches actual DOM element
- Verify DragManager instance exists (check console)

---

## Resources

- [Zustand Documentation](https://github.com/pmndrs/zustand)
- [Vite Documentation](https://vitejs.dev/)
- [React Documentation](https://react.dev/)
- [SolidJS Documentation](https://www.solidjs.com/)
