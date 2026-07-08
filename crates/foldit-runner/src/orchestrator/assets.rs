//! Plugin asset access — the source-of-truth for a plugin directory's
//! contents.
//!
//! A plugin is a directory containing `plugin.toml` + the per-kind files
//! (Python module, native binary, wasm module, etc.). [`PluginAssets`]
//! abstracts where those bytes come from so the orchestrator can ingest a
//! plugin from disk on desktop or from HTTP on web with the same flow.
//!
//! Today only [`FilesystemAssets`] is implemented (desktop). A web
//! `HttpAssets` impl slots in later without changing manifest parsing,
//! discovery, or registration.

use std::fs;
use std::path::{Path, PathBuf};

use super::manifest::{ManifestError, PluginManifest};

/// Read-only handle to a plugin directory's contents.
///
/// The trait deliberately exposes both byte access (`read_file`, used for
/// the manifest, glue JS, etc.) and path resolution (`resolve_native_path`,
/// used for spawning a binary). Bytes work universally; paths only work
/// when the host can hand the OS a filename — desktop only. The web impl
/// will return an error from `resolve_native_path` since wasm modules
/// don't get filesystem paths.
pub trait PluginAssets: Send + Sync {
    /// Human-readable identifier for this asset bundle, used in errors
    /// and logging. Typically the directory name or URL base.
    fn label(&self) -> &str;

    /// Read and parse the plugin's `plugin.toml`.
    ///
    /// # Errors
    ///
    /// Returns an error if the manifest can't be read or parsed.
    fn read_manifest(&self) -> Result<PluginManifest, ManifestError>;

    /// Read a named file from the plugin directory. The orchestrator
    /// uses this for non-executable assets (manifest, JS glue, optional
    /// data files).
    ///
    /// # Errors
    ///
    /// Returns an error if the asset is missing or unreadable.
    fn read_file(&self, name: &str) -> Result<Vec<u8>, AssetError>;

    /// Resolve a named file inside the plugin directory to an OS path.
    /// Used by the native spawn primitive to hand a binary path to
    /// `Command::new`.
    ///
    /// # Errors
    ///
    /// Returns `AssetError::NotAvailable` on backends that can't
    /// produce filesystem paths (web), or `AssetError::NotFound` if the
    /// asset is missing.
    fn resolve_native_path(&self, name: &str) -> Result<PathBuf, AssetError>;
}

/// Errors surfaced by [`PluginAssets`] implementations.
#[derive(Debug)]
pub enum AssetError {
    /// Underlying filesystem or network failure.
    Io(std::io::Error),
    /// Manifest parsing failure.
    Manifest(ManifestError),
    /// The asset backend can't service this request shape (e.g.
    /// `resolve_native_path` on a web HTTP backend).
    NotAvailable(String),
    /// Requested asset is not present in the bundle.
    NotFound(String),
}

impl std::fmt::Display for AssetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "asset I/O error: {e}"),
            Self::Manifest(e) => write!(f, "{e}"),
            Self::NotAvailable(s) => {
                write!(f, "asset operation not available: {s}")
            }
            Self::NotFound(s) => write!(f, "asset not found: {s}"),
        }
    }
}

impl std::error::Error for AssetError {}

impl From<std::io::Error> for AssetError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<ManifestError> for AssetError {
    fn from(e: ManifestError) -> Self {
        Self::Manifest(e)
    }
}

// FilesystemAssets — desktop impl

/// Filesystem-backed plugin assets. Wraps a directory containing a
/// `plugin.toml` and the per-kind files.
pub struct FilesystemAssets {
    dir: PathBuf,
    label: String,
}

impl FilesystemAssets {
    /// Wrap a plugin directory. The label defaults to the directory
    /// basename, falling back to the full display path.
    pub fn new(dir: PathBuf) -> Self {
        let label = dir
            .file_name()
            .and_then(|s| s.to_str())
            .map_or_else(|| dir.display().to_string(), str::to_owned);
        Self { dir, label }
    }

    /// Path to the wrapped plugin directory.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

impl PluginAssets for FilesystemAssets {
    fn label(&self) -> &str {
        &self.label
    }

    fn read_manifest(&self) -> Result<PluginManifest, ManifestError> {
        let path = self.dir.join("plugin.toml");
        let text = fs::read_to_string(&path).map_err(ManifestError::Io)?;
        PluginManifest::parse(&text)
    }

    fn read_file(&self, name: &str) -> Result<Vec<u8>, AssetError> {
        let path = self.dir.join(name);
        if !path.exists() {
            return Err(AssetError::NotFound(path.display().to_string()));
        }
        fs::read(&path).map_err(AssetError::Io)
    }

    fn resolve_native_path(&self, name: &str) -> Result<PathBuf, AssetError> {
        let path = self.dir.join(name);
        if !path.exists() {
            return Err(AssetError::NotFound(path.display().to_string()));
        }
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn write_tempdir_with_manifest(toml: &str) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("plugin.toml");
        let mut f = fs::File::create(&path).unwrap();
        f.write_all(toml.as_bytes()).unwrap();
        tmp
    }

    #[test]
    fn filesystem_assets_reads_manifest() {
        let dir = write_tempdir_with_manifest(
            r#"
                id = "x"
                kind = "python"
            "#,
        );
        let assets = FilesystemAssets::new(dir.path().to_path_buf());
        let m = assets.read_manifest().unwrap();
        assert_eq!(m.id, "x");
    }

    #[test]
    fn filesystem_assets_label_is_dir_name() {
        let tmp = tempfile::tempdir().unwrap();
        let assets = FilesystemAssets::new(tmp.path().to_path_buf());
        let name =
            String::from(tmp.path().file_name().unwrap().to_str().unwrap());
        assert_eq!(assets.label(), name);
    }

    #[test]
    fn read_file_missing_returns_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let assets = FilesystemAssets::new(tmp.path().to_path_buf());
        match assets.read_file("nope.txt") {
            Err(AssetError::NotFound(_)) => {}
            other => panic!("expected NotFound, got {other:?}"),
        }
    }
}
