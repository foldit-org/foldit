fn main() {
    // Set rpath to find libraries in the same directory as the binary (for bundled mode)
    #[cfg(target_os = "macos")]
    println!("cargo:rustc-link-arg=-Wl,-rpath,@loader_path");

    #[cfg(target_os = "linux")]
    println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN");

    // Link rosetta_interactive library
    // At build time, look in the bundle directory
    println!("cargo:rustc-link-search=native=bundle");
    println!("cargo:rustc-link-lib=dylib=rosetta_interactive");

    // Rerun if the library changes
    println!("cargo:rerun-if-changed=bundle/librosetta_interactive.dylib");
}
