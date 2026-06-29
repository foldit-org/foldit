/// Locate the runtime plugins directory.
#[cfg(not(target_arch = "wasm32"))]
#[must_use]
pub fn locate_plugins_root() -> Option<std::path::PathBuf> {
    if let Some(env) = std::env::var_os("FOLDIT_PLUGINS_ROOT") {
        let p = std::path::PathBuf::from(env);
        if p.is_dir() {
            return Some(p);
        }
    }
    let exe = std::env::current_exe().ok()?;
    if let Some(dir) = exe.parent() {
        let bundle = dir.join("plugins");
        if bundle.is_dir() {
            return Some(bundle);
        }
    }
    let mut cursor = exe.parent()?.to_path_buf();
    loop {
        let candidate = cursor.join("crates/foldit-runner/plugins");
        if candidate.is_dir() {
            return Some(candidate);
        }
        if !cursor.pop() {
            break;
        }
    }
    None
}

/// Servable plugin-UI module URL paths, for the release `.mjs` gate.
///
/// Walks `<plugins_root>/*/plugin.toml`, parses each manifest, and collects
/// every declared `[[panels]].entry` as the URL path the asset protocol
/// serves it under (`/plugins/<plugin_id>/<entry>`). The release custom-
/// protocol handler serves a `.mjs` request only when its path is in this
/// set; non-`.mjs` static assets (icons/css/fonts) are unaffected. Dev does
/// not use this gate (it serves any `/plugins/*.mjs`).
///
/// Returns an empty set when no plugins root is located or none declares a
/// panel. Manifests that fail to read or parse are skipped, never aborting.
#[cfg(not(target_arch = "wasm32"))]
#[must_use]
pub fn locate_plugin_ui_entrypoints() -> std::collections::HashSet<String> {
    use foldit_runner::orchestrator::manifest::PluginManifest;

    let mut out = std::collections::HashSet::new();
    let Some(root) = locate_plugins_root() else {
        return out;
    };
    let Ok(read) = std::fs::read_dir(&root) else {
        return out;
    };
    for entry in read.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let manifest_path = dir.join("plugin.toml");
        let Ok(src) = std::fs::read_to_string(&manifest_path) else {
            continue;
        };
        let Ok(manifest) = PluginManifest::parse(&src) else {
            continue;
        };
        for panel in &manifest.panels {
            let rel = panel.entry.to_string_lossy();
            let _ = out.insert(format!("/plugins/{}/{}", manifest.id, rel));
        }
    }
    out
}
