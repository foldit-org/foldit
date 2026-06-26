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

use foldit_gui::{AppPhase, DirtyFlags, GuiState};
use viso::{KeyBindings, VisoEngine};

use crate::gui_projector::GuiSources;
use crate::render_projector::RenderProjector;
use crate::runner_client::RunnerClient;
use crate::session::{Session, SessionUpdate, SessionUpdateConsumer};

mod dispatch;
pub(crate) mod input;
mod load;
#[cfg(not(target_arch = "wasm32"))]
mod ops;
mod preview;
mod projectors;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod score_apply;
mod startup;
#[cfg(test)]
mod tests;

use self::input::update_all_visualizations;
#[cfg(not(target_arch = "wasm32"))]
use self::ops::OpStreamState;
use self::projectors::Projectors;
#[cfg(not(target_arch = "wasm32"))]
pub use self::load::{locate_plugin_ui_entrypoints, locate_plugins_root};
pub use foldit_gui::TailUpdate;

/// Main application state - thin glue connecting the render engine,
/// plugin driver, the `Session` store, and the three projectors
/// (`RunnerProjector`, `RenderProjector`, `GuiProjector`).
///
/// `App` also owns the host-bound [`GuiState`] mirror (so the load
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

    pub(in crate::app) gui: GuiState,
    pub(in crate::app) keybindings: KeyBindings,

    // The three projector classes which are the consumers of Assembly updates.
    // The GUI projector also owns the App-side dirty accumulator.
    pub(in crate::app) projectors: Projectors,

    /// Host-provided filesystem / resource access. The only path through
    /// which foldit-core touches the filesystem outside puzzle loading.
    pub(in crate::app) host: Box<dyn crate::HostResources>,

    #[cfg(not(target_arch = "wasm32"))]
    pub(in crate::app) startup: self::startup::StartupState,
    /// The in-flight op-stream token maps (`score_targets`, `creates_previews`,
    /// `inplace_previews`), keyed by edit/request token.
    #[cfg(not(target_arch = "wasm32"))]
    pub(in crate::app) ops: OpStreamState,
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
            runner_client: RunnerClient::new(),
            projectors: Projectors::new(),
            host,
            gui: GuiState::new(),
            #[cfg(not(target_arch = "wasm32"))]
            startup: self::startup::StartupState {
                phase: self::load::StartupPhase::Idle,
                camera: self::load::StartupCamera::Fit,
                ss_override: None,
            },
            #[cfg(not(target_arch = "wasm32"))]
            ops: OpStreamState::new(),
        }
    }

    /// OR-accumulate App-owned GUI dirty bits into the GUI projector. These
    /// are the bits the `SessionUpdate` batch cannot express (segment)
    /// plus the full-populate seed (`DirtyFlags::all()`) the
    /// session-birth path raises; the projector drains them at the tick consume.
    fn mark_dirty(&mut self, flags: DirtyFlags) {
        self.projectors.gui.mark_dirty(flags);
    }

    /// Flip the App into the in-session lifecycle phase the moment a
    /// structure finishes loading, on every load path.
    pub(in crate::app) fn enter_session(&mut self) {
        self.set_app_phase(AppPhase::InSession);
        self.gui.set_score(0.0, true);
        self.gui.set_score_title(self.store.title().to_owned());
        self.mark_dirty(DirtyFlags::all());
    }

    /// Open the per-residue segment-info panel on `(eid, residue)`, marking
    /// the segment section dirty when the target resolves. A no-op when the
    /// entity or residue does not resolve.
    pub(in crate::app) fn open_segment(&mut self, eid: molex::EntityId, residue: usize) {
        let Some(target) = self.resolve_segment_target(eid, residue) else {
            return;
        };
        self.gui.set_segment_target(Some(target));
        self.mark_dirty(DirtyFlags::SEGMENT);
    }

    /// Resolve `(eid, residue)` into a segment target, computing the residue
    /// identity (number, chain, amino acid) and its secondary structure once
    /// via a single `recompute_ss()` over the head assembly and caching them
    /// on the target. `None` when the entity or residue does not resolve.
    fn resolve_segment_target(
        &self,
        eid: molex::EntityId,
        residue: usize,
    ) -> Option<foldit_gui::SegmentTarget> {
        let entity = self.store.entity(eid)?;
        let res = entity.residues()?.get(residue)?;
        let residue_number = res.seq_id();
        let chain = entity
            .pdb_chain_id()
            .map_or_else(String::new, str::to_owned);
        let aa = molex::chemistry::AminoAcid::from_code(res.name);
        let aa_three = String::from_utf8_lossy(&res.name).trim().to_owned();
        let aa_one = aa.map_or_else(String::new, |a| (a.one_letter() as char).to_string());

        let mut assembly = self.store.head_assembly();
        assembly.recompute_ss();
        let ss_label = ss_label(assembly.ss_types(eid).get(residue).copied());

        Some(foldit_gui::SegmentTarget {
            entity: eid,
            residue,
            residue_number,
            chain,
            aa_three,
            aa_one,
            ss_label,
        })
    }

    /// Close the segment-info panel. Marks the segment section dirty.
    pub(in crate::app) fn close_segment(&mut self) {
        self.gui.close_segment();
    }

    /// Show or hide a panel by id. Marks the panels section dirty.
    pub(in crate::app) fn set_panel_visible(&mut self, panel: String, visible: bool) {
        self.gui.set_panel_visible(panel, visible);
    }

    /// Record a panel's dragged top-left position, marking the panels section
    /// dirty.
    pub(in crate::app) fn set_panel_position(&mut self, panel: String, x: f32, y: f32) {
        self.gui.set_panel_position(panel, x, y);
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
        self.gui.record_progress(puzzle_id, score);
    }

    /// Wipe all recorded high-score progress, marking the progress section
    /// dirty.
    pub(in crate::app) fn clear_progress(&mut self) {
        self.gui.clear_progress();
    }

    /// Show or hide the tutorial-hint bubble. Marks the ui section dirty.
    pub(in crate::app) fn set_hints_visible(&mut self, v: bool) {
        self.gui.set_hints_visible(v);
    }

    /// Enter or leave OS fullscreen. Marks the ui section dirty and stages a
    /// value-gated change for the desktop host to pull. Only the false->true /
    /// true->false transition stages, so re-setting the same value pushes
    /// nothing to the host.
    pub(in crate::app) fn set_fullscreen(&mut self, v: bool) {
        self.gui.set_fullscreen(v);
    }

    /// Take the pending fullscreen change for the desktop host to apply to
    /// the winit window, or `None` when it did not change since the last
    /// pull. Returned at most once per change.
    pub const fn take_fullscreen_change(&mut self) -> Option<bool> {
        self.gui.take_fullscreen_change()
    }

    /// Take the serialized high-score progress map for the host to persist to
    /// disk, or `None` when it has not changed since the last pull. Returned
    /// at most once per change.
    pub fn take_progress_to_persist(&mut self) -> Option<Vec<u8>> {
        self.gui.take_progress_to_persist()
    }

    /// Merge a persisted high-score progress map (as written by
    /// [`App::take_progress_to_persist`]) back into the live map. Monotonic
    /// max per puzzle so any record made in-session before the async load
    /// completed is not clobbered by a stale on-disk best. Marks the GUI
    /// progress section dirty so the merged map projects, but deliberately
    /// does not set the persist-pending flag, so a load does not bounce back
    /// out as a save.
    pub fn import_progress(&mut self, bytes: &[u8]) {
        self.gui.import_progress(bytes);
    }

    /// Advance the App-lifetime phase and mirror it to the frontend
    /// transmit gate. `set_app_state` only marks the section dirty when
    /// the value actually changes, so re-setting the same phase is a
    /// no-op on the wire.
    pub(in crate::app) fn set_app_phase(&mut self, state: AppPhase) {
        self.gui.set_app_state(state);
    }

    /// The active view options, reconstructed from the frontend-held faithful
    /// (sparse) form. Faithful round-trip: display overrides left to inherit
    /// stay `None`, so re-applying preserves their inherit semantics.
    #[must_use]
    pub fn view_options(&self) -> viso::options::VisoOptions {
        serde_json::from_value(self.gui.view_options_raw().clone()).unwrap_or_default()
    }

    /// The name of the currently-loaded preset, or `None` when the active
    /// options were set manually.
    #[must_use]
    pub fn active_preset(&self) -> Option<&str> {
        self.gui.view.active_preset.as_deref()
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
        self.gui.set_log(log);
    }

    /// Serialize whatever sections of the owned [`GuiState`] are
    /// currently dirty into a JSON byte string suitable for an IPC push, and
    /// clear the dirty bits. Returns `None` when nothing changed since the
    /// last drain.
    pub fn serialize_frontend_dirty(&mut self) -> Option<Vec<u8>> {
        foldit_gui::bridge::push::serialize_dirty(&mut self.gui)
            .map(|v| v.to_string().into_bytes())
    }

    /// Take the pending segment-panel tail-tip change for the host to push,
    /// or `None` when the tip did not move since the last push. `Some` is
    /// returned at most once per change: `tick` sets it only on a value
    /// change and this clears it.
    pub const fn take_tail_update(&mut self) -> Option<TailUpdate> {
        self.gui.take_tail_update()
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
                let opts = self.view_options();
                crate::viz::refresh::apply_query_results(&mut self.store, &opts, viz_results);
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
                    && self.ops.creates_previews.is_empty())
                    || view_toggled)
            {
                let opts = self.view_options();
                crate::viz::refresh::refresh_external_cavities(
                    &mut self.runner_client,
                    &mut self.store,
                    &opts,
                );
                crate::viz::refresh::refresh_clashes(
                    &mut self.runner_client,
                    &mut self.store,
                    &opts,
                );
                crate::viz::refresh::refresh_exposed_hydrophobics(
                    &mut self.runner_client,
                    &mut self.store,
                    &opts,
                );
            }
        }

        if !changes.is_empty() {
            if let Some(orch) = self.runner_client.orchestrator_mut() {
                self.projectors.runner.consume(&changes, &mut self.store, orch);
            }

            // Materialize the reapply options before the `&mut engine` borrow:
            // they are reconstructed from the frontend-held faithful form, an
            // immutable `&self` read that cannot overlap the engine borrow.
            let reapply_options = changes
                .iter()
                .any(|c| matches!(c, SessionUpdate::ViewOptionsChanged))
                .then(|| self.view_options());
            if let Some(engine) = self.engine.as_mut() {
                self.projectors.render.consume(&changes, &mut self.store, engine);
                if let Some(opts) = reapply_options {
                    engine.set_options(opts);
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
                && self.ops.creates_previews.is_empty()
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
        // and stages a tip change only when it moved. Core resolves the open
        // target's CA to a screen position (off-screen / closed / no engine
        // all resolve to `None`); the debounce FSM lives on the frontend.
        let tail_screen_pos = match (self.gui.segment_target(), self.engine.as_ref()) {
            (Some(target), Some(engine)) => self
                .store
                .entity(target.entity)
                .and_then(|entity| crate::gui_projector::ca_world_position(entity, target.residue))
                .and_then(|world| engine.world_to_screen(world))
                .map(|v| (v.x, v.y)),
            _ => None,
        };
        self.gui.push_tail_tip(tail_screen_pos);

        // Each load path calls `enter_session` at its done-loading point, so
        // the frontend routes to the in-puzzle UI the moment loading
        // completes, with the score flowing in asynchronously.

        // Frontend projection: the GUI consumer derives its dirty set
        // entirely from this tick's `changes` batch, OR'd with the dirty
        // accumulator the GUI projector owns (the segment bits
        // plus the session-birth full-populate seed). The
        // accumulator is drained inside `consume`, which runs only when the
        // engine is present; with no engine attached yet the bits persist to
        // a later tick, so the birth populate is never dropped.
        if let Some(engine) = self.engine.as_ref() {
            let src = GuiSources {
                session: &self.store,
                engine,
                driver: &self.runner_client,
                host: self.host.as_ref(),
            };
            let segment_auto_closed =
                self.projectors
                    .gui
                    .consume(&changes, &src, &mut self.gui);
            if segment_auto_closed {
                self.gui.close_segment();
            }
        }
    }
}

/// Human-readable secondary-structure label for the segment panel.
fn ss_label(ss: Option<molex::SSType>) -> String {
    match ss {
        Some(molex::SSType::Helix) => "Helix",
        Some(molex::SSType::Sheet) => "Sheet",
        Some(molex::SSType::Coil) | None => "Loop",
    }
    .to_owned()
}

/// `App` is the sink for frontend -> backend commands dispatched by the GUI.
impl foldit_gui::Dispatcher for App {
    fn on_ready(&mut self) {
        self.gui.mark_all_dirty();
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
