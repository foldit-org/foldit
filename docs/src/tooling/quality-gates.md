# Quality Gates

The `justfile` collects the workspace checks. `just check-all` runs the four
gates in sequence:

```
check-all: clippy deny machete file-lengths
```

- **`clippy`** -- `cargo clippy --workspace --all-targets -- -D warnings`. Every
  warning is an error here, even though the workspace `[lints]` levels are
  `warn`: the lint table reports the debt picture, and this recipe is the gate
  that fails on it.
- **`deny`** -- `cargo deny check`. Advisory, license, and source audit.
- **`machete`** -- `cargo machete`. Unused-dependency report.
- **`file-lengths`** -- `python3 scripts/check_file_lengths.py`.

Other recipes: `just check` (a plain `cargo check --workspace`, which stays green
since warn-level lints do not break it), `just test`, `just doc`, `just warnings`
(a clippy warning count), and `just lint` (per-rule, per-crate clippy
breakdown). The `lint` recipe skips patched path deps (viso, molex): they are
local sources dragged in by `[patch]`, are not lint-capped, and own their own
gates in their own workspaces.

## File-length gate

`scripts/check_file_lengths.py` fails if any root-member Rust source file exceeds
800 lines. It walks only the root members' `src` trees (`foldit-core`,
`foldit-desktop`, `foldit-gui`, `foldit-web`, `xtask`, and
`foldit-runner/python-host`); the excluded submodule directories are skipped. A
file can opt out with the sentinel comment `// foldit:allow-long-file` in its
first few lines, reserved for files whose length is intrinsic (an exhaustive test
module), not as a way to dodge a real split.

## deny scoping

`deny.toml` scopes the duplicate-version analysis to the native desktop targets
(`aarch64-apple-darwin`, `x86_64-apple-darwin`), the platforms built and tested
today. Without a target list, cargo-deny unions every target and counts
Windows/Android/UEFI alternatives as duplicates. wasm32 is intentionally
omitted; reqwest uses the browser fetch API there, which cargo-deny cannot model.
When another platform is built, add its triple and re-run `cargo deny check
bans` to enumerate the new skips to curate.

## Workspace lint policy

`[workspace.lints.clippy]` in the root `Cargo.toml` mirrors viso's clippy set
(the broad groups plus a curated set of restriction lints) but every level is
`warn`, not `deny`. The intent is to surface technical debt across the root
members without the lint table itself gating the build; the `clippy` justfile
recipe is where the gate bites. See
[Workspace Layout](../getting-started/workspace-layout.md).
