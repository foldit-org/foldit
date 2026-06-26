//! App-owned panel UI state: which panels are shown and where the player
//! dragged each one.

/// The set of panels currently shown (by string id) plus each panel's dragged
/// top-left position.
///
/// Both are backend-authoritative so a panel's visibility and position survive
/// a reload. A panel absent from `open` is closed; a panel without a
/// `positions` entry renders at its layout default.
pub(in crate::app) struct PanelState {
    open: std::collections::BTreeSet<String>,
    positions: std::collections::BTreeMap<String, (f32, f32)>,
}

impl PanelState {
    pub(in crate::app) const fn new() -> Self {
        Self {
            open: std::collections::BTreeSet::new(),
            positions: std::collections::BTreeMap::new(),
        }
    }

    /// The set of open panel ids, for projection to the GUI.
    pub(in crate::app) const fn open(&self) -> &std::collections::BTreeSet<String> {
        &self.open
    }

    /// The per-panel dragged positions, for projection to the GUI.
    pub(in crate::app) const fn positions(
        &self,
    ) -> &std::collections::BTreeMap<String, (f32, f32)> {
        &self.positions
    }

    /// Show or hide a panel by id.
    pub(in crate::app) fn set_visible(&mut self, panel: String, visible: bool) {
        if visible {
            self.open.insert(panel);
        } else {
            self.open.remove(&panel);
        }
    }

    /// Record a panel's dragged top-left position.
    pub(in crate::app) fn set_position(&mut self, panel: String, x: f32, y: f32) {
        self.positions.insert(panel, (x, y));
    }
}
