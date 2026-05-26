//! Desktop implementation of [`foldit_core::HostResources`].
//!
//! Real filesystem reads + a hardcoded view-preset directory that lives
//! next to the desktop binary's working directory. The bootstrap
//! structure path is whatever `main.rs` resolved from argv via
//! `foldit_core::puzzle::resolve_structure_path`.

use std::io;
use std::path::{Path, PathBuf};

use foldit_core::HostResources;

pub(crate) struct DesktopHost {
    view_presets_dir: PathBuf,
    initial_structure_path: Option<String>,
}

impl DesktopHost {
    pub(crate) fn new(initial_structure_path: Option<String>) -> Self {
        Self {
            view_presets_dir: PathBuf::from("assets/view_presets"),
            initial_structure_path,
        }
    }
}

impl HostResources for DesktopHost {
    fn read_file(&self, path: &str) -> io::Result<Vec<u8>> {
        std::fs::read(path)
    }

    fn view_presets_dir(&self) -> Option<&Path> {
        Some(&self.view_presets_dir)
    }

    fn initial_structure_path(&self) -> Option<String> {
        self.initial_structure_path.clone()
    }
}
