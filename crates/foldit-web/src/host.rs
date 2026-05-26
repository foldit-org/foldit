//! Web implementation of [`foldit_core::HostResources`].
//!
//! The wasm shell has no synchronous filesystem and uses a separate flow
//! (JS-side fetch) to deliver structure bytes into the orchestrator, so
//! all three trait methods are effectively no-ops: `read_file` returns
//! `Unsupported`, `view_presets_dir` returns `None` (viso's path-based
//! preset API doesn't fit the web build), and `initial_structure_path`
//! is `None` (no startup file load on web).

pub(crate) struct WebHost;

impl foldit_core::HostResources for WebHost {
    fn read_file(&self, _path: &str) -> std::io::Result<Vec<u8>> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "web host: synchronous fs reads not supported; use the JS bridge",
        ))
    }

    fn view_presets_dir(&self) -> Option<&std::path::Path> {
        None
    }

    fn initial_structure_path(&self) -> Option<String> {
        None
    }
}
