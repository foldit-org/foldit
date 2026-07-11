# Render Engine (viso)

viso is the GPU render engine, pulled in as the `crates/viso` submodule. It
takes a molecule assembly and produces a 2D texture; the host decides what to do
with it (a winit window on desktop, an HTML canvas on web). Foldit constructs a
`VisoEngine`, hands it to the `App` through the engine harness, and the render
projector republishes geometry to it whenever the scene changes.

Foldit depends on viso by git tag (`v0.3.13`). The root `Cargo.toml` carries a
commented-out `[patch]` block that redirects it to the local submodule checkout
when uncommented. See
[Workspace Layout](../getting-started/workspace-layout.md).

This book does not re-derive viso's internals (impostor rendering, the
post-processing pipeline, GPU picking, the background mesh thread, the camera and
animation systems). Those are documented in viso's own book.

- Repository and book source: <https://github.com/foldit-org/viso> (mdbook under
  `crates/viso/docs/`).

For how Foldit drives viso each frame, see
[The Per-Frame Tick](../architecture/tick-loop.md); for the overlays Foldit
layers on top (clashes, voids, exposed hydrophobics, connections), see the
`viz` module in [foldit-core](foldit-core.md).
