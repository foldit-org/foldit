//! Session management for structure relationships and operation targeting.
//!
//! The Session module provides a single source of truth for:
//! - Structure relationships (original -> RFD3 design -> MPNN design)
//! - What structure backend operations (Wiggle/Shake) should target
//! - User focus state (Session view vs individual structure)

use crate::scene::{Scene, StructureId};
use glam::Vec3;

/// User's focus choice via Tab key.
/// Default is Session (all structures) when puzzle is loaded.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub enum Focus {
    /// Session view - operate on all structures (DEFAULT)
    #[default]
    Session,
    /// Explicit structure focus - operate on this structure only
    Structure(StructureId),
}

/// Tracks structure relationships and backend operation targeting.
#[derive(Debug, Default)]
pub struct Session {
    /// Original loaded structure
    pub original: Option<StructureId>,
    /// Most recent RFD3 backbone design
    pub rfd3_design: Option<StructureId>,
    /// Most recent MPNN-sequenced design
    pub mpnn_design: Option<StructureId>,
    /// In-flight ML animation structure
    pub animation_structure: Option<StructureId>,
    /// User's explicit focus choice
    pub focus: Focus,
    /// Pending MPNN sequence application
    pub mpnn_pending: bool,
    /// Original backbone CA positions for Kabsch alignment
    pub original_backbone_ca: Option<Vec<Vec3>>,
}

impl Session {
    /// Create a new empty session.
    pub fn new() -> Self {
        Self::default()
    }

    /// Check if we're in session mode (operate on all structures).
    pub fn is_session_mode(&self) -> bool {
        matches!(self.focus, Focus::Session)
    }

    /// Get the structure that backend operations should target.
    /// Returns None in session mode (use combined coords instead).
    pub fn operation_target(&self) -> Option<StructureId> {
        match self.focus {
            Focus::Session => None, // Session mode - all structures
            Focus::Structure(id) => Some(id),
        }
    }

    /// Get a structure ID for locking purposes.
    /// In session mode, returns the original structure ID as the lock target.
    /// In single-structure mode, returns the focused structure ID.
    pub fn lock_target(&self) -> Option<StructureId> {
        match self.focus {
            Focus::Session => self.original,
            Focus::Structure(id) => Some(id),
        }
    }

    /// Cycle focus: Session -> Structure 1 -> Structure 2 -> ... -> Session
    pub fn cycle_focus(&mut self, structure_ids: &[StructureId]) -> Focus {
        self.focus = match self.focus {
            Focus::Session => {
                // Session -> first structure (or stay at Session if empty)
                structure_ids
                    .first()
                    .map(|&id| Focus::Structure(id))
                    .unwrap_or(Focus::Session)
            }
            Focus::Structure(current_id) => {
                // Current structure -> next structure, or Session if at end
                let idx = structure_ids.iter().position(|&id| id == current_id);
                match idx {
                    Some(i) if i + 1 < structure_ids.len() => {
                        Focus::Structure(structure_ids[i + 1])
                    }
                    _ => Focus::Session, // Wrap back to Session
                }
            }
        };
        self.focus
    }

    /// Get human-readable description of current focus.
    pub fn focus_description(&self, scene: &Scene) -> String {
        match self.focus {
            Focus::Session => "Session (all structures)".to_string(),
            Focus::Structure(id) => {
                let ids = scene.structure_ids();
                let idx = ids.iter().position(|&i| i == id).unwrap_or(0) + 1;
                let name = scene
                    .get(id)
                    .map(|s| s.name.clone())
                    .unwrap_or_default();
                format!("Structure {} ({})", idx, name)
            }
        }
    }

    /// Get a short description for logging (without scene access).
    pub fn focus_short_description(&self) -> String {
        match self.focus {
            Focus::Session => "Session".to_string(),
            Focus::Structure(_) => "Single structure".to_string(),
        }
    }

    // ========== Lifecycle Hooks ==========
    // These track state but don't affect focus

    /// Called when the original structure is loaded.
    pub fn on_original_loaded(&mut self, structure_id: StructureId, backbone_ca: Vec<Vec3>) {
        self.original = Some(structure_id);
        self.original_backbone_ca = Some(backbone_ca);
    }

    /// Called when RFD3 structure design completes.
    pub fn on_rfd3_complete(&mut self, structure_id: StructureId) {
        self.rfd3_design = Some(structure_id);
        self.animation_structure = None;
    }

    /// Called when MPNN sequence design completes and structure is created.
    pub fn on_mpnn_complete(&mut self, structure_id: StructureId) {
        self.mpnn_design = Some(structure_id);
        self.mpnn_pending = false;
    }

    /// Called when MPNN sequence design starts.
    pub fn on_mpnn_start(&mut self) {
        self.mpnn_pending = true;
    }

    /// Called when an animation structure is created (intermediate ML result).
    pub fn on_animation_structure_created(&mut self, structure_id: StructureId) {
        self.animation_structure = Some(structure_id);
    }

    /// Called when animation structure is removed (cancelled or completed).
    pub fn on_animation_structure_removed(&mut self) {
        self.animation_structure = None;
    }

    /// Get the best structure for MPNN to target.
    /// Priority: focused > RFD3 design > animation structure > original
    pub fn mpnn_target(&self) -> Option<StructureId> {
        match self.focus {
            Focus::Structure(id) => Some(id),
            Focus::Session => self
                .rfd3_design
                .or(self.animation_structure)
                .or(self.original),
        }
    }

    /// Get the structure that Rosetta should apply MPNN results to.
    /// This is the RFD3 design if it exists, otherwise the original.
    pub fn mpnn_apply_target(&self) -> Option<StructureId> {
        self.rfd3_design.or(self.original)
    }

    /// Check if focused structure ID is still valid.
    /// Call this after structures are removed.
    pub fn validate_focus(&mut self, structure_ids: &[StructureId]) {
        if let Focus::Structure(id) = self.focus {
            if !structure_ids.contains(&id) {
                // Focused structure was removed, revert to session
                self.focus = Focus::Session;
            }
        }
    }
}
