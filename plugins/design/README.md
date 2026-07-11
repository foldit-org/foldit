# design plugin

A native Foldit plugin for **sequence design** — changing which amino acid sits
at a position in the protein.

Today it offers one operation, `mutate_residue`: select a single residue and
change its identity to any of the 20 amino acids. The picker you see in the UI
(the grid of amino-acid buttons) is declared in `plugin.toml` under
`[[buttons.options]]`, one option per amino acid in molex's `AminoAcid::ALL`
order; each option dispatches `mutate_residue` with its own `aa` parameter. The
Rust code validates the selection, applies the edit to its working assembly, and
hands the result back to the host.

## Kind

This is a `kind = "native"` plugin: a Rust crate compiled to a shared library
that exports the plugin vtable defined by
[`foldit-plugin-sdk`](../../crates/foldit-plugin-sdk). The host loads that dylib
directly and calls into it — no separate process protocol, no Python.

## Build

Native plugins are built through the workspace xtask, which compiles the crate
and installs the dylib where the runner looks for it:

```bash
cargo xtask setup-plugins design
```

See [Python and Native Plugins](../../docs/src/plugins/hosts.md) for how the
host discovers and loads a native plugin, and the SDK README for the `Plugin`
trait this crate implements.
