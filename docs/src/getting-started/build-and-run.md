# Build and Run

## Build

```bash
cargo build
```

The workspace `default-members` is `foldit-desktop`, so a plain build produces
the desktop binary, named `foldit` (`foldit-desktop`'s `[[bin]]` sets
`name = "foldit"` and `default-run = "foldit"`). There is no root `src/`; the
entry point is `crates/foldit-desktop/src/main.rs`.

A plain `cargo build` does not build the plugin worker. The worker
(`foldit-worker`) lives in the `foldit-runner` submodule, which is excluded
from the workspace, and the Python host dylib is loaded at runtime rather than
linked. Build both of those with:

```bash
cargo xtask build-host
```

`build-host` puts `foldit-worker` and the `foldit-python-host` dylib next to
the desktop exe under `target/<profile>/`, which is where the running app looks
for them. Run it after pulling or changing runner code; `cargo run` rebuilds
neither. See [xtask Commands](../tooling/xtask.md).

## Run

```bash
cargo run -- <PDB_ID>
# example:
cargo run -- 1bfe
```

The first CLI argument names the structure to load. `main.rs` resolves it
through `foldit_core::puzzle::resolve_structure_path`: an existing file path is
used as-is, and a four-character PDB id is downloaded from RCSB and cached under
`assets/models/`.

With no argument, the app starts at the menus (the `Landing` phase) and loads a
structure later through the UI:

```bash
cargo run
```

## Controls

Built-in camera, focus, and selection controls (handled by the engine and core,
not the binary):

| Input | Action |
| --- | --- |
| `Q` | Recenter the camera on the focused entity |
| `Tab` | Cycle focus (whole session, then each entity, then back); over a hovered residue, toggle its segment-info panel |
| `` ` `` | Reset focus to the whole scene |
| `Esc` | Cancel the in-progress operation |
| Left-drag on a residue | Pull |
| Right-drag residue to residue | Create a band |
| Mouse | Rotate / zoom the camera |

Action hotkeys are not hardcoded in the binary. Each plugin declares its own in
its `plugin.toml` manifest, as a `hotkey` field on a button using the winit
`KeyCode` spelling (`hotkey = "KeyW"`). The Rosetta plugin, for example, binds
`W` to Wiggle, `S` to Shake, and `B` to Rebuild. On the desktop build a key
press is resolved to the owning plugin op and dispatched; if two plugins claim
the same key, the first one registered wins.

## Release build

```bash
cargo build --release
```

For OS installers (a packaged `.app`, `.dmg`, `.deb`, AppImage, MSI), use
`cargo xtask package`; see [Assets and Bundling](../tooling/assets-bundling.md).
