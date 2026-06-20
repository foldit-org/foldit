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
#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod score_apply;
mod startup;
#[cfg(test)]
mod tests;

use self::input::update_all_visualizations;
#[cfg(not(target_arch = "wasm32"))]
pub use self::load::locate_plugins_root;

/// The open segment-info target plus the identity and secondary structure
/// cached at the moment it was set.
///
/// Identity and SS are computed once (a single `recompute_ss()` over the
/// head assembly) when the target opens and held here for its lifetime;
/// the GUI projection rebuilds only the energies and the screen anchor on
/// each score tick, so a streaming score never re-runs DSSP.
pub(crate) struct SegmentTarget {
    pub(crate) entity: molex::EntityId,
    pub(crate) residue: usize,
    pub(crate) residue_number: i32,
    pub(crate) chain: String,
    pub(crate) aa_three: String,
    pub(crate) aa_one: String,
    pub(crate) ss_label: String,
}

/// Last segment-panel tail tip projected to the screen, value-compared
/// each frame so an unchanged tip pushes nothing.
///
/// The `Unset` arm is distinct from `Hidden`: at rest (no panel ever
/// opened) the tip is `Unset`, and the off-screen path only emits a hide
/// when a `Visible` tip preceded it. Without that distinction every idle
/// frame would push a redundant hide.
#[derive(Clone, Copy, PartialEq)]
enum TailTip {
    /// No tip has been projected yet (no panel opened this session).
    Unset,
    /// The panel is open but its residue is off-screen / behind the camera.
    Hidden,
    /// The residue's CA projects to this screen position (pixels, top-left).
    Visible(f32, f32),
}

/// A tail-tip change the host should push to the webview this frame.
///
/// Returned by [`App::take_tail_update`] only when the tip changed since
/// the last push; an unchanged tip yields `None` and the host pushes
/// nothing.
pub enum TailUpdate {
    /// Move the tail tip to this screen position (pixels, origin top-left).
    Position(f32, f32),
    /// Hide the tail (the residue went off-screen, or the panel closed).
    Hide,
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
    /// One-shot accumulator of App-owned GUI dirty bits the `SessionUpdate`
    /// batch cannot express (segment / panels / ui / progress), plus the
    /// full-populate seed (`DirtyFlags::all()`) raised on session birth. The
    /// tick drains it once and ORs it into the GUI consumer's batch-derived
    /// set.
    pub(in crate::app) pending_dirty: DirtyFlags,
    /// Open segment-info target with its cached identity + SS, or `None`
    /// when the panel is closed.
    pub(in crate::app) open_segment: Option<SegmentTarget>,
    /// Panels currently shown (by string id). A panel absent from the set
    /// is closed. Backend-authoritative so visibility survives a reload.
    pub(in crate::app) open_panels: std::collections::BTreeSet<String>,
    /// Per-panel dragged top-left position (pixels, origin top-left). A
    /// panel without an entry renders at its layout default.
    pub(in crate::app) panel_positions: std::collections::BTreeMap<String, (f32, f32)>,
    /// Puzzle high-score progress: best recorded display score per puzzle id
    /// (monotonic max). Backend-authoritative; the tick records into it when
    /// a loaded puzzle's display score improves on its current best.
    pub(in crate::app) progress: std::collections::BTreeMap<u32, f64>,
    /// One-shot signal that `progress` changed and must be flushed to disk.
    /// Separate from the `PROGRESS` dirty bit (which drives the GUI reproject):
    /// the desktop host pulls the serialized map via
    /// [`App::take_progress_to_persist`] and writes it off the event-loop
    /// thread. Set only by a real in-session record/clear, never by a load
    /// import, so a load does not trigger a save back.
    pub(in crate::app) progress_persist_pending: bool,
    /// Whether the tutorial-hint bubble is shown. Backend-authoritative so
    /// the toggle survives a reload.
    pub(in crate::app) hints_visible: bool,
    /// Whether the window is in OS fullscreen. Backend-authoritative mirror;
    /// the desktop host pulls `take_fullscreen_change` and drives the winit
    /// window.
    pub(in crate::app) fullscreen: bool,
    /// Pending fullscreen change for the desktop host to pull this frame,
    /// staged only when `set_fullscreen` actually flipped the flag. The host
    /// takes it via [`App::take_fullscreen_change`].
    pub(in crate::app) pending_fullscreen: Option<bool>,
    /// Last tail tip pushed to the host, value-compared each frame so an
    /// unchanged tip pushes nothing. The panel body is placed once and is
    /// draggable; only its tail tip tracks the open residue's live screen
    /// position as the camera moves.
    pub(in crate::app) last_tail_tip: TailTip,
    /// Pending tail-tip change for the host to pull this frame, set only
    /// when the projected tip differed from `last_tail_tip`. The host takes
    /// it each frame via [`App::take_tail_update`].
    pub(in crate::app) pending_tail: Option<TailUpdate>,
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
            pending_dirty: DirtyFlags::empty(),
            open_segment: None,
            open_panels: std::collections::BTreeSet::new(),
            panel_positions: std::collections::BTreeMap::new(),
            progress: std::collections::BTreeMap::new(),
            progress_persist_pending: false,
            hints_visible: true,
            fullscreen: false,
            pending_fullscreen: None,
            last_tail_tip: TailTip::Unset,
            pending_tail: None,
            #[cfg(not(target_arch = "wasm32"))]
            score_targets: std::collections::HashMap::new(),
            #[cfg(not(target_arch = "wasm32"))]
            creates_previews: std::collections::HashMap::new(),
            #[cfg(not(target_arch = "wasm32"))]
            inplace_previews: std::collections::HashMap::new(),
            #[cfg(not(target_arch = "wasm32"))]
            inplace_edits: std::collections::HashMap::new(),
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
        self.pending_dirty |= DirtyFlags::all();
    }

    /// Open the per-residue segment-info panel on `(eid, residue)`.
    ///
    /// Computes the residue identity (number, chain, amino acid) and its
    /// secondary structure once, via a single `recompute_ss()` over the
    /// head assembly, and caches them on the target. A no-op when the
    /// entity or residue does not resolve. Marks the segment section dirty
    /// so the GUI projection reprojects on the next tick.
    pub(in crate::app) fn open_segment(&mut self, eid: molex::EntityId, residue: usize) {
        let Some(entity) = self.store.entity(eid) else {
            return;
        };
        let Some(residues) = entity.residues() else {
            return;
        };
        let Some(res) = residues.get(residue) else {
            return;
        };
        let residue_number = res.seq_id();
        let chain = entity
            .pdb_chain_id()
            .map_or_else(String::new, |c| (c as char).to_string());
        let aa = molex::chemistry::AminoAcid::from_code(res.name);
        let aa_three = String::from_utf8_lossy(&res.name).trim().to_owned();
        let aa_one = aa.map_or_else(String::new, |a| (a.one_letter() as char).to_string());

        let mut assembly = self.store.head_assembly();
        assembly.recompute_ss();
        let ss_label = ss_label(assembly.ss_types(eid).get(residue).copied());

        self.open_segment = Some(SegmentTarget {
            entity: eid,
            residue,
            residue_number,
            chain,
            aa_three,
            aa_one,
            ss_label,
        });
        self.pending_dirty |= DirtyFlags::SEGMENT;
    }

    /// Close the segment-info panel. Marks the segment section dirty so the
    /// GUI projection clears it on the next tick.
    pub(in crate::app) fn close_segment(&mut self) {
        self.open_segment = None;
        self.pending_dirty |= DirtyFlags::SEGMENT;
    }

    /// Show or hide a panel by id. Marks the panels section dirty so the
    /// GUI projection reprojects on the next tick.
    pub(in crate::app) fn set_panel_visible(&mut self, panel: String, visible: bool) {
        if visible {
            self.open_panels.insert(panel);
        } else {
            self.open_panels.remove(&panel);
        }
        self.pending_dirty |= DirtyFlags::PANELS;
    }

    /// Record a panel's dragged top-left position. Marks the panels section
    /// dirty so the GUI projection reprojects on the next tick.
    pub(in crate::app) fn set_panel_position(&mut self, panel: String, x: f32, y: f32) {
        self.panel_positions.insert(panel, (x, y));
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
        if score <= 0.0 {
            return;
        }
        let best = self.progress.entry(puzzle_id).or_insert(f64::NEG_INFINITY);
        if score > *best {
            *best = score;
            self.pending_dirty |= DirtyFlags::PROGRESS;
            self.progress_persist_pending = true;
        }
    }

    /// Wipe all recorded high-score progress. Marks the progress section
    /// dirty so the GUI projection reprojects the now-empty map.
    pub(in crate::app) fn clear_progress(&mut self) {
        if self.progress.is_empty() {
            return;
        }
        self.progress.clear();
        self.pending_dirty |= DirtyFlags::PROGRESS;
        self.progress_persist_pending = true;
    }

    /// Show or hide the tutorial-hint bubble. Marks the ui section dirty so
    /// the GUI projection reprojects `ui.hints_visible` on the next tick.
    pub(in crate::app) fn set_hints_visible(&mut self, v: bool) {
        self.hints_visible = v;
        self.pending_dirty |= DirtyFlags::UI;
    }

    /// Enter or leave OS fullscreen. Marks the ui section dirty so the GUI
    /// projection reprojects `ui.fullscreen`, and stages a value-gated change
    /// for the desktop host to pull and apply to the winit window. Only the
    /// false->true / true->false transition stages, so re-setting the same
    /// value pushes nothing to the host.
    pub(in crate::app) fn set_fullscreen(&mut self, v: bool) {
        if self.fullscreen != v {
            self.fullscreen = v;
            self.pending_fullscreen = Some(v);
        }
        self.pending_dirty |= DirtyFlags::UI;
    }

    /// Take the pending fullscreen change for the desktop host to apply to
    /// the winit window, or `None` when it did not change since the last
    /// pull. Returned at most once per change.
    pub const fn take_fullscreen_change(&mut self) -> Option<bool> {
        self.pending_fullscreen.take()
    }

    /// Take the serialized high-score progress map for the host to persist to
    /// disk, or `None` when it has not changed since the last pull. Returned
    /// at most once per change. The host owns the storage backend and the
    /// async I/O; foldit-core only hands over the bytes.
    pub fn take_progress_to_persist(&mut self) -> Option<Vec<u8>> {
        if !self.progress_persist_pending {
            return None;
        }
        self.progress_persist_pending = false;
        serde_json::to_vec(&self.progress).ok()
    }

    /// Merge a persisted high-score progress map (as written by
    /// [`App::take_progress_to_persist`]) back into the live map. Monotonic
    /// max per puzzle so any record made in-session before the async load
    /// completed is not clobbered by a stale on-disk best. Marks the GUI
    /// progress section dirty so the merged map projects, but deliberately
    /// does not set the persist-pending flag, so a load does not bounce back
    /// out as a save.
    pub fn import_progress(&mut self, bytes: &[u8]) {
        let Ok(loaded) = serde_json::from_slice::<std::collections::BTreeMap<u32, f64>>(bytes)
        else {
            return;
        };
        let mut changed = false;
        for (puzzle_id, score) in loaded {
            let best = self.progress.entry(puzzle_id).or_insert(f64::NEG_INFINITY);
            if score > *best {
                *best = score;
                changed = true;
            }
        }
        if changed {
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

    /// Take the pending segment-panel tail-tip change for the host to push,
    /// or `None` when the tip did not move since the last push. `Some` is
    /// returned at most once per change: `tick` sets it only on a value
    /// change and this clears it. The host distinguishes a position update
    /// from a hide via the [`TailUpdate`] variant.
    pub const fn take_tail_update(&mut self) -> Option<TailUpdate> {
        self.pending_tail.take()
    }

    /// Project the open segment target's CA to the screen and stage a
    /// tail-tip change when it differs from the last pushed tip. A closed
    /// panel or an off-screen residue resolves to `Hidden`; a hide is
    /// staged only when a `Visible` tip preceded it, so an idle frame with
    /// no panel pushes nothing.
    fn update_tail_tip(&mut self) {
        let current = match (self.open_segment.as_ref(), self.engine.as_ref()) {
            (Some(target), Some(engine)) => self
                .store
                .entity(target.entity)
                .and_then(|entity| crate::gui_projector::ca_world_position(entity, target.residue))
                .and_then(|world| engine.world_to_screen(world))
                .map_or(TailTip::Hidden, |v| TailTip::Visible(v.x, v.y)),
            _ => TailTip::Hidden,
        };

        if current == self.last_tail_tip {
            return;
        }

        match current {
            TailTip::Visible(x, y) => self.pending_tail = Some(TailUpdate::Position(x, y)),
            // Only emit a hide when something visible preceded it; the
            // initial `Unset` -> `Hidden` transition records state silently.
            TailTip::Hidden => {
                if matches!(self.last_tail_tip, TailTip::Visible(..)) {
                    self.pending_tail = Some(TailUpdate::Hide);
                }
            }
            TailTip::Unset => {}
        }
        self.last_tail_tip = current;
    }

    // App::tick is the per-frame drive loop
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
                let opts = &self.view_options;
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
                if changes
                    .iter()
                    .any(|c| matches!(c, SessionUpdate::ViewOptionsChanged))
                {
                    engine.set_options(self.view_options.clone());
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

        // 5. Fire the NEXT async rescore, the AT-REST rescore only. Scores go
        //    stale only on an assembly change (every mutation emits a
        //    SessionUpdate, including those from non-scoring plugins), so this
        //    gates on this tick's geometry change. It fires only when no edit is
        //    open: while a stream runs, each frame carries its own warm score and
        //    stamps its edit directly (in `apply_backend_updates`), so this query
        //    is not fired - it would only re-score a trailing frame. It is
        //    also held off until the startup machine settles: during bring-up
        //    the machine drives the first score itself (kicked once every
        //    plugin's Init has replied, so the scorer's pose is built), and
        //    firing here would race a query into the pose-less window before a
        //    plugin's Init replies, which comes back empty. After `Done` the
        //    machine is inert and this is the sole at-rest scorer again.
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

        // Tail tip: runs after `engine.update` (camera settled this frame)
        // and stages a tip change only when it moved.
        self.update_tail_tip();

        // The InSession flip is no longer a tick-stage: every load path calls
        // `enter_session` at its done-loading point, so the frontend routes to
        // the in-puzzle UI the moment loading completes, with the score
        // flowing in asynchronously via steps 2 + 5.

        // 8. Frontend projection: the GUI consumer derives its dirty set
        //    entirely from this tick's `changes` batch, OR'd with the App-side
        //    `pending_dirty` accumulator (the segment / panels / ui / progress
        //    bits plus the session-birth full-populate seed). The accumulator
        //    is drained only when the engine is present and the consumer runs;
        //    with no engine attached yet it persists to a later tick, so the
        //    birth populate is never dropped.
        if let Some(engine) = self.engine.as_ref() {
            let pending = std::mem::take(&mut self.pending_dirty);
            let src = GuiSources {
                session: &self.store,
                engine,
                driver: &self.runner_client,
                host: self.host.as_ref(),
                view_options: &self.view_options,
                active_preset: self.active_preset.as_deref(),
                open_segment: self.open_segment.as_ref(),
                open_panels: &self.open_panels,
                panel_positions: &self.panel_positions,
                progress: &self.progress,
                hints_visible: self.hints_visible,
                fullscreen: self.fullscreen,
            };
            let segment_auto_closed =
                self.gui_projector
                    .consume(&changes, pending, &src, &mut self.frontend);
            if segment_auto_closed {
                self.open_segment = None;
            }
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
