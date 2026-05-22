# Foldit

A reimagined Foldit decoupled into GUI, render engine, and backends for Rosetta and ML.

## Prerequisites

- **Rust**: Install via [rustup](https://rustup.rs)
- **Pixi**: For ML Python environments - [pixi.sh](https://pixi.sh) (optional, only for ML features)

## Quick Start

### Build

```bash
cargo build
```

This builds both:
- `foldit` - Main application
- `foldit-runner-worker` - ML worker process (needed at runtime)

### Run

```bash
cargo run -- <PDB_ID>
# Example:
cargo run -- 1bfe
```

## ML Backend Setup

To use ML-based structure prediction:

### 1. Install Python Environment

```bash
cd crates/foldit-runner

# GPU-enabled
pixi install -e foundry

# Or CPU-only
pixi install -e foundry-cpu
```

### 2. Generate Protobuf Bindings

```bash
pixi run generate-proto
```

### 3. Run the Application

```bash
cd ../..  # Back to workspace root
cargo run -- 1bfe
```

The application will spawn `foldit-runner-worker` processes as needed.

## Build Tasks (xtask)

The project includes an xtask system for common operations:

```bash
cargo xtask setup-ml        # Install all Python environments
cargo xtask bundle          # Create release bundle with ML runtime
cargo xtask bundle --cpu-only  # CPU-only bundle
cargo xtask download-models # Download model weights
```

Or via pixi (convenience wrappers):

```bash
pixi run setup-ml
pixi run bundle
pixi run download-models
```

## Project Structure

```
foldit/
├── src/
│   └── main.rs              # Main application
├── crates/
│   ├── foldit-runner/           # ML inference library
│   ├── viso/                # GPU render engine (external; optional submodule)
│   └── molex/               # Molecular structure parsing (external; optional submodule)
├── xtask/                   # Build automation
├── Cargo.toml               # Workspace manifest
└── pixi.toml                # Pixi task aliases
```

## Features

- **GPU-accelerated rendering** via wgpu
- **ML structure prediction** via foldit-runner (SimpleFold, RFdiffusion, ESMFold)
- **Zero-copy IPC** between main app and ML workers via Iceoryx2 shared memory
- **Rosetta integration** (optional, requires separate Rosetta installation)
- **Cross-platform**: macOS (Apple Silicon), Linux, Windows

## Development

### Release Build

```bash
cargo build --release
```

### Run Tests

```bash
cargo test --workspace
```

### Working on viso or molex locally

`viso` and `molex` are external crates (git tag and crates.io respectively); a
plain clone builds against the published versions and does not need their
submodules. To hack on either alongside foldit:

```bash
# 1. Pull the submodule you want to edit
git submodule update --init crates/viso     # or crates/molex

# 2. Uncomment the matching [patch] block at the bottom of Cargo.toml
# 3. cargo build now uses your local checkout
```

Each toggle is independent — you can have viso local and molex published, or
vice versa, or both local.

### Create Distribution Bundle

```bash
cargo xtask bundle
# Output: ./bundle/
```

The bundle includes:
- `foldit` binary
- `foldit-runner-worker` binary
- Python ML runtime (bundled via pixi)
- Model weights (if downloaded)
