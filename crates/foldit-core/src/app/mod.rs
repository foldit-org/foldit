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

use foldit_gui::{FrontendState, LoadingState};
use viso::{KeyBindings, VisoEngine};

use crate::gui_projector::{GuiProjector, GuiSources};
#[cfg(not(target_arch = "wasm32"))]
use crate::history::CheckpointId;
use crate::render_projector::RenderProjector;
use crate::runner_client::RunnerClient;
use crate::runner_projector::RunnerProjector;
use crate::session::{Session, SessionUpdate, SessionUpdateConsumer};

mod dispatch;
mod input;
mod load;
#[cfg(not(target_arch = "wasm32"))]
mod scores_coord;
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
/// the host seam) and the `LoadingState` machine that drives the startup
/// phases up to the first-score `InSession` flip.
pub struct App {
    pub(in crate::app) engine: Option<VisoEngine>,
    pub(in crate::app) keybindings: KeyBindings,
    pub(in crate::app) store: Session,
    pub(in crate::app) runner_client: RunnerClient,
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
    /// startup phases and flips it to `InSession` at the first-score gate
    /// (`awaiting_initial_score` + `has_initial_score()`). Mirrored
    /// verbatim to the frontend via [`FrontendState::set_app_state`].
    pub(in crate::app) lifecycle: LoadingState,
    /// Set after `load_initial_structure` returns; cleared in `tick`
    /// once the first plugin score lands. Mirrors the desktop runner's
    /// old field.
    pub(in crate::app) awaiting_initial_score: bool,
    /// One-shot "push every GUI section once" signal. Raised on session
    /// birth (the Loading → `InSession` flip for the initial load, and at the
    /// end of each reload path) and consumed + cleared by `tick` on the next
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
    pub(in crate::app) pending_pull_origin: Option<crate::pull_drag::PullRoute>,
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
            runner_client: RunnerClient::new(),
            runner_projector: RunnerProjector::new(),
            render_projector: RenderProjector::new(),
            gui_projector: GuiProjector::new(),
            host,
            frontend: FrontendState::new(),
            lifecycle: LoadingState::Initializing,
            awaiting_initial_score: false,
            needs_full_populate: false,
            #[cfg(not(target_arch = "wasm32"))]
            score_targets: std::collections::HashMap::new(),
            #[cfg(not(target_arch = "wasm32"))]
            pending_pull_origin: None,
        }
    }

    /// True once the Rosetta backend has delivered its first score
    /// update for the current session. Read by [`Self::tick`] to gate
    /// the Loading → `InSession` transition.
    fn has_initial_score(&self) -> bool {
        self.store.display_score().is_some()
    }

    /// Advance the App-lifetime phase and mirror it to the frontend
    /// transmit gate. `set_app_state` only marks the section dirty when
    /// the value actually changes, so re-setting the same phase is a
    /// no-op on the wire.
    pub(in crate::app) fn set_loading_state(&mut self, state: LoadingState) {
        self.lifecycle = state;
        self.frontend.set_app_state(self.lifecycle);
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
    /// 1. drain pending plugin updates (apply to `Session`; emits
    ///    `SessionUpdate`s through the funnel).
    /// 2. apply this tick's score replies (blocking until the first score,
    ///    async thereafter) so their `ScoresChanged` joins this tick's batch.
    /// 3. drain the `SessionUpdate` stream in one go.
    /// 4. route the batch: runner projector fan-out and render projector
    ///    publish + per-residue color re-derive (both no-op on empty batches;
    ///    the render projector also runs on a score-only batch).
    /// 5. fire the next steady-state async rescore (gated on a geometry
    ///    change; reply applies on a later tick's step 2).
    /// 6. engine update (camera animation, mesh upload, etc.).
    /// 7. visualization overlay (bands / pull).
    /// 8. `InSession` gate (one-shot, on first score; raises full populate).
    /// 9. GUI consumer projects the batch (+ one-shot full populate) into
    ///    the frontend so the next `serialize_frontend_dirty` carries the
    ///    latest snapshot.
    pub fn tick(&mut self, dt: f32) {
        // 1. Plugin updates.
        self.apply_backend_updates();

        // 2. Apply this tick's score replies BEFORE the drain so their
        //    `ScoresChanged` lands in this tick's `changes` batch. That makes
        //    the render projector re-derive the per-residue colors the same
        //    frame the score arrives (and, on the first score, the same frame
        //    the geometry publishes), instead of a tick late. The async
        //    request that triggers the NEXT rescore stays AFTER the drain
        //    (it gates on this tick's geometry change). Captured here, before
        //    applying, is whether a score already existed: the bootstrap
        //    blocking poll flips that true mid-tick, and the async request
        //    below must not also fire on the bootstrap tick.
        #[cfg(not(target_arch = "wasm32"))]
        let had_initial_score = self.has_initial_score();
        #[cfg(not(target_arch = "wasm32"))]
        {
            if had_initial_score {
                // Steady state: apply whatever async whole-assembly and
                // composition replies have arrived. Each stamps the session
                // and emits `ScoresChanged`, drained just below.
                self.poll_async_scores();
                self.poll_composition_scores();
            } else {
                // No score yet: blocking poll each tick until the first one
                // lands, so the InSession gate flips promptly. Brief, one-time
                // per load.
                self.poll_plugin_scores();
            }
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
        // / FocusChanged / view / bubble / puzzle update is not a scene
        // mutation, so it is excluded here: republishing geometry on such a
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
            // The render projector self-filters all five of its reactions
            // (selection / focus / view-options / geometry / scores), so it
            // runs on any non-empty batch and no-ops internally on one that
            // carries none of them.
            if let Some(engine) = self.engine.as_mut() {
                self.render_projector.consume(&changes, &self.store, engine);
            }
        }

        // 5. Fire the NEXT steady-state async rescore. Scores go stale only on
        //    an assembly change (every mutation emits a SessionUpdate,
        //    including those from non-scoring plugins), so this gates on this
        //    tick's geometry change. Fire-and-forget against the worker's
        //    already-built live pose (no per-frame pose rebuild); the reply
        //    applies on a later tick's step-2 drain. With exactly one edit
        //    open the live pose IS that edit's composition, so its reply
        //    attributes to the edit; per-edit exactness for any other case
        //    lands at commit via the commit-stamp. Skipped on the bootstrap
        //    tick (the blocking poll in step 2 already covered it, and
        //    `had_initial_score` was false then).
        #[cfg(not(target_arch = "wasm32"))]
        {
            if had_initial_score && render_changes > 0 {
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

        // 8. State-machine: flip into InSession the first time the plugin
        //    score lands for the just-loaded session. This is the only
        //    phase that routes the frontend to the in-puzzle UI.
        if self.awaiting_initial_score && self.has_initial_score() {
            self.set_loading_state(LoadingState::InSession);
            self.awaiting_initial_score = false;
            self.frontend.set_puzzle_loaded(true);
            self.frontend.set_score_title(self.store.title().to_owned());
            self.frontend
                .set_puzzle_scientist(self.store.title().to_owned());
            // Session birth: the GUI consumer below does a one-shot full
            // populate (every section once) rather than flooding the
            // transmit layer's dirty bits directly.
            self.needs_full_populate = true;
            log::info!("Initial plugin score received - app_state=InSession");
        }

        // 9. Frontend projection: the GUI consumer derives its dirty set
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
