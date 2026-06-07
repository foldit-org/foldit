//! Per-entity domain metadata: how an entity entered the scene. Owned
//! by [`super::Session`] in an `Arc`-shared `IndexMap`; re-exported
//! through the parent module.

/// How an entity entered the scene.
#[derive(Debug, Clone)]
pub enum EntityOrigin {
    /// Loaded from file or puzzle.
    Loaded,
}

/// Per-entity metadata that rides alongside the entity payload.
///
/// Visibility is **not** here - that lives on viso's
/// `EntityAnnotations`. The previous `is_preview: bool` flag is also
/// gone - presence in [`super::Session::transient`] is the preview signal.
#[derive(Debug, Clone)]
pub struct EntityMetadata {
    /// Display name.
    pub name: String,
    /// How the entity entered the scene.
    pub origin: EntityOrigin,
}

impl EntityMetadata {
    /// Build a minimal metadata record.
    #[must_use]
    pub const fn new(name: String, origin: EntityOrigin) -> Self {
        Self { name, origin }
    }
}
