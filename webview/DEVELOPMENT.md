# Foldit GUI Development Guide

This guide provides detailed instructions for developers working on the Foldit GUI codebase.

## Table of Contents

1. [Development Environment Setup](#development-environment-setup)
2. [Project Structure](#project-structure)
3. [SolidJS Development Patterns](#solidjs-development-patterns)
4. [State Management](#state-management)
5. [Backend Communication](#backend-communication)
6. [Testing](#testing)
7. [Code Quality](#code-quality)
8. [Troubleshooting](#troubleshooting)

## Development Environment Setup

### Prerequisites

- **Node.js 18+** - [Download](https://nodejs.org/)
- **pnpm 8+** - `npm install -g pnpm` (or use npm/yarn)
- **TypeScript 5.0+** - Included with the project
- **Git** - Version control

### Initial Setup

```bash
# Clone the repository
git clone https://github.com/foldit-org/foldit-gui.git
cd foldit-gui

# Install dependencies
pnpm install

# Start development server
pnpm dev
```

The development server will start at `http://localhost:5173`.

### Environment Variables

Create a `.env` file in the root directory for environment-specific configurations:

```env
# Development settings
VITE_APP_ENV=development
VITE_API_URL=http://localhost:3000
VITE_MATOMO_SITE_ID=1
```

### IDE Configuration

Recommended VS Code extensions:
- **TypeScript** (built-in)
- **SolidJS** (for JSX syntax highlighting)
- **ESLint** (code quality)
- **Prettier** (code formatting)
- **Tailwind CSS IntelliSense** (CSS utility classes)

## Project Structure

```
foldit-gui/
├── src/
│   ├── components/           # SolidJS UI components
│   │   ├── landing/         # Landing/menu screens
│   │   ├── panels/          # Draggable tool panels
│   │   ├── popups/          # Modal dialogs
│   │   ├── util/            # Reusable UI primitives
│   │   ├── viewport/        # Game viewport overlays
│   │   └── widgets/         # Persistent UI widgets
│   ├── services/            # Framework-agnostic services
│   │   ├── adapters/        # SolidJS-specific adapters
│   │   ├── StateManager.ts  # Centralized state access
│   │   ├── DragManager.ts   # Drag-and-drop functionality
│   │   ├── EventManager.ts  # WASM event handling
│   │   └── Backend.ts       # WASM/Webview communication
│   ├── hooks/               # Custom SolidJS hooks
│   ├── store/               # Zustand stores
│   ├── models/              # TypeScript type definitions
│   ├── data/                # Static data
│   ├── styles/              # CSS stylesheets
│   └── utils/               # Pure utility functions
├── test/                    # Test files
├── docs/                    # Documentation
└── public/                  # Static assets
```

### Key Directories Explained

**`src/components/`** - UI Components organized by feature area
- `landing/`: Initial screens, menus, and login
- `panels/`: Draggable tool panels (Undo, Alignment, Blueprint, etc.)
- `popups/`: Modal dialogs and overlays
- `util/`: Reusable UI primitives (Button, Panel, Slider, etc.)
- `viewport/`: Game viewport overlays and rendering
- `widgets/`: Persistent UI widgets (ScoreBar, SideBar, etc.)

**`src/services/`** - Business logic and backend communication
- Framework-agnostic services that can be used with any UI framework
- Adapters provide framework-specific integration

**`src/hooks/`** - Custom SolidJS hooks
- State management hooks
- UI interaction hooks (drag, resize, etc.)
- Backend integration hooks

## SolidJS Development Patterns

### Component Structure

```typescript
// src/components/util/ExampleComponent.tsx
import { Component, createSignal } from 'solid-js';

export interface ExampleComponentProps {
  title: string;
  onAction: () => void;
}

const ExampleComponent: Component<ExampleComponentProps> = (props) => {
  const [active, setActive] = createSignal(false);

  return (
    <div class="example-component">
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

export default ExampleComponent;
```

### Key SolidJS Concepts

1. **Signals**: Use `createSignal` for reactive state
2. **Effects**: Use `createEffect` for side effects
3. **Memos**: Use `createMemo` for computed values
4. **Props**: SolidJS props are reactive by default

### Component Guidelines

- **Small and focused**: Components should do one thing well
- **Props interface**: Always define a TypeScript interface for props
- **Signal-based state**: Use signals for component state, not class properties
- **Effect cleanup**: Always return cleanup functions from effects
- **Composition over inheritance**: Build complex UIs from simple components

## State Management

### Zustand Stores

The application uses Zustand for state management with a `StateManager` wrapper:

```typescript
// Access state in components
import { useUI, useBackendData } from '../services/adapters';

const ExampleComponent: Component = () => {
  const puzzleMenu = useUI(state => state.puzzleMenu);
  const score = useBackendData(state => state.currentScore);
  
  // Component logic...
};
```

### Available Stores

| Store | Location | Purpose |
|-------|----------|---------|
| `UIStore` | `src/store/UIStore.ts` | UI visibility, text bubbles, language |
| `BackendDataStore` | `src/store/BackendDataStore.ts` | Game data from WASM |
| `FrontendStateStore` | `src/store/FrontendStateStore.ts` | App state (screen, game mode) |
| `GameProgressStore` | `src/store/GameProgressStore.ts` | User progress |
| `BackendOptionsStore` | `src/store/BackendOptionsStore.ts` | View/behavior options |

### Creating a New Store

1. Create store file in `src/store/`
2. Define interface and create store with Zustand
3. Add to `StateManager` if needed for framework-agnostic access
4. Create adapter hook in `src/services/adapters/`

## Backend Communication

### Communication Flow

```
┌─────────┐      ┌─────────────┐      ┌─────────┐
│         │      │             │      │         │
│ SolidJS │─────▶│   Backend   │─────▶│  WASM   │
│Component│      │   Service   │      │ Backend │
│         │◀─────│             │◀─────│         │
└─────────┘      └─────────────┘      └─────────┘
     │                   │                   │
     ▼                   ▼                   ▼
  Zustand          EventManager        CustomEvent
   Store                                 Dispatch
```

### Sending Requests to Backend

```typescript
import { Backend, BackendEvent } from '../services/backend';

// Fire-and-forget
Backend.callBackend(BackendEvent.Undo);

// Request with response
const result = await Backend.request(BackendRequest.GetScore);
```

### Receiving Events from Backend

Events from WASM are dispatched as CustomEvents on `window`:

```typescript
// EventManager listens for these
window.dispatchEvent(new CustomEvent('cycleHasEnded', { detail: data }));
```

## Testing

### Running Tests

```bash
# Run all tests
pnpm test

# Run tests in watch mode
pnpm test:watch

# Run tests with UI
pnpm test:ui

# Run tests with coverage
pnpm test:coverage
```

### Test Structure

- **Component Tests**: Use `@solidjs/testing-library` for component testing
- **Hook Tests**: Test custom hooks in isolation
- **Service Tests**: Test framework-agnostic services
- **Utility Tests**: Test pure utility functions

### Example Component Test

```typescript
// src/components/util/Button.test.tsx
import { render, fireEvent } from '@solidjs/testing-library';
import { describe, it, expect } from 'vitest';
import Button from './Button';

describe('Button', () => {
  it('calls callback when clicked', () => {
    let clicked = false;
    const { getByRole } = render(() => 
      <Button callback={() => clicked = true}>Click me</Button>
    );
    
    fireEvent.click(getByRole('button'));
    expect(clicked).toBe(true);
  });
});
```

## Code Quality

### Linting

```bash
# Run ESLint
pnpm lint

# Fix automatically fixable issues
pnpm lint --fix
```

### Type Checking

```bash
# Run TypeScript compiler without emitting files
pnpm tsc --noEmit
```

### Code Style Guidelines

1. **TypeScript**: Use strict mode, explicit types for public APIs
2. **SolidJS**: Follow SolidJS best practices, use signals for state
3. **Naming**: Use descriptive names, follow project conventions
4. **Imports**: Group imports (SolidJS, styles, components, services)
5. **Comments**: Document complex logic, use JSDoc for public APIs

## Troubleshooting

### Common Issues

#### WASM Not Loading
- Check browser console for CORS errors
- Ensure dev server is running with correct headers
- Verify `.wasm` files are served with correct MIME type

#### State Not Updating
- Verify using adapter hooks, not direct store access
- Check selector is returning new reference (use `shallow` for objects)
- Confirm component is mounted when state changes

#### Drag Not Working
- Ensure element has `position: absolute` or `position: fixed`
- Check handle selector matches actual DOM element
- Verify DragManager instance exists (check console)

#### Build Errors
- Clear node_modules and reinstall: `rm -rf node_modules && pnpm install`
- Clear TypeScript cache: `pnpm tsc --build --clean`
- Check TypeScript version compatibility

### Debugging Tips

1. **Browser DevTools**: Use SolidJS DevTools extension
2. **Console Logging**: Use `console.log` with reactive values in effects
3. **State Inspection**: Use Redux DevTools with Zustand
4. **Network Inspection**: Monitor WebSocket/WASM communication

## Performance Optimization

### SolidJS Performance Tips

1. **Fine-grained reactivity**: Signals only update what changes
2. **Memoization**: Use `createMemo` for expensive calculations
3. **Lazy loading**: Use `lazy` for code splitting
4. **Virtual lists**: For long lists, use virtualization

### Bundle Optimization

1. **Code splitting**: Split by route/feature
2. **Tree shaking**: Ensure unused code is removed
3. **Asset optimization**: Compress images, use modern formats

## Deployment

### Web (WASM) Deployment

```bash
# Production build
pnpm build

# Preview build locally
pnpm preview

# Deploy to static hosting
# Copy dist/ contents to your web server
```

### Native (WebView) Deployment

1. Build GUI: `pnpm build`
2. Bundle with native application
3. Inject into WebView with appropriate bridge

## Further Reading

- [SolidJS Documentation](https://solidjs.com/docs)
- [TypeScript Handbook](https://www.typescriptlang.org/docs/)
- [Zustand Documentation](https://github.com/pmndrs/zustand)
- [Vite Documentation](https://vitejs.dev/guide/)
- [Tailwind CSS Documentation](https://tailwindcss.com/docs)