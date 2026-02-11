use std::path::Path;

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
    #[cfg(target_os = "windows")]
    {
        println!("cargo:rerun-if-changed=assets/libs/rosetta_interactive.lib");
        println!("cargo:rerun-if-changed=assets/libs/rosetta_interactive.dll");

        // Windows has no rpath — copy the DLL next to the output binary so it's found at runtime
        let out_dir = std::env::var("OUT_DIR").unwrap();
        // OUT_DIR is target/<profile>/build/<crate>/out — walk up to target/<profile>/
        let target_profile_dir = Path::new(&out_dir)
            .ancestors()
            .nth(3)
            .expect("couldn't find target profile directory");
        let dll_src = Path::new("assets/libs/rosetta_interactive.dll");
        if dll_src.exists() {
            // Copy to target/<profile>/ (where the binary lives)
            let dll_dst = target_profile_dir.join("rosetta_interactive.dll");
            std::fs::copy(dll_src, &dll_dst).ok();
            // Also copy to target/<profile>/deps/ (where test binaries live)
            let deps_dst = target_profile_dir.join("deps/rosetta_interactive.dll");
            std::fs::copy(dll_src, &deps_dst).ok();
        }
    }
    #[cfg(target_os = "macos")]
    println!("cargo:rerun-if-changed=assets/libs/librosetta_interactive.dylib");
    #[cfg(target_os = "linux")]
    println!("cargo:rerun-if-changed=assets/libs/librosetta_interactive.so");
}
