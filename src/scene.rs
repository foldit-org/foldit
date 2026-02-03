//! Scene and Structure management for multi-structure rendering
//!
//! Provides a scene graph that can contain multiple protein structures
//! (from files, ML predictions, or designs) and aggregates their data
//! for efficient rendering.

use foldit_conv::coords::{Coords, CoordsAtom, serialize_coords};
use foldit_render::bond_topology::get_residue_bonds;
use glam::Vec3;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// Unique identifier for structures in the scene
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StructureId(u64);

impl StructureId {
    pub fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(1);
        Self(COUNTER.fetch_add(1, Ordering::Relaxed))
    }
}

impl Default for StructureId {
    fn default() -> Self {
        Self::new()
    }
}

/// A single atom with all its properties
#[derive(Debug, Clone)]
pub struct Atom {
    pub position: Vec3,
    pub is_hydrophobic: bool,
    pub atom_name: String,
    pub residue_index: u32,
    pub chain_id: String,
}

/// A bond between two atoms (indices are local to the structure)
#[derive(Debug, Clone, Copy)]
pub struct Bond {
    pub atom_a: u32,
    pub atom_b: u32,
}

/// A bond from backbone CA to sidechain CB
#[derive(Debug, Clone)]
pub struct BackboneSidechainBond {
    pub ca_position: Vec3,
    pub cb_atom_index: u32,
}

/// Where this structure came from
#[derive(Debug, Clone)]
pub enum StructureSource {
    File { path: String },
    MLPredict { sequence: String, confidence: f32 },
    MLDesign { confidence: f32 },
    Manual,
}

/// A single structure (molecule, design, etc.)
#[derive(Debug, Clone)]
pub struct Structure {
    pub id: StructureId,
    pub name: String,
    pub source: StructureSource,

    /// Backbone chain positions (for tube rendering)
    /// Multiple chains, each is a sequence of N-CA-C positions
    pub backbone_chains: Vec<Vec<Vec3>>,

    /// Chain IDs for each backbone chain (parallel to backbone_chains)
    pub backbone_chain_ids: Vec<u8>,

    /// Sidechain atoms (for sphere rendering)
    pub sidechain_atoms: Vec<Atom>,

    /// Bonds between sidechain atoms
    pub sidechain_bonds: Vec<Bond>,

    /// Bonds connecting backbone CA to sidechain CB
    pub backbone_sidechain_bonds: Vec<BackboneSidechainBond>,

    /// Amino acid sequence (concatenation of all chains)
    pub sequence: String,

    /// Per-chain sequences: (chain_id, sequence)
    pub chain_sequences: Vec<(u8, String)>,

    /// Whether this structure is visible
    pub visible: bool,

    /// Cached COORDS binary data (for ML operations like MPNN)
    pub coords_bytes: Option<Vec<u8>>,
}

impl Structure {
    /// Create an empty structure with the given name
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            id: StructureId::new(),
            name: name.into(),
            source: StructureSource::Manual,
            backbone_chains: Vec::new(),
            backbone_chain_ids: Vec::new(),
            sidechain_atoms: Vec::new(),
            sidechain_bonds: Vec::new(),
            backbone_sidechain_bonds: Vec::new(),
            sequence: String::new(),
            chain_sequences: Vec::new(),
            visible: true,
            coords_bytes: None,
        }
    }

    /// Load structure from a PDB file
    /// Uses foldit-conv for parsing to ensure consistent COORDS handling.
    pub fn from_pdb_file<P: AsRef<Path>>(path: P) -> Result<Self, String> {
        use foldit_conv::coords::pdb::pdb_to_coords;

        let path_ref = path.as_ref();
        let name = path_ref
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("Unknown")
            .to_string();

        // Read file and parse through foldit-conv
        let content = std::fs::read_to_string(path_ref)
            .map_err(|e| format!("Failed to read PDB file: {}", e))?;

        let coords_bytes = pdb_to_coords(&content)
            .map_err(|e| format!("Failed to parse PDB: {:?}", e))?;

        let mut structure = Self::from_coords_bytes(&name, &coords_bytes, 1.0)?;
        structure.source = StructureSource::File {
            path: path_ref.to_string_lossy().to_string(),
        };

        Ok(structure)
    }

    /// Load structure from an mmCIF file
    /// Uses foldit-conv for parsing to ensure consistent COORDS handling.
    pub fn from_mmcif_file<P: AsRef<Path>>(path: P) -> Result<Self, String> {
        use foldit_conv::coords::pdb::mmcif_to_coords;

        let path_ref = path.as_ref();
        let name = path_ref
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("Unknown")
            .to_string();

        // Read file and parse through foldit-conv
        let content = std::fs::read_to_string(path_ref)
            .map_err(|e| format!("Failed to read mmCIF file: {}", e))?;

        let coords_bytes = mmcif_to_coords(&content)
            .map_err(|e| format!("Failed to parse mmCIF: {:?}", e))?;

        let mut structure = Self::from_coords_bytes(&name, &coords_bytes, 1.0)?;
        structure.source = StructureSource::File {
            path: path_ref.to_string_lossy().to_string(),
        };

        Ok(structure)
    }

    /// Load structure from file, auto-detecting format
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

    /// Create backbone-only structure from ML design (RFDiffusion3)
    pub fn from_backbone_design(
        name: impl Into<String>,
        backbone_chains: Vec<Vec<Vec3>>,
        confidence: f32,
    ) -> Self {
        let mut structure = Self::new(name);
        structure.source = StructureSource::MLDesign { confidence };
        structure.backbone_chains = backbone_chains;
        // No sidechains for backbone-only designs
        structure
    }

    /// Create structure from ML prediction (SimpleFold)
    pub fn from_ml_prediction(
        name: impl Into<String>,
        sequence: &str,
        backbone_chains: Vec<Vec<Vec3>>,
        sidechain_atoms: Vec<Atom>,
        sidechain_bonds: Vec<Bond>,
        confidence: f32,
    ) -> Self {
        let mut structure = Self::new(name);
        structure.source = StructureSource::MLPredict {
            sequence: sequence.to_string(),
            confidence,
        };
        structure.sequence = sequence.to_string();
        structure.backbone_chains = backbone_chains;
        structure.sidechain_atoms = sidechain_atoms;
        structure.sidechain_bonds = sidechain_bonds;
        structure
    }

    /// Create structure from COORDS binary data (from SimpleFold prediction)
    pub fn from_coords_bytes(
        name: impl Into<String>,
        coords_bytes: &[u8],
        confidence: f32,
    ) -> Result<Self, String> {
        use foldit_conv::coords::binary::deserialize;

        let coords = deserialize(coords_bytes)
            .map_err(|e| format!("Failed to parse COORDS: {:?}", e))?;

        let mut structure = Self::new(name);
        structure.coords_bytes = Some(coords_bytes.to_vec());

        // Track atoms by (chain_id, res_num, atom_name) for bond lookup
        let mut atom_index_map: std::collections::HashMap<(u8, i32, String), usize> =
            std::collections::HashMap::new();

        let mut current_chain: Vec<Vec3> = Vec::new();
        let mut current_chain_id: Option<u8> = None;
        let mut last_chain_id: Option<u8> = None;
        let mut last_res_num: Option<i32> = None;

        // Track per-chain sequences
        let mut current_chain_seq: String = String::new();
        let mut all_sequence_chars: Vec<char> = Vec::new();

        for i in 0..coords.num_atoms {
            let atom_name = std::str::from_utf8(&coords.atom_names[i])
                .unwrap_or("")
                .trim()
                .to_string();
            let chain_id = coords.chain_ids[i];
            let res_num = coords.res_nums[i];
            let res_name = std::str::from_utf8(&coords.res_names[i])
                .unwrap_or("UNK")
                .trim();
            let pos = Vec3::new(coords.atoms[i].x, coords.atoms[i].y, coords.atoms[i].z);

            // Check for chain/residue break
            let is_chain_break = last_chain_id.map_or(false, |c| c != chain_id);
            let is_sequence_gap = last_res_num.map_or(false, |r| (res_num - r).abs() > 1);

            if (is_chain_break || is_sequence_gap) && !current_chain.is_empty() {
                // Save current chain
                structure.backbone_chains.push(std::mem::take(&mut current_chain));
                if let Some(cid) = current_chain_id {
                    structure.backbone_chain_ids.push(cid);
                    if !current_chain_seq.is_empty() {
                        structure.chain_sequences.push((cid, std::mem::take(&mut current_chain_seq)));
                    }
                }
                current_chain_id = None;
            }

            // Track CA for sequence extraction (one per residue)
            if atom_name == "CA" {
                if last_res_num != Some(res_num) || last_chain_id != Some(chain_id) {
                    let aa = three_to_one(res_name);
                    current_chain_seq.push(aa);
                    all_sequence_chars.push(aa);
                }
            }

            // Backbone atoms go to chains (N, CA, C - skip O for spline)
            if atom_name == "N" || atom_name == "CA" || atom_name == "C" {
                current_chain.push(pos);
                if current_chain_id.is_none() {
                    current_chain_id = Some(chain_id);
                }
            } else if atom_name != "O" {
                // Skip hydrogen atoms (H, HA, HB, 1H, 2H, etc.)
                let is_hydrogen = atom_name.starts_with('H')
                    || atom_name.starts_with("1H")
                    || atom_name.starts_with("2H")
                    || atom_name.starts_with("3H")
                    || (atom_name.len() >= 2 && atom_name.chars().next().unwrap().is_ascii_digit()
                        && atom_name.chars().nth(1) == Some('H'));

                if !is_hydrogen {
                    // Sidechain atom (heavy atoms only)
                    let sidechain_idx = structure.sidechain_atoms.len();
                    // Debug: log first few sidechain atom names
                    if sidechain_idx < 15 {
                        log::info!(
                            "Sidechain atom {}: chain={}, res={}, name='{}' (bytes: {:?})",
                            sidechain_idx,
                            chain_id as char,
                            res_num,
                            atom_name,
                            atom_name.as_bytes()
                        );
                    }
                    atom_index_map.insert((chain_id, res_num, atom_name.clone()), sidechain_idx);

                    structure.sidechain_atoms.push(Atom {
                        position: pos,
                        is_hydrophobic: is_hydrophobic(res_name),
                        atom_name: atom_name.clone(),
                        residue_index: res_num as u32,
                        chain_id: format!("{}", chain_id as char),
                    });
                }
            }

            if atom_name == "CA" {
                last_res_num = Some(res_num);
            }
            last_chain_id = Some(chain_id);
        }

        // Don't forget the last chain
        if !current_chain.is_empty() {
            structure.backbone_chains.push(current_chain);
            if let Some(cid) = current_chain_id {
                structure.backbone_chain_ids.push(cid);
                if !current_chain_seq.is_empty() {
                    structure.chain_sequences.push((cid, current_chain_seq));
                }
            }
        }

        structure.sequence = all_sequence_chars.into_iter().collect();

        // Generate sidechain bonds from topology
        // First, build reverse lookup for residue info
        // Debug: log first few atom names in the map
        let mut debug_count = 0;
        for ((cid, rnum, aname), idx) in &atom_index_map {
            if debug_count < 10 {
                log::debug!(
                    "atom_index_map: chain={}, res={}, atom='{}' -> idx={}",
                    *cid as char, rnum, aname, idx
                );
                debug_count += 1;
            }
        }

        let mut bonds_attempted = 0;
        let mut bonds_found = 0;
        let mut ca_found = 0;
        let mut first_atoms_logged = 0;
        for i in 0..coords.num_atoms {
            let atom_name = std::str::from_utf8(&coords.atom_names[i])
                .unwrap_or("")
                .trim()
                .to_string();
            let chain_id = coords.chain_ids[i];
            let res_num = coords.res_nums[i];
            let res_name = std::str::from_utf8(&coords.res_names[i])
                .unwrap_or("UNK")
                .trim();

            // Debug: log first few atoms from coords to see what we're iterating
            if first_atoms_logged < 10 {
                log::info!(
                    "Bond loop atom {}: name='{}', res={}, res_name='{}'",
                    i, atom_name, res_num, res_name
                );
                first_atoms_logged += 1;
            }

            // Generate bonds for this residue's topology
            if atom_name == "CA" {
                ca_found += 1;
                if let Some(bonds) = get_residue_bonds(res_name) {
                    for (a1, a2) in bonds {
                        bonds_attempted += 1;
                        let key1 = (chain_id, res_num, a1.to_string());
                        let key2 = (chain_id, res_num, a2.to_string());

                        if let (Some(&idx1), Some(&idx2)) =
                            (atom_index_map.get(&key1), atom_index_map.get(&key2))
                        {
                            bonds_found += 1;
                            structure.sidechain_bonds.push(Bond {
                                atom_a: idx1 as u32,
                                atom_b: idx2 as u32,
                            });
                        }
                    }
                }

                // Add CA-CB bond
                let ca_pos = Vec3::new(coords.atoms[i].x, coords.atoms[i].y, coords.atoms[i].z);
                let cb_key = (chain_id, res_num, "CB".to_string());
                if let Some(&cb_idx) = atom_index_map.get(&cb_key) {
                    structure.backbone_sidechain_bonds.push(BackboneSidechainBond {
                        ca_position: ca_pos,
                        cb_atom_index: cb_idx as u32,
                    });
                }
            }
        }
        log::info!(
            "Bond generation: found {} CA atoms, attempted {} bonds, found {} matches",
            ca_found, bonds_attempted, bonds_found
        );

        structure.source = StructureSource::MLPredict {
            sequence: structure.sequence.clone(),
            confidence,
        };

        log::info!(
            "Created structure from COORDS: {} residues, {} backbone atoms, {} sidechain atoms, {} bonds",
            structure.sequence.len(),
            structure.backbone_chains.iter().map(|c| c.len()).sum::<usize>(),
            structure.sidechain_atoms.len(),
            structure.sidechain_bonds.len()
        );

        Ok(structure)
    }

    /// Get COORDS bytes for this structure (for ML operations like MPNN)
    /// Returns cached bytes if available, otherwise generates from backbone
    pub fn get_coords_bytes(&self) -> Option<Vec<u8>> {
        if let Some(ref bytes) = self.coords_bytes {
            return Some(bytes.clone());
        }

        // Generate backbone-only COORDS from backbone_chains
        // Each chain has N, CA, C atoms per residue (3 atoms per residue)
        self.backbone_to_coords()
    }

    /// Convert backbone chains to minimal COORDS format (backbone atoms only)
    fn backbone_to_coords(&self) -> Option<Vec<u8>> {
        if self.backbone_chains.is_empty() {
            return None;
        }

        let mut atoms = Vec::new();
        let mut chain_ids = Vec::new();
        let mut res_names = Vec::new();
        let mut res_nums = Vec::new();
        let mut atom_names = Vec::new();

        for (chain_idx, chain) in self.backbone_chains.iter().enumerate() {
            let chain_id = b'A' + (chain_idx as u8 % 26);

            // Each residue has 3 backbone atoms: N, CA, C
            // We need to add O as well for a complete backbone (MPNN expects N, CA, C, O)
            let num_residues = chain.len() / 3;

            for res_idx in 0..num_residues {
                let base = res_idx * 3;
                let n_pos = chain.get(base)?;
                let ca_pos = chain.get(base + 1)?;
                let c_pos = chain.get(base + 2)?;

                // Estimate O position (roughly 1.23Å from C, opposite to CA direction)
                let ca_to_c = (*c_pos - *ca_pos).normalize();
                let o_pos = *c_pos + ca_to_c * 1.23;

                // N atom
                atoms.push(CoordsAtom {
                    x: n_pos.x,
                    y: n_pos.y,
                    z: n_pos.z,
                    occupancy: 1.0,
                    b_factor: 0.0,
                });
                chain_ids.push(chain_id);
                res_names.push(*b"ALA");
                res_nums.push((res_idx + 1) as i32);
                atom_names.push(*b"N   ");

                // CA atom
                atoms.push(CoordsAtom {
                    x: ca_pos.x,
                    y: ca_pos.y,
                    z: ca_pos.z,
                    occupancy: 1.0,
                    b_factor: 0.0,
                });
                chain_ids.push(chain_id);
                res_names.push(*b"ALA");
                res_nums.push((res_idx + 1) as i32);
                atom_names.push(*b"CA  ");

                // C atom
                atoms.push(CoordsAtom {
                    x: c_pos.x,
                    y: c_pos.y,
                    z: c_pos.z,
                    occupancy: 1.0,
                    b_factor: 0.0,
                });
                chain_ids.push(chain_id);
                res_names.push(*b"ALA");
                res_nums.push((res_idx + 1) as i32);
                atom_names.push(*b"C   ");

                // O atom (estimated)
                atoms.push(CoordsAtom {
                    x: o_pos.x,
                    y: o_pos.y,
                    z: o_pos.z,
                    occupancy: 1.0,
                    b_factor: 0.0,
                });
                chain_ids.push(chain_id);
                res_names.push(*b"ALA");
                res_nums.push((res_idx + 1) as i32);
                atom_names.push(*b"O   ");
            }
        }

        if atoms.is_empty() {
            return None;
        }

        let coords = Coords {
            num_atoms: atoms.len(),
            atoms,
            chain_ids,
            res_names,
            res_nums,
            atom_names,
        };

        serialize_coords(&coords).ok()
    }

    /// Get total atom count (backbone + sidechain)
    pub fn atom_count(&self) -> usize {
        let backbone_atoms: usize = self.backbone_chains.iter().map(|c| c.len()).sum();
        backbone_atoms + self.sidechain_atoms.len()
    }

    /// Get sidechain positions as a flat vector
    pub fn sidechain_positions(&self) -> Vec<Vec3> {
        self.sidechain_atoms.iter().map(|a| a.position).collect()
    }

    /// Get sidechain hydrophobicity flags
    pub fn sidechain_hydrophobicity(&self) -> Vec<bool> {
        self.sidechain_atoms.iter().map(|a| a.is_hydrophobic).collect()
    }
}

/// Pre-computed aggregated data for efficient rendering
#[derive(Debug, Clone, Default)]
pub struct AggregatedData {
    pub backbone_chains: Vec<Vec<Vec3>>,
    pub sidechain_positions: Vec<Vec3>,
    pub sidechain_hydrophobicity: Vec<bool>,
    pub sidechain_residue_indices: Vec<u32>,
    pub sidechain_bonds: Vec<(u32, u32)>,
    pub backbone_sidechain_bonds: Vec<(Vec3, u32)>,
    pub all_positions: Vec<Vec3>,

    /// Mapping atom index back to structure
    pub atom_to_structure: Vec<StructureId>,
    /// Mapping chain index back to structure
    pub chain_to_structure: Vec<StructureId>,

    /// Per-residue render data from controllers (Rama, Blueprint, etc.)
    pub residue_render_data: ResidueRenderData,
}

/// Per-residue render data aggregated from controllers
#[derive(Debug, Clone, Default)]
pub struct ResidueRenderData {
    /// Rama-based colors per residue (if Rama coloring is active)
    pub rama_colors: Option<Vec<[f32; 3]>>,
    /// Blueprint-based colors per residue (if SS coloring is active)
    pub blueprint_colors: Option<Vec<[f32; 3]>>,
    /// Currently selected residue indices (1-indexed)
    pub selection: Vec<u32>,
    /// Active coloring mode
    pub color_mode: ResidueColorMode,
}

/// Which controller provides the current residue coloring
#[derive(Debug, Clone, Default, PartialEq)]
pub enum ResidueColorMode {
    #[default]
    Default,      // Use default hydrophobicity coloring
    Rama,         // Use Rama-based coloring
    Blueprint,    // Use SS-based coloring
    Alignment,    // Use alignment quality coloring
}

/// The complete scene containing all structures
pub struct Scene {
    structures: HashMap<StructureId, Structure>,
    insertion_order: Vec<StructureId>,
    cache: Option<AggregatedData>,
}

impl Scene {
    pub fn new() -> Self {
        Self {
            structures: HashMap::new(),
            insertion_order: Vec::new(),
            cache: None,
        }
    }

    /// Add a structure to the scene, returns its ID
    pub fn add(&mut self, structure: Structure) -> StructureId {
        let id = structure.id;
        self.insertion_order.push(id);
        self.structures.insert(id, structure);
        self.invalidate_cache();
        id
    }

    /// Remove a structure by ID
    pub fn remove(&mut self, id: StructureId) -> Option<Structure> {
        self.insertion_order.retain(|&i| i != id);
        let removed = self.structures.remove(&id);
        if removed.is_some() {
            self.invalidate_cache();
        }
        removed
    }

    /// Get immutable reference to a structure
    pub fn get(&self, id: StructureId) -> Option<&Structure> {
        self.structures.get(&id)
    }

    /// Get mutable reference (invalidates cache)
    pub fn get_mut(&mut self, id: StructureId) -> Option<&mut Structure> {
        self.invalidate_cache();
        self.structures.get_mut(&id)
    }

    /// Iterate over all structures in insertion order
    pub fn iter(&self) -> impl Iterator<Item = &Structure> {
        self.insertion_order
            .iter()
            .filter_map(|id| self.structures.get(id))
    }

    /// Get structure IDs in insertion order
    pub fn structure_ids(&self) -> &[StructureId] {
        &self.insertion_order
    }

    /// Get aggregated data for rendering (lazy computation)
    pub fn aggregated(&mut self) -> &AggregatedData {
        if self.cache.is_none() {
            self.cache = Some(self.compute_aggregated());
        }
        self.cache.as_ref().unwrap()
    }

    /// Check if a structure exists
    pub fn contains(&self, id: StructureId) -> bool {
        self.structures.contains_key(&id)
    }

    /// Number of structures
    pub fn len(&self) -> usize {
        self.structures.len()
    }

    /// Check if scene is empty
    pub fn is_empty(&self) -> bool {
        self.structures.is_empty()
    }

    /// Set visibility for a structure
    pub fn set_visible(&mut self, id: StructureId, visible: bool) {
        if let Some(structure) = self.structures.get_mut(&id) {
            if structure.visible != visible {
                structure.visible = visible;
                self.invalidate_cache();
            }
        }
    }

    /// Update backbone chains for a specific structure
    pub fn update_backbone(&mut self, id: StructureId, chains: Vec<Vec<Vec3>>) {
        if let Some(structure) = self.structures.get_mut(&id) {
            structure.backbone_chains = chains;
            self.invalidate_cache();
        }
    }

    fn invalidate_cache(&mut self) {
        self.cache = None;
    }

    fn compute_aggregated(&self) -> AggregatedData {
        let mut data = AggregatedData::default();

        for structure in self.iter() {
            if !structure.visible {
                continue;
            }

            let atom_offset = data.sidechain_positions.len() as u32;

            // Aggregate backbone chains
            for chain in &structure.backbone_chains {
                data.backbone_chains.push(chain.clone());
                data.chain_to_structure.push(structure.id);
                data.all_positions.extend(chain);
            }

            // Aggregate sidechain atoms
            for atom in &structure.sidechain_atoms {
                data.sidechain_positions.push(atom.position);
                data.sidechain_hydrophobicity.push(atom.is_hydrophobic);
                data.atom_to_structure.push(structure.id);
                data.all_positions.push(atom.position);
            }

            // Aggregate bonds (adjust indices by offset)
            for bond in &structure.sidechain_bonds {
                data.sidechain_bonds
                    .push((bond.atom_a + atom_offset, bond.atom_b + atom_offset));
            }

            // Aggregate backbone-sidechain bonds
            for bond in &structure.backbone_sidechain_bonds {
                data.backbone_sidechain_bonds
                    .push((bond.ca_position, bond.cb_atom_index + atom_offset));
            }
        }

        data
    }
}

impl Default for Scene {
    fn default() -> Self {
        Self::new()
    }
}

// Helper functions

fn is_hydrophobic(res_name: &str) -> bool {
    matches!(
        res_name,
        "ALA" | "VAL" | "ILE" | "LEU" | "MET" | "PHE" | "TRP" | "PRO"
    )
}

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
