fn main() {
    // Bundle mode: libraries in same directory as the binary
    #[cfg(target_os = "macos")]
    println!("cargo:rustc-link-arg=-Wl,-rpath,@loader_path");

    #[cfg(target_os = "linux")]
    println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN");

    // Dev mode: find Python in crates/foldit-runner/.pixi/
    #[cfg(target_os = "macos")]
    println!("cargo:rustc-link-arg=-Wl,-rpath,@loader_path/../../crates/foldit-runner/.pixi/envs/foundry/lib");
    // Dev mode: test binaries live in target/debug/deps/, one level deeper
    #[cfg(target_os = "macos")]
    println!("cargo:rustc-link-arg=-Wl,-rpath,@loader_path/../../../crates/foldit-runner/.pixi/envs/foundry/lib");

    #[cfg(target_os = "linux")]
    println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN/../../crates/foldit-runner/.pixi/envs/foundry/lib");
    #[cfg(target_os = "linux")]
    println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN/../../../crates/foldit-runner/.pixi/envs/foundry/lib");

    // Dev mode: find rosetta libs in assets/libs/ (from target/debug/ -> ../../assets/libs)
    #[cfg(target_os = "macos")]
    println!("cargo:rustc-link-arg=-Wl,-rpath,@loader_path/../../assets/libs");
    // Dev mode: test binaries live in target/debug/deps/, one level deeper
    #[cfg(target_os = "macos")]
    println!("cargo:rustc-link-arg=-Wl,-rpath,@loader_path/../../../assets/libs");

    #[cfg(target_os = "linux")]
    println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN/../../assets/libs");
    #[cfg(target_os = "linux")]
    println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN/../../../assets/libs");

    // Link rosetta_interactive library
    // At build time, look in assets/libs/
    println!("cargo:rustc-link-search=native=assets/libs");
    println!("cargo:rustc-link-lib=dylib=rosetta_interactive");

    // Rerun if the library changes
    println!("cargo:rerun-if-changed=assets/libs/librosetta_interactive.dylib");
}
