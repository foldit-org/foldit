use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

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

#[derive(Debug, Deserialize)]
pub struct PuzzleMeta {
    pub title: String,
    pub start_energy: f64,
    pub completion_score: f64,
    pub structure: StructureRef,
    pub camera: Camera,
    /// Optional view preset name (loads `assets/view_presets/{name}.toml`).
    pub view_preset: Option<String>,
    /// Optional per-puzzle scorefunction weight patch (`[puzzle.weights]`):
    #[serde(default)]
    pub weights: Option<HashMap<String, f32>>,
    /// Per-puzzle scored filters (`[[puzzle.filter]]`): each evaluates a
    /// named condition and either awards a RAW score bonus folded into the
    /// headline game score
    #[serde(default)]
    pub filter: Vec<FilterSpec>,
    /// Ligand entities to load alongside the structure (`[[puzzle.ligand]]`).
    /// Each references a rosetta `.params` file (and optional conformers).
    /// Empty for protein-only puzzles.
    #[serde(default)]
    pub ligand: Vec<LigandRef>,
    /// Optional catalytic constraints table (`[puzzle.constraints]`),
    /// referencing a `.cnstr` file in the puzzle dir.
    #[serde(default)]
    pub constraints: Option<ConstraintsRef>,
    /// Per-chain design masks (`[[puzzle.design_mask]]`) declaring which
    /// residues a designer may mutate
    #[serde(default)]
    pub design_mask: Vec<DesignMaskEntry>,
    // Remaining fields (view_setup, scorefxn, min_moves, guide_visible,
    // files, setup, view_options, etc.) are captured here and silently ignored.
    #[serde(flatten)]
    pub extra: HashMap<String, toml::Value>,
}

/// A single `[[puzzle.filter]]` declaration, mirroring the rosetta `Filter`
/// model: `kind` (TOML `type`) names the filter and `plugin` decides who
/// scores it.
///
/// Every filter parameter (including any named `max`/`bonus`) lives in `params`
/// so the same flat key/value set round-trips through serialization and is
/// forwarded to the bridge without loss. An unknown native `kind` parses but
/// evaluates to no bonus (forward-compatible).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FilterSpec {
    #[serde(rename = "type")]
    pub kind: String,
    /// Plugin to forward this filter to (e.g. `"rosetta"`). Absent means a
    /// foldit-native filter scored in-process. Omitted from serialized output
    /// when absent, so a native block carries no `plugin` line.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugin: Option<String>,
    /// All filter parameters (e.g. a native filter's threshold/bonus or a
    /// forwarded filter's ramp widths), in a deterministic key order so
    /// serialization is stable.
    #[serde(flatten)]
    pub params: BTreeMap<String, toml::Value>,
}

#[derive(Debug, Deserialize)]
pub struct StructureRef {
    // path and data are mutually exclusive methods for loading in structure
    pub path: Option<String>,
    pub data: Option<String>,
    pub format: String,
    pub ss: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Camera {
    pub center: [f64; 3],
    pub eye: [f64; 3],
    pub up: [f64; 3],
}

/// One `[[puzzle.ligand]]` entry: a ligand to load alongside the structure.
#[derive(Debug, Deserialize)]
pub struct LigandRef {
    /// Path (relative to the puzzle dir) to the rosetta `.params` file.
    pub params: String,
    /// Optional path (relative to the puzzle dir) to a conformer PDB.
    pub conformers: Option<String>,
}

/// A loaded ligand's asset bytes, read from the puzzle dir at load time.
///
/// Carried to the session `Puzzle` so the session-init path can deliver them
/// to the worker. `params` is the rosetta `.params` file bytes; `conformers`
/// is the optional conformer PDB as `(file_name, bytes)`.
#[derive(Debug, Clone)]
pub struct LigandAsset {
    /// `.params` file name (relative to the puzzle dir), e.g. "LG1.params".
    pub name: String,
    /// Raw `.params` file bytes.
    pub params: Vec<u8>,
    /// Optional conformer PDB: `(file_name, bytes)`.
    pub conformers: Option<(String, Vec<u8>)>,
}

/// The `[puzzle.constraints]` table: a reference to a `.cnstr` file.
#[derive(Debug, Deserialize)]
pub struct ConstraintsRef {
    /// Path (relative to the puzzle dir) to the `.cnstr` file.
    pub file: String,
}

/// One `[[puzzle.design_mask]]` entry: the designable-residue specification
/// for a single structure chain.
#[derive(Debug, Deserialize)]
pub struct DesignMaskEntry {
    /// Structure chain this mask applies to (e.g. "A"). The protein chain a
    /// designer may edit; chains with no entry (e.g. the ligand) are locked.
    pub chain: String,
    /// Inclusive `start-end` ranges separated by `||` (trailing `||` ok).
    pub can_design: String,
}

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

#[derive(Debug, thiserror::Error)]
pub enum PuzzleError {
    #[error("{}: {}", .0.display(), .1)]
    Io(PathBuf, #[source] std::io::Error),
    #[error("{}: {}", .0.display(), .1)]
    Parse(PathBuf, #[source] toml::de::Error),
}

/// Parse `<dir>/puzzle.toml` into a [`PuzzleLevel`].
///
/// # Errors
///
/// Returns [`PuzzleError::Io`] if the file cannot be read and
/// [`PuzzleError::Parse`] if its contents are not valid puzzle TOML.
pub fn load_puzzle(dir: &Path) -> Result<PuzzleLevel, PuzzleError> {
    let path = dir.join("puzzle.toml");
    let contents = std::fs::read_to_string(&path).map_err(|e| PuzzleError::Io(path.clone(), e))?;
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
    /// Optional per-puzzle scorefunction weight patch from `[puzzle.weights]`
    /// (`scoretype_name -> weight`). `None` when the puzzle declares none.
    pub weights: Option<HashMap<String, f32>>,
    /// Scored filters from `[[puzzle.filter]]`. Empty when the puzzle
    /// declares none. Carried to the session `Puzzle` so the score path can
    /// fold a met native filter's RAW bonus into the headline game score.
    pub filters: Vec<FilterSpec>,
    /// Ordered tutorial bubbles from `[[sequence]]`. Empty for puzzles
    /// with no intro. Tier-1 wiring pushes `bubbles[0]` to the GUI on
    /// load; advancement is unimplemented.
    pub bubbles: Vec<Bubble>,
    /// Catalytic constraints parsed from the `[puzzle.constraints]` file.
    /// Empty when the puzzle declares no constraints.
    pub constraints: Vec<crate::puzzle_setup::Constraint>,
    /// Per-chain designable-residue masks from `[[puzzle.design_mask]]`,
    /// keyed by structure chain. Empty when the puzzle declares no gating;
    /// a chain absent from the map is fully locked.
    pub design_masks: BTreeMap<String, crate::puzzle_setup::DesignMask>,
    /// Ligand asset bytes read from the puzzle dir (`[[puzzle.ligand]]`).
    /// Empty for protein-only puzzles. Carried to the session `Puzzle` so the
    /// session-init path can deliver them to the worker.
    pub ligands: Vec<LigandAsset>,
}

/// Resolve the absolute path to the `assets/levels` directory by
/// walking up from the running executable. Stops at the first
/// ancestor that contains an `assets/levels` directory.
///
/// # Errors
///
/// Returns an `Err` if no ancestor carries `assets/levels`
pub fn levels_root() -> Result<PathBuf, String> {
    // Explicit override, set by a packaged bundle whose assets live in a
    // platform resource dir that is not an ancestor of the executable (e.g. a
    // macOS .app's Contents/Resources). Points directly at the `levels` dir.
    if let Some(dir) = std::env::var_os("FOLDIT_LEVELS_ROOT") {
        let p = PathBuf::from(dir);
        if p.is_dir() {
            return Ok(p);
        }
    }

    let exe = std::env::current_exe().map_err(|e| format!("current_exe lookup failed: {e}"))?;
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
/// # Errors
///
/// Returns an `Err` if the levels root cannot be resolved, the puzzle
/// TOML fails to parse, or the referenced structure cannot be loaded.
pub fn load_puzzle_structure(puzzle_id: u32) -> Result<PuzzleData, String> {
    let puzzle_dir = levels_root()?.join(format!("{puzzle_id:010}"));
    load_puzzle_data_from_dir(&puzzle_dir)
}

/// Load a puzzle from an arbitrary directory containing a `puzzle.toml`.
///
/// # Errors
///
/// Returns an `Err` if the puzzle TOML fails to parse or the referenced
/// structure cannot be loaded.
pub fn load_puzzle_data_from_dir(puzzle_dir: &Path) -> Result<PuzzleData, String> {
    let mut puzzle = load_puzzle(puzzle_dir).map_err(|e| e.to_string())?;
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
                other => {
                    return Err(format!(
                        "Inline structure data not supported for format '{other}'"
                    ))
                }
            }
        }
        (Some(_), Some(_)) => {
            return Err("puzzle.structure: 'path' and 'data' are mutually exclusive".to_owned())
        }
        (None, None) => {
            return Err("puzzle.structure: either 'path' or 'data' must be specified".to_owned())
        }
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

    let (constraints, design_masks, ligands) = load_puzzle_setup(puzzle_dir, &puzzle.puzzle)?;

    Ok(PuzzleData {
        entities,
        name: puzzle.puzzle.title,
        ss_override,
        view_preset: puzzle.puzzle.view_preset,
        camera: puzzle.puzzle.camera,
        start_energy: puzzle.puzzle.start_energy,
        completion_score: puzzle.puzzle.completion_score,
        weights: puzzle.puzzle.weights.take(),
        filters: std::mem::take(&mut puzzle.puzzle.filter),
        bubbles: std::mem::take(&mut puzzle.sequence),
        constraints,
        design_masks,
        ligands,
    })
}

/// Read a ligand asset file, warning (and returning `None`) on a missing /
/// unreadable file rather than failing the load. `kind` labels the asset in
/// the warning ("params" / "conformers").
fn read_or_warn(path: &Path, title: &str, kind: &str) -> Option<Vec<u8>> {
    std::fs::read(path)
        .map_err(|_| {
            log::warn!(
                "Puzzle '{title}': declared ligand {kind} {} not found",
                path.display()
            );
        })
        .ok()
}

/// Parsed foldit-owned puzzle-setup inputs: catalytic constraints, the
/// per-chain design masks, and the ligand asset bytes.
type PuzzleSetup = (
    Vec<crate::puzzle_setup::Constraint>,
    BTreeMap<String, crate::puzzle_setup::DesignMask>,
    Vec<LigandAsset>,
);

/// Parse a puzzle's foldit-owned setup inputs: catalytic constraints
/// (`[puzzle.constraints]`) and the per-chain design masks
/// (`[[puzzle.design_mask]]`), and read any declared ligand asset bytes off
/// disk.
///
/// # Errors
///
/// Returns an `Err` if a referenced constraints file cannot be read, or if
/// the constraints or any chain's design-mask text is malformed.
fn load_puzzle_setup(puzzle_dir: &Path, meta: &PuzzleMeta) -> Result<PuzzleSetup, String> {
    let constraints = match &meta.constraints {
        Some(cref) => {
            let cstr_path = puzzle_dir.join(&cref.file);
            let text = std::fs::read_to_string(&cstr_path)
                .map_err(|e| format!("Failed to read constraints {}: {e}", cstr_path.display()))?;
            crate::puzzle_setup::parse_constraints(&text)
                .map_err(|e| format!("Failed to parse constraints {}: {e}", cstr_path.display()))?
        }
        None => Vec::new(),
    };

    let mut design_masks = BTreeMap::new();
    for entry in &meta.design_mask {
        let mask = crate::puzzle_setup::parse_design_mask(&entry.can_design).map_err(|e| {
            format!(
                "Failed to parse design mask for chain '{}': {e}",
                entry.chain
            )
        })?;
        design_masks.insert(entry.chain.clone(), mask);
    }

    // Read declared ligand asset bytes; a missing asset warns (doesn't fail)
    // so an authoring slip surfaces in the log without blocking the load, and
    // the missing ligand is simply skipped.
    let mut ligands = Vec::new();
    for lig in &meta.ligand {
        let params_path = puzzle_dir.join(&lig.params);
        let Some(params) = read_or_warn(&params_path, &meta.title, "params") else {
            continue;
        };
        let conformers = lig.conformers.as_ref().and_then(|conf| {
            let conf_path = puzzle_dir.join(conf);
            read_or_warn(&conf_path, &meta.title, "conformers").map(|bytes| (conf.clone(), bytes))
        });
        ligands.push(LigandAsset {
            name: lig.params.clone(),
            params,
            conformers,
        });
    }

    log::info!(
        "Puzzle '{}': {} catalytic constraints, {} designable chain(s), {} ligand(s)",
        meta.title,
        constraints.len(),
        design_masks.len(),
        ligands.len()
    );

    Ok((constraints, design_masks, ligands))
}

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
        "pdb" => molex::adapters::pdb::pdb_file_to_entities(path)
            .map_err(|e| format!("Failed to parse PDB: {e:?}")),
        "cif" | "mmcif" => molex::adapters::mmcif_file_to_entities(path)
            .map_err(|e| format!("Failed to parse mmCIF: {e:?}")),
        "bcif" => molex::adapters::bcif::bcif_file_to_entities(path)
            .map_err(|e| format!("Failed to parse BinaryCIF: {e:?}")),
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
        // The campaign puzzles are the 10-digit ID-keyed directories; other
        // puzzle dirs (named levels) are parsed separately.
        let mut entries: Vec<_> = std::fs::read_dir(&dir)
            .expect("assets/levels directory should exist")
            .filter_map(std::result::Result::ok)
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|n| n.len() == 10 && n.bytes().all(|b| b.is_ascii_digit()))
            })
            .collect();
        entries.sort_by_key(std::fs::DirEntry::file_name);

        assert_eq!(entries.len(), 40, "expected 40 campaign puzzle directories");

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

    #[test]
    fn load_bglb_ligand_constraints_and_mask() {
        let bglb_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../assets/levels/bglb");
        let data = load_puzzle_data_from_dir(&bglb_dir).expect("BglB puzzle should load");

        // The LG1 ligand must be present as a small-molecule entity.
        assert!(!data.entities.is_empty(), "expected entities");
        let has_lg1 = data.entities.iter().any(|e| {
            e.as_small_molecule()
                .is_some_and(|sm| &sm.residue_name == b"LG1")
        });
        assert!(has_lg1, "expected an LG1 small-molecule (ligand) entity");

        // Eight catalytic constraints.
        assert_eq!(data.constraints.len(), 8, "expected 8 constraints");

        // The LG1 ligand's `.params` bytes are read off disk and carried on
        // `PuzzleData` (the session-init payload source). Non-empty bytes
        // confirm the file was read, not just validated for presence.
        assert!(!data.ligands.is_empty(), "expected ligand assets");
        let lg1 = data
            .ligands
            .iter()
            .find(|l| l.name.contains("LG1"))
            .expect("expected an LG1 ligand asset");
        assert!(
            !lg1.params.is_empty(),
            "expected non-empty LG1 params bytes"
        );

        // The protein chain "A" carries a four-range design mask with the
        // catalytic gap locked; the LG1 ligand chain ("X") is intentionally
        // absent from the map and so is fully locked.
        assert_eq!(data.design_masks.len(), 1, "expected one designable chain");
        let mask = data
            .design_masks
            .get("A")
            .expect("expected a design mask for chain A");
        assert_eq!(mask.ranges.len(), 4, "expected 4 designable ranges");
        assert!(mask.is_designable(100));
        assert!(!mask.is_designable(164));
    }
}
