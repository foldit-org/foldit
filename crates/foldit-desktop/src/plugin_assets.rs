//! Static asset serving for the plugin tree under the `foldit://`
//! custom protocol.
//!
//! Wired alongside the GUI-bundle branch of the wry custom-protocol
//! handler (release builds) and mirrored by a Vite middleware in
//! `crates/foldit-gui/js/vite.config.ts` for dev builds, so the
//! frontend can render plugin-shipped icons via plain `<img src>`
//! regardless of mode.
//!
//! Scope is deliberately narrow. The extension whitelist refuses
//! anything that could carry executable semantics into the webview
//! (`.js`, `.html`, `.wasm`). The one exception is `.mjs`: plugins ship
//! custom-panel UI modules, but a `.mjs` is served only when its URL path
//! matches a manifest-declared `[[panels]]` entrypoint (the allowlist
//! passed to [`serve`]); every other `.mjs` request fails closed.
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

use std::collections::HashSet;
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
///
/// `mjs_allowlist` holds the servable `.mjs` URL paths (the
/// manifest-declared `[[panels]]` entrypoints, from
/// [`foldit_core::locate_plugin_ui_entrypoints`]). A `.mjs` request is
/// served only when `request_path` is in the set; non-`.mjs` static assets
/// (icons/css/fonts) ignore it.
pub fn serve(
    request_path: &str,
    plugins_root: &Path,
    mjs_allowlist: &HashSet<String>,
) -> AssetResponse {
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

    let Ok(canonical_asset) = asset_path.canonicalize() else {
        return AssetResponse::NotFound;
    };
    let Ok(canonical_root) = plugins_root.canonicalize() else {
        return AssetResponse::NotFound;
    };
    if !canonical_asset.starts_with(&canonical_root) {
        return AssetResponse::NotFound;
    }

    let ext = canonical_asset
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    let Some(mime) = ext.as_deref().and_then(plugin_asset_mime) else {
        return AssetResponse::NotFound;
    };

    // Plugin-shipped UI modules are gated to the manifest-declared
    // entrypoints; a `.mjs` off the allowlist fails closed even though it
    // passed containment. Other extensions are static assets, unaffected.
    if ext.as_deref() == Some("mjs") && !mjs_allowlist.contains(request_path) {
        return AssetResponse::NotFound;
    }

    std::fs::read(&canonical_asset)
        .map_or(AssetResponse::NotFound, |bytes| AssetResponse::Ok { bytes, mime })
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
        // Plugin-shipped UI modules. The MIME table alone serves nothing
        // executable: `.mjs` is additionally gated in `serve` against the
        // manifest-declared entrypoint allowlist, so only the entry a
        // plugin declares under `[[panels]]` is reachable.
        "mjs" => Some("application/javascript"),
        _ => None,
    }
}

/// Resolve the plugins root using the same order foldit-core uses for
/// discovery. Held in a `OnceLock` next to the webview so the closure
/// captures a cheap clone.
///
/// Its sole caller, `create_webview_release`, is `#[cfg(not(debug_assertions))]`,
/// so under a `test` build (where this module still compiles) it has no
/// caller and looks dead; the release build exercises it.
#[cfg_attr(test, allow(dead_code))]
pub fn resolve_plugins_root() -> Option<PathBuf> {
    foldit_core::locate_plugins_root()
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::needless_pass_by_value,
    reason = "test setup/assertions panic loudly on failure by design, and by-value args keep the helpers terse"
)]
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

    /// Empty `.mjs` allowlist — every non-`.mjs` test runs against this;
    /// no module path is servable.
    fn no_modules() -> HashSet<String> {
        HashSet::new()
    }

    /// Allowlist holding exactly the given URL paths.
    fn allow(paths: &[&str]) -> HashSet<String> {
        paths.iter().map(|p| (*p).to_owned()).collect()
    }

    #[test]
    fn serves_whitelisted_extension() {
        let td = tempdir();
        let root = td.path().to_path_buf();
        write(&root, "rosetta/icons/wiggle.png", b"PNG_BYTES");

        let r = serve("/plugins/rosetta/icons/wiggle.png", &root, &no_modules());
        let AssetResponse::Ok { bytes, mime } = r else {
            panic!("expected Ok");
        };
        assert_eq!(bytes, b"PNG_BYTES");
        assert_eq!(mime, "image/png");
    }

    #[test]
    fn serves_allowlisted_mjs() {
        let td = tempdir();
        let root = td.path().to_path_buf();
        write(&root, "rosetta/ui/panel.mjs", b"export default {}");

        let path = "/plugins/rosetta/ui/panel.mjs";
        let r = serve(path, &root, &allow(&[path]));
        let AssetResponse::Ok { bytes, mime } = r else {
            panic!("expected Ok for allowlisted .mjs");
        };
        assert_eq!(bytes, b"export default {}");
        assert_eq!(mime, "application/javascript");
    }

    #[test]
    fn rejects_unlisted_mjs() {
        let td = tempdir();
        let root = td.path().to_path_buf();
        write(&root, "rosetta/ui/sneaky.mjs", b"export default {}");

        // Present on disk + a valid MIME, but absent from the allowlist:
        // fails closed. The allowlist holds a *different* module.
        let allowed = allow(&["/plugins/rosetta/ui/declared.mjs"]);
        assert!(!is_ok(serve("/plugins/rosetta/ui/sneaky.mjs", &root, &allowed)));
        // Empty allowlist refuses everything.
        assert!(!is_ok(serve("/plugins/rosetta/ui/sneaky.mjs", &root, &no_modules())));
    }

    #[test]
    fn rejects_blacklisted_extension() {
        let td = tempdir();
        let root = td.path().to_path_buf();
        write(&root, "evil/run.js", b"alert(1)");

        assert!(!is_ok(serve("/plugins/evil/run.js", &root, &no_modules())));
    }

    #[test]
    fn rejects_html_and_wasm() {
        let td = tempdir();
        let root = td.path().to_path_buf();
        write(&root, "evil/page.html", b"<script>");
        write(&root, "evil/mod.wasm", b"\0asm");

        assert!(!is_ok(serve("/plugins/evil/page.html", &root, &no_modules())));
        assert!(!is_ok(serve("/plugins/evil/mod.wasm", &root, &no_modules())));
    }

    #[test]
    fn rejects_parent_directory_traversal() {
        let td = tempdir();
        let outer = td.path().to_path_buf();
        let root = outer.join("plugins");
        fs::create_dir_all(&root).unwrap();
        write(&outer, "secret.png", b"NOT_FOR_WEBVIEW");
        write(&root, "rosetta/icons/wiggle.png", b"OK");

        let r = serve("/plugins/rosetta/icons/../../../secret.png", &root, &no_modules());
        assert!(!is_ok(r));
        assert!(is_ok(serve("/plugins/rosetta/icons/wiggle.png", &root, &no_modules())));
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

            let r = serve("/plugins/rosetta/icons/escape.png", &root, &no_modules());
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
            assert!(!is_ok(serve("/plugins/link/leak.png", &root, &no_modules())));
        }
    }

    #[test]
    fn handles_uppercase_extension() {
        let td = tempdir();
        let root = td.path().to_path_buf();
        write(&root, "rosetta/icons/wiggle.PNG", b"P");
        // The whitelist normalizes to lowercase, so .PNG is treated
        // the same as .png.
        assert!(is_ok(serve("/plugins/rosetta/icons/wiggle.PNG", &root, &no_modules())));
    }

    #[test]
    fn rejects_directory_path() {
        let td = tempdir();
        let root = td.path().to_path_buf();
        fs::create_dir_all(root.join("rosetta/icons")).unwrap();

        assert!(!is_ok(serve("/plugins/rosetta/icons", &root, &no_modules())));
        assert!(!is_ok(serve("/plugins/rosetta/icons/", &root, &no_modules())));
    }

    #[test]
    fn rejects_extensionless_dotfile() {
        let td = tempdir();
        let root = td.path().to_path_buf();
        write(&root, "rosetta/.hidden", b"x");

        // No extension, so the whitelist lookup fails closed.
        assert!(!is_ok(serve("/plugins/rosetta/.hidden", &root, &no_modules())));
    }

    #[test]
    fn requires_plugins_prefix() {
        let td = tempdir();
        let root = td.path().to_path_buf();
        write(&root, "rosetta/icons/wiggle.png", b"P");

        assert!(!is_ok(serve("/something/else.png", &root, &no_modules())));
        assert!(!is_ok(serve("/plugins/", &root, &no_modules())));
        assert!(!is_ok(serve("/plugins", &root, &no_modules())));
    }
}
