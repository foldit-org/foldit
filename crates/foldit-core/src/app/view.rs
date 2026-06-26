//! App-owned view configuration: the active viso options, the named preset
//! they came from, and the player-touched latch.

/// The active view options plus the small preset/touched state machine.
///
/// `options` is the source of truth pushed to the engine and projected to the
/// GUI; it survives a session reset so a topology swap keeps the player's
/// coloring. `active_preset` names the preset `options` was loaded from, or
/// `None` when they were set manually. `touched` latches once the player
/// changes any view setting, after which a fresh load keeps the persisted
/// options instead of re-seeding the Default preset.
pub(in crate::app) struct ViewState {
    pub(in crate::app) options: viso::options::VisoOptions,
    pub(in crate::app) active_preset: Option<String>,
    pub(in crate::app) touched: bool,
}

impl ViewState {
    pub(in crate::app) fn new() -> Self {
        Self {
            options: viso::options::VisoOptions::default(),
            active_preset: None,
            touched: false,
        }
    }

    /// Apply a manual view-options edit: adopt the options, drop the active
    /// preset (manual options match no named preset), and latch `touched`.
    /// Returns whether the options or the preset actually changed, so the
    /// caller can note a single `ViewOptionsChanged` only on a real edit.
    pub(in crate::app) fn set_manual(&mut self, options: viso::options::VisoOptions) -> bool {
        let changed = self.options != options || self.active_preset.is_some();
        self.options = options;
        self.active_preset = None;
        self.touched = true;
        changed
    }

    /// Adopt a named preset's options and record the preset name.
    pub(in crate::app) fn set_preset(&mut self, options: viso::options::VisoOptions, name: &str) {
        self.options = options;
        self.active_preset = Some(name.to_owned());
    }
}
