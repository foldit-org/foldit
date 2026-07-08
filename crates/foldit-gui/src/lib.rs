// `thiserror` 1.x and 2.x both appear in the dependency tree via transitive
// deps we do not control; the duplication is not resolvable from this crate.
#![allow(
    clippy::multiple_crate_versions,
    reason = "duplicate thiserror versions come from transitive deps, not controllable here"
)]

pub mod actions;
pub mod bridge;
pub mod state;
pub mod wire;

use bitflags::bitflags;
use serde::Serialize;

pub use actions::{AppCommand, OpDispatch, ViewportInput};
pub use bridge::{IpcMessage, RequestKind, RequestResult, Transport};
pub use state::*;
pub use wire::{
    CheckpointId, CheckpointInfo, CheckpointKindTag, EntitySnapshotId, FilterStatus,
    HistoryCommand, HistoryLiveUpdate, HistorySection, WireId,
};

bitflags! {
    #[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
    pub struct DirtyFlags: u32 {
        const SCORE        = 0b000_0000_0001;
        const SELECTION    = 0b000_0000_0010;
        const VIEW         = 0b000_0000_0100;
        const UI           = 0b000_0000_1000;
        const LOADING      = 0b000_0001_0000;
        const ACTIONS      = 0b000_0010_0000;
        const SCENE        = 0b000_0100_0000;
        const PUZZLE       = 0b000_1000_0000;
        const APP_STATE    = 0b001_0000_0000;
        const HISTORY      = 0b010_0000_0000;
        /// Tentative-score patch (`HistoryLiveUpdate`); does NOT
        /// reproject the full graph.
        const HISTORY_LIVE = 0b100_0000_0000;
        /// Active tutorial bubble changed (cursor moved, or sequence
        /// cleared). Triggers a re-push of `ui.text_bubble` without
        /// touching the rest of the UI section.
        const TEXT_BUBBLE  = 0b1000_0000_0000;
        /// Open segment-info target changed, or a score update refreshed
        /// the open segment's energies.
        const SEGMENT      = 0b1_0000_0000_0000;
        /// Panel open/closed set or a panel position changed.
        const PANELS       = 0b10_0000_0000_0000;
        /// Puzzle high-score progress changed (a new best recorded, or
        /// progress cleared).
        const PROGRESS     = 0b100_0000_0000_0000;
        /// A host-raised notification was appended (the reusable app-wide
        /// notification channel the frontend renders as a toast).
        const NOTIFICATIONS = 0b1000_0000_0000_0000;
    }
}

/// Last segment-panel tail tip projected to the screen, value-compared each
/// frame so an unchanged tip pushes nothing.
///
/// The `Unset` arm is distinct from `Hidden`: at rest (no panel ever opened)
/// the tip is `Unset`, and the off-screen path only emits a hide when a
/// `Visible` tip preceded it. Without that distinction every idle frame
/// would push a redundant hide.
#[derive(Debug, Clone, Copy, PartialEq)]
enum TailTip {
    /// No tip has been projected yet (no panel opened this session).
    Unset,
    /// The panel is open but its residue is off-screen / behind the camera.
    Hidden,
    /// The residue's CA projects to this screen position (pixels, top-left).
    Visible(f32, f32),
}

/// State sections that get pushed to the GUI when dirty.
///
/// Only dirty sections are serialized and emitted via Tauri events.
/// The GUI merges partial updates into its local store.
#[derive(Debug, Clone, Serialize)]
pub struct GuiState {
    /// Top-level lifecycle phase. Primary gate for what the GUI renders at the
    /// root level. Drives the `LoadingScreen` → game UI transition; backend
    /// advances this through the startup phases and flips it to `InSession`
    /// once the first score lands.
    pub app_state: AppPhase,
    pub score: ScoreSection,
    pub puzzle: PuzzleSection,
    pub selection: SelectionSection,
    pub view: ViewSection,
    /// Authoritative faithful view options: the sparse serialization of
    /// viso's options, where display overrides left to inherit stay absent.
    /// Round-trip source the engine reapply and the dense `view.options`
    /// wire form are both derived from in core (which owns viso); kept off
    /// the wire because the GUI reads the dense `view.options` instead.
    #[serde(skip)]
    view_options_raw: serde_json::Value,
    /// Latches once the player changes any view setting, after which a fresh
    /// load keeps the persisted options instead of re-seeding the Default
    /// preset.
    #[serde(skip)]
    view_touched: bool,
    pub ui: UISection,
    pub actions: ActionsSection,
    pub loading: LoadingSection,
    pub scene: SceneSection,
    pub history: HistorySection,
    /// Small payload pushed alongside (or instead of) `history` when
    /// only the running tentative's score / label changed. Set with
    /// `set_history_live`; cleared after the next push by the
    /// `take_dirty` cycle. Frontend patches the matching checkpoint in
    /// `state.history.checkpoints` rather than re-rendering.
    pub history_live: Option<HistoryLiveUpdate>,
    /// Per-residue segment-info panel payload. `None` when no segment is
    /// open. Set by the GUI projection's segment arm.
    pub segment_info: Option<SegmentInfo>,
    /// Open segment-info target with its cached identity + SS, or `None` when
    /// the panel is closed. Set by the core open resolution (the only place
    /// that reads `Session`); the projection's segment arm reads it back to
    /// rebuild `segment_info`.
    #[serde(skip)]
    segment_target: Option<SegmentTarget>,
    /// Last tail tip pushed to the host, value-compared each frame so an
    /// unchanged tip pushes nothing.
    #[serde(skip)]
    last_tail_tip: TailTip,
    /// Pending tail-tip change for the host to pull this frame, set only when
    /// the projected tip differed from `last_tail_tip`.
    #[serde(skip)]
    pending_tail: Option<TailUpdate>,
    /// Backend-authoritative panel open/closed set and per-panel
    /// positions. Always present; empty when no panels are open and none
    /// have been moved. The wire mirror of `panels_open` / `panels_positions`,
    /// regenerated on every mutation.
    pub panels: PanelsSection,
    /// Backend-authoritative puzzle high-score progress. Always present;
    /// empty until the player scores on a puzzle. The wire mirror of
    /// `progress_map`, regenerated on every mutation.
    pub progress: ProgressSection,
    /// Host-raised user-facing notifications, oldest first. The first
    /// reusable app-wide notification channel: any backend surface can
    /// append via `push_notification` and the frontend toasts each new
    /// entry. Bounded to the most recent few so a long session cannot
    /// grow it unbounded; the frontend dedups by `Notification::id`.
    pub notifications: Vec<Notification>,
    /// Authoritative open panel set. The `panels` wire section above is
    /// regenerated from this (together with `panels_positions`) on each
    /// mutation; a panel absent here is closed.
    #[serde(skip)]
    panels_open: std::collections::BTreeSet<String>,
    /// Authoritative per-panel dragged top-left positions, source of truth
    /// alongside `panels_open`. A panel without an entry renders at its
    /// layout default.
    #[serde(skip)]
    panels_positions: std::collections::BTreeMap<String, (f32, f32)>,
    /// Authoritative best display score per puzzle id (monotonic max). The
    /// `progress` wire section above is regenerated from this on each
    /// mutation; this map is the source of truth.
    #[serde(skip)]
    progress_map: std::collections::BTreeMap<u32, f64>,
    /// Monotonic id assigned to the next notification. Never reset, so ids
    /// stay unique across the session and the frontend's dedup high-water
    /// mark only ever advances.
    #[serde(skip)]
    notification_id: u64,
    /// One-shot flag that `progress_map` changed and must be flushed to disk.
    /// Set only by a real in-session record/clear, never by an import, so a
    /// load does not bounce back out as a save. Drained by
    /// `take_progress_to_persist`.
    #[serde(skip)]
    progress_persist_pending: bool,
    /// Host outbox for an OS fullscreen change. Staged by `set_fullscreen`
    /// only on an actual value flip and drained by `take_fullscreen_change`;
    /// the desktop host pulls it to apply the change to the winit window. Not
    /// serialized to the GUI — `ui.fullscreen` carries the value there.
    #[serde(skip)]
    pending_fullscreen: Option<bool>,
    #[serde(skip)]
    dirty: DirtyFlags,
}

impl Default for GuiState {
    fn default() -> Self {
        Self {
            app_state: AppPhase::Initializing,
            score: ScoreSection::default(),
            puzzle: PuzzleSection::default(),
            selection: SelectionSection::default(),
            view: ViewSection::default(),
            view_options_raw: serde_json::Value::Null,
            view_touched: false,
            ui: UISection::default(),
            actions: ActionsSection::default(),
            loading: LoadingSection::default(),
            scene: SceneSection::default(),
            history: HistorySection::default(),
            history_live: None,
            segment_info: None,
            segment_target: None,
            last_tail_tip: TailTip::Unset,
            pending_tail: None,
            panels: PanelsSection::default(),
            progress: ProgressSection::default(),
            notifications: Vec::new(),
            notification_id: 0,
            panels_open: std::collections::BTreeSet::new(),
            panels_positions: std::collections::BTreeMap::new(),
            progress_map: std::collections::BTreeMap::new(),
            progress_persist_pending: false,
            pending_fullscreen: None,
            dirty: DirtyFlags::empty(),
        }
    }
}

impl GuiState {
    /// Cap on retained notifications. The frontend dedups by id, so keeping
    /// only the most recent entries never drops an un-toasted message.
    const MAX_NOTIFICATIONS: usize = 20;

    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Top-level state accessors. Backend advances the phase through startup
    /// and flips it to `InSession` once the first score lands.
    #[must_use]
    pub const fn app_state(&self) -> AppPhase {
        self.app_state
    }

    pub fn set_app_state(&mut self, state: AppPhase) {
        if self.app_state != state {
            self.app_state = state;
            self.dirty |= DirtyFlags::APP_STATE;
        }
    }

    /// Set all sections as dirty (used after initial hydration)
    pub const fn mark_all_dirty(&mut self) {
        self.dirty = DirtyFlags::all();
    }

    /// Mark specific sections as dirty
    pub fn mark_dirty(&mut self, flags: DirtyFlags) {
        self.dirty |= flags;
    }

    /// Take and clear dirty flags, returning what was dirty
    pub const fn take_dirty(&mut self) -> DirtyFlags {
        let flags = self.dirty;
        self.dirty = DirtyFlags::empty();
        flags
    }

    pub fn set_score(&mut self, value: f64, invalid: bool) {
        self.score.value = value;
        self.score.invalid = invalid;
        self.dirty |= DirtyFlags::SCORE;
    }

    pub fn set_score_title(&mut self, title: String) {
        self.score.title = title;
        self.dirty |= DirtyFlags::SCORE;
    }

    /// Replace the segment-info payload (or clear it with `None`). Marks
    /// `SEGMENT` dirty unconditionally; the projection only calls this
    /// when the open target changed or its energies were refreshed.
    pub fn set_segment_info(&mut self, info: Option<SegmentInfo>) {
        self.segment_info = info;
        self.dirty |= DirtyFlags::SEGMENT;
    }

    /// Replace the open segment target (or clear it with `None`). Pure: the
    /// core open resolution sets this after its `Session` reads; the segment
    /// projection arm reads it back via [`Self::segment_target`].
    pub fn set_segment_target(&mut self, target: Option<SegmentTarget>) {
        self.segment_target = target;
    }

    /// The open segment target, or `None` when the panel is closed.
    #[must_use]
    pub const fn segment_target(&self) -> Option<&SegmentTarget> {
        self.segment_target.as_ref()
    }

    /// Close the segment panel: clear both the open target and the wire
    /// payload, marking `SEGMENT` dirty so the next push emits the clear.
    pub fn close_segment(&mut self) {
        self.segment_target = None;
        self.segment_info = None;
        self.dirty |= DirtyFlags::SEGMENT;
    }

    /// Stage a tail-tip change when the projected screen position differs from
    /// the last pushed tip. `Some((x, y))` is a visible tip; `None` is
    /// off-screen / closed. A hide is staged only when a `Visible` tip
    /// preceded it, so the initial at-rest transition pushes nothing.
    pub fn push_tail_tip(&mut self, screen_pos: Option<(f32, f32)>) {
        let current = match screen_pos {
            Some((x, y)) => TailTip::Visible(x, y),
            None => TailTip::Hidden,
        };

        if current == self.last_tail_tip {
            return;
        }

        match current {
            TailTip::Visible(x, y) => self.pending_tail = Some(TailUpdate::Position(x, y)),
            TailTip::Hidden => {
                if matches!(self.last_tail_tip, TailTip::Visible(..)) {
                    self.pending_tail = Some(TailUpdate::Hide);
                }
            }
            TailTip::Unset => {}
        }
        self.last_tail_tip = current;
    }

    /// Take the pending tail-tip change for the host to push, or `None` when
    /// the tip did not move since the last push. Returned at most once per
    /// change: `push_tail_tip` sets it only on a value change and this clears
    /// it.
    pub const fn take_tail_update(&mut self) -> Option<TailUpdate> {
        self.pending_tail.take()
    }

    /// Show or hide a panel by id. Regenerates the wire `panels` section and
    /// marks `PANELS` dirty unconditionally.
    pub fn set_panel_visible(&mut self, panel: String, visible: bool) {
        if visible {
            self.panels_open.insert(panel);
        } else {
            self.panels_open.remove(&panel);
        }
        self.refresh_panels_section();
    }

    /// Open the action picker for `op_id`, or close any open picker when
    /// `None`. Pure UI state on the `actions` section; marks `ACTIONS` dirty
    /// so the next push carries the change.
    pub fn set_action_picker_open(&mut self, op_id: Option<String>) {
        self.actions.open_picker = op_id;
        self.dirty |= DirtyFlags::ACTIONS;
    }

    /// The `op_id` of the currently-open action picker, or `None`.
    #[must_use]
    pub fn action_picker_open(&self) -> Option<&str> {
        self.actions.open_picker.as_deref()
    }

    /// Replace the per-plugin download-progress map. Marks `ACTIONS` dirty
    /// so the next push carries the change. Empty map clears all progress
    /// fills.
    pub fn set_download_progress(
        &mut self,
        map: std::collections::HashMap<String, DownloadProgress>,
    ) {
        self.actions.download_progress = map;
        self.dirty |= DirtyFlags::ACTIONS;
    }

    /// Record a panel's dragged top-left position. Regenerates the wire
    /// `panels` section and marks `PANELS` dirty unconditionally.
    pub fn set_panel_position(&mut self, panel: String, x: f32, y: f32) {
        self.panels_positions.insert(panel, (x, y));
        self.refresh_panels_section();
    }

    /// Rebuild the `panels` wire section from the authoritative open set and
    /// position map (both in `BTreeSet` / `BTreeMap` sort order) and mark
    /// `PANELS` dirty so the next push emits it.
    fn refresh_panels_section(&mut self) {
        self.panels = PanelsSection {
            open: self.panels_open.iter().cloned().collect(),
            positions: self
                .panels_positions
                .iter()
                .map(|(panel, &(x, y))| PanelPosition {
                    panel: panel.clone(),
                    x,
                    y,
                })
                .collect(),
        };
        self.dirty |= DirtyFlags::PANELS;
    }

    /// Record a puzzle's display score against its high-score progress.
    /// Monotonic max: only writes (and arms the persist signal) when the
    /// score is positive and beats the puzzle's current best. A puzzle counts
    /// as complete once its best is positive, so this map is the sole gate the
    /// menu's unlock math reads. Refreshes the wire section and marks
    /// `PROGRESS` dirty when the best changed.
    pub fn record_progress(&mut self, puzzle_id: u32, score: f64) {
        if score <= 0.0 {
            return;
        }
        let best = self
            .progress_map
            .entry(puzzle_id)
            .or_insert(f64::NEG_INFINITY);
        if score > *best {
            *best = score;
            self.progress_persist_pending = true;
            self.refresh_progress_section();
        }
    }

    /// Wipe all recorded high-score progress, arming the persist signal.
    /// Refreshes the wire section and marks `PROGRESS` dirty when anything
    /// was cleared.
    pub fn clear_progress(&mut self) {
        if self.progress_map.is_empty() {
            return;
        }
        self.progress_map.clear();
        self.progress_persist_pending = true;
        self.refresh_progress_section();
    }

    /// Take the serialized high-score map for the host to persist to disk, or
    /// `None` when it has not changed since the last pull. Returned at most
    /// once per change.
    pub fn take_progress_to_persist(&mut self) -> Option<Vec<u8>> {
        if !self.progress_persist_pending {
            return None;
        }
        self.progress_persist_pending = false;
        serde_json::to_vec(&self.progress_map).ok()
    }

    /// Merge a persisted high-score map (as written by
    /// [`Self::take_progress_to_persist`]) back into the live map. Monotonic
    /// max per puzzle so any record made in-session before the async load
    /// completed is not clobbered by a stale on-disk best. Refreshes the wire
    /// section and marks `PROGRESS` dirty when the merge changed anything, but
    /// deliberately does not arm the persist signal, so a load does not bounce
    /// back out as a save.
    pub fn import_progress(&mut self, bytes: &[u8]) {
        let Ok(loaded) = serde_json::from_slice::<std::collections::BTreeMap<u32, f64>>(bytes)
        else {
            return;
        };
        let mut changed = false;
        for (puzzle_id, score) in loaded {
            let best = self
                .progress_map
                .entry(puzzle_id)
                .or_insert(f64::NEG_INFINITY);
            if score > *best {
                *best = score;
                changed = true;
            }
        }
        if changed {
            self.refresh_progress_section();
        }
    }

    /// Rebuild the `progress` wire section from the authoritative map and mark
    /// `PROGRESS` dirty so the next push emits it.
    fn refresh_progress_section(&mut self) {
        self.progress = ProgressSection {
            entries: self
                .progress_map
                .iter()
                .map(|(&puzzle_id, &high_score)| ProgressEntry {
                    puzzle_id,
                    high_score,
                })
                .collect(),
        };
        self.dirty |= DirtyFlags::PROGRESS;
    }

    /// Use `set_puzzle_game` for tutorial/campaign puzzles (with
    /// target/starting scores from the toml) and `set_puzzle_scientist`
    /// for free-form / CLI loads.
    pub fn set_puzzle_game(
        &mut self,
        puzzle_id: u32,
        title: String,
        starting_score: f64,
        target_score: f64,
    ) {
        self.puzzle = PuzzleSection {
            mode: ScoringMode::Game,
            puzzle_id,
            title,
            starting_score,
            target_score,
            complete: false,
        };
        self.dirty |= DirtyFlags::PUZZLE;
    }

    pub fn set_puzzle_scientist(&mut self, title: String) {
        self.puzzle = PuzzleSection {
            mode: ScoringMode::Scientist,
            puzzle_id: 0,
            title,
            starting_score: 0.0,
            target_score: 0.0,
            complete: false,
        };
        self.dirty |= DirtyFlags::PUZZLE;
    }

    /// Latch the puzzle as complete. Idempotent — only marks dirty on the
    /// false→true transition so the frontend sees a single victory event.
    pub fn mark_puzzle_complete(&mut self) {
        if !self.puzzle.complete {
            self.puzzle.complete = true;
            self.dirty |= DirtyFlags::PUZZLE;
        }
    }

    pub fn set_fps(&mut self, fps: f32) {
        self.ui.fps = fps;
        self.dirty |= DirtyFlags::UI;
    }

    /// Replace the active text bubble (or clear with `None`). Marks UI
    /// dirty unconditionally so the frontend sees explicit clears even
    /// when re-setting an equivalent payload.
    pub fn set_text_bubble(&mut self, bubble: Option<TextBubblePayload>) {
        self.ui.text_bubble = bubble;
        self.dirty |= DirtyFlags::UI;
    }

    pub fn set_log(&mut self, log: String) {
        self.ui.log = log;
        self.dirty |= DirtyFlags::UI;
    }

    /// Enter or leave OS fullscreen. Value-gates the host outbox: only sets
    /// `ui.fullscreen` and stages `pending_fullscreen` on an actual
    /// false->true / true->false flip, so re-setting the same value stages
    /// nothing for the host to pull. Marks `UI` dirty.
    pub fn set_fullscreen(&mut self, value: bool) {
        if self.ui.fullscreen != value {
            self.ui.fullscreen = value;
            self.pending_fullscreen = Some(value);
        }
        self.dirty |= DirtyFlags::UI;
    }

    /// Drain the staged fullscreen change for the desktop host, or `None` when
    /// it did not flip since the last pull. Returned at most once per change.
    pub const fn take_fullscreen_change(&mut self) -> Option<bool> {
        self.pending_fullscreen.take()
    }

    /// Show or hide the tutorial-hint bubble. Marks `UI` dirty.
    pub fn set_hints_visible(&mut self, value: bool) {
        self.ui.hints_visible = value;
        self.dirty |= DirtyFlags::UI;
    }

    pub fn set_loading_progress(&mut self, progress: Option<f32>) {
        self.loading.progress = progress;
        self.dirty |= DirtyFlags::LOADING;
    }

    pub fn set_puzzle_loaded(&mut self, loaded: bool) {
        self.loading.puzzle_loaded = loaded;
        self.dirty |= DirtyFlags::LOADING;
    }

    pub fn set_actions(
        &mut self,
        available: Vec<state::ActionInfo>,
        groups: Vec<state::PluginGroupInfo>,
    ) {
        self.actions.available = available;
        self.actions.groups = groups;
        // Close an open picker whose trigger is gone from the re-projected
        // catalog, or is present but no longer enabled (e.g. the selection
        // grew past a single residue). The picker's open/closed flag is
        // otherwise preserved across re-projection.
        if let Some(op) = self.actions.open_picker.as_deref() {
            let still_open = self
                .actions
                .available
                .iter()
                .any(|a| a.op_id == op && a.enabled);
            if !still_open {
                self.actions.open_picker = None;
            }
        }
        self.dirty |= DirtyFlags::ACTIONS;
    }

    /// Replace the per-entity selection list. Marks `SELECTION` dirty
    /// unconditionally — callers only invoke this when [`App::selection`]
    /// has actually changed.
    pub fn set_selection(&mut self, entries: Vec<state::EntitySelection>) {
        self.selection.entries = entries;
        self.dirty |= DirtyFlags::SELECTION;
    }

    pub fn set_scene_entities(&mut self, entities: Vec<state::SceneEntityInfo>) {
        if self.scene.entities != entities {
            self.scene.entities = entities;
            self.dirty |= DirtyFlags::SCENE;
        }
    }

    /// Set the currently-focused entity (mirrors viso's `Focus`). Pass
    /// `None` for whole-session focus.
    pub fn set_focused_entity(&mut self, focused: Option<u32>) {
        if self.scene.focused_entity != focused {
            self.scene.focused_entity = focused;
            self.dirty |= DirtyFlags::SCENE;
        }
    }

    /// Apply a manual view-options edit: adopt the faithful options, drop
    /// the active preset (manual options match no named preset), and latch
    /// `view_touched`. Returns whether the options or the preset actually
    /// changed, so the caller notes a single view change only on a real edit.
    pub fn set_view_manual(&mut self, raw: serde_json::Value) -> bool {
        let changed = self.view_options_raw != raw || self.view.active_preset.is_some();
        self.view_options_raw = raw;
        self.view.active_preset = None;
        self.view_touched = true;
        changed
    }

    /// Adopt a named preset's faithful options and record the preset name.
    /// Deliberately does not latch `view_touched` — an automatic seed is not
    /// a player edit; the explicit preset-pick path latches it separately.
    pub fn set_view_preset(&mut self, raw: serde_json::Value, name: String) {
        self.view_options_raw = raw;
        self.view.active_preset = Some(name);
    }

    /// The authoritative faithful view options. Core deserializes this into
    /// viso's options to reapply to the engine and to densify for the wire.
    #[must_use]
    pub const fn view_options_raw(&self) -> &serde_json::Value {
        &self.view_options_raw
    }

    /// Whether the player has changed any view setting this run.
    #[must_use]
    pub const fn view_touched(&self) -> bool {
        self.view_touched
    }

    /// Latch or clear the player-touched view flag. The explicit
    /// preset-pick path latches it so the choice survives later loads.
    pub const fn set_view_touched(&mut self, touched: bool) {
        self.view_touched = touched;
    }

    /// Push the densified wire form of the view options into the section the
    /// GUI settings panel reads. Core resolves the `None` display overrides
    /// to their effective values first; that densify is a viso operation and
    /// stays in core.
    pub fn set_view_options_dense(&mut self, dense: serde_json::Value) {
        self.view.options = dense;
    }

    /// History section accessors.
    #[must_use]
    pub const fn history(&self) -> &HistorySection {
        &self.history
    }

    /// Replace the history section. Marks dirty unconditionally (caller
    /// only invokes this when `History::topology_version()` has bumped).
    pub fn set_history(&mut self, history: HistorySection) {
        self.history = history;
        self.dirty |= DirtyFlags::HISTORY;
    }

    /// Stage a live tentative-score patch. Marks `HISTORY_LIVE` dirty
    /// only — does NOT mark `HISTORY` (no full reproject). Callers
    /// invoke this when only `History::live_version()` ticked. Frontend
    /// patches the matching checkpoint in place.
    pub fn set_history_live(&mut self, update: HistoryLiveUpdate) {
        self.history_live = Some(update);
        self.dirty |= DirtyFlags::HISTORY_LIVE;
    }

    /// Append a user-facing notification and mark `NOTIFICATIONS` dirty. Each
    /// call assigns a fresh monotonic id; the frontend toasts only ids above
    /// the highest it has shown. The retained list is capped at the most
    /// recent [`Self::MAX_NOTIFICATIONS`] so it never grows unbounded, which
    /// is safe because dropped entries keep ids below that high-water mark.
    pub fn push_notification(&mut self, level: NotificationLevel, text: String) {
        self.notification_id += 1;
        self.notifications.push(Notification {
            id: self.notification_id,
            level,
            text,
        });
        let len = self.notifications.len();
        if len > Self::MAX_NOTIFICATIONS {
            self.notifications.drain(0..len - Self::MAX_NOTIFICATIONS);
        }
        self.dirty |= DirtyFlags::NOTIFICATIONS;
    }
}
