use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

// -- Top-level --

#[derive(Debug, Deserialize)]
pub struct PuzzleLevel {
    pub puzzle: PuzzleMeta,
    #[serde(default)]
    pub sequence: Vec<Bubble>,
    #[serde(default)]
    pub sequence_alt: Vec<Bubble>,
    #[serde(default)]
    pub branch: HashMap<String, Vec<Bubble>>,
    #[serde(default)]
    pub on_event: Vec<EventBubble>,
    #[serde(default)]
    pub game_event: Vec<GameEvent>,
}

// -- Puzzle metadata ([puzzle] + nested tables) --

#[derive(Debug, Deserialize)]
pub struct PuzzleMeta {
    pub title: String,
    pub start_energy: f64,
    pub completion_score: f64,
    pub structure: StructureRef,
    pub camera: Camera,
    /// Optional view preset name (loads `assets/view_presets/{name}.toml`).
    pub view_preset: Option<String>,
    // Remaining fields (view_setup, scorefxn, min_moves, guide_visible,
    // files, setup, view_options, etc.) are captured here and silently ignored.
    #[serde(flatten)]
    pub extra: HashMap<String, toml::Value>,
}

// -- Sub-structs --

#[derive(Debug, Deserialize)]
pub struct StructureRef {
    /// Path to a structure file (relative to puzzle dir). Mutually exclusive with `data`.
    pub path: Option<String>,
    /// Base64-encoded `BinaryCIF` data, inline. Mutually exclusive with `path`.
    pub data: Option<String>,
    pub format: String,
    /// DSSP-style secondary structure annotation (e.g. "EEE" for 3 sheet residues).
    /// When present, overrides auto-detection.
    pub ss: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Camera {
    pub center: [f64; 3],
    pub eye: [f64; 3],
    pub up: [f64; 3],
}

// -- Bubbles --

#[derive(Debug, Deserialize)]
pub struct Bubble {
    #[serde(default)]
    pub text: String,
    pub color: Option<String>,
    pub point_to: Option<String>,
    pub point_to_index: Option<toml::Value>, // i64 or String
    pub image: Option<String>,
    pub button: Option<String>,
    pub alt_button: Option<String>,
    pub alt_skip: Option<i32>,
    pub alt_next: Option<String>,
    #[serde(default)]
    pub no_repeat: bool,
    pub link_name: Option<String>,
    pub link_url: Option<String>,
    pub trigger: Option<String>,
}

// -- Events --

#[derive(Debug, Deserialize)]
pub struct EventBubble {
    pub event: String,
    #[serde(default)]
    pub once: bool,
    pub threshold: Option<i32>,
    #[serde(flatten)]
    pub bubble: Bubble,
}

#[derive(Debug, Deserialize)]
pub struct GameEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    pub condition: Option<String>,
    pub parameter: Option<i64>,
    #[serde(flatten)]
    pub fields: HashMap<String, toml::Value>,
}

// -- Error --

#[derive(Debug, thiserror::Error)]
pub enum PuzzleError {
    #[error("{}: {}", .0.display(), .1)]
    Io(PathBuf, #[source] std::io::Error),
    #[error("{}: {}", .0.display(), .1)]
    Parse(PathBuf, #[source] toml::de::Error),
}

// -- Load --

pub fn load_puzzle(dir: &Path) -> Result<PuzzleLevel, PuzzleError> {
    let path = dir.join("puzzle.toml");
    let contents =
        std::fs::read_to_string(&path).map_err(|e| PuzzleError::Io(path.clone(), e))?;
    toml::from_str(&contents).map_err(|e| PuzzleError::Parse(path, e))
}

/// Data returned from loading a puzzle.
pub struct PuzzleData {
    pub entities: Vec<molex::MoleculeEntity>,
    pub name: String,
    pub ss_override: Option<Vec<molex::SSType>>,
    /// Optional view preset name from puzzle.toml `[puzzle] view_preset`.
    pub view_preset: Option<String>,
    /// Initial camera pose from `[puzzle.camera]`.
    pub camera: Camera,
    /// Starting score from `[puzzle] start_energy` (rosetta units).
    pub start_energy: f64,
    /// Completion target from `[puzzle] completion_score` (game units).
    pub completion_score: f64,
    /// Ordered tutorial bubbles from `[[sequence]]`. Empty for puzzles
    /// with no intro. Tier-1 wiring pushes `bubbles[0]` to the GUI on
    /// load; advancement is unimplemented.
    pub bubbles: Vec<Bubble>,
}

/// Resolve the absolute path to the `assets/levels` directory by
/// walking up from the running executable. Stops at the first
/// ancestor that contains an `assets/levels` directory.
///
/// Works in three shapes:
/// - test binaries (`target/debug/deps/foo-HASH`) → walks up to the
///   workspace root,
/// - dev binaries (`target/debug/foo`, `cargo run`) → same,
/// - installed binaries (`<prefix>/bin/foldit` shipped with sibling
///   `<prefix>/bin/assets/`) → finds the sibling.
///
/// Returns an `Err` if no ancestor carries `assets/levels` - better
/// than a CWD-dependent silent miss, since the caller can surface the
/// problem with the actual exe path.
pub fn levels_root() -> Result<PathBuf, String> {
    let exe = std::env::current_exe()
        .map_err(|e| format!("current_exe lookup failed: {e}"))?;
    let mut dir = exe.parent();
    while let Some(d) = dir {
        let candidate = d.join("assets/levels");
        if candidate.is_dir() {
            return Ok(candidate);
        }
        dir = d.parent();
    }
    Err(format!(
        "levels_root: no `assets/levels` directory found in any ancestor of {}",
        exe.display()
    ))
}

/// Load a puzzle by ID: parse its TOML and return entities for the engine.
///
/// Looks up `<levels_root>/{puzzle_id:010}/puzzle.toml` (see
/// [`levels_root`]) and resolves the structure from
/// `[puzzle.structure]` - either via `path` (file reference) or
/// `data` (base64-encoded inline `BinaryCIF`).
pub fn load_puzzle_structure(puzzle_id: u32) -> Result<PuzzleData, String> {
    let puzzle_dir = levels_root()?.join(format!("{puzzle_id:010}"));
    let mut puzzle = load_puzzle(&puzzle_dir).map_err(|e| e.to_string())?;
    let structure = &puzzle.puzzle.structure;

    let entities = match (&structure.path, &structure.data) {
        (Some(path), None) => {
            let structure_path = puzzle_dir.join(path);
            log::info!(
                "Puzzle '{}': loading structure from {}",
                puzzle.puzzle.title,
                structure_path.display()
            );
            load_entities_from_file(&structure_path)?
        }
        (None, Some(data_b64)) => {
            use base64::Engine;

            log::info!(
                "Puzzle '{}': loading inline {} structure",
                puzzle.puzzle.title,
                structure.format
            );

            let raw = base64::engine::general_purpose::STANDARD
                .decode(data_b64)
                .map_err(|e| format!("Failed to decode base64 structure data: {e}"))?;

            match structure.format.as_str() {
                "bcif" => {
                    use molex::adapters::bcif::bcif_to_entities;
                    bcif_to_entities(&raw)
                        .map_err(|e| format!("Failed to parse inline BinaryCIF: {e:?}"))?
                }
                other => return Err(format!(
                    "Inline structure data not supported for format '{other}'"
                )),
            }
        }
        (Some(_), Some(_)) => return Err(
            "puzzle.structure: 'path' and 'data' are mutually exclusive".to_owned()
        ),
        (None, None) => return Err(
            "puzzle.structure: either 'path' or 'data' must be specified".to_owned()
        ),
    };

    let ss_override = structure.ss.as_ref().map(|ss_str| {
        let ss = molex::analysis::ss::string::from_string(ss_str);
        log::info!(
            "Puzzle '{}': applying SS override ({} residues)",
            puzzle.puzzle.title,
            ss.len()
        );
        ss
    });

    Ok(PuzzleData {
        entities,
        name: puzzle.puzzle.title,
        ss_override,
        view_preset: puzzle.puzzle.view_preset,
        camera: puzzle.puzzle.camera,
        start_energy: puzzle.puzzle.start_energy,
        completion_score: puzzle.puzzle.completion_score,
        bubbles: std::mem::take(&mut puzzle.sequence),
    })
}

/// Load a file (PDB/CIF/BCIF) and return entities + name (file stem).
pub fn load_file_as_entities(
    path: &str,
) -> Result<(Vec<molex::MoleculeEntity>, String), String> {
    let p = Path::new(path);
    let name = p
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Unknown").to_owned();

    let entities = load_entities_from_file(p)?;
    Ok((entities, name))
}

/// Check if a string looks like a PDB ID (4 alphanumeric characters).
fn is_pdb_id(s: &str) -> bool {
    s.len() == 4 && s.chars().all(|c| c.is_ascii_alphanumeric())
}

/// Resolve a PDB ID or path to an actual file path, downloading if necessary.
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

    let response = reqwest::blocking::get(&url)
        .map_err(|e| format!("Failed to download {pdb_id}: {e}"))?;

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

    std::fs::write(&local_path, &content)
        .map_err(|e| format!("Failed to save CIF file: {e}"))?;

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
pub fn load_entities_from_file(
    path: &Path,
) -> Result<Vec<molex::MoleculeEntity>, String> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_lowercase)
        .unwrap_or_default();

    match ext.as_str() {
        "pdb" => {
            molex::adapters::pdb::pdb_file_to_entities(path)
                .map_err(|e| format!("Failed to parse PDB: {e:?}"))
        }
        "cif" | "mmcif" => {
            molex::adapters::mmcif_file_to_entities(path)
                .map_err(|e| format!("Failed to parse mmCIF: {e:?}"))
        }
        "bcif" => {
            molex::adapters::bcif::bcif_file_to_entities(path)
                .map_err(|e| format!("Failed to parse BinaryCIF: {e:?}"))
        }
        _ => Err(format!("Unknown file extension: {ext}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn levels_dir() -> PathBuf {
        levels_root().expect("levels_root must resolve under cargo test")
    }

    #[test]
    fn parse_all_puzzles() {
        let dir = levels_dir();
        let mut entries: Vec<_> = std::fs::read_dir(&dir)
            .expect("assets/levels directory should exist")
            .filter_map(std::result::Result::ok)
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .collect();
        entries.sort_by_key(std::fs::DirEntry::file_name);

        assert_eq!(entries.len(), 40, "expected 40 puzzle directories");

        for entry in &entries {
            let puzzle_dir = entry.path();
            let result = load_puzzle(&puzzle_dir);
            assert!(
                result.is_ok(),
                "failed to parse {}: {}",
                puzzle_dir.display(),
                result.unwrap_err()
            );
        }
    }

    #[test]
    fn spot_check_intro() {
        let puzzle = load_puzzle(&levels_dir().join("0000010001")).unwrap();
        assert_eq!(puzzle.puzzle.title, "Intro to Foldit");
        assert_eq!(puzzle.sequence.len(), 6);
        assert_eq!(puzzle.on_event.len(), 1);
        assert_eq!(puzzle.on_event[0].event, "level_complete");
        assert!(puzzle.branch.is_empty());
        assert!(puzzle.game_event.is_empty());
    }

    #[test]
    fn spot_check_branches_and_game_events() {
        let puzzle = load_puzzle(&levels_dir().join("0000010019")).unwrap();
        assert_eq!(puzzle.puzzle.title, "Close the Gap");
        assert_eq!(puzzle.branch.len(), 2);
        assert!(puzzle.branch.contains_key("branch_301"));
        assert!(puzzle.branch.contains_key("alt_branch_1301"));
        assert_eq!(puzzle.branch["branch_301"].len(), 2);
        assert_eq!(puzzle.game_event.len(), 2);
        assert_eq!(puzzle.game_event[0].event_type, "show_voids");
    }

    #[test]
    fn spot_check_sequence_alt_and_point_to_index() {
        let puzzle = load_puzzle(&levels_dir().join("0000010020")).unwrap();
        assert_eq!(puzzle.puzzle.title, "Basic Threading");
        assert!(!puzzle.sequence_alt.is_empty());
        // point_to_index as string
        let last_alt = puzzle.sequence_alt.last().unwrap();
        assert!(last_alt.no_repeat);
        assert!(last_alt.point_to_index.is_some());
    }

    #[test]
    fn spot_check_covid_puzzle() {
        let puzzle = load_puzzle(&levels_dir().join("0000010040")).unwrap();
        assert_eq!(puzzle.puzzle.title, "COVID-19 Spike Binder");
        assert_eq!(puzzle.sequence.len(), 8);
        assert!(puzzle.puzzle.extra.contains_key("view_options"));
    }
}
