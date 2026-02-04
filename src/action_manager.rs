//! Action Manager - Per-structure action locking system
//!
//! Tracks which structures have active operations, enabling per-structure locking
//! and preventing conflicting operations on the same structure.

use crate::scene::StructureId;
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

/// Types of actions that can be performed on a structure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionType {
    RosettaWiggle,
    RosettaShake,
    RosettaMutate,
    MLPredict,
    MLSequenceDesign,
    MLStructureDesign,
}

impl std::fmt::Display for ActionType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ActionType::RosettaWiggle => write!(f, "Wiggle"),
            ActionType::RosettaShake => write!(f, "Shake"),
            ActionType::RosettaMutate => write!(f, "Mutate"),
            ActionType::MLPredict => write!(f, "Predict"),
            ActionType::MLSequenceDesign => write!(f, "Sequence Design"),
            ActionType::MLStructureDesign => write!(f, "Structure Design"),
        }
    }
}

/// A lock held on a structure while an action is in progress.
#[derive(Debug)]
pub struct ActionLock {
    pub structure_id: StructureId,
    pub action_type: ActionType,
    pub cancel_flag: Arc<AtomicBool>,
}

/// Manages active actions on structures, providing per-structure locking.
#[derive(Debug, Default)]
pub struct ActionManager {
    active: HashMap<StructureId, ActionLock>,
}

impl ActionManager {
    /// Create a new action manager.
    pub fn new() -> Self {
        Self {
            active: HashMap::new(),
        }
    }

    /// Try to acquire a lock on a structure for the given action.
    /// Returns the cancel flag if successful, None if the structure is already locked.
    pub fn try_lock(
        &mut self,
        structure_id: StructureId,
        action_type: ActionType,
    ) -> Option<Arc<AtomicBool>> {
        if self.active.contains_key(&structure_id) {
            return None;
        }

        let cancel_flag = Arc::new(AtomicBool::new(false));
        self.active.insert(
            structure_id,
            ActionLock {
                structure_id,
                action_type,
                cancel_flag: cancel_flag.clone(),
            },
        );

        Some(cancel_flag)
    }

    /// Release the lock on a structure.
    pub fn unlock(&mut self, structure_id: StructureId) {
        self.active.remove(&structure_id);
    }

    /// Check if a structure is currently locked.
    pub fn is_locked(&self, structure_id: StructureId) -> bool {
        self.active.contains_key(&structure_id)
    }

    /// Get the action type for a locked structure.
    pub fn get_action_type(&self, structure_id: StructureId) -> Option<ActionType> {
        self.active.get(&structure_id).map(|lock| lock.action_type)
    }

    /// Request cancellation of the action on a structure.
    /// Returns true if the structure was locked and cancellation was requested.
    pub fn request_cancel(&self, structure_id: StructureId) -> bool {
        if let Some(lock) = self.active.get(&structure_id) {
            lock.cancel_flag
                .store(true, std::sync::atomic::Ordering::SeqCst);
            true
        } else {
            false
        }
    }

    /// Get all currently locked structure IDs.
    pub fn locked_structures(&self) -> Vec<StructureId> {
        self.active.keys().copied().collect()
    }
}
