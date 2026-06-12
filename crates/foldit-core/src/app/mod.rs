//! Foldit application state - host-agnostic.
//!
//! `App` owns the `Session`, `RunnerClient` (which carries the
//! orchestrator), the three projectors (`RunnerProjector`,
//! `RenderProjector`, `GuiProjector`), and the cross-cutting
//! bookkeeping (puzzle metadata, viso engine handle, dirty-flags,
//! history-version trackers). Both the desktop (`foldit-desktop`) and
//! web (`foldit-web`) builds wrap this in their host-specific lifecycle:
//!
//! - desktop: `window::AppRunner` holds the wry webview + winit window
//!   alongside `App`; winit events are converted to host-agnostic
//!   types before being forwarded to `App`'s methods.
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
mod startup;
#[cfg(not(target_arch = "wasm32"))]
mod scores_coord;
#[cfg(not(target_arch = "wasm32"))]
mod voids_coord;
#[cfg(not(target_arch = "wasm32"))]
mod clash_coord;
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
    pub(crate) engine: Option<VisoEngine>,
    pub(in crate::app) keybindings: KeyBindings,
    pub(crate) store: Session,
    /// Active view options (render settings). App-owned so they survive a
    /// puzzle/structure reload (`Session::reset` no longer touches them); the
    /// tick applies them to the engine on every
    /// [`SessionUpdate::ViewOptionsChanged`] the App emits. The single source
    /// of truth for what viso renders and what the view panel binds.
    pub(in crate::app) view_options: viso::options::VisoOptions,
    /// Name of the preset whose options are currently loaded, or `None` when
    /// the active options were set manually (a manual edit no longer matches
    /// any named preset). App-owned alongside [`Self::view_options`].
    pub(in crate::app) active_preset: Option<String>,
    /// Latched the first time the player touches any view setting (a manual
    /// option edit or an explicit preset pick). Once set, the per-load preset
    /// seed is skipped so the player's persisted choice overrides the
    /// puzzle/Default seed on every subsequent load. A fresh app starts
    /// `false`, so its first load still seeds from the puzzle/Default preset.
    pub(in crate::app) view_settings_touched: bool,
    pub(crate) runner_client: RunnerClient,
    /// Plugin projection of the `SessionUpdate` stream. A peer field to
    /// `runner_client` (not nested inside it) so the tick seam can borrow
    /// the orchestrator handle and this projector disjointly.
    pub(in crate::app) runner_projector: RunnerProjector,
    pub(in crate::app) render_projector: RenderProjector,
    pub(in crate::app) gui_projector: GuiProjector,
    /// Host-provided filesystem / resource access. The only path through
    /// which foldit-core touches the filesystem outside puzzle loading.
    pub(in crate::app) host: Box<dyn crate::HostResources>,
    /// Frontend mirror - written by the GUI consumer
    /// ([`GuiProjector::consume`]) at the end of each tick and drained by
    /// the host via [`Self::serialize_frontend_dirty`].
    pub(in crate::app) frontend: FrontendState,
    /// App-lifetime lifecycle phase. `App` advances this through the
    /// startup phases and flips it to `InSession` the moment a structure
    /// finishes loading (see [`Self::enter_session`]). Mirrored verbatim
    /// to the frontend via [`FrontendState::set_app_state`].
    pub(in crate::app) lifecycle: AppPhase,
    /// Non-blocking startup state-machine. Armed by
    /// [`Self::begin_startup`] (the host trigger) and advanced one step per
    /// frame by [`Self::advance_startup`] near the top of [`Self::tick`], so
    /// bring-up's worker round-trips (warm connect, plugin `Init`, normalize,
    /// first score) run across frames while the host keeps rendering.
    #[cfg(not(target_arch = "wasm32"))]
    pub(in crate::app) startup: self::load::StartupPhase,
    /// One-shot "push every GUI section once" signal. Raised on session
    /// birth (the Loading → `InSession` flip on every load path) and
    /// consumed + cleared by `tick` on the next
    /// GUI-consumer pass, which projects a full `DirtyFlags::all()` populate.
    /// The incremental sections during a load still flow through the ordinary
    /// `SessionUpdate` batch; this catches the sections no batch variant
    /// carries (a free-form reload's puzzle-panel title, the post-load score /
    /// action catalog).
    pub(in crate::app) needs_full_populate: bool,
    /// Commit-stamp correlation: each in-flight commit-time composition-score
    /// `request_id` → the committed checkpoint its reply stamps. The checkpoint
    /// is immutable, so its identity is stable until the reply lands. Cleared
    /// on orchestrator reinit (request ids restart at 1 there, so a stale
    /// entry could otherwise collide with a fresh edit id).
    #[cfg(not(target_arch = "wasm32"))]
    pub(in crate::app) score_targets: std::collections::HashMap<u64, CheckpointId>,
    /// `request_id` → (preview entity id, its atom count) for the
    /// transient entity a creates-entities stream is animating. Created on
    /// the first streamed frame, coord-updated per frame while the atom
    /// count is unchanged (rebuilt under a new id when it changes, so the
    /// render projector does a topology rebuild rather than a desyncing
    /// coord update), and discarded at the terminal (the final full-atom
    /// entity is adopted fresh).
    #[cfg(not(target_arch = "wasm32"))]
    pub(in crate::app) creates_previews:
        std::collections::HashMap<u64, (molex::EntityId, usize)>,
    /// Pull-drag intent captured at left-button-down. The pull is
    /// determined by the down-target, not by where the cursor later
    /// wanders: a drag that began on empty background must never grab a
    /// residue it crosses, and a drag that began on a residue must pull
    /// *that* residue. `Some(route)` after a left-down that resolved to a
    /// pullable target; `None` after a down on empty / non-pullable
    /// surface (that gesture can only camera-rotate). The first qualifying
    /// pointer-move takes the route to open the stream; `PointerUp` clears
    /// it.
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
    ///
    /// - `set_app_phase(InSession)` is the routing signal the
    ///   frontend reads; flipping it here clears the loading screen.
    /// - The score gauge is reset to "not scored yet" so a reload never
    ///   displays the previous structure's score. `project_score` no-ops
    ///   when `display_score()` is `None` (and never resets `score.invalid`),
    ///   so this is the only place the stale value is cleared. When the load
    ///   path stamped a head score (see `score_head_now`), the one-shot full
    ///   populate below re-derives the gauge from that stamp on the next pass.
    /// - The score title is read from the store here because `project_score`
    ///   does not write the title.
    /// - `needs_full_populate` reprojects every session section (score
    ///   panel, title, history, scene, view, selection, actions) on the
    ///   next GUI-consumer pass.
    ///
    /// Who stamps the first score depends on the path. The two mid-session
    /// reload paths stamp it synchronously (via `score_head_now`) *before*
    /// this flip, so the backbone is already colored when the scene is first
    /// shown. The startup path flips into the session as soon as the first
    /// async score has stamped the head (the startup machine watches for it
    /// before calling this), so the backbone is colored there too. Either
    /// way, this method no longer requests the score.
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

    // ── View-options reads ──

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

    // The GUI projection now lives on `GuiProjector` as the third
    // `SessionUpdate` consumer; see `impl GuiProjector` below. The tick
    // builds a `GuiSources` and calls `gui_projector.consume(...)` at the
    // end-of-tick route. There is no `populate_frontend` method anymore:
    // its body moved verbatim onto the consumer, reading named inputs
    // instead of `&mut self`.

    // ── The per-frame drive loop ──

    /// Drive one frame.
    ///
    /// Order:
    /// 0. advance the non-blocking startup state-machine (native only): one
    ///    bring-up step, before the drain so a publish it triggers is routed
    ///    this frame. Inert once startup is done.
    /// 1. drain pending plugin updates (apply to `Session`; emits
    ///    `SessionUpdate`s through the funnel).
    /// 2. apply this tick's async score replies so their `ScoresChanged`
    ///    joins this tick's batch.
    /// 3. drain the `SessionUpdate` stream in one go.
    /// 4. route the batch: runner projector fan-out and render projector
    ///    publish + per-residue color re-derive (both no-op on empty batches;
    ///    the render projector also runs on a score-only batch).
    /// 5. fire the next async rescore (gated on a geometry change; reply
    ///    applies on a later tick's step 2). The FIRST score per session is
    ///    stamped either synchronously by a reload path (`score_head_now`) or
    ///    asynchronously by the startup machine's first-score kick, not by
    ///    this at-rest gate.
    /// 6. engine update (camera animation, mesh upload, etc.).
    /// 7. visualization overlay (bands / pull).
    /// 8. GUI consumer projects the batch (+ one-shot full populate) into
    ///    the frontend so the next `serialize_frontend_dirty` carries the
    ///    latest snapshot.
    pub fn tick(&mut self, dt: f32) {
        // 0. Advance the non-blocking startup state-machine. Runs before the
        //    drain so a publish a startup step triggers (the structure parse,
        //    a committed normalize) lands in this frame's `changes` batch and
        //    is projected the same frame. Inert once bring-up is done.
        #[cfg(not(target_arch = "wasm32"))]
        self.advance_startup();

        // 1. Plugin updates.
        self.apply_backend_updates();

        // 2. Apply this tick's score replies BEFORE the drain so their
        //    `ScoresChanged` lands in this tick's `changes` batch. That makes
        //    the render projector re-derive the per-residue colors the same
        //    frame the score arrives (and, on the first score, the same frame
        //    the geometry publishes), instead of a tick late. The async
        //    request that triggers the NEXT rescore stays AFTER the drain
        //    (it gates on this tick's geometry change). Always async: the
        //    session goes live before the first score now, so the render
        //    thread must never block on the worker, including pre-first-score.
        #[cfg(not(target_arch = "wasm32"))]
        {
            // Apply whatever async whole-assembly and composition replies have
            // arrived (none until the first request below lands). Each stamps
            // the session and emits `ScoresChanged`, drained just below.
            self.poll_async_scores();
            self.poll_composition_scores();
        }

        // 3-4. Drain the SessionUpdate stream once and route to both
        //      projectors. The tick is the sole drain. Handlers used to call
        //      `pump_scene_changes` per-event, but that race-conditioned
        //      against the render projector reading the same update queue, so
        //      the per-handler pumps were removed.
        let changes = self.store.take_updates();
        // `render_changes` counts only the scene-mutating updates: an
        // assembly republish keys off these, and the steady-state async
        // rescore (step 5) gates on them. A ScoresChanged / SelectionChanged
        // / FocusChanged / view / bubble / puzzle / appearance update is not a
        // scene mutation, so it is excluded here: republishing geometry on such a
        // reply is wasted work (and forces a spurious full rebuild on a
        // topology tick), and re-querying scores in response would loop. The
        // render projector still runs on a score-only batch to re-derive
        // colors, but it self-filters and does not republish geometry there.
        let render_changes = changes
            .iter()
            .filter(|c| {
                !matches!(
                    c,
                    SessionUpdate::ScoresChanged
                        | SessionUpdate::SelectionChanged
                        | SessionUpdate::FocusChanged
                        | SessionUpdate::BubbleChanged
                        | SessionUpdate::PuzzleChanged
                        | SessionUpdate::ViewOptionsChanged
                        | SessionUpdate::EntityAppearanceChanged
                )
            })
            .count();
        // BubbleChanged / PuzzleChanged have no viso side effect; their GUI
        // dirty (TEXT_BUBBLE / PUZZLE) is derived from the batch by the GUI
        // consumer below, so there are no tick arms for them anymore.
        if !changes.is_empty() {
            // The projector self-filters score-only batches (scores are
            // not an observable mutation for plugins); a no-op call is cheap.
            if let Some(orch) = self.runner_client.orchestrator_mut() {
                self.runner_projector
                    .consume(&changes, &self.store, orch);
            }
            // The render projector self-filters its reactions (selection /
            // focus / geometry / scores), so it runs on any non-empty batch
            // and no-ops internally on one that carries none of them. The
            // view-options reaction is driven here, not in the projector: the
            // options live on `App` (so they persist across a topology swap),
            // and applying them to the engine is gated on the same
            // `ViewOptionsChanged` signal the projector would have keyed off.
            if let Some(engine) = self.engine.as_mut() {
                self.render_projector.consume(&changes, &self.store, engine);
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
            if self.startup_settled() && render_changes > 0 && !self.store.has_pending() {
                self.request_scores();
            }
        }

        // 5b. Refresh the engine's external cavity set from the plugin's
        //     `voids` query. Like the at-rest rescore, voids go stale on a
        //     geometry change, so refresh on the same at-rest gate; also
        //     refresh when the cavity-display toggle flipped (a
        //     ViewOptionsChanged carries it) so toggling on requests voids
        //     and toggling off clears them. The call self-gates on the engine
        //     being present, the cavity display being on, and a plugin
        //     advertising `voids`, so it is an inert no-op until the plugin
        //     implements the query.
        #[cfg(not(target_arch = "wasm32"))]
        {
            let view_toggled = changes
                .iter()
                .any(|c| matches!(c, SessionUpdate::ViewOptionsChanged));
            if (self.startup_settled() && render_changes > 0 && !self.store.has_pending())
                || view_toggled
            {
                self.refresh_external_cavities();
            }
        }

        // 5c. Refresh the engine's steric-clash arcs from the plugin's
        //     `clashes` query, on the same at-rest geometry gate as the
        //     rescore and voids refresh (clashes go stale on a geometry
        //     change); also refresh when the clash-display toggle flipped (a
        //     ViewOptionsChanged carries it) so toggling on requests clashes
        //     and toggling off clears them. The call self-gates on the engine
        //     being present, the clash display being on, and a plugin
        //     advertising `clashes`, so it is an inert no-op until the plugin
        //     implements the query.
        #[cfg(not(target_arch = "wasm32"))]
        {
            let view_toggled = changes
                .iter()
                .any(|c| matches!(c, SessionUpdate::ViewOptionsChanged));
            if (self.startup_settled() && render_changes > 0 && !self.store.has_pending())
                || view_toggled
            {
                self.refresh_clashes();
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

// ---------------------------------------------------------------------------
// Bridge: Dispatcher trait impl
// ---------------------------------------------------------------------------

impl foldit_gui::Dispatcher for App {
    /// Webview signaled it's ready - mark every section of the owned
    /// `FrontendState` dirty so the next `serialize_frontend_dirty`
    /// emits a full snapshot. App owns the frontend mirror, so
    /// this lives here rather than on the host.
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
