# Local Development on Submodules

A fresh clone does not populate the submodules. The root crates depend on viso
and the plugin SDK through `[patch]` blocks that point at the local submodule
paths, so those paths must exist for a default build. Populate them with the
submodule update command:

```bash
git submodule update --init
```

or pull a single one by passing its path (for example `crates/viso`).

## Switching a dependency between local and published

Two `[patch]` blocks at the bottom of the root `Cargo.toml` redirect published
dependencies to local submodule checkouts:

```toml
[patch."https://github.com/foldit-org/viso"]
viso = { path = "crates/viso" }

[patch.crates-io]
foldit-plugin-sdk = { path = "crates/foldit-plugin-sdk" }
```

While a block is active, cargo builds that dependency from the local checkout,
so edits there take effect on the next build. Comment a block out to build
against the published version (the git tag for viso, the crates.io release for
the plugin SDK); the local checkout is then ignored.

molex has no patch block. It always resolves to the published `0.7.1` crate. To
hack on molex locally, add a `[patch.crates-io]` entry pointing `molex` at
`crates/molex` after checking that submodule out.

Each toggle is independent: viso can build from a local checkout while the
plugin SDK builds from crates.io, or any other combination.
