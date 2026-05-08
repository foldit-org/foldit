//! Focus translation helpers.
//!
//! Convert between viso's `Focus` (Session vs. specific entity) and the
//! raw entity ids used by backend operations.

use viso::Focus;

/// Whether focus is session-wide.
pub fn is_session_mode(focus: &Focus) -> bool {
    matches!(focus, Focus::Session)
}

/// Which entity a backend operation should target.
/// Returns `Some(id)` when focused on a specific entity.
pub fn operation_target(focus: &Focus) -> Option<u32> {
    match focus {
        Focus::Session => None,
        Focus::Entity(id) => Some(id.raw()),
    }
}

/// Entity to lock for backend operations. Falls back to loaded entity.
pub fn lock_target(focus: &Focus, loaded: Option<u32>) -> Option<u32> {
    match focus {
        Focus::Session => loaded,
        Focus::Entity(id) => Some(id.raw()),
    }
}
