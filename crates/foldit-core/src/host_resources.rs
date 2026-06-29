//! Host-provided filesystem / resource access.
//!
//! `foldit-core` runs in two shells: the wry/winit desktop binary and
//! the wasm-bindgen web binary. Filesystem access only makes sense on
//! desktop, and even there the host owns the layout of bundled assets.
//! This trait keeps `std::fs` and asset-path strings out of `foldit-core`
//! so the same App code compiles and runs in both environments.
//!
//! puzzle.rs's `std::fs` calls (file-format parsing) remain direct for
//! now - file-format loaders are a separate concern from host resource
//! plumbing.

use std::io;
use std::path::Path;

/// Host-provided filesystem / resource access. foldit-core never touches
/// `std::fs` directly outside puzzle loading; it goes through this trait.
pub trait HostResources {
    /// Read an arbitrary asset file by host-resolvable path. Used by the
    /// `ReadResourceFile` action handler and any other ad-hoc resource read.
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] if the path cannot be resolved or read.
    fn read_file(&self, path: &str) -> io::Result<Vec<u8>>;

    /// Directory containing viso view-preset TOML files. foldit-core passes
    /// this to [`viso::options::VisoOptions::list_presets`] and to
    /// `engine.load_preset(name, dir)`. `None` means presets are unavailable
    /// (e.g., on web, where viso's path-based preset API doesn't apply -
    /// foldit-core skips list/load when `None`).
    fn view_presets_dir(&self) -> Option<&Path>;

    /// Bootstrap structure path read by `App::begin_startup`. `None` means
    /// no initial structure (e.g., web, which loads via a separate host
    /// flow), and the App settles at `Landing`.
    fn initial_structure_path(&self) -> Option<String>;
}
