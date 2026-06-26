//! Foldit application state - host-agnostic.
//!
//! `App` owns the following:
//! - `Session`
//! - `RunnerClient` (which carries the orchestrator)
//! - The three projectors (`RunnerProjector`, `RenderProjector`, `GuiProjector`)
//! - The cross-cutting bookkeeping
//!   (Puzzle metadata, viso engine handle, dirty-flags, history-version trackers).
//!
//! Host crates wrap this in their own lifecycle, forwarding host-agnostic
//! input types to `App`'s methods.

use foldit_gui::{AppPhase, DirtyFlags, FrontendState};
use viso::{KeyBindings, VisoEngine};

use crate::gui_projector::{GuiProjector, GuiSources};
#[cfg(not(target_arch = "wasm32"))]
use crate::history::CheckpointId;
use crate::render_projector::RenderProjector;
use crate::runner_client::RunnerClient;
use crate::runner_projector::RunnerProjector;
use crate::session::{Session, SessionUpdate, SessionUpdateConsumer};

mod dispatch;
pub(crate) mod input;
mod load;
mod panels;
mod preview;
mod progress;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod score_apply;
mod segment;
mod startup;
#[cfg(test)]
mod tests;
mod ui;
mod view;

use self::input::update_all_visualizations;
use self::panels::PanelState;
use self::progress::ProgressStore;
use self::segment::SegmentPanel;
use self::ui::UiToggles;
use self::view::ViewState;
#[cfg(not(target_arch = "wasm32"))]
pub use self::load::{locate_plugin_ui_entrypoints, locate_plugins_root};
pub use self::segment::TailUpdate;
pub(crate) use self::segment::SegmentTarget;

/// Main application state - thin glue connecting the render engine,
/// plugin driver, the `Session` store, and the three projectors
/// (`RunnerProjector`, `RenderProjector`, `GuiProjector`).
///
/// `App` also owns the host-bound [`FrontendState`] mirror (so the load
/// state-machine and the GUI projection both live on the same side of
/// the host seam) and the `AppPhase` machine that drives the startup
/// phases up to the first-score `InSession` flip.
pub struct App {
    // Session encapsulates all state that shares a lifecycle with
    // a structure or puzzle that is loaded into the client.
    pub(crate) store: Session,

    // The Viso Engine and Runner Client encapsulate app level state that
    // inits on app startup, predating and outliving any individual session
    pub(crate) engine: Option<VisoEngine>,
    pub(crate) runner_client: RunnerClient,

    pub(in crate::app) frontend: FrontendState,
    pub(in crate::app) keybindings: KeyBindings,
    pub(in crate::app) lifecycle: AppPhase,

    /// Active view options, the preset they came from, and the player-touched
    /// latch. App-owned so they persist across a reload.
    pub(in crate::app) view: ViewState,

    // Three projector classes which are the consumers of Assembly updates
    pub(in crate::app) runner_projector: RunnerProjector,
    pub(in crate::app) render_projector: RenderProjector,
    pub(in crate::app) gui_projector: GuiProjector,

    /// Host-provided filesystem / resource access. The only path through
    /// which foldit-core touches the filesystem outside puzzle loading.
    pub(in crate::app) host: Box<dyn crate::HostResources>,

    #[cfg(not(target_arch = "wasm32"))]
    pub(in crate::app) startup: self::startup::StartupState,
    /// One-shot accumulator of App-owned GUI dirty bits the `SessionUpdate`
    /// batch cannot express (segment / panels / ui / progress), plus the
    /// full-populate seed (`DirtyFlags::all()`) raised on session birth.
    pub(in crate::app) pending_dirty: DirtyFlags,
    /// Per-residue segment-info panel: open target with cached identity + SS,
    /// plus the tail-tip debounce cursor.
    pub(in crate::app) segment: SegmentPanel,
    /// Panel UI state: which panels are shown (by string id) and each panel's
    /// dragged top-left. Backend-authoritative so both survive a reload.
    pub(in crate::app) panels: PanelState,
    /// Puzzle high-score progress: best recorded display score per puzzle id
    /// (monotonic max) plus the disk-persist signal. Backend-authoritative.
    pub(in crate::app) progress: ProgressStore,
    /// OS fullscreen mirror + its host outbox and the tutorial-hint bubble
    /// visibility. Backend-authoritative so the toggles survive a reload.
    pub(in crate::app) ui: UiToggles,
    #[cfg(not(target_arch = "wasm32"))]
    pub(in crate::app) score_targets: std::collections::HashMap<u64, CheckpointId>,
    #[cfg(not(target_arch = "wasm32"))]
    pub(in crate::app) creates_previews: std::collections::HashMap<u64, (molex::EntityId, usize)>,
    /// Live in-place preview ghosts keyed by edit token, each `(ghost entity
    /// id, last atom count)`. A preview-style op opens its in-place edit
    /// normally (the lane stays frozen) and animates a discardable gray clone
    /// here; the ghost is removed at the terminal, never promoted. Kept
    /// separate from `creates_previews` so the commit fork stays unambiguous.
    #[cfg(not(target_arch = "wasm32"))]
    pub(in crate::app) inplace_previews: std::collections::HashMap<u64, (molex::EntityId, usize)>,
    /// `begin_action` args (`lanes`, `kind`, `display`) retained per edit
    /// token for preview ops so a non-terminal checkpoint can re-open the
    /// same edit for the next segment. Populated where the edit is first
    /// opened, removed at the terminal alongside `inplace_previews`.
    #[cfg(not(target_arch = "wasm32"))]
    pub(in crate::app) inplace_edits:
        std::collections::HashMap<u64, (Vec<molex::EntityId>, crate::history::CheckpointKind, String)>,
}

impl App {
    #[must_use]
    pub fn new(host: Box<dyn crate::HostResources>) -> Self {
        Self {
            engine: None,
            keybindings: {
                let mut kb = KeyBindings::default();
                // Focus is foldit-core session state: neutralize viso's
                // Tab/Backquote focus bindings on this instance so the core
                // key paths intercept these keys before any dispatch and drive
                // `Session::set_focus` instead.
                kb.insert("Tab".to_owned(), Box::new(|_: &mut VisoEngine| {}));
                kb.insert("Backquote".to_owned(), Box::new(|_: &mut VisoEngine| {}));
                kb
            },
            store: Session::new(),
            view: ViewState::new(),
            runner_client: RunnerClient::new(),
            runner_projector: RunnerProjector::new(),
            render_projector: RenderProjector::new(),
            gui_projector: GuiProjector::new(),
            host,
            frontend: FrontendState::new(),
            lifecycle: AppPhase::Initializing,
            #[cfg(not(target_arch = "wasm32"))]
            startup: self::startup::StartupState {
                phase: self::load::StartupPhase::Idle,
                camera: self::load::StartupCamera::Fit,
                ss_override: None,
            },
            pending_dirty: DirtyFlags::empty(),
            segment: SegmentPanel::new(),
            panels: PanelState::new(),
            progress: ProgressStore::new(),
            ui: UiToggles::new(),
            #[cfg(not(target_arch = "wasm32"))]
            score_targets: std::collections::HashMap::new(),
            #[cfg(not(target_arch = "wasm32"))]
            creates_previews: std::collections::HashMap::new(),
            #[cfg(not(target_arch = "wasm32"))]
            inplace_previews: std::collections::HashMap::new(),
            #[cfg(not(target_arch = "wasm32"))]
            inplace_edits: std::collections::HashMap::new(),
        }
    }

    /// Flip the App into the in-session lifecycle phase the moment a
    /// structure finishes loading, on every load path.
    pub(in crate::app) fn enter_session(&mut self) {
        self.set_app_phase(AppPhase::InSession);
        self.frontend.set_score(0.0, true);
        self.frontend.set_score_title(self.store.title().to_owned());
        self.pending_dirty |= DirtyFlags::all();
    }

    /// Open the per-residue segment-info panel on `(eid, residue)`, marking
    /// the segment section dirty when the target resolves. A no-op when the
    /// entity or residue does not resolve.
    pub(in crate::app) fn open_segment(&mut self, eid: molex::EntityId, residue: usize) {
        if self.segment.open(&self.store, eid, residue) {
            self.pending_dirty |= DirtyFlags::SEGMENT;
        }
    }

    /// Close the segment-info panel. Marks the segment section dirty.
    pub(in crate::app) fn close_segment(&mut self) {
        self.segment.close();
        self.pending_dirty |= DirtyFlags::SEGMENT;
    }

    /// Show or hide a panel by id. Marks the panels section dirty.
    pub(in crate::app) fn set_panel_visible(&mut self, panel: String, visible: bool) {
        self.panels.set_visible(panel, visible);
        self.pending_dirty |= DirtyFlags::PANELS;
    }

    /// Record a panel's dragged top-left position, marking the panels section
    /// dirty.
    pub(in crate::app) fn set_panel_position(&mut self, panel: String, x: f32, y: f32) {
        self.panels.set_position(panel, x, y);
        self.pending_dirty |= DirtyFlags::PANELS;
    }

    /// Record the loaded puzzle's display score against its high-score
    /// progress. Monotonic max: only writes (and marks the progress section
    /// dirty) when a puzzle is loaded, the score is positive, and it beats
    /// the puzzle's current best. A puzzle counts as complete once its best
    /// is positive, so this is the sole gate the menu's unlock math reads.
    fn record_progress(&mut self) {
        let Some(puzzle_id) = self.store.puzzle().map(|p| p.id) else {
            return;
        };
        let Some(score) = self.store.display_score() else {
            return;
        };
        if self.progress.record(puzzle_id, score) {
            self.pending_dirty |= DirtyFlags::PROGRESS;
        }
    }

    /// Wipe all recorded high-score progress, marking the progress section
    /// dirty.
    pub(in crate::app) fn clear_progress(&mut self) {
        if self.progress.clear() {
            self.pending_dirty |= DirtyFlags::PROGRESS;
        }
    }

    /// Show or hide the tutorial-hint bubble. Marks the ui section dirty.
    pub(in crate::app) fn set_hints_visible(&mut self, v: bool) {
        self.ui.set_hints_visible(v);
        self.pending_dirty |= DirtyFlags::UI;
    }

    /// Enter or leave OS fullscreen. Marks the ui section dirty and stages a
    /// value-gated change for the desktop host to pull. Only the false->true /
    /// true->false transition stages, so re-setting the same value pushes
    /// nothing to the host.
    pub(in crate::app) fn set_fullscreen(&mut self, v: bool) {
        self.ui.set_fullscreen(v);
        self.pending_dirty |= DirtyFlags::UI;
    }

    /// Take the pending fullscreen change for the desktop host to apply to
    /// the winit window, or `None` when it did not change since the last
    /// pull. Returned at most once per change.
    pub const fn take_fullscreen_change(&mut self) -> Option<bool> {
        self.ui.take_fullscreen_change()
    }

    /// Take the serialized high-score progress map for the host to persist to
    /// disk, or `None` when it has not changed since the last pull. Returned
    /// at most once per change.
    pub fn take_progress_to_persist(&mut self) -> Option<Vec<u8>> {
        self.progress.take_to_persist()
    }

    /// Merge a persisted high-score progress map (as written by
    /// [`App::take_progress_to_persist`]) back into the live map. Monotonic
    /// max per puzzle so any record made in-session before the async load
    /// completed is not clobbered by a stale on-disk best. Marks the GUI
    /// progress section dirty so the merged map projects, but deliberately
    /// does not set the persist-pending flag, so a load does not bounce back
    /// out as a save.
    pub fn import_progress(&mut self, bytes: &[u8]) {
        if self.progress.import(bytes) {
            self.pending_dirty |= DirtyFlags::PROGRESS;
        }
    }

    /// Advance the App-lifetime phase and mirror it to the frontend
    /// transmit gate. `set_app_state` only marks the section dirty when
    /// the value actually changes, so re-setting the same phase is a
    /// no-op on the wire.
    pub(in crate::app) fn set_app_phase(&mut self, state: AppPhase) {
        self.lifecycle = state;
        self.frontend.set_app_state(self.lifecycle);
    }

    /// The active view options. App-owned so they persist across a reload.
    #[must_use]
    pub const fn view_options(&self) -> &viso::options::VisoOptions {
        &self.view.options
    }

    /// The name of the currently-loaded preset, or `None` when the active
    /// options were set manually.
    #[must_use]
    pub fn active_preset(&self) -> Option<&str> {
        self.view.active_preset.as_deref()
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if let Some(engine) = &mut self.engine {
            engine.resize(width, height);
        }
    }

    pub fn set_surface_scale(&mut self, scale_factor: f64) {
        if let Some(ref mut engine) = self.engine {
            engine.set_render_scale(if scale_factor < 2.0 { 2 } else { 1 });
        }
    }

    pub fn update_engine(&mut self, dt: f32) {
        if let Some(engine) = &mut self.engine {
            engine.update(dt);
        }
    }

    pub fn render(&mut self) {
        if let Some(engine) = &mut self.engine {
            if let Err(e) = engine.render() {
                log::error!("Render error: {e:?}");
            }
        }
    }

    /// Set the host log mirror on the owned frontend.
    pub fn set_frontend_log(&mut self, log: String) {
        self.frontend.set_log(log);
    }

    /// Serialize whatever sections of the owned [`FrontendState`] are
    /// currently dirty into a JSON byte string suitable for an IPC push, and
    /// clear the dirty bits. Returns `None` when nothing changed since the
    /// last drain.
    pub fn serialize_frontend_dirty(&mut self) -> Option<Vec<u8>> {
        foldit_gui::bridge::push::serialize_dirty(&mut self.frontend)
            .map(|v| v.to_string().into_bytes())
    }

    /// Take the pending segment-panel tail-tip change for the host to push,
    /// or `None` when the tip did not move since the last push. `Some` is
    /// returned at most once per change: `tick` sets it only on a value
    /// change and this clears it.
    pub const fn take_tail_update(&mut self) -> Option<TailUpdate> {
        self.segment.take_update()
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the per-frame drive loop sequences every subsystem; splitting it would scatter the frame order that must stay readable in one place"
    )]
    pub fn tick(&mut self, dt: f32) {
        // Advance the non-blocking startup state-machine. Runs before the
        // drain so a publish a startup step triggers (the structure parse,
        // the committed post-Init adoption) lands in this frame's `changes`
        // batch and is projected the same frame. Inert once bring-up is done.
        #[cfg(not(target_arch = "wasm32"))]
        self.advance_startup();

        // Plugin updates.
        self.apply_backend_updates();

        // Apply this tick's score replies BEFORE the drain so their
        // `ScoresChanged` lands in this tick's `changes` batch. That makes
        // the render projector re-derive the per-residue colors the same
        // frame the score arrives
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.poll_async_scores();
            self.poll_composition_scores();
            // Apply any async overlay-query replies (voids, clashes,
            // exposed-hydrophobics) that landed since the last tick. They
            // arrive off the dispatch path a tick or two after their refresh
            // fired the query, so this drains every tick regardless of whether
            // this tick carries a session change; each applied reply marks the
            // viz cache dirty for the push below.
            let viz_results = self.runner_client.poll_query_results();
            if !viz_results.is_empty() {
                let opts = &self.view.options;
                crate::viz::refresh::apply_query_results(&mut self.store, opts, viz_results);
            }
        }

        // Record the loaded puzzle's display score into high-score progress.
        // Runs after the score polls above so it sees this tick's applied
        // score; the monotonic-max gate inside is a cheap read at rest.
        self.record_progress();

        // Drain the SessionUpdate stream once and route to projectors.
        let changes = self.store.take_updates();

        // `has_geometry` is true when this batch carries a scene-mutating
        // update: an assembly republish keys off of it.
        let has_geometry = changes.iter().any(SessionUpdate::is_geometry);

        #[cfg(not(target_arch = "wasm32"))]
        {
            if self.engine.is_some()
                && self.startup_settled()
                && has_geometry
                && !self.store.has_pending()
            {
                crate::viz::refresh::refresh_connections(&mut self.runner_client, &mut self.store);
            }
        }

        // Refresh the structural-viz overlays into the viz cache: external
        // cavities (voids), steric-clash arcs, and exposed-hydrophobic grease
        // beads. All three go stale on a geometry change, so they run on the
        // same at-rest geometry gate as the rescore below; each also refreshes
        // when a view toggle flipped (a ViewOptionsChanged carries it) so
        // toggling on requests the overlay and toggling off clears it. Each
        // call self-gates on the engine being present, its display toggle, and
        // a plugin advertising its query, so each is an inert no-op until the
        // plugin implements that query. Refresh order is voids, clashes,
        // exposed-hydrophobics. Each refresh stores into the viz cache and
        // marks it dirty; the render projector pushes the cache to the engine
        // on the consume drain below. This runs BEFORE the drain so the same
        // tick that recomputes the overlays also pushes them.
        #[cfg(not(target_arch = "wasm32"))]
        {
            let view_toggled = changes
                .iter()
                .any(|c| matches!(c, SessionUpdate::ViewOptionsChanged));
            // A creates-entities op (e.g. RFdiffusion3) opens no edit, so
            // `has_pending` is false while it streams. Its frames are noisy
            // diffusion intermediates (the first is near-superimposed atoms),
            // so evaluating overlays against them yields garbage clashes that
            // then persist for the whole action. Gate the refresh off while a
            // streaming preview is live; it re-runs on the committed design.
            if self.engine.is_some()
                && ((self.startup_settled()
                    && has_geometry
                    && !self.store.has_pending()
                    && self.creates_previews.is_empty())
                    || view_toggled)
            {
                crate::viz::refresh::refresh_external_cavities(
                    &mut self.runner_client,
                    &mut self.store,
                    &self.view.options,
                );
                crate::viz::refresh::refresh_clashes(
                    &mut self.runner_client,
                    &mut self.store,
                    &self.view.options,
                );
                crate::viz::refresh::refresh_exposed_hydrophobics(
                    &mut self.runner_client,
                    &mut self.store,
                    &self.view.options,
                );
            }
        }

        if !changes.is_empty() {
            if let Some(orch) = self.runner_client.orchestrator_mut() {
                self.runner_projector.consume(&changes, &mut self.store, orch);
            }

            if let Some(engine) = self.engine.as_mut() {
                self.render_projector.consume(&changes, &mut self.store, engine);
                if changes
                    .iter()
                    .any(|c| matches!(c, SessionUpdate::ViewOptionsChanged))
                {
                    engine.set_options(self.view.options.clone());
                }
            }
        }

        // Push the structural-viz overlays from the viz cache to the engine,
        // gated on the cache being dirty. This runs EVERY tick (not behind the
        // changes drain): an async voids/clashes reply can land on a tick that
        // carries no session change, so the push must not be gated on
        // `!changes.is_empty()`. `push_overlays` self-gates on `viz_dirty`,
        // so it is a cheap no-op when the cache is clean. The projector is the
        // single pusher; it reports back so we can clear the flag it cannot
        // clear through its shared `&Session`.
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(engine) = self.engine.as_mut() {
            if RenderProjector::push_overlays(&self.store, engine) {
                self.store.viz.viz_dirty = false;
            }
        }

        // Fire the NEXT async rescore, the AT-REST rescore only. Scores go
        // stale only on an assembly change (every mutation emits a
        // SessionUpdate, including those from non-scoring plugins), so this
        // gates on this tick's geometry change. It fires only when no edit is
        // open: while a stream runs, each frame carries its own warm score and
        // stamps its edit directly (in `apply_backend_updates`), so this query
        // is not fired - it would only re-score a trailing frame. It is
        // also held off until the startup machine settles: during bring-up
        // the machine drives the first score itself (kicked once every
        // plugin's Init has replied, so the scorer's pose is built), and
        // firing here would race a query into the pose-less window before a
        // plugin's Init replies, which comes back empty. After `Done` the
        // machine is inert and this is the sole at-rest scorer again.
        // Fire-and-forget against the worker's already-built live pose (no
        // per-frame pose rebuild); `request_scores` coalesces, so one
        // outstanding query per provider is the most in flight.
        #[cfg(not(target_arch = "wasm32"))]
        {
            // Also held off while a creates-entities preview streams: those
            // frames are noisy diffusion intermediates, so scoring them yields
            // a meaningless clash-saturated total. Re-scores on the committed
            // design once the stream's terminal clears `creates_previews`.
            if self.startup_settled()
                && has_geometry
                && !self.store.has_pending()
                && self.creates_previews.is_empty()
            {
                self.request_scores();
            }
        }

        // Engine update + visualization overlay.
        #[cfg(not(target_arch = "wasm32"))]
        let pull = self.runner_client.pull_drag_pull_info();
        #[cfg(target_arch = "wasm32")]
        let pull: Option<viso::PullInfo> = None;
        if let Some(engine) = self.engine.as_mut() {
            engine.update(dt);
            update_all_visualizations(engine, pull);
        }

        // Tail tip: runs after `engine.update` (camera settled this frame)
        // and stages a tip change only when it moved.
        self.segment.update_tail_tip(&self.store, self.engine.as_ref());

        // Each load path calls `enter_session` at its done-loading point, so
        // the frontend routes to the in-puzzle UI the moment loading
        // completes, with the score flowing in asynchronously.

        // Frontend projection: the GUI consumer derives its dirty set
        // entirely from this tick's `changes` batch, OR'd with the App-side
        // `pending_dirty` accumulator (the segment / panels / ui / progress
        // bits plus the session-birth full-populate seed). The accumulator
        // is drained only when the engine is present and the consumer runs;
        // with no engine attached yet it persists to a later tick, so the
        // birth populate is never dropped.
        if let Some(engine) = self.engine.as_ref() {
            let pending = std::mem::take(&mut self.pending_dirty);
            let src = GuiSources {
                session: &self.store,
                engine,
                driver: &self.runner_client,
                host: self.host.as_ref(),
                view_options: &self.view.options,
                active_preset: self.view.active_preset.as_deref(),
                open_segment: self.segment.target(),
                open_panels: self.panels.open(),
                panel_positions: self.panels.positions(),
                progress: self.progress.map(),
                hints_visible: self.ui.hints_visible(),
                fullscreen: self.ui.fullscreen(),
            };
            let segment_auto_closed =
                self.gui_projector
                    .consume(&changes, pending, &src, &mut self.frontend);
            if segment_auto_closed {
                self.segment.close();
            }
        }
    }
}

/// `App` is the sink for frontend -> backend commands dispatched by the GUI.
impl foldit_gui::Dispatcher for App {
    fn on_ready(&mut self) {
        self.frontend.mark_all_dirty();
    }

    fn on_viewport_input(&mut self, input: foldit_gui::ViewportInput) {
        self.handle_viewport_input(input);
    }

    fn on_dispatch_op(&mut self, op: foldit_gui::OpDispatch) {
        self.handle_dispatch_op(op);
    }

    fn on_app_command(&mut self, command: foldit_gui::AppCommand) {
        self.handle_app_command(command);
    }

    fn on_set_selection(&mut self, entries: Vec<foldit_gui::EntitySelection>) {
        self.handle_set_selection(entries);
    }

    fn handle_request(
        &mut self,
        kind: foldit_gui::RequestKind,
        payload: serde_json::Value,
    ) -> foldit_gui::RequestResult {
        use foldit_gui::RequestKind;
        match kind {
            RequestKind::ReadResourceFile => {
                use base64::Engine;
                let filepath = payload
                    .get("filepath")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "missing 'filepath'".to_owned())?;
                let bytes = self
                    .host
                    .read_file(filepath)
                    .map_err(|e| format!("read {filepath}: {e}"))?;
                let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                Ok(serde_json::json!({ "encoding": "base64", "content": b64 }))
            }
            RequestKind::GetHotkeyText => {
                // Stub: real implementation would look up display strings
                // for hotkey ids. Until that surface lands, return empty so
                // HelpMenuPanel rejects gracefully instead of timing out.
                let hotkey = payload.get("hotkey").and_then(|v| v.as_str()).unwrap_or("");
                Err(format!("hotkey lookup not implemented (hotkey={hotkey})"))
            }
            RequestKind::ServerRequest => {
                // Stub: server requests (news, etc.) require an HTTP client
                // bound here. Defer until a dedicated request handler exists.
                let endpoint = payload
                    .get("endpoint")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                Err(format!(
                    "server request not implemented (endpoint={endpoint})"
                ))
            }
            RequestKind::PanelsCatalog => {
                #[cfg(not(target_arch = "wasm32"))]
                let panels = self.runner_client.panels_catalog();
                #[cfg(target_arch = "wasm32")]
                let panels: Vec<foldit_gui::state::PanelInfo> = Vec::new();
                Ok(serde_json::to_value(panels).map_err(|e| e.to_string())?)
            }
            RequestKind::SettingsCatalog => {
                #[cfg(not(target_arch = "wasm32"))]
                let tabs = self.runner_client.settings_catalog();
                #[cfg(target_arch = "wasm32")]
                let tabs: Vec<foldit_gui::state::SettingsTabInfo> = Vec::new();
                Ok(serde_json::to_value(tabs).map_err(|e| e.to_string())?)
            }
        }
    }
}
