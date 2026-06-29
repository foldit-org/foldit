# Workspace Layout

The root `Cargo.toml` defines the workspace. Its members are all root-owned:

| Member | Path | Kind | Role |
| --- | --- | --- | --- |
| `foldit-core` | `crates/foldit-core` | lib | Host-agnostic application logic |
| `foldit-desktop` | `crates/foldit-desktop` | bin (`foldit`) | winit + wry + wgpu desktop shell |
| `foldit-web` | `crates/foldit-web` | cdylib + rlib | wasm32 entry, canvas-mounted |
| `foldit-gui` | `crates/foldit-gui` | lib | Wire and state types, JS-to-Rust bridge |
| `foldit-python-host` | `crates/foldit-runner/python-host` | cdylib | Python plugin host dylib |
| `xtask` | `xtask` | bin | Build and packaging automation |

`default-members = ["crates/foldit-desktop"]`, so bare cargo commands target the
desktop binary.

## Two non-obvious members

- **`foldit-gui` is in-tree, not a submodule.** It is absent from
  `.gitmodules`, tracked directly in this repository, and its `repository`
  field points at this repo. (A comment in the root `Cargo.toml` describes it
  as submodule-resident; that comment is stale.)
- **`foldit-python-host` lives inside the `foldit-runner` submodule** at
  `crates/foldit-runner/python-host`, yet it is a root workspace member. It is
  built from this workspace even though its source sits inside a submodule
  checkout.

## Submodules

`.gitmodules` declares four submodules, each in its own repository:

| Submodule | Path |
| --- | --- |
| `foldit-runner` | `crates/foldit-runner` |
| `molex` | `crates/molex` |
| `viso` | `crates/viso` |
| `foldit-plugin-sdk` | `crates/foldit-plugin-sdk` |

These are listed in the workspace `exclude` set. Without the exclude, cargo's
workspace discovery walks into them from the outside (for example when maturin
runs `cargo metadata` on `crates/molex/Cargo.toml`) and errors. They build under
their own manifests, not as members here.

## How the submodule crates are depended on

The root crates depend on viso, molex, and the plugin SDK as published crates,
and `[patch]` redirects two of them to local checkouts:

- **viso**: declared as a git dependency (`tag = "v0.3.6"`). A
  `[patch."https://github.com/foldit-org/viso"]` block currently redirects it
  to the local `crates/viso` checkout.
- **foldit-plugin-sdk**: declared as crates.io `0.1`. A `[patch.crates-io]`
  block currently redirects it to the local `crates/foldit-plugin-sdk` checkout.
- **molex**: declared as crates.io `0.7.1` in every crate that uses it. There
  is no molex `[patch]` block, so molex builds from the published crate even
  when the `crates/molex` checkout is present.

To build against the published viso or plugin SDK instead of the local checkout,
comment out the matching `[patch]` block. See
[Local Development on Submodules](submodules.md).

## Workspace lints

`[workspace.lints.clippy]` mirrors viso's clippy set but sets every level to
`warn` rather than `deny`: the checks report the technical-debt picture across
the root members without gating the build. Members opt in with
`[lints]` plus `workspace = true`. The gates that do fail live in the
[justfile](../tooling/quality-gates.md).
