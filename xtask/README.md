# xtask

The workspace's build and packaging automation, run as `cargo xtask <command>`.
It exists because some of Foldit's build steps are more than a plain `cargo
build`: the plugin worker and Python host are separate targets, the GUI is a
`bun` project, plugins have their own native/Python setup recipes, and shipping
an installer means invoking a packager. `xtask` wraps all of that behind one
tool so you never have to remember the individual incantations.

(`xtask` is a common Rust convention: a small binary crate in the workspace that
you invoke through a `cargo` alias, giving you "project scripts" in Rust instead
of a Makefile.)

## Commands

| Command | What it does |
| --- | --- |
| `cargo xtask setup` | One command to make `cargo run` work: builds the host artifacts and sets up every plugin. Start here after a fresh clone. |
| `cargo xtask setup-plugins [id]` | Set up plugins (native build recipes, Python envs). Optional id restricts to one plugin. |
| `cargo xtask build-host` | Build `foldit-worker` and the `foldit-python-host` dylib next to the app exe. Run after changing runner code. Defaults to release. |
| `cargo xtask build-gui` | Build the front end (`bun run build`) and copy the output into `assets/gui`. |
| `cargo xtask build-web` | Build the wasm bundle into `webview/public/pkg/`. Requires the nightly toolchain. |
| `cargo xtask build-molex` | Rebuild the molex Python extension from local source. |
| `cargo xtask package` | Produce OS installers (macOS `.app`/`.dmg`, Windows MSI, Linux `.deb`/AppImage) via cargo-packager. |
| `cargo xtask package-web` | Build the static web site ready for deployment. |
| `cargo xtask transcribe-filters` | One-shot: transcribe legacy `.ir_puzzle.filters` and `can_design` masks into `[[puzzle.filter]]` / `[[puzzle.design_mask]]` blocks in the curated levels. |

See [xtask Commands](../docs/src/tooling/xtask.md) in the workspace book for the
details behind each one.
