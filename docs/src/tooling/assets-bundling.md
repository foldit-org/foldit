# Assets and Bundling

## Asset layout

Read-only assets live under `assets/` at the repo root:

| Directory | Contents |
| --- | --- |
| `assets/gui` | The built front end (HTML/JS/wasm glue). Populated by `cargo xtask build-gui`; gitignored, not committed |
| `assets/levels` | Per-puzzle level directories (the curated `puzzle.toml`, setup, and assets) |
| `assets/scoring` | Score-term weight tables |
| `assets/view_presets` | viso view-preset TOML files |
| `assets/models` | Cached structure files (a PDB id fetched from RCSB lands here) |
| `assets/puzzle_setup` | Puzzle setup inputs |

## Resolver env overrides

Each asset root is resolved relative to the executable so a bundle launched from
any working directory finds its files, with a dev fallback that walks up to the
repo root. The resolution can be overridden by environment variable:

| Variable | Points at |
| --- | --- |
| `FOLDIT_GUI_ROOT` | `assets/gui` |
| `FOLDIT_VIEW_PRESETS_DIR` | `assets/view_presets` |
| `FOLDIT_LEVELS_ROOT` | `assets/levels` |
| `FOLDIT_SCORING_DIR` | `assets/scoring` |
| `FOLDIT_PLUGINS_ROOT` | `plugins` |
| `FOLDIT_PYTHON_HOST_DYLIB` | the python-host dylib (absolute path) |

On startup the desktop binary probes for a packaged resource directory and, if
it finds one, sets the unset overrides to point inside it (see
[foldit-desktop](../crates/foldit-desktop.md)). A pre-existing value always wins,
and a dev `cargo run` finds no bundle, so the overrides stay unset in
development and the repo-root `assets/` are used.

## Desktop bundling

`cargo xtask package` assembles a staging payload under `target/staging` and runs
cargo-packager. The cargo-packager config is `packager.json`: it names the main
binary (`foldit`) and the `foldit-worker` sidecar, and copies `assets/`,
`plugins/`, and the python-host dylib in as bundled resources. The output lands
in `dist/`. See [xtask Commands](xtask.md).

## Web bundling

`cargo xtask build-web` produces the wasm artifact and JS glue under
`webview/public/pkg/`; `cargo xtask package-web` additionally runs the front
end's web build to produce a static site ready to deploy.
