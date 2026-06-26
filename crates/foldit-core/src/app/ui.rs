//! App-owned UI toggles: the OS fullscreen mirror plus its host outbox, and
//! the tutorial-hint bubble visibility.

/// Backend-authoritative UI toggles that survive a reload.
///
/// `fullscreen` mirrors the OS fullscreen flag and pairs with
/// `pending_fullscreen` as a value+outbox: a flag flip stages the new value
/// for the desktop host to pull, while re-setting the same value stages
/// nothing. `hints_visible` controls whether the tutorial-hint bubble is shown.
pub(in crate::app) struct UiToggles {
    fullscreen: bool,
    pending_fullscreen: Option<bool>,
    hints_visible: bool,
}

impl UiToggles {
    pub(in crate::app) const fn new() -> Self {
        Self {
            fullscreen: false,
            pending_fullscreen: None,
            hints_visible: true,
        }
    }

    /// The OS fullscreen mirror, for projection to the GUI.
    pub(in crate::app) const fn fullscreen(&self) -> bool {
        self.fullscreen
    }

    /// Whether the tutorial-hint bubble is shown, for projection to the GUI.
    pub(in crate::app) const fn hints_visible(&self) -> bool {
        self.hints_visible
    }

    /// Enter or leave OS fullscreen. Stages the change for the host to pull
    /// only on a false->true / true->false flip, so re-setting the same value
    /// stages nothing.
    pub(in crate::app) const fn set_fullscreen(&mut self, value: bool) {
        if self.fullscreen != value {
            self.fullscreen = value;
            self.pending_fullscreen = Some(value);
        }
    }

    /// Drain the staged fullscreen change for the desktop host, or `None` when
    /// it did not flip since the last pull. Returned at most once per change.
    pub(in crate::app) const fn take_fullscreen_change(&mut self) -> Option<bool> {
        self.pending_fullscreen.take()
    }

    /// Show or hide the tutorial-hint bubble.
    pub(in crate::app) const fn set_hints_visible(&mut self, visible: bool) {
        self.hints_visible = visible;
    }
}
