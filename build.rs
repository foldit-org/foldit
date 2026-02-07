fn main() {
    // Tauri 2 build step (generates context for tauri::generate_context!())
    tauri_build::build();

    // Bundle mode: libraries in same directory as the binary
    #[cfg(target_os = "macos")]
    println!("cargo:rustc-link-arg=-Wl,-rpath,@loader_path");

    #[cfg(target_os = "linux")]
    println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN");

    // Dev mode: find Python in crates/foldit-runner/.pixi/
    #[cfg(target_os = "macos")]
    println!("cargo:rustc-link-arg=-Wl,-rpath,@loader_path/../../crates/foldit-runner/.pixi/envs/foundry/lib");

    #[cfg(target_os = "linux")]
    println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN/../../crates/foldit-runner/.pixi/envs/foundry/lib");

    // Dev mode: find rosetta libs in bundle/ (from target/debug/ -> ../../bundle)
    #[cfg(target_os = "macos")]
    println!("cargo:rustc-link-arg=-Wl,-rpath,@loader_path/../../bundle");

    #[cfg(target_os = "linux")]
    println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN/../../bundle");

    // Link rosetta_interactive library
    // At build time, look in the bundle directory
    println!("cargo:rustc-link-search=native=bundle");
    println!("cargo:rustc-link-lib=dylib=rosetta_interactive");

    // Rerun if the library changes
    println!("cargo:rerun-if-changed=bundle/librosetta_interactive.dylib");
}
