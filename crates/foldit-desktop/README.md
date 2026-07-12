# foldit-desktop

The desktop application. This is the binary you build and run to play Foldit on
your machine; it produces an executable named `foldit`.

It is deliberately thin. All the real logic lives in
[`foldit-core`](../foldit-core); this crate only owns the desktop shell: it
parses the command line, sets up logging, resolves the structure to load,
constructs a `foldit_core::App`, and hands it to the window event loop
(`src/window.rs`), which drives the render surface with winit (windowing), wry
(the embedded webview for the UI), and wgpu (the GPU).

## Run it

```bash
cargo run -- <PDB_ID_or_file>
# example:
cargo run -- 1ubq
```

A four-character PDB id is downloaded from RCSB and cached under
`assets/models/`; a file path is loaded as-is. With no argument the app starts
at the menus and you load a structure through the UI.

Note that a plain build does not compile the plugin worker or the Python host —
run `cargo xtask build-host` once (and `cargo xtask setup` for the plugins) so
scoring and the scientific backends work. See
[Build and Run](../../docs/src/getting-started/build-and-run.md).

## Controls

| Input | Action |
| --- | --- |
| Left-drag on a residue | Pull |
| Right-drag residue to residue | Create a band |
| Mouse | Rotate / zoom the camera |
| `Q` | Recenter the camera on the focused entity |
| `Tab` | Cycle focus; over a hovered residue, toggle its segment-info panel |
| `` ` `` | Reset focus to the whole scene |
| `Esc` | Cancel the operation / clear selection / clear bands |

Action hotkeys (Wiggle, Shake, Predict, and so on) are **not** hardcoded here —
each plugin declares its own keys in its `plugin.toml`, and the desktop build
resolves a key press to the owning plugin op. The Rosetta plugin, for example,
binds `W` to Wiggle, `S` to Shake, and `B` to Rebuild.

## Packaging

For OS installers (`.app`, `.dmg`, `.deb`, AppImage, MSI) use `cargo xtask
package`; see [Assets and Bundling](../../docs/src/tooling/assets-bundling.md).
