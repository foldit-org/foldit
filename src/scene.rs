//! Scene and Structure management for multi-structure rendering
//!
//! Provides a scene graph that can contain multiple protein structures
//! (from files, ML predictions, or designs) and aggregates their data
//! for efficient rendering.

use foldit_conv::coords::{Coords, CoordsAtom, protein_only, serialize_coords, deserialize_coords_internal};
use foldit_conv::secondary_structure::SSType;
use foldit_render::bond_topology::get_residue_bonds;
use glam::Vec3;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// Unique identifier for structures in the scene
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StructureId(pub u64);

impl StructureId {
    pub fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(1);
        Self(COUNTER.fetch_add(1, Ordering::Relaxed))
    }

    /// Create a StructureId from a raw u64 value.
    /// Used for converting from RosettaStructureId.
    pub fn from_raw(id: u64) -> Self {
        Self(id)
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

    /// Canonical coordinate data (source of truth for atom positions)
    /// This replaces coords_bytes as the primary storage format
    pub coords: Option<Coords>,

    /// Secondary structure override (e.g. from puzzle.toml `ss` field).
    /// When set, renderers should use this instead of auto-detection.
    pub ss_override: Option<Vec<SSType>>,
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
            coords: None,
            ss_override: None,
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

    /// Load structure from a BinaryCIF file
    /// Uses foldit-conv for parsing to ensure consistent COORDS handling.
    pub fn from_bcif_file<P: AsRef<Path>>(path: P) -> Result<Self, String> {
        use foldit_conv::coords::bcif::bcif_file_to_coords;
        use foldit_conv::coords::binary::serialize;

        let path_ref = path.as_ref();
        let name = path_ref
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("Unknown")
            .to_string();

        let coords = bcif_file_to_coords(path_ref)
            .map_err(|e| format!("Failed to parse BinaryCIF: {:?}", e))?;

        let coords_bytes = serialize(&coords)
            .map_err(|e| format!("Failed to serialize coords: {:?}", e))?;

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
            "bcif" => Self::from_bcif_file(path),
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
        use foldit_conv::coords::{extract_sequences, RenderCoords};

        let coords = deserialize(coords_bytes)
            .map_err(|e| format!("Failed to parse COORDS: {:?}", e))?;

        // Filter to protein-only residues (exclude water, ligands, etc.)
        // This is important for MPNN/Rosetta which can't handle non-protein residues
        let coords = protein_only(&coords);

        // Use RenderCoords to extract all render-ready data
        let render = RenderCoords::from_coords_with_topology(
            &coords,
            is_hydrophobic,
            |name| get_residue_bonds(name).map(|b| b.to_vec()),
        );

        // Extract sequences
        let (sequence, chain_sequences) = extract_sequences(&coords);

        let mut structure = Self::new(name);
        structure.coords = Some(coords);  // Store Coords as source of truth
        structure.sequence = sequence.clone();
        structure.chain_sequences = chain_sequences;

        // Copy backbone data from RenderCoords
        structure.backbone_chains = render.backbone_chains;
        structure.backbone_chain_ids = render.backbone_chain_ids;

        // Convert sidechain atoms from RenderCoords format to Structure format
        structure.sidechain_atoms = render.sidechain_atoms.iter().map(|a| Atom {
            position: a.position,
            is_hydrophobic: a.is_hydrophobic,
            atom_name: a.atom_name.clone(),
            residue_index: a.residue_idx,
            chain_id: format!("{}", a.chain_id as char),
        }).collect();

        // Convert bonds from RenderCoords format
        structure.sidechain_bonds = render.sidechain_bonds.iter().map(|(a, b)| Bond {
            atom_a: *a,
            atom_b: *b,
        }).collect();

        // Convert backbone-sidechain bonds
        structure.backbone_sidechain_bonds = render.backbone_sidechain_bonds.iter().map(|(ca_pos, cb_idx)| {
            BackboneSidechainBond {
                ca_position: *ca_pos,
                cb_atom_index: *cb_idx,
            }
        }).collect();

        structure.source = StructureSource::MLPredict {
            sequence,
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
    /// Serializes from stored Coords if available, otherwise generates from backbone
    pub fn get_coords_bytes(&self) -> Option<Vec<u8>> {
        if let Some(ref coords) = self.coords {
            return serialize_coords(coords).ok();
        }

        // Fallback: Generate backbone-only COORDS from backbone_chains
        // Each chain has N, CA, C atoms per residue (3 atoms per residue)
        self.backbone_to_coords()
    }

    /// Get a reference to the Coords data if available
    pub fn get_coords(&self) -> Option<&Coords> {
        self.coords.as_ref()
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

                // Estimate CB position using ideal tetrahedral geometry
                // Standard formula from protein structure analysis:
                // CB is positioned at tetrahedral angle from CA, opposite to the N-CA-C plane
                let ca_n = (*n_pos - *ca_pos).normalize();
                let ca_c = (*c_pos - *ca_pos).normalize();
                let n_vec = ca_c.cross(ca_n).normalize(); // Normal to backbone plane

                // Ideal tetrahedral geometry coefficients (from crystallography)
                let cb_dir = ca_n * 0.56802827 + ca_c * (-0.58273431) + n_vec * (-0.54067466);
                let cb_pos = *ca_pos + cb_dir.normalize() * 1.521; // CA-CB bond length

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

                // CB atom (estimated from tetrahedral geometry)
                atoms.push(CoordsAtom {
                    x: cb_pos.x,
                    y: cb_pos.y,
                    z: cb_pos.z,
                    occupancy: 1.0,
                    b_factor: 0.0,
                });
                chain_ids.push(chain_id);
                res_names.push(*b"ALA");
                res_nums.push((res_idx + 1) as i32);
                atom_names.push(*b"CB  ");
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
    pub sidechain_atom_names: Vec<String>,
    pub sidechain_bonds: Vec<(u32, u32)>,
    pub backbone_sidechain_bonds: Vec<(Vec3, u32)>,
    pub all_positions: Vec<Vec3>,

    /// Mapping atom index back to structure
    pub atom_to_structure: Vec<StructureId>,
    /// Mapping chain index back to structure
    pub chain_to_structure: Vec<StructureId>,

    /// Per-residue render data from controllers (Rama, Blueprint, etc.)
    pub residue_render_data: ResidueRenderData,

    /// Pre-computed secondary structure types (from ss_override or auto-detect).
    /// When present, renderers should use these instead of auto-detecting.
    pub ss_types: Option<Vec<SSType>>,
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

/// Result of combining coords from all visible structures.
/// Used for Rosetta session management with multi-structure support.
#[derive(Debug, Clone)]
pub struct CombinedCoordsResult {
    /// Serialized COORDS bytes containing all structures
    pub bytes: Vec<u8>,
    /// Atom ranges for splitting updates: (StructureId, start_atom, end_atom) - 0-indexed
    /// NOTE: These are only valid for the INPUT coords. After Rosetta processes them,
    /// atom counts may change due to rebuilt atoms. Use chain_ids_per_structure for splitting exports.
    pub atom_ranges: Vec<(StructureId, usize, usize)>,
    /// Residue ranges per structure: StructureId -> (start_residue, end_residue) - 1-indexed, inclusive
    pub residue_ranges: HashMap<StructureId, (usize, usize)>,
    /// Chain IDs assigned to each structure (for splitting Rosetta exports by chain)
    /// This is the reliable way to split exports since Rosetta may add/remove atoms
    pub chain_ids_per_structure: Vec<(StructureId, Vec<u8>)>,
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

    /// Remove all structures and reset the scene to empty.
    pub fn clear(&mut self) {
        self.structures.clear();
        self.insertion_order.clear();
        self.invalidate_cache();
    }

    fn invalidate_cache(&mut self) {
        self.cache = None;
    }

    /// Get visible structure IDs and their residue counts.
    /// Used for checking if Rosetta session topology has changed.
    pub fn get_visible_structure_residue_counts(&self) -> (Vec<StructureId>, HashMap<StructureId, usize>) {
        let mut ids = Vec::new();
        let mut counts = HashMap::new();

        for structure in self.iter() {
            if !structure.visible {
                continue;
            }

            let residue_count: usize = structure
                .backbone_chains
                .iter()
                .map(|c| c.len() / 3)
                .sum();

            ids.push(structure.id);
            counts.insert(structure.id, residue_count);
        }

        (ids, counts)
    }

    /// Get combined COORDS bytes from all visible structures for Rosetta operations.
    /// Returns the combined coords with atom and residue range mappings.
    /// This creates a single COORDS buffer with all structures merged, using unique
    /// chain IDs for each structure.
    pub fn get_combined_coords_bytes(&self) -> Option<CombinedCoordsResult> {
        use foldit_conv::coords::{Coords, deserialize_coords_internal as deserialize, serialize_coords as serialize};

        let mut combined_atoms = Vec::new();
        let mut combined_chain_ids = Vec::new();
        let mut combined_res_names = Vec::new();
        let mut combined_res_nums = Vec::new();
        let mut combined_atom_names = Vec::new();

        // Track which atom ranges belong to which structure: (structure_id, start_idx, end_idx)
        let mut atom_ranges: Vec<(StructureId, usize, usize)> = Vec::new();
        // Track residue ranges per structure: StructureId -> (start_residue, end_residue) 1-indexed
        let mut residue_ranges: HashMap<StructureId, (usize, usize)> = HashMap::new();
        // Track chain IDs assigned to each structure (for splitting exports)
        let mut chain_ids_per_structure: Vec<(StructureId, Vec<u8>)> = Vec::new();

        let mut next_chain_id = b'A';
        let mut global_residue_offset: usize = 0;

        for structure in self.iter() {
            if !structure.visible {
                continue;
            }

            // Count residues for this structure (backbone has 3 atoms per residue: N, CA, C)
            let structure_residue_count: usize = structure
                .backbone_chains
                .iter()
                .map(|c| c.len() / 3)
                .sum();

            // Get coords for this structure (cached or generated from backbone)
            let coords_bytes = match structure.get_coords_bytes() {
                Some(bytes) => bytes,
                None => continue,
            };

            // Deserialize to access individual atoms
            let coords: Coords = match deserialize(&coords_bytes) {
                Ok(c) => c,
                Err(_) => continue,
            };

            let start_idx = combined_atoms.len();

            // Map original chain IDs to new unique IDs
            let mut chain_id_map = std::collections::HashMap::new();

            for i in 0..coords.num_atoms {
                let orig_chain = coords.chain_ids[i];
                let mapped_chain = *chain_id_map.entry(orig_chain).or_insert_with(|| {
                    let id = next_chain_id;
                    next_chain_id = if next_chain_id == b'Z' { b'a' } else { next_chain_id + 1 };
                    id
                });

                combined_atoms.push(coords.atoms[i].clone());
                combined_chain_ids.push(mapped_chain);
                combined_res_names.push(coords.res_names[i]);
                combined_res_nums.push(coords.res_nums[i]);
                combined_atom_names.push(coords.atom_names[i]);
            }

            let end_idx = combined_atoms.len();
            if end_idx > start_idx {
                atom_ranges.push((structure.id, start_idx, end_idx));

                // Store the chain IDs assigned to this structure
                let assigned_chain_ids: Vec<u8> = chain_id_map.values().copied().collect();
                chain_ids_per_structure.push((structure.id, assigned_chain_ids));

                // Calculate 1-indexed residue range for this structure
                if structure_residue_count > 0 {
                    let start_residue = global_residue_offset + 1; // 1-indexed
                    let end_residue = global_residue_offset + structure_residue_count;
                    residue_ranges.insert(structure.id, (start_residue, end_residue));
                    global_residue_offset = end_residue;
                }
            }
        }

        if combined_atoms.is_empty() {
            return None;
        }

        let num_atoms = combined_atoms.len();
        let combined = Coords {
            atoms: combined_atoms,
            chain_ids: combined_chain_ids,
            res_names: combined_res_names,
            res_nums: combined_res_nums,
            atom_names: combined_atom_names,
            num_atoms,
        };

        serialize(&combined).ok().map(|bytes| CombinedCoordsResult {
            bytes,
            atom_ranges,
            residue_ranges,
            chain_ids_per_structure,
        })
    }

    /// Apply combined Rosetta update to all structures in the session.
    /// Splits the exported coords by chain ID since Rosetta may add/remove atoms.
    pub fn apply_combined_update(
        &mut self,
        coords_bytes: &[u8],
        chain_ids_per_structure: &[(StructureId, Vec<u8>)],
    ) -> Result<(), String> {
        use foldit_conv::coords::{Coords, CoordsAtom, deserialize_coords_internal as deserialize, serialize_coords as serialize};

        let coords: Coords = deserialize(coords_bytes)
            .map_err(|e| format!("Failed to deserialize combined coords: {:?}", e))?;

        for (structure_id, chain_ids) in chain_ids_per_structure {
            // Filter atoms belonging to this structure by chain ID
            let mut structure_atoms: Vec<CoordsAtom> = Vec::new();
            let mut structure_chain_ids: Vec<u8> = Vec::new();
            let mut structure_res_names: Vec<[u8; 3]> = Vec::new();
            let mut structure_res_nums: Vec<i32> = Vec::new();
            let mut structure_atom_names: Vec<[u8; 4]> = Vec::new();

            for i in 0..coords.num_atoms {
                if chain_ids.contains(&coords.chain_ids[i]) {
                    structure_atoms.push(coords.atoms[i].clone());
                    structure_chain_ids.push(coords.chain_ids[i]);
                    structure_res_names.push(coords.res_names[i]);
                    structure_res_nums.push(coords.res_nums[i]);
                    structure_atom_names.push(coords.atom_names[i]);
                }
            }

            if structure_atoms.is_empty() {
                log::warn!("No atoms found for structure {:?} with chain IDs {:?}",
                    structure_id, chain_ids);
                continue;
            }

            let structure_coords = Coords {
                num_atoms: structure_atoms.len(),
                atoms: structure_atoms,
                chain_ids: structure_chain_ids,
                res_names: structure_res_names,
                res_nums: structure_res_nums,
                atom_names: structure_atom_names,
            };

            // Serialize back to bytes for this structure
            let structure_bytes = serialize(&structure_coords)
                .map_err(|e| format!("Failed to serialize structure coords: {:?}", e))?;

            // Parse into a Structure and update
            match Structure::from_coords_bytes("temp", &structure_bytes, 1.0) {
                Ok(new_data) => {
                    if let Some(structure) = self.structures.get_mut(structure_id) {
                        structure.backbone_chains = new_data.backbone_chains;
                        structure.sidechain_atoms = new_data.sidechain_atoms;
                        structure.sidechain_bonds = new_data.sidechain_bonds;
                        structure.backbone_sidechain_bonds = new_data.backbone_sidechain_bonds;
                        structure.coords = Some(structure_coords);  // Store Coords directly
                        log::info!("Updated structure {:?} from combined session ({} atoms)",
                            structure_id, new_data.coords.as_ref().map_or(0, |c| c.num_atoms));
                    }
                }
                Err(e) => {
                    log::warn!("Failed to parse structure {:?} from combined update: {}", structure_id, e);
                }
            }
        }

        self.invalidate_cache();
        Ok(())
    }

    fn compute_aggregated(&self) -> AggregatedData {
        let mut data = AggregatedData::default();
        let mut global_residue_offset: u32 = 0;
        let mut has_any_ss_override = false;
        let mut ss_parts: Vec<(u32, Option<&Vec<SSType>>, u32)> = Vec::new(); // (offset, override, count)

        for structure in self.iter() {
            if !structure.visible {
                continue;
            }

            let atom_offset = data.sidechain_positions.len() as u32;

            // Count residues in this structure (backbone has 3 atoms per residue: N, CA, C)
            let structure_residue_count: u32 = structure
                .backbone_chains
                .iter()
                .map(|c| (c.len() / 3) as u32)
                .sum();

            // Track SS override info
            if structure.ss_override.is_some() {
                has_any_ss_override = true;
            }
            ss_parts.push((global_residue_offset, structure.ss_override.as_ref(), structure_residue_count));

            // Aggregate backbone chains
            for chain in &structure.backbone_chains {
                data.backbone_chains.push(chain.clone());
                data.chain_to_structure.push(structure.id);
                data.all_positions.extend(chain);
            }

            // Aggregate sidechain atoms with global residue indices
            for atom in &structure.sidechain_atoms {
                data.sidechain_positions.push(atom.position);
                data.sidechain_hydrophobicity.push(atom.is_hydrophobic);
                // Map local residue_index to global residue index
                data.sidechain_residue_indices.push(atom.residue_index + global_residue_offset);
                data.sidechain_atom_names.push(atom.atom_name.clone());
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

            // Update global residue offset for next structure
            global_residue_offset += structure_residue_count;
        }

        // Build flat ss_types if any structure has an override
        if has_any_ss_override {
            let total_residues = global_residue_offset as usize;
            let mut ss_types = vec![SSType::Coil; total_residues];
            for (offset, ss_override, count) in &ss_parts {
                if let Some(overrides) = ss_override {
                    let start = *offset as usize;
                    let end = (start + *count as usize).min(total_residues);
                    for (i, &ss) in overrides.iter().enumerate() {
                        if start + i < end {
                            ss_types[start + i] = ss;
                        }
                    }
                }
            }
            data.ss_types = Some(ss_types);
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
