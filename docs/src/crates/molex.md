# Molecule Model (molex)

molex is the molecule model and structure-format codec, pulled in as the
`crates/molex` submodule. It defines the `Assembly`, `MoleculeEntity`, and
`EntityId` types that flow through the whole client: the session stores entities
as `Arc<MoleculeEntity>`, history snapshots own them, the render projector hands
assemblies to viso, and the plugin protocol serializes them across the IPC
boundary.

molex is depended on as the published crates.io crate (`0.7.4`). As with viso and
the plugin SDK, its `[patch]` entry in the root `Cargo.toml` is commented out by
default, so it builds from the release even when the `crates/molex` submodule is
checked out. See [Workspace Layout](../getting-started/workspace-layout.md).

This book does not re-derive molex's internals (the entity and assembly model,
the format adapters, analysis, the codec and wire format, the Python bindings).
Those are documented in molex's own book.

- Repository and book source: <https://github.com/foldit-org/molex> (mdbook
  under `crates/molex/docs/`).
