# xtal plugin

The **crystallography** plugin. When a puzzle ships experimental X-ray data,
this native plugin is what lets the player fit the model to that data.

It does four things:

- Builds `ExperimentalData` from the structure-factor CIF the host delivers at
  session init.
- Computes the electron-density map and publishes it through the well-known
  `density` query. The host stores that map and forwards it to any plugin that
  declares it `uses_density`.
- Runs **B-factor refinement** as the streaming `refine_b` op.
- Reports **R-free** (the standard crystallographic agreement metric) and its
  puzzle-objective bonus through the well-known `score` query.

The crystallography math itself lives in molex's `xtal` module (density
synthesis, bulk-solvent masking, sigma-A estimation, B-factor refinement); this
plugin is the wiring that runs it as a Foldit backend.

## A note on the GPU

The plugin creates its own `wgpu` device. A GPU device is process-local and
cannot be shared across the worker-process boundary, so the plugin cannot borrow
the app's renderer device and must stand up its own for the density compute.

## Kind and provides_density

`kind = "native"` (a Rust dylib exporting the SDK vtable). Its `plugin.toml`
sets `provides_density = true`, and the host initializes density providers ahead
of density consumers (like Rosetta's `elec_dens` term) so the map exists before
anything asks for it.

## Build

```bash
cargo xtask setup-plugins xtal
```
