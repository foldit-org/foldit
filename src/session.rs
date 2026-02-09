//! Session management for structure relationships and operation targeting.
//!
//! The Session module provides a single source of truth for:
//! - Structure relationships (original -> RFD3 design -> MPNN design)
//! - What structure backend operations (Wiggle/Shake) should target
//!
//! Focus state is owned by the render engine's Scene.

use foldit_render::scene::{Focus, GroupId};
use glam::Vec3;

/// Tracks structure relationships and backend operation targeting.
#[derive(Debug, Default)]
pub struct Session {
    /// Original loaded structure
    pub original: Option<GroupId>,
    /// Most recent RFD3 backbone design
    pub rfd3_design: Option<GroupId>,
    /// Most recent MPNN-sequenced design
    pub mpnn_design: Option<GroupId>,
    /// In-flight ML animation structure
    pub animation_structure: Option<GroupId>,
    /// Pending MPNN sequence application
    pub mpnn_pending: bool,
    /// Original backbone CA positions for Kabsch alignment
    pub original_backbone_ca: Option<Vec<Vec3>>,
}

impl Session {
    pub fn new() -> Self {
        Self::default()
    }

    /// Check if we're in session mode (operate on all structures).
    pub fn is_session_mode(&self, focus: &Focus) -> bool {
        matches!(focus, Focus::Session)
    }

    /// Get the structure that backend operations should target.
    /// Returns None in session mode (use combined coords instead).
    pub fn operation_target(&self, focus: &Focus) -> Option<GroupId> {
        match focus {
            Focus::Session | Focus::Entity(..) => None,
            Focus::Group(id) => Some(*id),
        }
    }

    /// Get a group ID for locking purposes.
    /// In session mode, returns the original group ID as the lock target.
    /// In single-structure mode, returns the focused group ID.
    pub fn lock_target(&self, focus: &Focus) -> Option<GroupId> {
        match focus {
            Focus::Session | Focus::Entity(..) => self.original,
            Focus::Group(id) => Some(*id),
        }
    }

    /// Get human-readable description of current focus (short, without scene access).
    pub fn focus_short_description(&self, focus: &Focus) -> String {
        match focus {
            Focus::Session => "Session".to_string(),
            Focus::Group(_) => "Single structure".to_string(),
            Focus::Entity(_) => "Entity".to_string(),
        }
    }

    // ========== Lifecycle Hooks ==========

    pub fn on_original_loaded(&mut self, group_id: GroupId, backbone_ca: Vec<Vec3>) {
        self.original = Some(group_id);
        self.original_backbone_ca = Some(backbone_ca);
    }

    pub fn on_rfd3_complete(&mut self, group_id: GroupId) {
        self.rfd3_design = Some(group_id);
        self.animation_structure = None;
    }

    pub fn on_mpnn_complete(&mut self, group_id: GroupId) {
        self.mpnn_design = Some(group_id);
        self.mpnn_pending = false;
    }

    pub fn on_mpnn_start(&mut self) {
        self.mpnn_pending = true;
    }

    pub fn on_animation_structure_created(&mut self, group_id: GroupId) {
        self.animation_structure = Some(group_id);
    }

    pub fn on_animation_structure_removed(&mut self) {
        self.animation_structure = None;
    }

    /// Get the best structure for MPNN to target.
    /// Priority: focused > RFD3 design > animation structure > original
    pub fn mpnn_target(&self, focus: &Focus) -> Option<GroupId> {
        match focus {
            Focus::Group(id) => Some(*id),
            Focus::Session | Focus::Entity(..) => self
                .rfd3_design
                .or(self.animation_structure)
                .or(self.original),
        }
    }

    /// Get the structure that Rosetta should apply MPNN results to.
    pub fn mpnn_apply_target(&self) -> Option<GroupId> {
        self.rfd3_design.or(self.original)
    }
}
