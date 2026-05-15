//! Static asset serving for the plugin tree under the `foldit://`
//! custom protocol.
//!
//! Wired alongside the GUI-bundle branch of the wry custom-protocol
//! handler (release builds) and mirrored by a Vite middleware in
//! `crates/foldit-gui/js/vite.config.ts` for dev builds, so the
//! frontend can render plugin-shipped icons via plain `<img src>`
//! regardless of mode.
//!
//! Scope is deliberately narrow: this is a static-asset surface, not a
//! plugin-shipped-UI surface. The extension whitelist refuses anything
//! that could carry executable semantics into the webview (`.js`,
//! `.html`, `.wasm`, `.mjs`); enabling those would be the Tier-3
//! decision the protocol design has explicitly deferred.
//!
//! Security envelope:
//! - Canonicalize the resolved asset path with `canonicalize()` before
//!   any FS read, so `..` traversal and symlink escapes resolve to
//!   their real target.
//! - Containment check uses `Path::starts_with` against the
//!   canonicalized plugins root. That's component-aware in Rust, so a
//!   sibling directory whose name happens to share a byte prefix
//!   (`plugins-root` vs `plugins-root-evil`) cannot pass.
//! - Extension lookup happens after canonicalization, against a
//!   lower-cased copy, so dotfiles and case variants are gated by the
//!   same whitelist.

use std::path::{Path, PathBuf};

/// Outcome of an asset lookup. `caller` builds a wry response from it.
pub enum AssetResponse {
    /// Read succeeded and the extension is on the whitelist.
    Ok { bytes: Vec<u8>, mime: &'static str },
    /// Path failed validation (off-tree, missing, unreadable, or the
    /// extension isn't on the whitelist). Caller serves 404 — no
    /// further detail leaks to the webview.
    NotFound,
}

/// Try to serve `request_path` (URL path component, e.g.
/// `/plugins/rosetta/icons/wiggle.png`) from `plugins_root`. The caller
/// is responsible for routing — only invoke this when the path begins
/// with the `/plugins/` prefix.
pub fn serve(request_path: &str, plugins_root: &Path) -> AssetResponse {
    let Some(rel) = request_path
        .strip_prefix('/')
        .and_then(|p| p.strip_prefix("plugins/"))
    else {
        return AssetResponse::NotFound;
    };
    if rel.is_empty() {
        return AssetResponse::NotFound;
    }
    let asset_path = plugins_root.join(rel);

    let canonical_asset = match asset_path.canonicalize() {
        Ok(p) => p,
        Err(_) => return AssetResponse::NotFound,
    };
    let canonical_root = match plugins_root.canonicalize() {
        Ok(p) => p,
        Err(_) => return AssetResponse::NotFound,
    };
    if !canonical_asset.starts_with(&canonical_root) {
        return AssetResponse::NotFound;
    }

    let ext = canonical_asset
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    let Some(mime) = ext.as_deref().and_then(plugin_asset_mime) else {
        return AssetResponse::NotFound;
    };

    match std::fs::read(&canonical_asset) {
        Ok(bytes) => AssetResponse::Ok { bytes, mime },
        Err(_) => AssetResponse::NotFound,
    }
}

/// Static-asset MIME whitelist. Any extension absent from this table
/// fails closed — the webview gets a 404. New entries here are an
/// intentional surface decision, not a routine addition.
fn plugin_asset_mime(ext: &str) -> Option<&'static str> {
    match ext {
        "svg" => Some("image/svg+xml"),
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "webp" => Some("image/webp"),
        "gif" => Some("image/gif"),
        "css" => Some("text/css"),
        "woff2" => Some("font/woff2"),
        "ttf" => Some("font/ttf"),
        _ => None,
    }
}

/// Resolve the plugins root using the same order foldit-core uses for
/// discovery. Held in a `OnceLock` next to the webview so the closure
/// captures a cheap clone.
pub fn resolve_plugins_root() -> Option<PathBuf> {
    foldit_core::locate_plugins_root()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().expect("create tempdir")
    }

    fn write(root: &Path, rel: &str, contents: &[u8]) {
        let p = root.join(rel);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut f = fs::File::create(&p).unwrap();
        f.write_all(contents).unwrap();
    }

    fn is_ok(r: AssetResponse) -> bool {
        matches!(r, AssetResponse::Ok { .. })
    }

    #[test]
    fn serves_whitelisted_extension() {
        let td = tempdir();
        let root = td.path().to_path_buf();
        write(&root, "rosetta/icons/wiggle.png", b"PNG_BYTES");

        let r = serve("/plugins/rosetta/icons/wiggle.png", &root);
        let AssetResponse::Ok { bytes, mime } = r else {
            panic!("expected Ok");
        };
        assert_eq!(bytes, b"PNG_BYTES");
        assert_eq!(mime, "image/png");
    }

    #[test]
    fn rejects_blacklisted_extension() {
        let td = tempdir();
        let root = td.path().to_path_buf();
        write(&root, "evil/run.js", b"alert(1)");

        assert!(!is_ok(serve("/plugins/evil/run.js", &root)));
    }

    #[test]
    fn rejects_html_and_wasm() {
        let td = tempdir();
        let root = td.path().to_path_buf();
        write(&root, "evil/page.html", b"<script>");
        write(&root, "evil/mod.wasm", b"\0asm");

        assert!(!is_ok(serve("/plugins/evil/page.html", &root)));
        assert!(!is_ok(serve("/plugins/evil/mod.wasm", &root)));
    }

    #[test]
    fn rejects_parent_directory_traversal() {
        let td = tempdir();
        let outer = td.path().to_path_buf();
        let root = outer.join("plugins");
        fs::create_dir_all(&root).unwrap();
        write(&outer, "secret.png", b"NOT_FOR_WEBVIEW");
        write(&root, "rosetta/icons/wiggle.png", b"OK");

        let r = serve("/plugins/rosetta/icons/../../../secret.png", &root);
        assert!(!is_ok(r));
        assert!(is_ok(serve("/plugins/rosetta/icons/wiggle.png", &root)));
    }

    #[test]
    fn rejects_symlink_escape() {
        #[cfg(unix)]
        {
            let td = tempdir();
            let outer = td.path().to_path_buf();
            let root = outer.join("plugins");
            fs::create_dir_all(root.join("rosetta/icons")).unwrap();
            write(&outer, "secret.png", b"NOT_FOR_WEBVIEW");
            std::os::unix::fs::symlink(
                outer.join("secret.png"),
                root.join("rosetta/icons/escape.png"),
            )
            .unwrap();

            let r = serve("/plugins/rosetta/icons/escape.png", &root);
            assert!(!is_ok(r));
        }
    }

    #[test]
    fn rejects_prefix_collision_directory() {
        let td = tempdir();
        let outer = td.path().to_path_buf();
        let root = outer.join("plugins");
        let evil = outer.join("plugins-evil");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&evil).unwrap();
        write(&evil, "leak.png", b"NOT_FOR_WEBVIEW");

        // `Path::starts_with` is component-aware, so even if a future
        // refactor produces a path under `plugins-evil/` it cannot
        // satisfy containment against `plugins/`. Direct attempt via
        // symlink to the sibling tree:
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&evil, root.join("link")).unwrap();
            assert!(!is_ok(serve("/plugins/link/leak.png", &root)));
        }
    }

    #[test]
    fn handles_uppercase_extension() {
        let td = tempdir();
        let root = td.path().to_path_buf();
        write(&root, "rosetta/icons/wiggle.PNG", b"P");
        // The whitelist normalizes to lowercase, so .PNG is treated
        // the same as .png.
        assert!(is_ok(serve("/plugins/rosetta/icons/wiggle.PNG", &root)));
    }

    #[test]
    fn rejects_directory_path() {
        let td = tempdir();
        let root = td.path().to_path_buf();
        fs::create_dir_all(root.join("rosetta/icons")).unwrap();

        assert!(!is_ok(serve("/plugins/rosetta/icons", &root)));
        assert!(!is_ok(serve("/plugins/rosetta/icons/", &root)));
    }

    #[test]
    fn rejects_extensionless_dotfile() {
        let td = tempdir();
        let root = td.path().to_path_buf();
        write(&root, "rosetta/.hidden", b"x");

        // No extension, so the whitelist lookup fails closed.
        assert!(!is_ok(serve("/plugins/rosetta/.hidden", &root)));
    }

    #[test]
    fn requires_plugins_prefix() {
        let td = tempdir();
        let root = td.path().to_path_buf();
        write(&root, "rosetta/icons/wiggle.png", b"P");

        assert!(!is_ok(serve("/something/else.png", &root)));
        assert!(!is_ok(serve("/plugins/", &root)));
        assert!(!is_ok(serve("/plugins", &root)));
    }
}
