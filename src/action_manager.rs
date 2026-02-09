//! Action Manager - Per-structure action locking system
//!
//! Tracks which groups have active operations, enabling per-group locking
//! and preventing conflicting operations on the same group.

use foldit_render::scene::GroupId;
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

/// A lock held on a group while an action is in progress.
#[derive(Debug)]
pub struct ActionLock {
    pub group_id: GroupId,
    pub action_type: ActionType,
    pub cancel_flag: Arc<AtomicBool>,
}

/// Manages active actions on groups, providing per-group locking.
#[derive(Debug, Default)]
pub struct ActionManager {
    active: HashMap<u64, ActionLock>,
}

impl ActionManager {
    pub fn new() -> Self {
        Self {
            active: HashMap::new(),
        }
    }

    /// Try to acquire a lock on a group for the given action.
    /// Returns the cancel flag if successful, None if the group is already locked.
    pub fn try_lock(
        &mut self,
        group_id: GroupId,
        action_type: ActionType,
    ) -> Option<Arc<AtomicBool>> {
        if self.active.contains_key(&group_id.0) {
            return None;
        }

        let cancel_flag = Arc::new(AtomicBool::new(false));
        self.active.insert(
            group_id.0,
            ActionLock {
                group_id,
                action_type,
                cancel_flag: cancel_flag.clone(),
            },
        );

        Some(cancel_flag)
    }

    /// Release the lock on a group.
    pub fn unlock(&mut self, group_id: GroupId) {
        self.active.remove(&group_id.0);
    }

    /// Check if a group is currently locked.
    pub fn is_locked(&self, group_id: GroupId) -> bool {
        self.active.contains_key(&group_id.0)
    }

    /// Get the action type for a locked group.
    pub fn get_action_type(&self, group_id: GroupId) -> Option<ActionType> {
        self.active.get(&group_id.0).map(|lock| lock.action_type)
    }

    /// Request cancellation of the action on a group.
    pub fn request_cancel(&self, group_id: GroupId) -> bool {
        if let Some(lock) = self.active.get(&group_id.0) {
            lock.cancel_flag
                .store(true, std::sync::atomic::Ordering::SeqCst);
            true
        } else {
            false
        }
    }

    /// Get all currently locked group IDs.
    pub fn locked_groups(&self) -> Vec<GroupId> {
        self.active.values().map(|lock| lock.group_id).collect()
    }
}
