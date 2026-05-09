// Rosetta is intentionally NOT statically linked into the foldit-web
// wasm. The wasm path drives Rosetta as a separate wasm module loaded
// by JS (analog of the dylib boundary on native). The cmake build still
// produces `librosetta_interactive_bundle.a` for that standalone wasm,
// but we don't merge it into foldit-web's cdylib here.
//
// See `crates/foldit-runner/build.rs` for the matching no-op on the
// rlib side, and `crates/foldit-runner/src/backends/rosetta/executor.rs`
// where the FFI call sites are cfg-gated to `not(target_arch = "wasm32")`
// so no `ri_*` symbol is ever referenced from reachable wasm code.

fn main() {}
