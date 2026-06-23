//! Session-scoped derived viz cache.

use std::collections::{BTreeSet, HashMap};

/// Session-scoped DERIVED viz cache. Regenerated from the structure via
/// plugin queries; it does NOT emit `SessionUpdate`s and is never
/// serialized or history-versioned. Cleared by [`super::Session::reset`].
///
/// Holds two kinds of derived render state. The connections set is stamped
/// onto every published assembly by the render projector. The three
/// structural-viz overlays (external cavities/voids, steric-clash arcs,
/// exposed-hydrophobic grease beads) hold viso-ready payloads the render
/// projector pushes to the engine on the drain when [`Self::viz_dirty`] is
/// set; the `crate::viz::refresh` overlay coordinators recompute them at rest each geometry
/// change (and on a view toggle) and sets the dirty flag, so they update on
/// the same ticks they go stale and freeze during a wiggle.
#[derive(Default)]
pub struct VizState {
    /// Rendering connections (hydrogen bonds + disulfides) the plugin
    /// provided for the current topology. `Some(map)` means the plugin is
    /// the live connections provider and this held, atom-index map is the
    /// one the render projector stamps verbatim (no molex geometry runs);
    /// `None` means no plugin provides them, so molex's geometric fallback
    /// is detected per publish.
    pub held_connections:
        Option<HashMap<molex::ConnectionType, Vec<molex::AtomLink>>>,
    /// Entity-id set of the assembly `held_connections` was queried for.
    /// Compared against the current head's id set each refresh to detect a
    /// topology change that invalidates the held set.
    pub connections_topology_ids: BTreeSet<molex::EntityId>,
    /// External (host-supplied) void distance field for the cavity overlay,
    /// decoded from the plugin's `voids` query. The cleared form (zero dims,
    /// empty `phi`) is the engine's signal to drop the external set, so a
    /// toggle-off stores the cleared field rather than skipping the push.
    #[cfg(not(target_arch = "wasm32"))]
    pub void_field: crate::viz::voids::VoidFieldData,
    /// Steric-clash arcs, fully resolved to viso endpoints, from the
    /// plugin's `clashes` query. An empty vec clears the arcs.
    #[cfg(not(target_arch = "wasm32"))]
    pub clashes: Vec<viso::ClashInfo>,
    /// Exposed-hydrophobic grease-bead markers, resolved to viso refs, from
    /// the plugin's `exposed_hydrophobics` query. An empty vec clears the
    /// beads.
    #[cfg(not(target_arch = "wasm32"))]
    pub exposed_hydrophobics: Vec<viso::ExposedHydrophobicInfo>,
    /// Set when any of the three overlay payloads above changed since the
    /// last push. The render projector pushes the overlays to the engine on
    /// the drain only when this is set, then clears it, so the overlays do
    /// not re-push during a wiggle (when the refresh does not run and the
    /// flag stays clear).
    #[cfg(not(target_arch = "wasm32"))]
    pub viz_dirty: bool,
    /// Entity-id set of the last published assembly, as a membership set.
    /// The render projector compares the next drain's id set against this to
    /// choose between `set_assembly` (same membership) and `replace_assembly`
    /// (an id joined or left). A per-session diff baseline owned here so
    /// [`super::Session::reset`] clears it: a new puzzle reusing the outgoing
    /// puzzle's entity ids must not inherit a stale membership set.
    pub last_published_ids: BTreeSet<molex::entity::molecule::id::EntityId>,
    /// Entity ids whose appearance overrides were last pushed to the engine
    /// working copy. The render projector uses it to detect an entry the
    /// session dropped since the last push so the engine can clear the
    /// now-stale override. A per-session diff baseline owned here so
    /// [`super::Session::reset`] clears it on a topology swap.
    pub last_pushed_appearance: BTreeSet<molex::entity::molecule::id::EntityId>,
    /// The last SS-bearing published assembly (the one a `recompute_ss` ran
    /// on), cached so a streaming tentative frame can carry its secondary
    /// structure forward onto the new coords without re-running DSSP. `None`
    /// until the first committed / load publish. A per-session diff baseline
    /// owned here so [`super::Session::reset`] clears it on a topology swap.
    pub last_ss: Option<molex::Assembly>,
}
