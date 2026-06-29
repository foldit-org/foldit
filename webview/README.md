# Foldit GUI

A modern, high-performance GUI for the Foldit scientific puzzle game, built with **SolidJS** and **TypeScript**. This interface connects to a WebAssembly (WASM) backend that handles protein folding simulation and game logic.

[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![SolidJS](https://img.shields.io/badge/framework-SolidJS-2c4f7c?logo=solid)](https://solidjs.com)
[![TypeScript](https://img.shields.io/badge/language-TypeScript-3178c6?logo=typescript)](https://typescriptlang.org)
[![Vite](https://img.shields.io/badge/build-Vite-646cff?logo=vite)](https://vitejs.dev)

## Overview

Foldit is a revolutionary scientific discovery game where players solve puzzles by folding proteins. This GUI provides an intuitive interface for interacting with the protein folding simulation, featuring:

- **Real-time 3D protein visualization** (via WebGL/Canvas)
- **Interactive tools** for manipulating protein structures
- **Game progress tracking** and scoring system
- **Multi-platform support** (Web/WASM and Native/WebView)
- **Localization** for international accessibility

## Architecture

The GUI follows a **service-oriented architecture** with clear separation between UI components, state management, and backend communication:

```
src/
├── components/           # SolidJS UI components
│   ├── landing/         # Landing/menu screens
│   ├── panels/          # Draggable tool panels (Undo, Alignment, Blueprint, etc.)
│   ├── popups/          # Modal dialogs
│   ├── util/            # Reusable UI primitives
│   ├── viewport/        # Game viewport overlays
│   └── widgets/         # Persistent UI widgets
├── services/            # Framework-agnostic services
│   ├── adapters/        # SolidJS-specific adapters for services
│   ├── StateManager.ts  # Centralized state access
│   ├── DragManager.ts   # Drag-and-drop functionality
│   ├── EventManager.ts  # WASM event handling
│   └── Backend.ts       # WASM/Webview communication
├── hooks/               # Custom SolidJS hooks and utilities
├── store/               # Zustand stores (signals, contexts)
├── models/              # TypeScript type definitions
├── data/                # Static data (puzzles, tips, configs)
├── styles/              # CSS stylesheets
└── utils/               # Pure utility functions
```

### Key Design Principles

1. **SolidJS-First**: All components and logic built using SolidJS primitives and patterns
2. **TypeScript Safety**: Comprehensive type coverage for all components, hooks, and services
3. **Service Layer**: Stateful operations go through singleton services (StateManager, DragManager, EventManager)
4. **Signal-Based Reactivity**: Leverage SolidJS signals for fine-grained reactivity
5. **Framework-Agnostic Core**: Core services remain independent of UI framework

## Quick Start

### Prerequisites

- **Node.js** 18+ and **pnpm** 8+ (or npm/yarn)
- **TypeScript** 5.0+
- **Modern browser** with WebAssembly support

### Installation

```bash
# Clone the repository
git clone https://github.com/foldit-org/foldit-gui.git
cd foldit-gui

# Install dependencies
pnpm install

# Start development server
pnpm dev
```

The application will be available at `http://localhost:5173`.

### Development Commands

```bash
# Start development server with hot reload
pnpm dev

# Build for production
pnpm build

# Run TypeScript type checking
pnpm tsc --noEmit

# Run tests
pnpm test

# Run tests in watch mode
pnpm test:watch

# Run linting
pnpm lint
```

## Backend Communication

The GUI communicates with a C++ backend compiled to WebAssembly (WASM) for web deployment, or through a native WebView bridge for desktop applications.

### Communication Flow

1. **GUI → Backend**: Actions are sent via `Backend.callBackend()` or `Backend.request()`
2. **Backend → GUI**: Events are dispatched as CustomEvents on `window` and handled by `EventManager`
3. **State Updates**: EventManager updates Zustand stores, triggering SolidJS component updates

### Key Services

- **`Backend`**: Handles communication with WASM/WebView backend
- **`EventManager`**: Processes incoming events from backend
- **`StateManager`**: Centralized state access wrapper around Zustand stores
- **`DragManager`**: Framework-agnostic drag implementation

## Development Guide

### Adding a New Component

1. Create component in appropriate `src/components/` subdirectory
2. Use existing patterns: SolidJS components with TypeScript interfaces
3. Access state via adapter hooks (`useUI`, `useBackendData`) not direct store imports
4. Add CSS in co-located or `src/styles/` directory

### Adding a New Service

1. Create in `src/services/` as a framework-agnostic singleton
2. Create adapter hook in `src/services/adapters/` if needed for SolidJS integration
3. Follow existing service patterns (see `StateManager.ts`)

### State Management

The application uses **Zustand** stores with a **StateManager** wrapper:

```typescript
// Access state in components
import { useUI, useBackendData } from './services/adapters';

const puzzleMenu = useUI(state => state.puzzleMenu);
const score = useBackendData(state => state.currentScore);
```

### Available Stores

| Store | Purpose | Key Data |
|-------|---------|----------|
| `UIStore` | UI visibility, text bubbles, language | `puzzleMenu`, `textBubble`, `language` |
| `BackendDataStore` | Game data from WASM | `currentScore`, `undoHistory`, `selection` |
| `FrontendStateStore` | App state | `screen`, `gameMode`, `loading` |
| `GameProgressStore` | User progress | `completedPuzzles`, `highScores` |
| `BackendOptionsStore` | View/behavior options | `viewOptions`, `behaviorOptions` |

## Testing

```bash
# Run all tests
pnpm test

# Run tests with coverage
pnpm test:coverage

# Run tests in watch mode
pnpm test:watch
```

Tests use **Vitest** and **@solidjs/testing-library**. Follow these patterns:

1. Test behavior, not implementation details
2. Mock services using `src/test-utils/`
3. Use `@testing-library/solid` for component tests

## Building and Deployment

### Web (WASM) Build

```bash
# Production build
pnpm build

# Preview production build locally
pnpm preview
```

The build output is in `dist/` directory, ready to be served with any static file server.

### Native (WebView) Build

For native desktop applications, the GUI is bundled and injected into a WebView. The build process is identical, but the backend communication uses a different bridge (injected by the native host).

## Contributing

1. **Fork** the repository
2. **Create a feature branch**: `git checkout -b feature/amazing-feature`
3. **Commit changes**: Follow [Conventional Commits](https://www.conventionalcommits.org/)
4. **Push to branch**: `git push origin feature/amazing-feature`
5. **Open a Pull Request**

### Code Standards

- Follow TypeScript strict mode
- Use SolidJS best practices (signals, effects, memos)
- Maintain framework-agnostic service layer
- Write comprehensive tests for new functionality
- Update documentation for API changes

## Documentation

- **[AGENTS.md](./AGENTS.md)**: Development guide for AI assistants
- **[docs/](./docs/)**: Architecture and design documents
- **Code Comments**: Comprehensive JSDoc-style comments throughout codebase

## Related Projects

- **[Foldit Backend](https://github.com/petridecus/foldit-backend)**: C++ game logic and simulation engine
- **[Foldit WASM](https://github.com/petridecus/foldit-wasm)**: WebAssembly build pipeline
- **[Foldit Native](https://github.com/petridecus/foldit-native)**: Desktop application wrapper

## License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details.

## Acknowledgments

- University of Washington's Center for Game Science
- Rosetta Commons for molecular modeling software
- All Foldit players and contributors

---

**Foldit GUI Team** | [Report Issue](https://github.com/foldit-org/foldit-gui/issues)