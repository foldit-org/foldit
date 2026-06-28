use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use crate::puzzle_toml::{Bubble, Camera, FilterSpec, PuzzleMeta, load_puzzle};
use crate::structure_io::load_entities_from_file;

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
                "bcif" => molex::Assembly::from_bcif(&raw)
                    .map_err(|e| format!("Failed to parse inline BinaryCIF: {e:?}"))?
                    .into_entities(),
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

#[cfg(test)]
mod tests {
    use super::*;

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
