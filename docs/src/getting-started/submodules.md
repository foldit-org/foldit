# Local Development on Submodules

A fresh clone does not populate the submodules, and a default build does not need
them: every `[patch]` block is commented out, so viso, molex, and the plugin SDK
all resolve from their published sources. You only need a submodule checked out
when you want to build that dependency from local source. Populate them with the
submodule update command:

```bash
git submodule update --init
```

or pull a single one by passing its path (for example `crates/viso`).

## Switching a dependency between local and published

The `[patch]` blocks at the bottom of the root `Cargo.toml` redirect published
dependencies to local submodule checkouts. All three ship commented out:

```toml
# [patch."https://github.com/foldit-org/viso"]
# viso = { path = "crates/viso" }

# [patch.crates-io]
# foldit-plugin-sdk = { path = "crates/foldit-plugin-sdk" }
# molex = { path = "crates/molex" }
```

Uncomment an entry (and its `[patch...]` header) to build that dependency from
the local checkout, so edits there take effect on the next build. Leave it
commented to build against the published version -- the git tag for viso, the
crates.io release for molex and the plugin SDK -- and the local checkout is then
ignored.

Uncommenting a `[patch.crates-io]` entry requires the matching submodule to be
checked out; cargo errors on a missing path.

Each toggle is independent: viso can build from a local checkout while the
plugin SDK builds from crates.io, or any other combination.
