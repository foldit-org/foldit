// build.rs for foldit-desktop
//
// The desktop binary is a thin shell around foldit-core + the wry
// webview. It does NOT link against any plugin code at compile time --
// plugins are runtime-loaded by `Orchestrator::discover_plugins` from
// `crates/foldit-runner/plugins/<name>/` and dispatched via the
// foldit-plugin C ABI. libpython is brought in by `foldit-python-host`
// (a dlopen-only cdylib) and never touches the desktop binary's link
// surface.
//
// The single rpath here is the @loader_path / $ORIGIN bundle convention
// so that any future host-bundled siblings (icon resources, web bundle,
// etc.) resolve correctly when the binary is moved out of `target/`.

fn main() {
    #[cfg(target_os = "macos")]
    println!("cargo:rustc-link-arg=-Wl,-rpath,@loader_path");

    #[cfg(target_os = "linux")]
    println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN");
}
