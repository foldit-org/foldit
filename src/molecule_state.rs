//! Molecule state management for ML visualization
//!
//! Handles loading protein structures from PDB/CIF files and converting
//! between different coordinate representations.

use glam::Vec3;
use pdbtbx::{Format, PDB, ReadOptions};
use std::path::Path;

/// Amino acid hydrophobicity lookup
fn is_hydrophobic(res_name: &str) -> bool {
    matches!(
        res_name,
        "ALA" | "VAL" | "ILE" | "LEU" | "MET" | "PHE" | "TRP" | "PRO"
    )
}

/// Maximum distance (Angstroms) for a valid peptide bond
const MAX_PEPTIDE_BOND_DISTANCE: f32 = 2.5;

/// Current molecule state for rendering and ML operations
pub struct MoleculeState {
    /// Atom positions (sidechain atoms)
    pub positions: Vec<Vec3>,
    /// Hydrophobicity flags per atom
    pub hydrophobicity: Vec<bool>,
    /// Amino acid sequence (extracted from structure)
    pub sequence: String,
    /// Backbone chain positions (for backbone rendering)
    pub backbone_chains: Vec<Vec<Vec3>>,
    /// All atom positions (for camera fitting)
    pub all_positions: Vec<Vec3>,
}

impl MoleculeState {
    /// Load molecule from a PDB file
    pub fn from_pdb_file<P: AsRef<Path>>(path: P) -> Result<Self, String> {
        let path_str = path.as_ref().to_string_lossy();
        let (pdb, _errors) = ReadOptions::default()
            .set_format(Format::Pdb)
            .set_level(pdbtbx::StrictnessLevel::Loose)
            .read(&*path_str)
            .map_err(|e| format!("Failed to parse PDB: {:?}", e))?;

        Self::from_pdb(&pdb)
    }

    /// Load molecule from an mmCIF file
    pub fn from_mmcif_file<P: AsRef<Path>>(path: P) -> Result<Self, String> {
        let path_str = path.as_ref().to_string_lossy();
        let (pdb, _errors) = ReadOptions::default()
            .set_format(Format::Mmcif)
            .set_level(pdbtbx::StrictnessLevel::Loose)
            .read(&*path_str)
            .map_err(|e| format!("Failed to parse mmCIF: {:?}", e))?;

        Self::from_pdb(&pdb)
    }

    /// Load molecule from file, auto-detecting format based on extension
    pub fn from_file<P: AsRef<Path>>(path: P) -> Result<Self, String> {
        let path_ref = path.as_ref();
        let ext = path_ref
            .extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_lowercase())
            .unwrap_or_default();

        match ext.as_str() {
            "pdb" => Self::from_pdb_file(path),
            "cif" | "mmcif" => Self::from_mmcif_file(path),
            _ => Err(format!("Unknown file extension: {}", ext)),
        }
    }

    /// Extract molecule data from a parsed PDB structure
    fn from_pdb(pdb: &PDB) -> Result<Self, String> {
        let mut backbone_chains: Vec<Vec<Vec3>> = Vec::new();
        let mut positions: Vec<Vec3> = Vec::new();
        let mut hydrophobicity: Vec<bool> = Vec::new();
        let mut all_positions: Vec<Vec3> = Vec::new();
        let mut sequence = String::new();

        // Process each chain
        for chain in pdb.chains() {
            let mut current_segment: Vec<Vec3> = Vec::new();
            let mut prev_c_pos: Option<Vec3> = None;
            let mut prev_res_serial: Option<isize> = None;

            for residue in chain.residues() {
                let res_serial = residue.serial_number();
                let res_name = residue.name().unwrap_or("UNK");
                let hydrophobic = is_hydrophobic(res_name);

                // Extract one-letter code for sequence
                let one_letter = three_to_one(res_name);
                sequence.push(one_letter);

                // Collect backbone atoms
                let mut n_pos: Option<Vec3> = None;
                let mut ca_pos: Option<Vec3> = None;
                let mut c_pos: Option<Vec3> = None;

                for atom in residue.atoms() {
                    let atom_name = atom.name().trim();
                    let pos = Vec3::new(atom.x() as f32, atom.y() as f32, atom.z() as f32);

                    all_positions.push(pos);

                    match atom_name {
                        "N" => n_pos = Some(pos),
                        "CA" => ca_pos = Some(pos),
                        "C" => c_pos = Some(pos),
                        "O" => {} // Skip O for backbone spline
                        _ => {
                            // Sidechain atom
                            positions.push(pos);
                            hydrophobicity.push(hydrophobic);
                        }
                    }
                }

                // Check for chain break
                let is_chain_break = if let (Some(prev_c), Some(n)) = (prev_c_pos, n_pos) {
                    (n - prev_c).length() > MAX_PEPTIDE_BOND_DISTANCE
                } else {
                    false
                };

                // Check for sequence gap
                let has_sequence_gap = if let Some(prev_serial) = prev_res_serial {
                    (res_serial - prev_serial).abs() > 1
                } else {
                    false
                };

                // Start new segment on chain break
                if (is_chain_break || has_sequence_gap) && !current_segment.is_empty() {
                    backbone_chains.push(std::mem::take(&mut current_segment));
                }

                // Add backbone atoms in order for smooth spline
                if let Some(n) = n_pos {
                    current_segment.push(n);
                }
                if let Some(ca) = ca_pos {
                    current_segment.push(ca);
                }
                if let Some(c) = c_pos {
                    current_segment.push(c);
                    prev_c_pos = Some(c);
                } else {
                    prev_c_pos = None;
                }

                prev_res_serial = Some(res_serial);
            }

            // Save last segment
            if !current_segment.is_empty() {
                backbone_chains.push(current_segment);
            }
        }

        Ok(Self {
            positions,
            hydrophobicity,
            sequence,
            backbone_chains,
            all_positions,
        })
    }

    /// Get the sequence for ML operations
    pub fn get_sequence(&self) -> &str {
        &self.sequence
    }

    /// Update positions (for animation)
    pub fn update_positions(&mut self, positions: Vec<Vec3>) {
        // Adjust hydrophobicity vector length if needed
        if self.hydrophobicity.len() != positions.len() {
            self.hydrophobicity = vec![false; positions.len()];
        }
        self.positions = positions;
    }
}

/// Convert three-letter amino acid code to one-letter code
fn three_to_one(three: &str) -> char {
    match three {
        "ALA" => 'A',
        "ARG" => 'R',
        "ASN" => 'N',
        "ASP" => 'D',
        "CYS" => 'C',
        "GLN" => 'Q',
        "GLU" => 'E',
        "GLY" => 'G',
        "HIS" => 'H',
        "ILE" => 'I',
        "LEU" => 'L',
        "LYS" => 'K',
        "MET" => 'M',
        "PHE" => 'F',
        "PRO" => 'P',
        "SER" => 'S',
        "THR" => 'T',
        "TRP" => 'W',
        "TYR" => 'Y',
        "VAL" => 'V',
        _ => 'X',
    }
}
