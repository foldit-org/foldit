# Foldit-RS

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
- `foldit-rs` - Main application
- `foldit-ml-worker` - ML worker process (needed at runtime)

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
cd crates/foldit-ml

# GPU-enabled
pixi install -e foundry

# Or CPU-only
pixi install -e foundry-cpu
```

### 2. Generate Protobuf Bindings

```bash
pixi run generate-proto
```

### 3. Download Model Weights

```bash
pixi run download-foundry
```

### 4. Run the Application

```bash
cd ../..  # Back to workspace root
cargo run -- 1bfe
```

The application will spawn `foldit-ml-worker` processes as needed.

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
foldit-rs/
├── src/
│   └── main.rs              # Main application
├── crates/
│   ├── foldit-ml/           # ML inference library
│   ├── foldit-render/       # GPU render engine
│   └── foldit-conv/         # Format conversion
├── xtask/                   # Build automation
├── Cargo.toml               # Workspace manifest
└── pixi.toml                # Pixi task aliases
```

## Features

- **GPU-accelerated rendering** via wgpu
- **ML structure prediction** via foldit-ml (SimpleFold, RFdiffusion, ESMFold)
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

### Create Distribution Bundle

```bash
cargo xtask bundle
# Output: ./bundle/
```

The bundle includes:
- `foldit-rs` binary
- `foldit-ml-worker` binary
- Python ML runtime (bundled via pixi)
- Model weights (if downloaded)
