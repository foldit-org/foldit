// Serde parse model: fields are populated from the puzzle TOML for a faithful
// (forward-compatible) parse; not every one is consumed at runtime.
#![allow(dead_code)]

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

#[cfg(test)]
mod tests {
    use super::*;

    fn levels_dir() -> PathBuf {
        crate::puzzle_load::levels_root().expect("levels_root must resolve under cargo test")
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
}
