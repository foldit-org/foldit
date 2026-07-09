# Workspace Layout

The root `Cargo.toml` defines the workspace. Its members are all root-owned:

| Member | Path | Kind | Role |
| --- | --- | --- | --- |
| `foldit-core` | `crates/foldit-core` | lib | Host-agnostic application logic |
| `foldit-desktop` | `crates/foldit-desktop` | bin (`foldit`) | winit + wry + wgpu desktop shell |
| `foldit-web` | `crates/foldit-web` | cdylib + rlib | wasm32 entry, canvas-mounted |
| `foldit-gui` | `crates/foldit-gui` | lib | Wire and state types, JS-to-Rust bridge |
| `foldit-runner` | `crates/foldit-runner` | rlib + bin (`foldit-worker`) | Plugin runner and worker binary |
| `foldit-python-host` | `crates/foldit-runner/python-host` | cdylib | Python plugin host dylib |
| `xtask` | `xtask` | bin | Build and packaging automation |

`default-members = ["crates/foldit-desktop"]`, so bare cargo commands target the
desktop binary.

## Two non-obvious members

- **`foldit-gui` is in-tree, not a submodule.** It is absent from `.gitmodules`,
  tracked directly in this repository, and its `repository` field points at this
  repo.
- **`foldit-runner` is also in-tree**, likewise absent from `.gitmodules`. Both
  it and the nested `foldit-python-host` at `crates/foldit-runner/python-host`
  are root workspace members; `foldit-core` depends on the runner by path.

## Submodules

`.gitmodules` declares six submodules, each in its own repository:

| Submodule | Path |
| --- | --- |
| `molex` | `crates/molex` |
| `viso` | `crates/viso` |
| `foldit-plugin-sdk` | `crates/foldit-plugin-sdk` |
| `foundry` | `plugins/foundry` |
| `rosetta` | `plugins/rosetta` |
| `simplefold` | `plugins/simplefold` |

The three under `crates/` are listed in the workspace `exclude` set. Without the
exclude, cargo's workspace discovery walks into them from the outside (for
example when maturin runs `cargo metadata` on `crates/molex/Cargo.toml`) and
errors. They build under their own manifests, not as members here. The three
under `plugins/` are Python projects with no Cargo manifest, so cargo never sees
them.

## How the submodule crates are depended on

The root crates depend on viso, molex, and the plugin SDK as published crates.
The root `Cargo.toml` carries a `[patch]` entry for each, all commented out by
default, so a default build resolves all three from their published sources and
needs no submodule checkout:

- **viso**: declared as a git dependency (`tag = "v0.3.11"`). A
  `[patch."https://github.com/foldit-org/viso"]` block redirects it to the local
  `crates/viso` checkout when uncommented.
- **foldit-plugin-sdk**: declared as crates.io `0.1.6`. A `[patch.crates-io]`
  entry redirects it to the local `crates/foldit-plugin-sdk` checkout when
  uncommented.
- **molex**: declared as crates.io `0.7.4` in every crate that uses it. A
  `[patch.crates-io]` entry redirects it to the local `crates/molex` checkout
  when uncommented.

To build a dependency from a local checkout instead of its release, uncomment the
matching `[patch]` entry. See
[Local Development on Submodules](submodules.md).

## Workspace lints

`[workspace.lints.clippy]` mirrors viso's clippy set but sets every level to
`warn` rather than `deny`: the checks report the technical-debt picture across
the root members without gating the build. Members opt in with
`[lints]` plus `workspace = true`. The gates that do fail live in the
[justfile](../tooling/quality-gates.md).
