//! App-owned bundle of the in-flight op-stream token maps.

use std::collections::HashMap;

use crate::history::CheckpointId;

/// The three per-token registries that track an in-flight op stream, all keyed
/// by the same `u64` edit/request token and torn down together at the stream
/// terminal (or on reload via [`OpStreamState::clear`]). They are borrowed
/// independently from `preview.rs` / `dispatch.rs` / `score_apply.rs` alongside
/// other `App` state, so the sub-fields stay public to the `app` module rather
/// than hiding behind combined accessors.
pub(in crate::app) struct OpStreamState {
    pub(in crate::app) score_targets: HashMap<u64, CheckpointId>,
    pub(in crate::app) creates_previews: HashMap<u64, (molex::EntityId, usize)>,
    /// Live in-place preview ghosts keyed by edit token, each `(ghost entity
    /// id, last atom count)`. A preview-style op opens its in-place edit
    /// normally (the lane stays frozen) and animates a discardable gray clone
    /// here; the ghost is removed at the terminal, never promoted. Kept
    /// separate from `creates_previews` so the commit fork stays unambiguous.
    pub(in crate::app) inplace_previews: HashMap<u64, (molex::EntityId, usize)>,
}

impl OpStreamState {
    pub(in crate::app) fn new() -> Self {
        Self {
            score_targets: HashMap::new(),
            creates_previews: HashMap::new(),
            inplace_previews: HashMap::new(),
        }
    }

    /// Drop every token map so the op-stream registries fall out of scope
    /// together with the `History` rebuild a reload performs.
    pub(in crate::app) fn clear(&mut self) {
        self.score_targets.clear();
        self.creates_previews.clear();
        self.inplace_previews.clear();
    }
}
