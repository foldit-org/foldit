//! Desktop implementation of [`foldit_core::HostResources`].
//!
//! Real filesystem reads + a view-preset directory resolved relative to the
//! executable (so a bundle launched from any cwd finds its presets), with a
//! dev fallback that walks up to the repo-root `assets/view_presets`. The
//! bootstrap structure path is whatever `main.rs` resolved from argv via
//! `foldit_core::puzzle::resolve_structure_path`.

use std::io;
use std::path::{Path, PathBuf};

use foldit_core::HostResources;

pub struct DesktopHost {
    view_presets_dir: Option<PathBuf>,
    initial_structure_path: Option<String>,
}

impl DesktopHost {
    pub(crate) fn new(initial_structure_path: Option<String>) -> Self {
        let view_presets_dir = Self::resolve_view_presets_dir();
        if view_presets_dir.is_none() {
            log::warn!(
                "View-preset directory not found (looked for `assets/view_presets` \
                 next to the executable and up-tree from it); the preset menu will \
                 be empty and no startup preset will apply. Set \
                 FOLDIT_VIEW_PRESETS_DIR or run a build that bundles assets."
            );
        }
        Self {
            view_presets_dir,
            initial_structure_path,
        }
    }

    /// Resolve the directory holding the view-preset library. Mirrors
    /// [`foldit_core::locate_plugins_root`]'s bundle-vs-dev resolution so the
    /// exe finds presets regardless of launch cwd:
    ///
    ///   * Bundle: `assets/view_presets/` sits next to the executable (xtask
    ///     `bundle()` copies the repo's `assets/view_presets` there).
    ///   * Dev build (`cargo run`, `target/{debug,release}/foldit`): no sibling
    ///     `assets/`, so walk up from the exe for the repo-root
    ///     `assets/view_presets`.
    ///
    /// `FOLDIT_VIEW_PRESETS_DIR` overrides both. Returns `None` if nothing
    /// resolves, leaving the preset menu empty and startup-preset application a
    /// logged no-op.
    fn resolve_view_presets_dir() -> Option<PathBuf> {
        if let Some(env) = std::env::var_os("FOLDIT_VIEW_PRESETS_DIR") {
            let p = PathBuf::from(env);
            if p.is_dir() {
                return Some(p);
            }
        }
        // Walk up from the executable. Iteration 0 (the exe's own dir) is the
        // bundle layout; a higher ancestor is the dev-tree repo root. Both put
        // the library at `assets/view_presets`.
        let exe = std::env::current_exe().ok()?;
        let mut cursor = exe.parent()?.to_path_buf();
        loop {
            let candidate = cursor.join("assets/view_presets");
            if candidate.is_dir() {
                return Some(candidate);
            }
            if !cursor.pop() {
                break;
            }
        }
        None
    }
}

impl HostResources for DesktopHost {
    fn read_file(&self, path: &str) -> io::Result<Vec<u8>> {
        std::fs::read(path)
    }

    fn view_presets_dir(&self) -> Option<&Path> {
        self.view_presets_dir.as_deref()
    }

    fn initial_structure_path(&self) -> Option<String> {
        self.initial_structure_path.clone()
    }
}
