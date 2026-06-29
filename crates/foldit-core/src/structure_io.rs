use std::path::{Path, PathBuf};

// Re-exports so `foldit_core::puzzle::{levels_root, FilterSpec}` resolve (puzzle aliases this module).
pub use crate::puzzle_load::levels_root;
pub use crate::puzzle_toml::FilterSpec;

/// Load a file (PDB/CIF/BCIF) and return entities + name (file stem).
///
/// # Errors
///
/// Returns an `Err` if the file cannot be read or its contents cannot
/// be parsed into entities.
pub fn load_file_as_entities(path: &str) -> Result<(Vec<molex::MoleculeEntity>, String), String> {
    let p = Path::new(path);
    let name = p
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Unknown")
        .to_owned();

    let entities = load_entities_from_file(p)?;
    Ok((entities, name))
}

/// How a user-picked filesystem path should be loaded.
pub enum SessionLoadKind {
    /// A directory containing `puzzle.toml` (carries the directory).
    PuzzleDir(PathBuf),
    /// A bare structure file (pdb/cif/mmcif/bcif).
    Structure(PathBuf),
    /// Not a recognized session input.
    Unsupported,
}

/// Classify a picked path for the Load Session flow. Pure (no native deps),
/// so it is shared by the desktop dialog and any future web picker.
#[must_use]
pub fn classify_session_path(path: &Path) -> SessionLoadKind {
    if path.is_dir() {
        return if path.join("puzzle.toml").is_file() {
            SessionLoadKind::PuzzleDir(path.to_path_buf())
        } else {
            SessionLoadKind::Unsupported
        };
    }
    if path.file_name().and_then(|n| n.to_str()) == Some("puzzle.toml") {
        return path
            .parent()
            .map_or(SessionLoadKind::Unsupported, |parent| {
                SessionLoadKind::PuzzleDir(parent.to_path_buf())
            });
    }
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("pdb" | "cif" | "mmcif" | "bcif") => SessionLoadKind::Structure(path.to_path_buf()),
        _ => SessionLoadKind::Unsupported,
    }
}

/// Check if a string looks like a PDB ID (4 alphanumeric characters).
fn is_pdb_id(s: &str) -> bool {
    s.len() == 4 && s.chars().all(|c| c.is_ascii_alphanumeric())
}

/// Resolve a PDB ID or path to an actual file path, downloading if necessary.
///
/// # Errors
///
/// Returns an `Err` if `input` is neither an existing path nor a
/// resolvable PDB ID, or if a required download fails.
pub fn resolve_structure_path(input: &str) -> Result<String, String> {
    if Path::new(input).exists() {
        return Ok(input.to_owned());
    }

    if is_pdb_id(input) {
        return resolve_pdb_id(input);
    }

    Err(format!("File not found: {input}"))
}

/// Native: download a PDB by id from RCSB, cache to `assets/models/`, return the path.
#[cfg(not(target_arch = "wasm32"))]
fn resolve_pdb_id(input: &str) -> Result<String, String> {
    let pdb_id = input.to_lowercase();
    let models_dir = Path::new("assets/models");
    let local_path = models_dir.join(format!("{pdb_id}.cif"));

    if local_path.exists() {
        log::info!("Found local copy: {}", local_path.display());
        return Ok(local_path.to_string_lossy().to_string());
    }

    if !models_dir.exists() {
        std::fs::create_dir_all(models_dir)
            .map_err(|e| format!("Failed to create models directory: {e}"))?;
    }

    let url = format!("https://files.rcsb.org/download/{pdb_id}.cif");
    log::info!("Downloading {} from RCSB...", pdb_id.to_uppercase());

    let response =
        reqwest::blocking::get(&url).map_err(|e| format!("Failed to download {pdb_id}: {e}"))?;

    if !response.status().is_success() {
        return Err(format!(
            "Failed to download {}: HTTP {}",
            pdb_id,
            response.status()
        ));
    }

    let content = response
        .text()
        .map_err(|e| format!("Failed to read response: {e}"))?;

    std::fs::write(&local_path, &content).map_err(|e| format!("Failed to save CIF file: {e}"))?;

    log::info!("Downloaded to {}", local_path.display());
    Ok(local_path.to_string_lossy().to_string())
}

/// Wasm: PDB-ID resolution from inside foldit-core isn't supported. The web
/// entry crate (foldit-web) is responsible for fetching `.cif` bytes via
/// `web_sys::window().fetch_with_str(...)` and feeding them through the
/// bytes-loading entry point (`load_entities_from_file` after a temp write,
/// or a future `load_entities_from_bytes`).
#[cfg(target_arch = "wasm32")]
fn resolve_pdb_id(input: &str) -> Result<String, String> {
    Err(format!(
        "RCSB download for PDB id '{input}' must be performed by the host on web; \
         foldit-core does not contain an HTTP client on wasm targets"
    ))
}

/// Load a structure file and return classified entities (auto-detecting format).
///
/// # Errors
///
/// Returns an `Err` if the file extension is unsupported or the file
/// cannot be read or parsed.
pub fn load_entities_from_file(path: &Path) -> Result<Vec<molex::MoleculeEntity>, String> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_lowercase)
        .unwrap_or_default();

    match ext.as_str() {
        "pdb" => molex::Assembly::from_file(path)
            .map_err(|e| format!("Failed to parse PDB: {e:?}"))
            .map(molex::Assembly::into_entities),
        "cif" | "mmcif" => molex::Assembly::from_file(path)
            .map_err(|e| format!("Failed to parse mmCIF: {e:?}"))
            .map(molex::Assembly::into_entities),
        "bcif" => {
            let bytes = std::fs::read(path)
                .map_err(|e| format!("Failed to read BinaryCIF: {e}"))?;
            molex::Assembly::from_bcif(&bytes)
                .map_err(|e| format!("Failed to parse BinaryCIF: {e:?}"))
                .map(molex::Assembly::into_entities)
        }
        _ => Err(format!("Unknown file extension: {ext}")),
    }
}
