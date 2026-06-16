//! Foldit application state - host-agnostic.
//!
//! `App` owns the following:
//! - `Session`
//! - `RunnerClient` (which carries the orchestrator)
//! - The three projectors (`RunnerProjector`, `RenderProjector`, `GuiProjector`)
//! - The cross-cutting bookkeeping
//!   (Puzzle metadata, viso engine handle, dirty-flags, history-version trackers).
//!
//! Both the desktop (`foldit-desktop`) and web (`foldit-web`) builds
//! wrap this in their host-specific lifecycle:
//!
//! - desktop: `window::AppRunner` holds the wry webview + winit window
//!   alongside `App`; winit events are converted to host-agnostic
//!   types before being forwarded to `App`'s methods.
//!
//! - web: `foldit_web::FolditApp` holds `App` plus the canvas and JS
//!   callbacks; DOM events are forwarded as `ViewportInput` JSON.

use foldit_gui::{AppPhase, FrontendState};
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
#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod score_apply;
mod startup;
#[cfg(test)]
mod tests;

use self::input::update_all_visualizations;
#[cfg(not(target_arch = "wasm32"))]
pub use self::load::locate_plugins_root;

/// Main application state - thin glue connecting the render engine,
/// plugin driver, document, and the two projectors.
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

    // Frontend state/input handling fields
    pub(in crate::app) frontend: FrontendState,
    pub(in crate::app) keybindings: KeyBindings,
    pub(in crate::app) lifecycle: AppPhase,

    // Viso option fields
    pub(in crate::app) view_options: viso::options::VisoOptions,
    pub(in crate::app) active_preset: Option<String>,
    pub(in crate::app) view_settings_touched: bool,

    // Three projector classes which are the consumers of Assembly updates
    pub(in crate::app) runner_projector: RunnerProjector,
    pub(in crate::app) render_projector: RenderProjector,
    pub(in crate::app) gui_projector: GuiProjector,

    /// Host-provided filesystem / resource access. The only path through
    /// which foldit-core touches the filesystem outside puzzle loading.
    pub(in crate::app) host: Box<dyn crate::HostResources>,

    #[cfg(not(target_arch = "wasm32"))]
    pub(in crate::app) startup: self::load::StartupPhase,
    #[cfg(not(target_arch = "wasm32"))]
    pub(in crate::app) startup_camera: self::load::StartupCamera,
    #[cfg(not(target_arch = "wasm32"))]
    pub(in crate::app) startup_ss_override: Option<(u32, Vec<molex::SSType>)>,
    pub(in crate::app) needs_full_populate: bool,
    #[cfg(not(target_arch = "wasm32"))]
    pub(in crate::app) score_targets: std::collections::HashMap<u64, CheckpointId>,
    #[cfg(not(target_arch = "wasm32"))]
    pub(in crate::app) creates_previews: std::collections::HashMap<u64, (molex::EntityId, usize)>,
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) pending_pull_origin: Option<crate::pull_drag::PullRoute>,
}

impl App {
    #[must_use]
    pub fn new(host: Box<dyn crate::HostResources>) -> Self {
        Self {
            engine: None,
            keybindings: {
                let mut kb = KeyBindings::default();
                // Focus is foldit-core session state now: neutralize viso's
                // Tab/Backquote focus bindings on this instance so viso no
                // longer owns focus. The core key paths intercept these keys
                // before any dispatch and drive `Session::set_focus` instead.
                kb.insert("Tab".to_owned(), Box::new(|_: &mut VisoEngine| {}));
                kb.insert("Backquote".to_owned(), Box::new(|_: &mut VisoEngine| {}));
                kb
            },
            store: Session::new(),
            view_options: viso::options::VisoOptions::default(),
            active_preset: None,
            view_settings_touched: false,
            runner_client: RunnerClient::new(),
            runner_projector: RunnerProjector::new(),
            render_projector: RenderProjector::new(),
            gui_projector: GuiProjector::new(),
            host,
            frontend: FrontendState::new(),
            lifecycle: AppPhase::Initializing,
            #[cfg(not(target_arch = "wasm32"))]
            startup: self::load::StartupPhase::Idle,
            #[cfg(not(target_arch = "wasm32"))]
            startup_camera: self::load::StartupCamera::Fit,
            #[cfg(not(target_arch = "wasm32"))]
            startup_ss_override: None,
            needs_full_populate: false,
            #[cfg(not(target_arch = "wasm32"))]
            score_targets: std::collections::HashMap::new(),
            #[cfg(not(target_arch = "wasm32"))]
            creates_previews: std::collections::HashMap::new(),
            #[cfg(not(target_arch = "wasm32"))]
            pending_pull_origin: None,
        }
    }

    /// Flip the App into the in-session lifecycle phase the moment a
    /// structure finishes loading, on every load path.
    pub(in crate::app) fn enter_session(&mut self) {
        self.set_app_phase(AppPhase::InSession);
        self.frontend.set_score(0.0, true);
        self.frontend.set_score_title(self.store.title().to_owned());
        self.needs_full_populate = true;
    }

    /// Advance the App-lifetime phase and mirror it to the frontend
    /// transmit gate. `set_app_state` only marks the section dirty when
    /// the value actually changes, so re-setting the same phase is a
    /// no-op on the wire.
    pub(in crate::app) fn set_app_phase(&mut self, state: AppPhase) {
        self.lifecycle = state;
        self.frontend.set_app_state(self.lifecycle);
    }

    /// The active view options. App-owned so they persist across a reload;
    /// the view panel binds these and the render projection re-applies them
    /// to the engine on each `ViewOptionsChanged`.
    #[must_use]
    pub const fn view_options(&self) -> &viso::options::VisoOptions {
        &self.view_options
    }

    /// The name of the currently-loaded preset, or `None` when the active
    /// options were set manually.
    #[must_use]
    pub fn active_preset(&self) -> Option<&str> {
        self.active_preset.as_deref()
    }

    // ── Engine-only delegation ──

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
    // ── Frontend state sync ──

    /// Set the host log mirror on the owned frontend. Hosts call this
    /// to ship the latest log buffer (drained from their own tee).
    pub fn set_frontend_log(&mut self, log: String) {
        self.frontend.set_log(log);
    }

    /// Serialize whatever sections of the owned [`FrontendState`] are
    /// currently dirty into a JSON byte string suitable for an IPC
    /// push, and clear the dirty bits. Returns `None` when nothing
    /// changed since the last drain. The host pipes the bytes straight
    /// into its webview / `wasm-bindgen` callback.
    pub fn serialize_frontend_dirty(&mut self) -> Option<Vec<u8>> {
        foldit_gui::bridge::push::serialize_dirty(&mut self.frontend)
            .map(|v| v.to_string().into_bytes())
    }

    // App::tick is the per-frame drive loop
    pub fn tick(&mut self, dt: f32) {
        // Advance the non-blocking startup state-machine. Runs before the
        // drain so a publish a startup step triggers (the structure parse,
        // a committed normalize) lands in this frame's `changes` batch and
        // is projected the same frame. Inert once bring-up is done.
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
        }

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
            if self.engine.is_some()
                && ((self.startup_settled() && has_geometry && !self.store.has_pending())
                    || view_toggled)
            {
                crate::viz::refresh::refresh_external_cavities(
                    &mut self.runner_client,
                    &mut self.store,
                    &self.view_options,
                );
                crate::viz::refresh::refresh_clashes(
                    &mut self.runner_client,
                    &mut self.store,
                    &self.view_options,
                );
                crate::viz::refresh::refresh_exposed_hydrophobics(
                    &mut self.runner_client,
                    &mut self.store,
                    &self.view_options,
                );
            }
        }

        if !changes.is_empty() {
            // RunnerProjector consumes changes, see `RunnerProjector`
            if let Some(orch) = self.runner_client.orchestrator_mut() {
                self.runner_projector.consume(&changes, &self.store, orch);
            }

            // RenderProjector consumes changes and engine receives view option updates
            if let Some(engine) = self.engine.as_mut() {
                self.render_projector.consume(&changes, &self.store, engine);
                // Push the structural-viz overlays from the viz cache in the
                // same engine-borrow stage as the consume drain, gated on the
                // cache being dirty (set by this tick's overlay refresh above).
                // The projector is the single pusher; it reports back so we can
                // clear the flag it cannot clear through its shared `&Session`.
                #[cfg(not(target_arch = "wasm32"))]
                if RenderProjector::push_overlays(&self.store, engine) {
                    self.store.viz.viz_dirty = false;
                }
                if changes
                    .iter()
                    .any(|c| matches!(c, SessionUpdate::ViewOptionsChanged))
                {
                    engine.set_options(self.view_options.clone());
                }
            }
        }

        // 5. Fire the NEXT async rescore, the AT-REST rescore only. Scores go
        //    stale only on an assembly change (every mutation emits a
        //    SessionUpdate, including those from non-scoring plugins), so this
        //    gates on this tick's geometry change. It fires only when no edit is
        //    open: while a stream runs, each frame carries its own warm score and
        //    stamps its edit directly (in `apply_backend_updates`), so this query
        //    is not fired - it would only re-score a trailing frame. It is
        //    also held off until the startup machine settles: during bring-up
        //    the machine drives the first score itself (kicked post-normalize,
        //    once the scorer's pose is built), and firing here would race a
        //    query into the pose-less window between a plugin's Init and its
        //    normalize, which comes back empty. After `Done` the machine is
        //    inert and this is the sole at-rest scorer again.
        //    Fire-and-forget against the worker's already-built live pose (no
        //    per-frame pose rebuild); `request_scores` coalesces, so one
        //    outstanding query per provider is the most in flight.
        #[cfg(not(target_arch = "wasm32"))]
        {
            if self.startup_settled() && has_geometry && !self.store.has_pending() {
                self.request_scores();
            }
        }

        // 6. Engine update + 7. visualization overlay.
        #[cfg(not(target_arch = "wasm32"))]
        let pull = self.runner_client.pull_drag_pull_info();
        #[cfg(target_arch = "wasm32")]
        let pull: Option<viso::PullInfo> = None;
        if let Some(engine) = self.engine.as_mut() {
            engine.update(dt);
            update_all_visualizations(engine, pull);
        }

        // The InSession flip is no longer a tick-stage: every load path calls
        // `enter_session` at its done-loading point, so the frontend routes to
        // the in-puzzle UI the moment loading completes, with the score
        // flowing in asynchronously via steps 2 + 5.

        // 8. Frontend projection: the GUI consumer derives its dirty set
        //    entirely from this tick's `changes` batch, plus the one-shot
        //    `needs_full_populate` signal (session birth). The signal is
        //    consumed (taken + cleared) only when the engine is present and
        //    the consumer runs; with no engine attached yet it persists to a
        //    later tick, so the birth populate is never dropped.
        if let Some(engine) = self.engine.as_ref() {
            let full_populate = self.needs_full_populate;
            self.needs_full_populate = false;
            let src = GuiSources {
                session: &self.store,
                engine,
                driver: &self.runner_client,
                host: self.host.as_ref(),
                view_options: &self.view_options,
                active_preset: self.active_preset.as_deref(),
            };
            self.gui_projector
                .consume(&changes, full_populate, &src, &mut self.frontend);
        }
    }
}

/// The foldit app implements the `foldit_gui::Dispatcher` trait
///
/// This essentially just denotes the App as the struct that processes
/// frontend -> backend commands dispatched by the gui
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
        }
    }
}
