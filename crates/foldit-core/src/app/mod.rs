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

use crate::gui_projector::GuiSources;
use crate::render_projector::RenderSources;
use crate::runner_client::RunnerClient;
use crate::session::{Session, SessionUpdate, SessionUpdateConsumer};
use crate::viz::Viz;

mod command;
mod dispatch;
#[cfg(not(target_arch = "wasm32"))]
mod gesture;
mod harness;
pub mod input;
mod load;
mod plugins;
mod preview;
mod projectors;
pub mod score_coordinator;
mod startup;
#[cfg(test)]
mod tests;
mod view_options;

use self::harness::EngineHarness;
pub use self::plugins::{locate_plugin_ui_entrypoints, locate_plugins_root};
use self::projectors::Projectors;
use self::score_coordinator::ScoreCoordinator;
pub use foldit_gui::TailUpdate;

/// Main application state - thin glue connecting the render engine,
/// plugin driver, the `Session` store, and the three projectors
/// (`RunnerProjector`, `RenderProjector`, `GuiProjector`).
pub struct App {
    // Session encapsulates all state that shares a lifecycle with
    // a structure or puzzle that is loaded into the client.
    pub(in crate::app) store: Session,

    // The viso engine + keybinding table.
    pub(in crate::app) harness: EngineHarness,
    pub(in crate::app) runner_client: RunnerClient,
    pub(in crate::app) gui: GuiState,

    // In parallel to the 3 structs that encapsulate render, plugin, and gui state
    // we have 3 projectors which forward assembly updates to 3 corresponding consumers
    pub(in crate::app) projectors: Projectors,

    /// Host-provided filesystem / resource access. The only path through
    /// which foldit-core touches the filesystem outside puzzle loading.
    pub(in crate::app) host: Box<dyn crate::HostResources>,

    pub(in crate::app) bringup: self::startup::BringupState,

    /// Score request/reply coordinator: owns the composition score targets
    /// and the score-stamp methods.
    pub(in crate::app) scores: ScoreCoordinator,

    /// App-owned derived overlay cache (connections + the structural-viz
    /// overlays).
    pub(in crate::app) viz: Viz,

    /// Dispatches queued this tick, drained on the next `tick`.
    pub(in crate::app) pending_dispatches: Vec<foldit_gui::OpDispatch>,
}

impl App {
    #[must_use]
    pub fn new(host: Box<dyn crate::HostResources>) -> Self {
        Self {
            harness: EngineHarness::new(),
            store: Session::new(),
            runner_client: RunnerClient::new(),
            projectors: Projectors::new(),
            host,
            gui: GuiState::new(),
            bringup: self::startup::BringupState {
                phase: self::startup::StartupPhase::Idle,
                camera: self::startup::StartupCamera::Fit,
                ss_override: None,
            },
            scores: ScoreCoordinator::new(),
            viz: Viz::new(),
            pending_dispatches: Vec::new(),
        }
    }

    /// Shut down backends and scene processor.
    pub fn shutdown(&mut self) {
        self.runner_client.shutdown();
        self.harness.shutdown();
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

    /// Map wire selection entries and apply them.
    pub fn handle_set_selection(&mut self, entries: Vec<foldit_gui::EntitySelection>) {
        self.store
            .set_selection_entries(entries.into_iter().map(|e| (e.entity_id, e.residues)));
    }

    /// Webview signaled ready: mark every section dirty so the next push is a
    /// full snapshot.
    pub const fn on_ready(&mut self) {
        self.gui.mark_all_dirty();
    }

    /// Enqueue a plugin op dispatch, drained on the next `tick`.
    pub fn on_dispatch_op(&mut self, op: foldit_gui::OpDispatch) {
        self.pending_dispatches.push(op);
    }

    /// Synchronously resolve a JS-side request.
    ///
    /// # Errors
    ///
    /// Returns `Err(message)` when the request cannot be served (unknown
    /// kind, malformed payload, or an underlying operation fails); the
    /// string is surfaced to the JS caller as the rejection reason.
    #[allow(clippy::needless_pass_by_value)]
    pub fn handle_request(
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

    /// Open the per-residue segment-info panel on `(eid, residue)`, marking
    /// the segment section dirty when the target resolves. A no-op when the
    /// entity or residue does not resolve.
    pub(in crate::app) fn open_segment(&mut self, eid: molex::EntityId, residue: usize) {
        let Some(target) = crate::gui_projector::resolve_segment_target(&self.store, eid, residue)
        else {
            return;
        };
        self.gui.set_segment_target(Some(target));
        self.mark_dirty(DirtyFlags::SEGMENT);
    }

    /// Merge a persisted high-score progress map back into the live map. Monotonic
    /// max per puzzle so any record made in-session before the async load
    /// completed is not clobbered by a stale on-disk best. Marks the GUI
    /// progress section dirty so the merged map projects, but deliberately
    /// does not set the persist-pending flag, so a load does not bounce back
    /// out as a save.
    pub fn import_progress(&mut self, bytes: &[u8]) {
        self.gui.import_progress(bytes);
    }

    // TODO: does not belong directly in app
    /// Advance the App-lifetime phase and mirror it to the frontend
    /// transmit gate. `set_app_state` only marks the section dirty when
    /// the value actually changes, so re-setting the same phase is a
    /// no-op on the wire.
    pub(in crate::app) fn set_app_phase(&mut self, state: AppPhase) {
        self.gui.set_app_state(state);
    }

    // TODO: does not belong directly in app
    pub fn resize(&mut self, width: u32, height: u32) {
        self.harness.resize(width, height);
    }

    pub fn set_surface_scale(&mut self, scale_factor: f64) {
        self.harness.set_surface_scale(scale_factor);
    }

    pub fn update_engine(&mut self, dt: f32) {
        self.harness.update(dt);
    }

    pub fn render(&mut self) {
        self.harness.render();
    }

    /// Set the host log mirror on the owned frontend.
    pub fn set_frontend_log(&mut self, log: String) {
        self.gui.set_log(log);
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the per-frame drive loop sequences every subsystem; splitting it would scatter the frame order that must stay readable in one place"
    )]
    pub fn tick(&mut self, dt: f32, fx: &mut dyn crate::HostEffects) {
        // Advance the non-blocking startup state-machine. Runs before the
        // drain so a publish a startup step triggers (the structure parse,
        // the committed post-Init adoption) lands in this frame's `changes`
        // batch and is projected the same frame. Inert once bring-up is done.
        self.advance_startup();

        // Plugin updates.
        self.apply_backend_updates();

        for op in std::mem::take(&mut self.pending_dispatches) {
            self.handle_dispatch_op(op);
        }

        // Apply this tick's score replies before the drain so their
        // `ScoresChanged` lands in this tick's `changes` batch.
        self.scores.poll(&mut self.runner_client, &mut self.store);

        let viz_results = self.runner_client.poll_query_results();
        if !viz_results.is_empty() {
            let opts = self.view_options();
            self.viz
                .apply_replies(&self.store, &mut self.scores, &opts, viz_results);
        }

        // Drain the SessionUpdate stream once and route to projectors.
        let changes = self.store.take_updates();

        // `has_geometry` is true when this batch carries a scene-mutating
        // update: an assembly republish keys off of it.
        let has_geometry = changes.iter().any(SessionUpdate::is_geometry);

        if self.harness.engine.is_some()
            && self.startup_settled()
            && has_geometry
            && !self.store.has_pending()
        {
            self.viz.refresh_connections(&mut self.runner_client, &self.store);
        }

        let view_toggled = changes
            .iter()
            .any(|c| matches!(c, SessionUpdate::ViewOptionsChanged));

        // A creates-entities op (e.g. RFdiffusion3) opens no edit, so
        // `has_pending` is false while it streams.
        if self.harness.engine.is_some()
            && ((self.startup_settled()
                && has_geometry
                && !self.store.has_pending()
                && !self.store.has_active_creates_previews())
                || view_toggled)
        {
            let opts = self.view_options();
            self.viz
                .step(&mut self.runner_client, &self.store, &mut self.scores, &opts);
        }

        if !changes.is_empty() {
            if let Some(orch) = self.runner_client.orchestrator_mut() {
                self.projectors
                    .runner
                    .consume(&changes, &mut self.store, orch);
            }
        }

        // `viz.push` flushes the overlay cache even on an empty batch.
        let reapply_options = changes
            .iter()
            .any(|c| matches!(c, SessionUpdate::ViewOptionsChanged))
            .then(|| self.view_options());
        if let Some(engine) = self.harness.engine.as_mut() {
            let src = RenderSources {
                session: &mut self.store,
                reapply_options,
                scores: &self.scores,
                held_connections: self.viz.held_connections(),
            };
            self.projectors.render.consume(&changes, src, engine);
            self.viz.push(engine);
        }

        if self.startup_settled()
            && has_geometry
            && !self.store.has_pending()
            && !self.store.has_active_creates_previews()
        {
            self.runner_client.request_scores();
        }

        // Engine update + visualization overlay.
        self.update_engine(dt);
        self.update_frame_visuals();

        // Tail tip: runs after `engine.update` (camera settled this frame)
        // and stages a tip change only when it moved. Core resolves the open
        // target's CA to a screen position (off-screen / closed / no engine
        // all resolve to `None`); the debounce FSM lives on the frontend.
        let tail_screen_pos = self.gui.segment_target().and_then(|target| {
            self.store
                .entity(target.entity)
                .and_then(|entity| crate::gui_projector::ca_world_position(entity, target.residue))
                .and_then(|world| self.harness.world_to_screen(world))
                .map(|v| (v.x, v.y))
        });
        self.gui.push_tail_tip(tail_screen_pos);

        if let Some(engine) = self.harness.engine.as_ref() {
            let src = GuiSources {
                session: &self.store,
                engine,
                driver: &self.runner_client,
                host: self.host.as_ref(),
                scores: &self.scores,
            };
            let segment_auto_closed = self.projectors.gui.consume(&changes, src, &mut self.gui);
            if segment_auto_closed {
                self.gui.close_segment();
            }
        }

        if let Some(json) =
            foldit_gui::bridge::push::serialize_dirty(&mut self.gui).map(|v| v.to_string().into_bytes())
        {
            fx.push_state(&json);
        }
        if let Some(update) = self.gui.take_tail_update() {
            fx.push_tail(update);
        }
        if let Some(value) = self.gui.take_fullscreen_change() {
            fx.set_fullscreen(value);
        }
        if let Some(bytes) = self.gui.take_progress_to_persist() {
            fx.persist_progress(bytes);
        }
    }
}
