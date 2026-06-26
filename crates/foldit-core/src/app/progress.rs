//! App-owned puzzle high-score progress: the best display score per puzzle id
//! plus the one-shot disk-persist signal.

/// Best recorded display score per puzzle id (monotonic max) and the one-shot
/// flag that the map changed and must be flushed to disk.
///
/// A puzzle counts as complete once its best is positive, so `map` is the sole
/// gate the menu's unlock math reads. `persist_pending` is set only by a real
/// in-session record/clear, never by a load import, so a load does not bounce
/// back out as a save.
pub(in crate::app) struct ProgressStore {
    map: std::collections::BTreeMap<u32, f64>,
    persist_pending: bool,
}

impl ProgressStore {
    pub(in crate::app) const fn new() -> Self {
        Self {
            map: std::collections::BTreeMap::new(),
            persist_pending: false,
        }
    }

    /// The live high-score map, for projection to the GUI.
    pub(in crate::app) const fn map(&self) -> &std::collections::BTreeMap<u32, f64> {
        &self.map
    }

    /// Record a puzzle's display score against its high-score progress.
    /// Monotonic max: only writes (and arms the persist signal) when the score
    /// is positive and beats the puzzle's current best. Returns whether the
    /// best changed so the caller can mark the progress section dirty.
    pub(in crate::app) fn record(&mut self, puzzle_id: u32, score: f64) -> bool {
        if score <= 0.0 {
            return false;
        }
        let best = self.map.entry(puzzle_id).or_insert(f64::NEG_INFINITY);
        if score > *best {
            *best = score;
            self.persist_pending = true;
            return true;
        }
        false
    }

    /// Wipe all recorded high-score progress, arming the persist signal.
    /// Returns whether anything was cleared so the caller can mark the progress
    /// section dirty.
    pub(in crate::app) fn clear(&mut self) -> bool {
        if self.map.is_empty() {
            return false;
        }
        self.map.clear();
        self.persist_pending = true;
        true
    }

    /// Take the serialized high-score map for the host to persist to disk, or
    /// `None` when it has not changed since the last pull. Returned at most once
    /// per change.
    pub(in crate::app) fn take_to_persist(&mut self) -> Option<Vec<u8>> {
        if !self.persist_pending {
            return None;
        }
        self.persist_pending = false;
        serde_json::to_vec(&self.map).ok()
    }

    /// Merge a persisted high-score map (as written by [`Self::take_to_persist`])
    /// back into the live map. Monotonic max per puzzle so any record made
    /// in-session before the async load completed is not clobbered by a stale
    /// on-disk best. Returns whether the merge changed anything so the caller
    /// can mark the progress section dirty, but deliberately does not arm the
    /// persist signal, so a load does not bounce back out as a save.
    pub(in crate::app) fn import(&mut self, bytes: &[u8]) -> bool {
        let Ok(loaded) = serde_json::from_slice::<std::collections::BTreeMap<u32, f64>>(bytes)
        else {
            return false;
        };
        let mut changed = false;
        for (puzzle_id, score) in loaded {
            let best = self.map.entry(puzzle_id).or_insert(f64::NEG_INFINITY);
            if score > *best {
                *best = score;
                changed = true;
            }
        }
        changed
    }
}
