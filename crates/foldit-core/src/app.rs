//! Foldit application state — host-agnostic.
//!
//! `App` owns the `Session`, `PluginDriver` (which carries the
//! orchestrator + scene-broadcaster), the two projectors
//! (`RenderProjector`, `GuiProjector`), and the cross-cutting
//! bookkeeping (puzzle metadata, viso engine handle, dirty-flags,
//! history-version trackers). Both the desktop (`foldit-desktop`) and
//! web (`foldit-web`) builds wrap this in their host-specific lifecycle:
//!
//! - desktop: `window::AppRunner` holds the wry webview + winit window
//!   alongside `App`; winit events are converted to host-agnostic
//!   types before being forwarded to `App`'s methods.
//! - web: `foldit_web::FolditApp` holds `App` plus the canvas and JS
//!   callbacks; DOM events are forwarded as `ViewportInput` JSON.

use std::collections::{BTreeMap, BTreeSet};

use web_time::{Instant, UNIX_EPOCH};

use foldit_gui::{
    CheckpointInfo, CheckpointKindTag, DirtyFlags, FilterStatus, FrontendState, HistoryCommand,
    HistoryLiveUpdate, HistorySection, LoadingState, TextBubbleButton, TextBubblePayload, WireId,
};
use molex::entity::molecule::id::EntityId;
use viso::{
    classify_click_for_selection, ClickEvent, ClickSelectionAction, Focus, KeyBindings, VisoEngine,
};

use crate::gui_projector::GuiProjector;
use crate::history::{
    CheckpointId, CheckpointKind, FilterStatus as HistoryFilterStatus, History,
};
use crate::plugin_driver::PluginDriver;
#[cfg(not(target_arch = "wasm32"))]
use crate::plugin_driver::{
    DispatchError, DispatchIntent, EditScope, OpEvent, OpOutcome, StreamStartIntent,
};
use crate::render_projector::{self, RenderProjector};
use crate::session::{
    EntityOrigin, Puzzle, Session, SessionError, SessionUpdate, SessionUpdateConsumer,
};

/// Convert a rosetta raw score (REU) to foldit's game-mode display number.
/// Verbatim port of `rosetta_score_to_game_score_either(use_minimum=true,
/// internal=false)` (`rosetta_util.cc:2702`, constants at lines 2662-2664).
/// The linear map is universal foldit policy, not rosetta-specific, so it
/// lives next to the score-view selector that picks which representation
/// reaches the GUI (game when a puzzle is loaded, raw otherwise). Applied
/// to both whole-assembly and composition scores so neither ever displays
/// raw REU.
#[cfg(not(target_arch = "wasm32"))]
fn rosetta_raw_to_game(raw: f64) -> f64 {
    const SCORE_OFFSET: f64 = 800.0;
    const SCORE_SCALE: f64 = 10.0;
    const SCORE_MINIMUM: f64 = 0.0;
    ((-raw + SCORE_OFFSET) * SCORE_SCALE).max(SCORE_MINIMUM)
}

/// Accumulate one report's per-residue scores into the
/// `entity_id -> Vec<(residue_index, score)>` map (later writers stack on
/// earlier ones). Shared by the whole-assembly and composition paths.
#[cfg(not(target_arch = "wasm32"))]
fn accumulate_per_residue(
    per_entity: &mut std::collections::HashMap<u32, Vec<(u32, f64)>>,
    report: &foldit_runner::proto::plugin::ScoreReport,
) {
    for rs in &report.per_residue {
        let Some(rref) = rs.residue.as_ref() else {
            continue;
        };
        #[allow(clippy::cast_possible_truncation)]
        let entity_id = rref.entity_id as u32;
        per_entity
            .entry(entity_id)
            .or_default()
            .push((rref.residue_index, f64::from(rs.score)));
    }
}

fn timestamp_ms(t: web_time::SystemTime) -> f64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0)
}

/// Convert a parsed [`crate::puzzle::Bubble`] into the GUI-bound IPC
/// twin. Tier-1 conversion: text/color/image pass through; buttons are
/// built from `bubble.button` (defaulting to `"Next"`) plus an optional
/// `alt_button`, with `goto` left `None` since clicks close locally.
fn bubble_to_payload(b: &crate::puzzle::Bubble) -> TextBubblePayload {
    let mut buttons = vec![TextBubbleButton {
        text: b.button.clone().unwrap_or_else(|| "Next".to_string()),
        goto: None,
    }];
    if let Some(alt) = b.alt_button.as_ref() {
        buttons.push(TextBubbleButton {
            text: alt.clone(),
            goto: None,
        });
    }
    TextBubblePayload {
        text: b.text.clone(),
        color: b.color.clone(),
        image: b.image.clone(),
        buttons,
    }
}

fn checkpoint_kind_tag(k: &CheckpointKind) -> CheckpointKindTag {
    match k {
        CheckpointKind::Loaded { .. } => CheckpointKindTag::Load,
        CheckpointKind::PromotedPreview { .. } => CheckpointKindTag::PromotedPreview,
        CheckpointKind::AddEntity { .. } => CheckpointKindTag::AddEntity,
        CheckpointKind::RemoveEntity { .. } => CheckpointKindTag::RemoveEntity,
        CheckpointKind::LaneUndo { .. } => CheckpointKindTag::LaneUndo,
        CheckpointKind::PluginOp { .. } => CheckpointKindTag::PluginOp,
    }
}

fn filter_status_wire(s: &HistoryFilterStatus) -> FilterStatus {
    match s {
        HistoryFilterStatus::Pass => FilterStatus::Pass,
        HistoryFilterStatus::Fail(_) => FilterStatus::Fail,
        HistoryFilterStatus::NotEvaluated => FilterStatus::NotEvaluated,
    }
}

/// Project the backend `History` into the wire payload consumed by
/// the `HistoryPanel`.
fn project_history(store: &Session) -> HistorySection {
    let history = store.history();
    let cps = history.checkpoints();
    let head_id = cps.head();
    let root_id = cps.root();

    let checkpoints: Vec<CheckpointInfo> = cps
        .iter()
        .map(|(id, ckpt)| {
            let entity_heads = ckpt
                .entity_heads
                .iter()
                .map(|(eid, snap)| (*eid, WireId::new(*snap)))
                .collect();
            CheckpointInfo {
                id: WireId::new(id),
                parent: ckpt.parent.map(WireId::new),
                children: ckpt.children.iter().copied().map(WireId::new).collect(),
                entity_heads,
                entity: ckpt.kind.entity(),
                kind: checkpoint_kind_tag(&ckpt.kind),
                label: ckpt.label.to_string(),
                timestamp_ms: timestamp_ms(ckpt.timestamp),
                raw_score: ckpt.raw_score,
                game_score: ckpt.game_score,
                filter_status: filter_status_wire(&ckpt.filter_status),
                // No committed checkpoint is ever tentative.
                tentative: false,
                pinned: cps.is_pinned(id),
                exclude_from_best: ckpt.exclude_from_best,
            }
        })
        .collect();

    HistorySection {
        checkpoints,
        checkpoint_head: Some(WireId::new(head_id)),
        checkpoint_root: Some(WireId::new(root_id)),
        best: cps.best().map(WireId::new),
        best_that_counts: cps.best_that_counts().map(WireId::new),
        topology_version: history.topology_version() as f64,
    }
}

/// Build the small `HistoryLiveUpdate` payload for the current head
/// (always the tentative when `ongoing == Active`; when Idle, the head
/// is the recently-stamped checkpoint).
fn project_history_live(history: &History) -> Option<HistoryLiveUpdate> {
    let head_id = history.checkpoints().head();
    let ckpt = history.checkpoint(head_id)?;
    Some(HistoryLiveUpdate {
        checkpoint_id: WireId::new(head_id),
        raw_score: ckpt.raw_score,
        game_score: ckpt.game_score,
        label: ckpt.label.to_string(),
        filter_status: filter_status_wire(&ckpt.filter_status),
    })
}

/// Outcome of a [`HistoryCommand`] dispatch — drives the per-frame
/// follow-up the dispatcher must run (republish to viso, mark dirty,
/// or nothing at all).
enum HistoryOutcome {
    /// Checkpoint head moved; rerun [`App::after_head_move`].
    HeadMoved,
    /// Curation flag changed (pin / unpin / exclude from best). No
    /// head move; just mark `ACTIONS` dirty so the GUI reflects it.
    Curated,
    /// The command was a no-op (e.g., undo at root). No follow-up.
    Noop,
}

/// Read the score for the *current composition node* (the open pending
/// edit when an action is in flight, else the committed head checkpoint),
/// projected through the active scoring mode. Following the composition
/// node keeps the displayed score on an in-flight action's streamed score
/// without ever reading the committed parent (G1: derive, don't store).
fn head_score(store: &Session) -> Option<f64> {
    let (raw, game) = store.current_composition_scores();
    // A loaded puzzle displays the foldit game score; the free-form
    // session displays the raw Rosetta score.
    if store.puzzle().is_some() {
        game
    } else {
        raw
    }
}

/// The `TextBubblePayload` for the active puzzle's current bubble, or
/// `None` when no puzzle is loaded, the puzzle has no tutorial sequence, or
/// the cursor has walked past the last bubble.
fn current_bubble_payload(puzzle: Option<&Puzzle>) -> Option<TextBubblePayload> {
    let puzzle = puzzle?;
    let cursor = puzzle.current_bubble?;
    puzzle.bubbles.as_ref()?.get(cursor).map(bubble_to_payload)
}

/// Move one freshly-loaded entity through the preview→promote pipeline
/// so it lands in history with an `AddEntity` checkpoint. Returns the
/// committed [`EntityId`].
///
/// Ambient (water / ion / solvent) and zero-residue entities — the
/// het-residue stubs the parser emits for cofactors / waters in
/// structure files — are kept as previews (transient) so viso still
/// renders them, but they DO NOT push a history checkpoint. They aren't
/// undoable from the user's perspective; pushing one `AddEntity` per
/// stub clutters the history (`1bfe` produced 3 root-level dots: one
/// `Loaded` + two `AddEntity` for chain A and a water).
fn load_entity_into_history(
    store: &mut Session,
    entity: molex::MoleculeEntity,
    name: String,
) -> Option<EntityId> {
    use molex::MoleculeType;
    let mol_type = entity.molecule_type();
    let is_ambient = matches!(
        mol_type,
        MoleculeType::Water | MoleculeType::Ion | MoleculeType::Solvent
    );
    let zero_residue = entity.residue_count() == 0;
    let id = store.insert_preview(entity, name.clone(), EntityOrigin::Loaded);
    if is_ambient || zero_residue {
        // Leave it transient: visible in viso, absent from history.
        return Some(id);
    }
    match store.promote_preview(
        id,
        CheckpointKind::AddEntity {
            entity: id,
            kind: mol_type,
        },
        None,
        None,
        std::borrow::Cow::Owned(format!("Loaded {name}")),
    ) {
        Ok(_) => Some(id),
        Err(e) => {
            log::error!("Failed to promote loaded entity '{name}': {e}");
            None
        }
    }
}

/// Overwrite the ongoing action's tentative payload from a streaming
/// assembly. `action_update` fans the closure across every lane the edit
/// locked, and each lane is rewritten only when the incoming assembly
/// carries a matching entity id — so a single-entity edit rewrites its one
/// lane and a multi-entity edit (post-Init normalization) rewrites each of
/// its lanes that the stream touched. Score fields are propagated when the
/// plugin embedded a total; per-residue / game scoring stay on their own
/// refresh path.
///
/// Returns `true` if at least one payload swap actually fired.
fn apply_streaming_assembly(
    store: &mut Session,
    incoming: &molex::Assembly,
    raw_score: Option<f64>,
    request_id: u64,
) -> bool {
    let mut applied = false;
    let res = store.action_update(request_id, raw_score, raw_score, None, |entity_mut| {
        if let Some(src) = incoming.entity(entity_mut.id()) {
            *entity_mut = src.clone();
            applied = true;
        }
    });
    if let Err(e) = res {
        log::trace!("action_update skipped: {e}");
        return false;
    }
    applied
}

/// Main application state — thin glue connecting the render engine,
/// plugin driver, document, and the two projectors. `App` also owns the
/// host-bound [`FrontendState`] mirror (so the load state-machine and
/// the GUI projection both live on the same side of the host seam) and
/// the `LoadingState` machine that drives the startup phases up to the
/// first-score `InSession` flip.
pub struct App {
    engine: Option<VisoEngine>,
    keybindings: KeyBindings,
    store: Session,
    plugin_driver: PluginDriver,
    render_projector: RenderProjector,
    gui_projector: GuiProjector,
    /// Host-provided filesystem / resource access. The only path through
    /// which foldit-core touches the filesystem outside puzzle loading.
    host: Box<dyn crate::HostResources>,
    /// Frontend mirror — written by the GUI consumer
    /// ([`GuiProjector::consume`]) at the end of each tick and drained by
    /// the host via [`Self::serialize_frontend_dirty`].
    frontend: FrontendState,
    /// App-lifetime lifecycle phase. `App` advances this through the
    /// startup phases and flips it to `InSession` at the first-score gate
    /// (`awaiting_initial_score` + `has_initial_score()`). Mirrored
    /// verbatim to the frontend via [`FrontendState::set_app_state`].
    app_state: LoadingState,
    /// Set after `load_initial_structure` returns; cleared in `tick`
    /// once the first plugin score lands. Mirrors the desktop runner's
    /// old field.
    awaiting_initial_score: bool,
    /// One-shot "push every GUI section once" signal. Raised on session
    /// birth (the Loading → InSession flip for the initial load, and at the
    /// end of each reload path) and consumed + cleared by `tick` on the next
    /// GUI-consumer pass, which projects a full `DirtyFlags::all()` populate.
    /// The incremental sections during a load still flow through the ordinary
    /// `SessionUpdate` batch; this catches the sections no batch variant
    /// carries (a free-form reload's puzzle-panel title, the post-load score /
    /// action catalog).
    needs_full_populate: bool,
    /// Commit-stamp correlation: each in-flight commit-time composition-score
    /// `request_id` → the committed checkpoint its reply stamps. The checkpoint
    /// is immutable, so its identity is stable until the reply lands. Cleared
    /// on orchestrator reinit (request ids restart at 1 there, so a stale
    /// entry could otherwise collide with a fresh edit id).
    #[cfg(not(target_arch = "wasm32"))]
    score_targets: std::collections::HashMap<u64, CheckpointId>,
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
    pending_pull_origin: Option<crate::pull_drag::PullRoute>,
}

/// Advance focus one step through `focusable`, wrapping back to
/// [`Focus::All`] after the last entity. `All` advances to the first
/// focusable entity (or stays `All` when none are focusable). The
/// Tab-cycle step, owned by foldit-core now that focus is session state;
/// `focusable` is viso's eligibility list (`engine.focusable_entities()`).
fn next_focus(current: Focus, focusable: &[EntityId]) -> Focus {
    match current {
        Focus::All => focusable
            .first()
            .map_or(Focus::All, |&id| Focus::Entity(id)),
        Focus::Entity(cur) => match focusable.iter().position(|&id| id == cur) {
            Some(i) if i + 1 < focusable.len() => Focus::Entity(focusable[i + 1]),
            _ => Focus::All,
        },
    }
}

/// App-residue inputs the GUI projection needs beyond the `Session`,
/// `VisoEngine`, and `PluginDriver`. Named explicitly (not `&App`) so the
/// projection's real dependencies are visible at the call site rather than
/// hidden behind a god-object borrow.
pub(crate) struct GuiContext<'a> {
    /// Host resource access — the view-preset directory listing for the
    /// `VIEW` section. Read only on `not(wasm)`.
    pub(crate) host: &'a dyn crate::HostResources,
}

impl GuiProjector {
    /// Project the live `Session` / `VisoEngine` / `PluginDriver` state into
    /// `frontend` — the third consumer of the `SessionUpdate` batch,
    /// alongside the render and plugin projectors.
    ///
    /// Unlike those two it reads several subsystems (the GUI mirrors score,
    /// selection, scene, history, puzzle, bubble, focus, view, loading), so
    /// it does not implement the two-input `SessionUpdateConsumer<Sink>`
    /// trait: that signature can express only one read input (`session`).
    /// Naming the extra inputs here — `engine`, `driver`, `ctx` — is what
    /// keeps this honest and out of the `&App` fake-abstraction trap.
    ///
    /// Per-section dirtiness is derived entirely from the drained `updates`
    /// batch — each `SessionUpdate` variant maps to the GUI sections it
    /// invalidates — plus a one-shot `full_populate` flag the tick raises on
    /// session birth (the Loading → InSession flip and every reload) to push
    /// every section once. There is no longer an App-side dirty residue: the
    /// mutations that used to raise flags at their App sites now produce the
    /// covering `SessionUpdate` variants, and those variants are mapped here.
    pub(crate) fn consume(
        &mut self,
        updates: &[SessionUpdate],
        full_populate: bool,
        session: &Session,
        engine: &VisoEngine,
        driver: &PluginDriver,
        ctx: &GuiContext<'_>,
        frontend: &mut FrontendState,
    ) {
        // FPS and selected count change every frame — always push them.
        frontend.set_fps(engine.fps());
        frontend.ui.selected_count = session.selection_total_count();

        let mut dirty = if full_populate {
            DirtyFlags::all()
        } else {
            DirtyFlags::empty()
        };
        for update in updates {
            dirty |= match update {
                SessionUpdate::ScoresChanged => DirtyFlags::SCORE,
                SessionUpdate::Edit { tentative: true } => DirtyFlags::SCENE,
                SessionUpdate::Edit { tentative: false } => {
                    DirtyFlags::SCENE | DirtyFlags::ACTIONS
                }
                SessionUpdate::HeadMoved => {
                    DirtyFlags::SCENE | DirtyFlags::SCORE | DirtyFlags::ACTIONS
                }
                SessionUpdate::PreviewAdded | SessionUpdate::PreviewDiscarded => {
                    DirtyFlags::SCENE | DirtyFlags::ACTIONS
                }
                SessionUpdate::ViewOptionsChanged => DirtyFlags::VIEW,
                SessionUpdate::SelectionChanged => DirtyFlags::SELECTION | DirtyFlags::ACTIONS,
                SessionUpdate::FocusChanged => DirtyFlags::SCENE | DirtyFlags::ACTIONS,
                SessionUpdate::BubbleChanged => DirtyFlags::TEXT_BUBBLE,
                SessionUpdate::PuzzleChanged => DirtyFlags::PUZZLE,
            };
        }

        if dirty.is_empty() {
            return;
        }

        // PUZZLE before SCORE: a fresh `set_puzzle_*` resets `complete=false`,
        // and then the score check below can latch victory in the same frame
        // without being overwritten.
        if dirty.contains(DirtyFlags::PUZZLE) {
            // The puzzle panel's title is the standalone session title,
            // which on a puzzle load equals the puzzle name.
            match session.puzzle() {
                Some(p) => frontend.set_puzzle_game(
                    p.id,
                    session.title().to_string(),
                    p.start_energy,
                    p.completion_energy,
                ),
                // The free-form session has no objective; the title is the
                // file-derived structure name.
                None => frontend.set_puzzle_scientist(session.title().to_string()),
            }
            // Bubble push on puzzle swap: render the cursor's current
            // bubble (always index 0 right after a puzzle load, since the
            // cursor starts there). Subsequent AdvanceBubble actions
            // re-push via the DirtyFlags::TEXT_BUBBLE arm below.
            frontend.set_text_bubble(current_bubble_payload(session.puzzle()));
        }
        if dirty.contains(DirtyFlags::TEXT_BUBBLE) {
            frontend.set_text_bubble(current_bubble_payload(session.puzzle()));
        }
        if dirty.contains(DirtyFlags::SCORE) {
            if let Some(score) = head_score(session) {
                frontend.set_score(score, false);
                // Victory check: with a puzzle loaded, latch it complete the
                // first time the score crosses the toml completion energy.
                // Higher game score = better fold (game-score formula
                // negates), so the comparison is `>=`.
                if let Some(p) = session.puzzle() {
                    if p.completion_energy > 0.0 && score >= p.completion_energy {
                        frontend.mark_puzzle_complete();
                    }
                }
            }
        }
        if dirty.contains(DirtyFlags::ACTIONS) {
            // Availability depends on focus + selection + lock state.
            // Source focus from the authoritative session (same as the
            // SCENE arm below), then hand the driver the selection + an
            // entity-type closure.
            let focus = match session.focus() {
                Focus::Entity(eid) => Some(eid),
                Focus::All => None,
            };
            let actions =
                driver.actions_catalog(focus, session.selection(), |id| session.entity_type(id));
            frontend.set_actions(actions);
        }
        if dirty.contains(DirtyFlags::VIEW) {
            // Source of truth is the session, not the engine: the engine is
            // a follower that the tick re-applies on `ViewOptionsChanged`.
            frontend.view.options =
                serde_json::to_value(session.view_options()).unwrap_or_default();

            // Schema is static — only set once
            if frontend.view.options_schema.is_null() {
                frontend.view.options_schema =
                    serde_json::to_value(viso::options::VisoOptions::json_schema())
                        .unwrap_or_default();
            }

            // The presets *list* is a disk/library read (App/host), not
            // session state, so it stays here.
            #[cfg(not(target_arch = "wasm32"))]
            {
                frontend.view.available_presets = ctx
                    .host
                    .view_presets_dir()
                    .map(viso::options::VisoOptions::list_presets)
                    .unwrap_or_default();
            }
            frontend.view.active_preset = session.active_preset().map(String::from);
        }
        if dirty.contains(DirtyFlags::SELECTION) {
            let entries: Vec<foldit_gui::EntitySelection> = session
                .selection()
                .iter()
                .map(|(eid, residues)| foldit_gui::EntitySelection {
                    entity_id: eid.raw(),
                    residues: residues.iter().copied().collect(),
                })
                .collect();
            frontend.set_selection(entries);
        }
        if dirty.contains(DirtyFlags::SCENE) {
            use molex::MoleculeType;
            let mut scene_entities = Vec::new();
            for (eid, _meta) in session.iter() {
                let Some(entity) = session.entity(eid) else {
                    continue;
                };
                let mol_str = match entity.molecule_type() {
                    MoleculeType::Protein => "protein",
                    MoleculeType::DNA => "dna",
                    MoleculeType::RNA => "rna",
                    MoleculeType::Ligand => "ligand",
                    MoleculeType::Ion => "ion",
                    MoleculeType::Water => "water",
                    MoleculeType::Lipid => "lipid",
                    MoleculeType::Cofactor => "cofactor",
                    MoleculeType::Solvent => "solvent",
                };
                scene_entities.push(foldit_gui::SceneEntityInfo {
                    entity_id: entity.id().raw(),
                    label: entity.label(),
                    molecule_type: mol_str.to_string(),
                    atom_count: entity.atom_count(),
                    residue_count: entity.residue_count(),
                });
            }
            frontend.set_scene_entities(scene_entities);
            let focused = match session.focus() {
                Focus::Entity(eid) => Some(eid.raw()),
                Focus::All => None,
            };
            frontend.set_focused_entity(focused);
        }

        // History push (two-channel):
        //   - topology bump → full `HistorySection`
        //   - live bump only → small `HistoryLiveUpdate` patch, with a
        //     50ms (20Hz) debounce so per-cycle Rosetta scores don't
        //     saturate the IPC. The final cycle on commit always lands
        //     because committing also bumps `topology_version`.
        let topology = session.history().topology_version();
        let live = session.history().live_version();
        let cursor = &mut self.history_sync;
        let topology_changed = cursor.last_topology != Some(topology);
        let live_changed = cursor.last_live != Some(live);

        if topology_changed {
            frontend.set_history(project_history(session));
            cursor.last_topology = Some(topology);
            cursor.last_live = Some(live);
            cursor.last_live_push_at = Some(Instant::now());
        } else if live_changed {
            let now = Instant::now();
            let debounced = cursor
                .last_live_push_at
                .map_or(false, |t| now.duration_since(t).as_millis() < 50);
            if !debounced {
                if let Some(update) = project_history_live(session.history()) {
                    frontend.set_history_live(update);
                    cursor.last_live = Some(live);
                    cursor.last_live_push_at = Some(now);
                }
            }
        }
    }
}

impl App {
    pub fn new(host: Box<dyn crate::HostResources>) -> Self {
        Self {
            engine: None,
            keybindings: {
                let mut kb = KeyBindings::default();
                // Focus is foldit-core session state now: neutralize viso's
                // Tab/Backquote focus bindings on this instance so viso no
                // longer owns focus. The core key paths intercept these keys
                // before any dispatch and drive `Session::set_focus` instead.
                kb.insert("Tab".to_string(), Box::new(|_: &mut VisoEngine| {}));
                kb.insert("Backquote".to_string(), Box::new(|_: &mut VisoEngine| {}));
                kb
            },
            store: Session::new(),
            plugin_driver: PluginDriver::new(),
            render_projector: RenderProjector::new(),
            gui_projector: GuiProjector::new(),
            host,
            frontend: FrontendState::new(),
            app_state: LoadingState::Initializing,
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
    /// the Loading → InSession transition.
    fn has_initial_score(&self) -> bool {
        head_score(&self.store).is_some()
    }

    /// Advance the App-lifetime phase and mirror it to the frontend
    /// transmit gate. `set_app_state` only marks the section dirty when
    /// the value actually changes, so re-setting the same phase is a
    /// no-op on the wire.
    fn set_loading_state(&mut self, state: LoadingState) {
        self.app_state = state;
        self.frontend.set_app_state(self.app_state);
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
                log::error!("Render error: {:?}", e);
            }
        }
    }

    // ── Backend update processing ──

    pub fn apply_backend_updates(&mut self) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let events = self.plugin_driver.drain_op_events();
            if events.is_empty() {
                return;
            }

            // Every branch below routes through the mutation funnel, which
            // emits the covering `SessionUpdate`: a mid-stream update emits
            // a tentative `Edit`, a commit emits `HeadMoved` (+ a deferred
            // `ScoresChanged` from the commit-stamp), an abort emits its own
            // update. The GUI consumer derives its dirty set from that batch,
            // so no explicit raise is needed here.
            for event in events {
                match event {
                    OpEvent::Update { token, assembly } => {
                        apply_streaming_assembly(&mut self.store, &assembly, None, token);
                    }
                    OpEvent::Commit { token, assembly } => {
                        if let Some(token) = token {
                            if apply_streaming_assembly(&mut self.store, &assembly, None, token) {
                                // Stream finished: commit the tentative so
                                // the partial result becomes a permanent
                                // undo entry, then score the committed
                                // union so the new checkpoint gets a
                                // correctly-attributed score even while a
                                // peer edit is still open.
                                match self.store.commit_action(token) {
                                    Ok(ckpt) => self.score_committed_checkpoint(ckpt),
                                    Err(e) => log::warn!("commit_action failed: {e}"),
                                }
                                // The edit's correlation id is now spent;
                                // drop any lingering composition target.
                                let _ = self.score_targets.remove(&token);
                            }
                        }
                    }
                    OpEvent::Abort { token, reason } => {
                        // Spontaneous failure: never commits; aborts
                        // exactly the edit this stream owns. A terminal
                        // with no open edit, or whose edit already
                        // committed, is a no-op.
                        if let Some(token) = token {
                            if self.store.is_pending(token) {
                                if let Err(e) = self.store.abort_action(token) {
                                    log::warn!("abort_action failed: {e}");
                                }
                            }
                        }
                        log::warn!("plugin op aborted: {reason}");
                    }
                }
            }
        }
    }

    /// Query every plugin's `score` op, merge totals into the head
    /// checkpoint (bumping `live_version` for the GuiProjector to pick
    /// up), and push per-residue scores directly to viso for
    /// color-by-score display modes. Off the `SessionUpdate` stream
    /// entirely: scores have two consumers (the GuiProjector via
    /// `HistorySyncCursor` and viso via a direct overlay push) and
    /// neither needs to ride the `SessionUpdate` stream.
    ///
    /// Synchronous (blocking) score poll. `tick` calls this each frame
    /// only until the first score lands, so the InSession gate
    /// flips promptly; once a score exists `tick` switches to the async
    /// path (`request_scores` + `poll_async_scores`). Dirty flags are set
    /// by `apply_score_reports` when a report actually applies.
    #[cfg(not(target_arch = "wasm32"))]
    fn poll_plugin_scores(&mut self) {
        if self.plugin_driver.orchestrator.is_none() {
            return;
        }
        self.refresh_scores();
    }

    /// Fan out the well-known `score` query across every plugin that
    /// registered it, merge totals into the head checkpoint, and push
    /// per-residue scores to the render engine for color-by-score modes.
    ///
    /// Called once at bootstrap (flips `has_initial_score()`, opening the
    /// loading gate) and again after every host-originated broadcast (so
    /// post-edit rescores update both the score widget and the residue
    /// colors).
    ///
    /// Today only Rosetta returns a non-trivial report. When more scorers
    /// come online the merge becomes app-wide -- the host stays generic
    /// either way.
    #[cfg(not(target_arch = "wasm32"))]
    fn refresh_scores(&mut self) {
        // Blocking score round-trip. Used only until the first score
        // lands, where a synchronous result keeps the InSession
        // flip deterministic. Once a score exists the caller switches to
        // `request_scores` + `poll_async_scores` so the render thread
        // never blocks on the worker.
        let reports = self.plugin_driver.collect_scores_blocking();
        self.apply_score_reports(reports);
    }

    /// Fire a non-blocking `score` query at every provider with no query
    /// already in flight. The reply lands on a stored receiver drained by
    /// [`Self::poll_async_scores`]; the render thread never blocks. One
    /// outstanding query per provider coalesces a fast pose stream
    /// against a slow scorer.
    #[cfg(not(target_arch = "wasm32"))]
    fn request_scores(&mut self) {
        self.plugin_driver.request_scores();
    }

    /// Drain whatever async `score` replies have arrived and apply them.
    /// Non-blocking; no-op when nothing is ready.
    #[cfg(not(target_arch = "wasm32"))]
    fn poll_async_scores(&mut self) {
        let reports = self.plugin_driver.poll_score_results();
        self.apply_score_reports(reports);
    }

    /// Merge score reports into the head checkpoint and push per-residue
    /// scores to viso. Shared tail of the blocking (bootstrap) and async
    /// (steady-state) score paths; no-op on an empty report set. The SCORE
    /// dirty flag is set here so both paths mark it exactly when a report
    /// actually applies.
    #[cfg(not(target_arch = "wasm32"))]
    fn apply_score_reports(
        &mut self,
        reports: std::collections::HashMap<String, foldit_runner::proto::plugin::ScoreReport>,
    ) {
        use std::collections::HashMap;

        if reports.is_empty() {
            return;
        }

        let mut total: Option<f64> = None;
        // entity_id -> Vec<(residue_index, score)>; merged across all
        // reporting plugins (later writers stack on top of earlier ones
        // for now -- when multiple plugins score per-residue we'll need a
        // merge strategy choice).
        let mut per_entity: HashMap<u32, Vec<(u32, f64)>> = HashMap::new();
        for (plugin_id, report) in &reports {
            if total.is_none() {
                total = Some(f64::from(report.total));
            }
            log::info!(
                "[App] score from {plugin_id}: total={} terms={} per_residue={}",
                report.total,
                report.terms.len(),
                report.per_residue.len()
            );
            accumulate_per_residue(&mut per_entity, report);
        }
        if let Some(raw) = total {
            // Whole-assembly score of the worker's live pose. With exactly
            // one edit open, the live pose IS that edit's composition (its
            // tentative + peers' committed heads), so the total is correctly
            // the edit's score → stamp the edit. With zero or >=2 edits open,
            // stamp the committed head; the >=2 case is transiently imperfect
            // for live display (each open edit keeps its last value) but exact
            // per-edit values still land at commit via the commit-stamp.
            let game = rosetta_raw_to_game(raw);
            match self.store.sole_pending_request_id() {
                Some(rid) => self.store.set_edit_scores(rid, Some(raw), Some(game)),
                None => self.store.set_head_scores(Some(raw), Some(game)),
            }
        }

        self.push_per_residue_to_viso(per_entity);
    }

    /// Push per-residue scores into the engine so Score / ScoreRelative
    /// color schemes have data. Each entity's score Vec is sized to its
    /// full residue count; missing residues default to 0.0 (the mid-palette
    /// stop in absolute mode, the lower quantile in relative mode -- close
    /// enough for a first-pass render). The push is unconditional: viso is
    /// the single owner of per-entity score state (it retains scores across
    /// `replace_assembly` and reconciles by id), so foldit-core keeps no
    /// shadow copy to dedup against. viso recolors on every push, but a
    /// score query only fires on a genuine geometry change (the steady-state
    /// poll gates on a non-score, non-selection update), and such a change
    /// generally moves the scores too, so a byte-identical re-push is rare.
    /// Shared by the whole-assembly and composition score paths.
    #[cfg(not(target_arch = "wasm32"))]
    fn push_per_residue_to_viso(
        &mut self,
        per_entity: std::collections::HashMap<u32, Vec<(u32, f64)>>,
    ) {
        use std::collections::HashMap;
        if per_entity.is_empty() {
            return;
        }
        // Borrow the two disjoint `self` fields the loop touches (the engine
        // sink + the last-pushed cache) so the dirty-check doesn't fight the
        // `&mut self.engine` borrow.
        let Some(engine) = self.engine.as_mut() else {
            return;
        };
        // Build (raw_entity_id -> residue_count) once via head_assembly so
        // we don't need a mut borrow on store to mint molex EntityIds.
        let head = self.store.head_assembly();
        let residue_counts: HashMap<u32, usize> = head
            .entities()
            .iter()
            .map(|e| (e.id().raw(), e.residue_count()))
            .collect();
        for (entity_id, mut entries) in per_entity {
            let Some(&residue_count) = residue_counts.get(&entity_id) else {
                log::warn!(
                    "[App] per-residue scores arrived for unknown entity \
                     {entity_id} (host has entities {:?})",
                    residue_counts.keys().collect::<Vec<_>>()
                );
                continue;
            };
            let mut scores = vec![0.0_f64; residue_count];
            entries.sort_unstable_by_key(|(idx, _)| *idx);
            let entry_count = entries.len();
            for (idx, val) in entries {
                let i = idx as usize;
                if i < scores.len() {
                    scores[i] = val;
                }
            }
            log::info!(
                "[App] applied {entry_count} per-residue scores to viso entity \
                 {entity_id} (residue_count={residue_count})"
            );
            engine.set_per_residue_scores(entity_id, Some(scores));
        }
    }

    /// Fire a composition score for the committed union of `ckpt_id` under a
    /// fresh `request_id`, routing the reply to stamp that (now-immutable)
    /// checkpoint. Called right after a user-action commit so the new
    /// checkpoint gets a correctly-attributed score even when a peer edit is
    /// still open (so the idle whole-assembly path is not the one running).
    #[cfg(not(target_arch = "wasm32"))]
    fn score_committed_checkpoint(&mut self, ckpt_id: CheckpointId) {
        let Some(rid) = self.plugin_driver.alloc_request_id() else {
            return;
        };
        let Some(assembly) = self.store.checkpoint_assembly(ckpt_id) else {
            return;
        };
        let Ok(bytes) = molex::ops::wire::serialize_assembly(&assembly) else {
            log::warn!("[App] commit-stamp serialize failed for checkpoint {ckpt_id:?}");
            return;
        };
        self.plugin_driver.score_composition(bytes, rid);
        let _ = self.score_targets.insert(rid, ckpt_id);
    }

    /// Drain composition-score replies and stamp each commit-time checkpoint
    /// via the `request_id` map (`set_checkpoint_scores`). A `request_id`
    /// absent from the map is just "not a commit-stamp" and needs no action.
    /// Per-residue scores still flow to viso for every reply. The raw REU →
    /// game-points map applies here too, so composition scores never display
    /// raw REU.
    #[cfg(not(target_arch = "wasm32"))]
    fn poll_composition_scores(&mut self) {
        let replies = self.plugin_driver.poll_composition_scores();
        if replies.is_empty() {
            return;
        }
        for (rid, report) in replies {
            let raw = f64::from(report.total);
            let game = rosetta_raw_to_game(raw);
            if let Some(ckpt_id) = self.score_targets.get(&rid).copied() {
                self.store.set_checkpoint_scores(ckpt_id, Some(raw), Some(game));
                let _ = self.score_targets.remove(&rid);
            }
            let mut per_entity = std::collections::HashMap::new();
            accumulate_per_residue(&mut per_entity, &report);
            self.push_per_residue_to_viso(per_entity);
        }
    }

    // ── Keybinding dispatch ──

    /// Catalog hotkey fallback. Runs only after a viso built-in
    /// `handle_key_press` *miss*, so built-ins always win. On a match
    /// against a plugin manifest `[[buttons]]` hotkey, dispatch the op
    /// through the same `handle_dispatch_op` sink a button click uses;
    /// that sink sources the live focus + selection itself, so the
    /// hotkey op runs on the same target a button click would. Returns
    /// true if an op was dispatched.
    #[cfg(not(target_arch = "wasm32"))]
    fn try_hotkey_dispatch(&mut self, key_str: &str) -> bool {
        let op_id = self
            .plugin_driver
            .hotkey_to_op(key_str)
            .map(|(_plugin_id, op_id)| op_id);
        let Some(op_id) = op_id else { return false };
        log::info!("hotkey {key_str:?} -> dispatch plugin op {op_id:?}");
        self.handle_dispatch_op(foldit_gui::OpDispatch {
            op_id,
            focused_entity_id: None,
            params: std::collections::HashMap::new(),
        });
        true
    }

    #[cfg(target_arch = "wasm32")]
    fn try_hotkey_dispatch(&mut self, _key_str: &str) -> bool {
        false
    }

    /// Dispatch a keybinding by physical-key string ("KeyR", "KeyT",
    /// "Tab", ...). Hosts convert their native keycode to this string
    /// before calling (winit: `format!("{key:?}")`; web: DOM `code`).
    /// On a viso built-in miss, falls through to the plugin hotkey
    /// catalog (built-ins win by being checked first).
    pub fn handle_keybinding(&mut self, key_str: &str) -> bool {
        // foldit-specific overrides: trajectory load-on-demand, ESC =
        // cancel-in-flight-op, and the dropped auto-rotate binding.
        // These short-circuit the generic viso keybinding dispatch.
        match key_str {
            "KeyT" => {
                let Some(engine) = &mut self.engine else {
                    return false;
                };
                if engine.has_trajectory() {
                    engine.toggle_trajectory();
                } else if let Some(path) = trajectory_path_from_args() {
                    engine.load_trajectory(std::path::Path::new(&path));
                } else {
                    log::info!("No trajectory loaded. Pass --trajectory <path.dcd> to load one.");
                }
                return true;
            }
            "Escape" => {
                // ESC is cancel-only: drop in-flight streams + the
                // current operation's previews. It never clears the
                // selection (which is ambient); the only selection
                // clearer is the empty-background click.
                #[cfg(not(target_arch = "wasm32"))]
                self.plugin_driver.cancel_all_active_streams();
                self.cancel_operations();
                return true;
            }
            // Auto-rotate keybinding is intentionally dropped in foldit.
            "KeyR" => return true,
            _ => {}
        }

        let Some(engine) = &mut self.engine else {
            return false;
        };

        // Focus is foldit-core session state: intercept the focus gestures
        // before viso's keybinding table and mutate the session. The tick's
        // `FocusChanged` reaction pushes viso's camera mirror, reframes, and
        // raises the projector dirty (the catalog re-projects because per-op
        // availability is focus-dependent).
        if matches!(key_str, "Tab" | "Backquote") {
            let next = match key_str {
                "Tab" => next_focus(self.store.focus(), &engine.focusable_entities()),
                _ => Focus::All,
            };
            self.store.set_focus(next);
            log::info!(
                "Focus: {}",
                render_projector::focus_description(&self.store, &self.store.focus())
            );
            return true;
        }

        if !self.keybindings.dispatch(key_str, engine) {
            return self.try_hotkey_dispatch(key_str);
        }
        true
    }

    /// Cancel the in-flight operation: drop any in-progress preview
    /// entities, republish, and flag the GUI dirty. Selection is a
    /// separate concept (see `clear_selection`); cancelling an operation
    /// does not touch it. Stream lock release + commit live in
    /// `apply_backend_updates`' terminal arms; doing them here races a
    /// follow-up dispatch that's quick enough to slip in before the
    /// terminal drains. Lives on `App` so the `RenderProjector` stays a
    /// field touched only inside App methods (the coordination
    /// boundary), never threaded as a parameter.
    fn cancel_operations(&mut self) {
        if self.engine.is_none() {
            return;
        }
        log::info!("Cancelling current operation");
        let preview_ids: Vec<EntityId> = self.store.preview_ids().collect();
        if !preview_ids.is_empty() {
            for id in &preview_ids {
                self.store.remove_preview(*id);
            }
            // PreviewDiscarded rides the `SessionUpdate` stream — the next tick's render
            // projector republishes and the GUI consumer derives SCENE +
            // ACTIONS dirty from the same batch.
            log::info!("Removed {} in-progress preview entities", preview_ids.len());
        }
    }

    // ── Viewport input (from webview) ──

    pub fn handle_viewport_input(&mut self, input: foldit_gui::ViewportInput) {
        use foldit_gui::ViewportInput;

        // Pull-drag interception runs ahead of viso's regular input
        // routing so an active drag suppresses camera rotation/pan.
        // The pull target is locked at button-down, not at the move:
        // `PointerDown` resolves the pull route at the down-cursor and
        // stores it in `pending_pull_origin` (`None` when the down-target
        // is empty / non-pullable). A left-press+release with no move
        // falls through to viso as a residue selection; a press that
        // resolved to a route opens the pull on the first move with the
        // button still held, anchored to the down-target regardless of
        // where the cursor has since wandered. `mouse_pressed()` is viso's
        // own press bit, set by the preceding PointerDown.
        #[cfg(not(target_arch = "wasm32"))]
        {
            match &input {
                ViewportInput::PointerMove { x, y, .. }
                    if self
                        .engine
                        .as_ref()
                        .is_some_and(viso::VisoEngine::mouse_pressed)
                        && !self.plugin_driver.has_active_pull_drag()
                        && self.pending_pull_origin.is_some() =>
                {
                    // The pull intent was locked at button-down; the move
                    // only supplies the drag endpoint. Take the route so
                    // this gesture makes at most one start attempt — a
                    // failed stream start falls through to camera for the
                    // rest of the drag rather than retrying mid-gesture.
                    let route = self
                        .pending_pull_origin
                        .take()
                        .expect("guard guarantees Some");
                    if self.begin_pull_drag_from_route(route, *x, *y) {
                        // viso recorded the press; drop its mouse
                        // state so the now-suppressed pointer-up
                        // can't fire a stray click → selection.
                        if let Some(engine) = self.engine.as_mut() {
                            engine.release_mouse_state();
                        }
                        self.update_pull_drag(*x, *y);
                        self.finalize_viewport_input();
                        return;
                    }
                }
                ViewportInput::PointerMove { x, y, .. }
                    if self.plugin_driver.has_active_pull_drag() =>
                {
                    self.update_pull_drag(*x, *y);
                    self.finalize_viewport_input();
                    return;
                }
                ViewportInput::PointerUp { .. }
                    if self.plugin_driver.has_active_pull_drag() =>
                {
                    self.end_pull_drag();
                    self.finalize_viewport_input();
                    return;
                }
                _ => {}
            }
        }

        // Hotkey resolved in the `Key` arm below via a disjoint field
        // borrow (`self.plugin_driver`, not `self.engine`); the actual
        // dispatch is deferred to after the match so the `engine`
        // borrow is released before `handle_dispatch_op` takes
        // `&mut self`.
        #[cfg(not(target_arch = "wasm32"))]
        let mut pending_hotkey_op: Option<String> = None;
        // ESC routing needs `&mut self`, but `engine` is borrowed for
        // the rest of the match and used again by
        // `update_all_visualizations` after it. Defer past that last
        // engine use, mirroring the `pending_hotkey_op` deferral. ESC is
        // cancel-only — it never touches the selection — so the deferred
        // block is unconditional and needs no live state read.
        let mut pending_escape = false;

        let Some(engine) = &mut self.engine else {
            return;
        };

        // `Some` only if a left-button release classified as a click;
        // deferred so the selection mutations below run after the
        // `engine` borrow ends.
        let mut pending_click: Option<ClickEvent> = None;

        match input {
            ViewportInput::PointerDown {
                x,
                y,
                button,
                shift,
                ..
            } => {
                let viso_button = match button {
                    0 => viso::MouseButton::Left,
                    2 => viso::MouseButton::Right,
                    1 => viso::MouseButton::Middle,
                    _ => return,
                };
                engine.feed_modifiers(shift);
                engine.set_cursor_pos(x, y);
                engine.feed_pointer_motion(x, y);
                let _ = engine.feed_pointer_button(viso_button, true);
                // Lock the pull intent at the down-target. The engine
                // cursor was just fed to (x, y), so resolving the route
                // here captures what is under the press; a later move
                // can only supply the drag endpoint, never re-pick the
                // target. Left button only — right/middle are camera.
                #[cfg(not(target_arch = "wasm32"))]
                {
                    self.pending_pull_origin = if button == 0 {
                        Self::resolve_pull_route(engine, &self.store, x, y)
                    } else {
                        None
                    };
                }
            }
            ViewportInput::PointerUp {
                x,
                y,
                button,
                shift,
                ..
            } => {
                let viso_button = match button {
                    0 => viso::MouseButton::Left,
                    2 => viso::MouseButton::Right,
                    1 => viso::MouseButton::Middle,
                    _ => return,
                };
                engine.feed_modifiers(shift);
                engine.set_cursor_pos(x, y);
                engine.feed_pointer_motion(x, y);
                pending_click = engine.feed_pointer_button(viso_button, false);
                // Gesture over: a pull that started already took the
                // route (it's `None`); a click / camera-rotate gesture
                // that never pulled drops its stored origin here.
                #[cfg(not(target_arch = "wasm32"))]
                {
                    self.pending_pull_origin = None;
                }
            }
            ViewportInput::PointerMove { x, y, shift, .. } => {
                engine.feed_modifiers(shift);
                engine.set_cursor_pos(x, y);
                engine.feed_pointer_motion(x, y);
            }
            ViewportInput::Scroll { delta } => {
                engine.feed_scroll(delta);
            }
            ViewportInput::Key { code, pressed } => {
                if pressed {
                    // foldit-specific overrides land first; viso's
                    // generic table picks up the rest.
                    match code.as_str() {
                        // Drop viso's R-binding for turntable auto-rotate;
                        // we don't expose a rotate keybinding in foldit.
                        "KeyR" => {}
                        "KeyT" => {
                            if engine.has_trajectory() {
                                engine.toggle_trajectory();
                            } else if let Some(path) = trajectory_path_from_args() {
                                engine.load_trajectory(std::path::Path::new(&path));
                            }
                        }
                        "Escape" => {
                            // ESC cancels the in-flight action only; it
                            // never clears the selection. Resolved in the
                            // deferred block below, past the last `engine`
                            // use. Mirrors the `handle_keybinding` ESC arm.
                            pending_escape = true;
                        }
                        // Focus is foldit-core session state: mutate the
                        // session here (disjoint `self.store` borrow). The
                        // tick's `FocusChanged` reaction pushes viso's camera
                        // mirror, reframes, and raises the projector dirty.
                        "Tab" => {
                            let next =
                                next_focus(self.store.focus(), &engine.focusable_entities());
                            self.store.set_focus(next);
                        }
                        "Backquote" => {
                            self.store.set_focus(Focus::All);
                        }
                        other => {
                            if !self.keybindings.dispatch(other, engine) {
                                // No viso built-in claims this key — resolve it
                                // against the plugin hotkey catalog. Disjoint
                                // field borrow (`self.plugin_driver`) so it
                                // coexists with the live `engine` borrow;
                                // dispatch is deferred to after the match.
                                #[cfg(not(target_arch = "wasm32"))]
                                {
                                    pending_hotkey_op = self
                                        .plugin_driver
                                        .hotkey_to_op(other)
                                        .map(|(_plugin_id, op_id)| op_id);
                                    if pending_hotkey_op.is_none() {
                                        log::debug!("Unhandled key code from frontend: {other}");
                                    }
                                }
                                #[cfg(target_arch = "wasm32")]
                                log::debug!("Unhandled key code from frontend: {other}");
                            }
                        }
                    }
                }
            }
            ViewportInput::Resize { .. } => {
                // Ignored: JS sends CSS pixels (logical) which are wrong on HiDPI.
            }
        }

        // Update drag/pull/band visualizations after input
        #[cfg(not(target_arch = "wasm32"))]
        let pull = self.plugin_driver.pull_drag_pull_info();
        #[cfg(target_arch = "wasm32")]
        let pull: Option<viso::PullInfo> = None;
        update_all_visualizations(engine, pull);

        // `engine`'s last use was above — `&mut self` is free again, so
        // the deferred actions below can run. Apply any pending click (a
        // left-release that classified as a click) to the selection; the
        // empty-background case clears it, a residue hit selects.
        if let Some(click) = pending_click {
            self.apply_click_to_selection(&click);
        }

        if pending_escape {
            // ESC is cancel-only: drop in-flight streams + the current
            // operation's previews. The selection is ambient and left
            // untouched (the only selection clearer is the empty-click
            // path in `apply_click_to_selection`).
            #[cfg(not(target_arch = "wasm32"))]
            self.plugin_driver.cancel_all_active_streams();
            self.cancel_operations();
        }

        // A hotkey resolved in the `Key` arm dispatches through the same
        // sink a button click uses (item 78); built-ins already won by
        // `handle_key_press` being checked first.
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(op_id) = pending_hotkey_op {
            log::info!("hotkey -> dispatch plugin op {op_id:?}");
            self.handle_dispatch_op(foldit_gui::OpDispatch {
                op_id,
                focused_entity_id: None,
                params: std::collections::HashMap::new(),
            });
        }
    }

    /// Called after the pull-drag interception path. Mirrors the
    /// trailing visualization update the regular `handle_viewport_input`
    /// flow does (the `SessionUpdate` drain itself is tick-driven now).
    /// Pre-snapshots the pull info so the engine borrow doesn't overlap
    /// with the live pull-drag state held in the plugin driver.
    #[cfg(not(target_arch = "wasm32"))]
    fn finalize_viewport_input(&mut self) {
        let pull = self.plugin_driver.pull_drag_pull_info();
        if let Some(engine) = self.engine.as_mut() {
            update_all_visualizations(engine, pull);
        }
    }

    /// Classify the pick under `(x, y)` into a pull route, or `None` if
    /// the target is empty / non-pullable (non-protein entity, hydrogen
    /// atom, no atom under the cursor). Pure resolution: reads the engine
    /// pick + store but mutates nothing, so it can run at `PointerDown`
    /// against the just-fed down-cursor to lock the pull's anchor. Takes
    /// `engine` + `store` as borrows rather than `&self` so the caller can
    /// invoke it while holding a disjoint `&mut self.engine` borrow.
    #[cfg(not(target_arch = "wasm32"))]
    fn resolve_pull_route(
        engine: &viso::VisoEngine,
        store: &Session,
        x: f32,
        y: f32,
    ) -> Option<crate::pull_drag::PullRoute> {
        match engine.hovered_target() {
            viso::PickTarget::Atom {
                entity_id,
                atom_idx,
            } => crate::pull_drag::route_atom_pick(store, entity_id, atom_idx),
            viso::PickTarget::Residue(flat) => {
                engine.picked_residue_atom(flat, (x, y)).and_then(|picked| {
                    let molex_id = store.ids().find(|id| id.raw() == picked.entity_id)?;
                    crate::pull_drag::route_residue_pick(
                        store,
                        flat,
                        &picked.atom_name,
                        molex_id,
                        picked.local_residue,
                    )
                })
            }
            viso::PickTarget::None => None,
        }
    }

    /// Open a pull-drag stream from a pre-resolved `route` (locked at
    /// button-down) with `(x, y)` as the current drag endpoint: dispatch
    /// the matching pull op-id, open the history edit, and install drag
    /// state. Returns true if the stream started (so the caller suppresses
    /// the regular viso input flow), false if `start_stream` failed (the
    /// gesture then falls through to camera handling).
    #[cfg(not(target_arch = "wasm32"))]
    fn begin_pull_drag_from_route(
        &mut self,
        route: crate::pull_drag::PullRoute,
        x: f32,
        y: f32,
    ) -> bool {
        let pull_info = crate::pull_drag::build_pull_info(&route, (x, y));

        let store = &self.store;
        let intent = StreamStartIntent {
            op_id: route.op_id,
            focused_entity: route.entity_id,
            residue_in_entity: route.residue_in_entity,
            atom_name: route.atom_name.clone(),
        };
        let (rid, plugin_id) =
            match self.plugin_driver.start_stream(intent, |id| store.entity_type(id)) {
                Ok(v) => v,
                Err(e) => {
                    log::warn!(
                        "begin_pull_drag_from_route: start_stream {:?} failed: {e:?}",
                        route.op_id,
                    );
                    return false;
                }
            };

        // History side-effect — same shape as button-driven dispatch
        // so the drag's eventual commit_action lands as a regular
        // PluginOp entry. Failure is non-fatal (commit_action becomes
        // a no-op on an idle store).
        let action_entity = self
            .store
            .ids()
            .find(|id| id.raw() == route.entity_id.raw());
        if let Some(entity) = action_entity {
            let kind = CheckpointKind::PluginOp {
                plugin_id: plugin_id.clone(),
                op_id: String::from(route.op_id),
                display: String::from("Pull"),
            };
            // Open the edit under the dispatch's request_id; the stream
            // table is keyed by the same id, so the terminal commit lands
            // on this edit.
            if let Err(e) =
                self.store.begin_action([entity], kind, String::from("Pull"), rid)
            {
                log::trace!("begin_pull_drag_from_route: begin_action skipped: {e}");
            }
        }

        self.plugin_driver.set_pull_drag(crate::pull_drag::PullDrag {
            request_id: rid,
            plugin_id,
            pull_info,
        });
        true
    }

    /// Pointer-move during an active drag: re-resolve the world-space
    /// drag target through the camera, and push a single-key
    /// `endpoint` Vec3 update to the running stream. Also refreshes
    /// `pull_info.screen_target` so the next visualization pass moves
    /// the cone tip with the cursor.
    #[cfg(not(target_arch = "wasm32"))]
    fn update_pull_drag(&mut self, x: f32, y: f32) {
        let Some(drag) = self.plugin_driver.pull_drag_mut() else {
            return;
        };
        drag.pull_info.screen_target = (x, y);
        let (residue, atom_name, plugin_id, request_id) = (
            drag.pull_info.atom.residue,
            drag.pull_info.atom.atom_name.clone(),
            drag.plugin_id.clone(),
            drag.request_id,
        );

        let Some(engine) = self.engine.as_ref() else {
            return;
        };
        let Some(atom_pos) = engine.resolve_atom_position(residue, &atom_name) else {
            return;
        };
        let target = engine.screen_to_world_at_depth(glam::Vec2::new(x, y), atom_pos);

        self.plugin_driver.update_stream(request_id, &plugin_id, target);
    }

    /// Pointer-up (or any cancel signal): tear down the drag state
    /// and ask the orchestrator to cancel the stream. The stream's
    /// terminal `PluginUpdate::Cancelled` flows through
    /// `apply_backend_updates` → `commit_action`, so the partial pull
    /// becomes a permanent undo entry.
    #[cfg(not(target_arch = "wasm32"))]
    fn end_pull_drag(&mut self) {
        let Some(drag) = self.plugin_driver.take_pull_drag() else {
            return;
        };
        self.plugin_driver.end_stream(drag.request_id, &drag.plugin_id);
    }

    /// Dispatch a plugin op by op-id. Resolves the op against the
    /// orchestrator's `PluginRegistry` to pick Invoke vs Start_stream;
    /// builds a `DispatchContext` from the GUI-provided focus and the
    /// authoritative in-core `App.selection`. Op-ids unknown to the
    /// registry are logged and dropped (the catalog couldn't have
    /// surfaced them, so this is either a stale GUI cache or a
    /// misrouted message).
    pub fn handle_dispatch_op(&mut self, op: foldit_gui::OpDispatch) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            // Drain pending terminals so rapid follow-up dispatches
            // see released locks.
            self.apply_backend_updates();

            // Source the focused entity authoritatively from the session's
            // current focus, not the GUI-supplied `op.focused_entity_id`
            // (which the hotkey paths leave as None). This makes every
            // dispatch path -- button or hotkey -- carry the live focus to
            // the worker, paired with the authoritative `App.selection`
            // read into the intent below. Raw gui-wire id (u32 widened to
            // u64), the shape `DispatchIntent` expects.
            let focused_entity_id: Option<u64> = match self.store.focus() {
                Focus::Entity(eid) => Some(eid.raw() as u64),
                Focus::All => None,
            };

            let Some(orch) = self.plugin_driver.orchestrator.as_mut() else {
                log::warn!(
                    "handle_dispatch_op({:?}): orchestrator not initialized",
                    op.op_id
                );
                return;
            };

            let Some(cached) = orch.plugin_registry().get_op(&op.op_id).cloned() else {
                log::warn!("handle_dispatch_op: op-id {:?} not in registry", op.op_id);
                return;
            };

            // `plugin_id` is needed below for `begin_action`; it's a plain
            // `String` field off `cached`, so reading it here names no
            // orchestrator type.
            let plugin_id = cached.plugin_id.clone();

            // Drop the orchestrator borrow before reaching back into
            // `self.plugin_driver` for the catalog label + dispatch.
            let _ = orch;

            // Resolve the display label from the manifest catalog. Falls
            // back to the op id when the op isn't surfaced as a button
            // (the dispatcher still routes; the history entry just shows
            // the op id).
            let display = self
                .plugin_driver
                .op_display(&plugin_id, &op.op_id)
                .unwrap_or_else(|| op.op_id.clone());
            // Hand the driver a core-shaped intent: the selection flatten,
            // param conversion, and `DispatchContext` build all live behind
            // `dispatch_op` now, so this path names no orchestrator type.
            let intent = DispatchIntent {
                selection: self.store.selection().clone(),
                focused_entity_id,
                op_id: op.op_id.clone(),
                params: op.params,
            };
            // Hoist a shared borrow of the store so the lookup closure
            // can capture it alongside the upcoming `&mut self.plugin_driver`
            // call (disjoint field paths).
            let store = &self.store;
            let dispatch_outcome =
                self.plugin_driver
                    .dispatch_op(intent, plugin_id.clone(), |id| {
                        store.entity_type(id)
                    });

            // The dispatch allocated the id the edit and the stream table
            // both key on, and resolved the entity set the op operates on.
            // Pull both from the successful outcome; the edit opens over the
            // whole resolved set (a whole-pose op moves every entity, so a
            // single-entity edit would drop every other entity's result and
            // commit a geometrically inconsistent pose). Filter to entities
            // with a committed lane — `begin_action` forks each lane from its
            // current head, and transient stubs (ambient / zero-residue) have
            // none — mirroring the post-Init normalization path.
            let lanes: Option<Vec<EntityId>> = match &dispatch_outcome {
                Ok(OpOutcome::Stream { scope, .. })
                | Ok(OpOutcome::Invoke { scope, .. }) => {
                    Some(self.lanes_for_scope(scope))
                }
                Err(_) => None,
            };
            let dispatch_id = match &dispatch_outcome {
                Ok(OpOutcome::Stream { request_id, .. })
                | Ok(OpOutcome::Invoke { request_id, .. }) => Some(*request_id),
                Err(_) => None,
            };

            // Open the edit under the dispatch id over the resolved lane set.
            // Skipped on dispatch failure (any open tentative belongs to a
            // prior op) or when the resolved set has no editable lane.
            let edit_token = dispatch_id.zip(lanes).and_then(|(request_id, lanes)| {
                if lanes.is_empty() {
                    return None;
                }
                let kind = CheckpointKind::PluginOp {
                    plugin_id: plugin_id.clone(),
                    op_id: op.op_id.clone(),
                    display: display.clone(),
                };
                match self.store.begin_action(lanes, kind, display.clone(), request_id) {
                    Ok(()) => Some(request_id),
                    Err(e) => {
                        log::trace!(
                            "handle_dispatch_op({:?}): begin_action skipped: {e}",
                            op.op_id
                        );
                        None
                    }
                }
            });

            match dispatch_outcome {
                Ok(OpOutcome::Stream { .. }) => {
                    // The stream table entry (inserted by
                    // `PluginDriver::dispatch_op`) and the edit are keyed
                    // by the same dispatch id; the terminal arm commits /
                    // aborts via that id. Nothing to reconcile here.
                }
                Ok(OpOutcome::Invoke { bytes, .. }) => {
                    self.apply_invoke_result(&bytes, edit_token);
                }
                Err(DispatchError::EntityLocked { entity }) => {
                    // Advisory refusal: the target entity is busy with
                    // another op. No edit was begun (gated on `is_ok`), so
                    // there is nothing to open or roll back.
                    log::warn!(
                        "handle_dispatch_op({:?}): dispatch refused, entity {entity} locked",
                        op.op_id
                    );
                }
                Err(DispatchError::BackendBusy { plugin_id }) => {
                    // Advisory refusal: the plugin's backend worker is
                    // already running an op. No edit was begun (gated on
                    // `is_ok`), so there is nothing to open or roll back.
                    log::info!("dispatch refused: backend {plugin_id} busy");
                }
                Err(DispatchError::Failed(s)) => {
                    log::error!("handle_dispatch_op({:?}): dispatch failed: {s}", op.op_id);
                }
            }
            // GUI dirty is derived from the batch: an Invoke commits
            // (HeadMoved → SCENE | SCORE | ACTIONS) and a Stream's frames
            // emit tentative Edits, then a HeadMoved at commit. A dispatch
            // changes neither focus nor selection, so the action catalog
            // (which depends only on those) is unchanged; no ACTIONS push
            // is owed at dispatch time.
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = op;
        }
    }

    /// Resolve a dispatch's [`EditScope`] into the concrete set of lanes the
    /// edit opens over. A whole-pose op (`AllEntities`) spans every committed
    /// entity; an entity-scoped op spans its resolved set. Either way the
    /// result is filtered to entities that hold a committed lane — the only
    /// ones `begin_action` can fork a tentative from — matching the post-Init
    /// normalization path's lane filter. Transient stubs (ambient /
    /// zero-residue entities) drop out silently.
    #[cfg(not(target_arch = "wasm32"))]
    fn lanes_for_scope(&self, scope: &EditScope) -> Vec<EntityId> {
        let has_lane = |id: &EntityId| self.store.history().lane(*id).is_some();
        match scope {
            EditScope::AllEntities => self.store.ids().filter(has_lane).collect(),
            EditScope::Entities(set) => {
                set.iter().copied().filter(has_lane).collect()
            }
        }
    }

    /// Apply the assembly bytes returned by a one-shot `dispatch_invoke`
    /// to the ongoing tentative and commit it. Mirrors the Stream-side
    /// `Final` path; called from `handle_dispatch_op` for `OpKind::Invoke`.
    /// The transition is inferred from the prior-vs-result structural
    /// delta and queued on the locked entity so the next tick's
    /// render-projector publish eases the result in rather than snapping.
    #[cfg(not(target_arch = "wasm32"))]
    fn apply_invoke_result(&mut self, bytes: &[u8], edit_token: Option<u64>) {
        let Some(token) = edit_token else {
            // No edit was begun for this invoke (begin skipped), so there
            // is nothing to apply into or commit.
            return;
        };
        let assembly = match molex::ops::wire::deserialize_assembly(bytes) {
            Ok(a) => a,
            Err(e) => {
                log::warn!("dispatch_invoke: decode failed: {e:?}");
                if self.store.is_pending(token) {
                    let _ = self.store.commit_action(token);
                }
                return;
            }
        };
        let applied = apply_streaming_assembly(&mut self.store, &assembly, None, token);
        if applied {
            match self.store.commit_action(token) {
                Ok(ckpt) => self.score_committed_checkpoint(ckpt),
                Err(e) => log::warn!("dispatch_invoke: commit_action failed: {e}"),
            }
            // `commit_action` emits `HeadMoved`, from which the GUI consumer
            // derives SCENE (+ SCORE + ACTIONS); no explicit raise is owed.
        } else if self.store.is_pending(token) {
            // Nothing matched (e.g. plugin returned an empty / unrelated
            // assembly): drop the tentative.
            let _ = self.store.commit_action(token);
        }
        // The edit's correlation id is spent; drop any lingering target.
        let _ = self.score_targets.remove(&token);
    }

    pub fn handle_app_command(&mut self, command: foldit_gui::AppCommand) {
        use foldit_gui::AppCommand;

        // History-side commands take &mut self (no engine borrow held).
        if let AppCommand::History { cmd } = command {
            self.run_history_command(cmd);
            return;
        }

        // Bubble cursor advance is engine-independent.
        if let AppCommand::AdvanceBubble { back } = command {
            self.advance_bubble(back);
            return;
        }

        if self.engine.is_none() {
            return;
        }

        // Engine borrow is taken per-arm now (LoadStructure / LoadPuzzle
        // need to release the borrow before `self.tick(0.0)`, which is
        // how the render projector republishes after a load).
        match command {
            AppCommand::LoadStructure { path } => self.handle_load_structure(path),
            AppCommand::LoadPuzzle { puzzle_id } => self.handle_load_puzzle(puzzle_id),
            AppCommand::SetViewOptions { options } => {
                // The session is the source of truth: store the options and
                // let the tick apply them to the engine (+ raise VIEW) off
                // the emitted `ViewOptionsChanged`.
                match serde_json::from_value::<viso::options::VisoOptions>(options) {
                    Ok(opts) => self.store.set_view_options(opts),
                    Err(e) => log::error!("Failed to deserialize view options: {}", e),
                }
            }
            AppCommand::LoadViewPreset { name } => {
                #[cfg(not(target_arch = "wasm32"))]
                if let Some(dir) = self.host.view_presets_dir() {
                    if let Some(engine) = self.engine.as_mut() {
                        // Use the engine to read the preset file off disk,
                        // then record it as the active preset on the session
                        // (the source of truth). The tick re-applies the
                        // options to the engine + raises VIEW.
                        engine.load_preset(&name, dir);
                        let opts = engine.options().clone();
                        self.store.apply_preset(name, opts);
                    }
                }
                #[cfg(target_arch = "wasm32")]
                let _ = name;
            }
            AppCommand::SaveViewPreset { name } => {
                // Writes to the preset *library* on disk; it does not change
                // the active view options, only the available-presets list.
                // No `SessionUpdate` carries a disk-library change, so refresh
                // just that list onto the frontend at-site (the same read the
                // VIEW arm of the GUI consumer does) rather than re-pushing
                // the whole VIEW section.
                #[cfg(not(target_arch = "wasm32"))]
                // Own the dir so the `&self.host` borrow is released before
                // the disjoint `&mut self.engine` / `&mut self.frontend`
                // borrows below.
                if let Some(dir) = self.host.view_presets_dir().map(std::path::Path::to_path_buf) {
                    if let Some(engine) = self.engine.as_mut() {
                        engine.save_preset(&name, &dir);
                    }
                    self.frontend.view.available_presets =
                        viso::options::VisoOptions::list_presets(&dir);
                    self.frontend.mark_dirty(DirtyFlags::VIEW);
                }
                #[cfg(target_arch = "wasm32")]
                let _ = name;
            }
            AppCommand::History { .. } | AppCommand::AdvanceBubble { .. } => {
                // Handled in the early-return block above. The match is
                // exhaustive over `AppCommand` (G10): a new variant
                // without a handler is a compile error.
            }
        }
    }

    /// Free-form file load (Scientist mode). Ingest entities, set
    /// metadata, then tick + fit the camera (tick is how the render
    /// projector republishes — the `SessionUpdate` stream carries `PreviewAdded`s and
    /// `HeadMoved`s from `load_entity_into_history`).
    fn handle_load_structure(&mut self, path: String) {
        self.set_loading_state(LoadingState::LoadingSession);
        match crate::puzzle::load_file_as_entities(&path) {
            Ok((entities, name)) => {
                log::info!("Loaded structure via IPC: {}", name);
                for entity in entities {
                    let _ = load_entity_into_history(&mut self.store, entity, name.clone());
                }
                // Free-form load: set the title and drop any puzzle
                // objective + tutorial bubbles through the create seam.
                // `start` emits `PuzzleChanged` (via `clear_puzzle`) when
                // there was a puzzle to clear, which the tick turns into
                // PUZZLE dirty. A scientist→scientist reload where
                // `clear_puzzle` is a no-op emits nothing, so the puzzle
                // panel's title refresh rides the full populate below.
                self.store.start(name.clone(), None);

                // Publish + fit. tick(0.0) drains the `SessionUpdate` stream, publishes
                // via the render projector, and runs engine.update(0.0)
                // so fit_camera_to_focus has bounding-radius to read.
                self.tick(0.0);
                if let Some(engine) = self.engine.as_mut() {
                    engine.fit_camera_to_focus();
                }
            }
            Err(e) => {
                log::error!("Failed to load structure '{}': {}", path, e);
            }
        }
        // A reload does not re-arm the Loading → InSession gate (that fires
        // once, at the initial load), so signal a one-shot full populate
        // here: the next tick's GUI consumer pushes every section, covering
        // the puzzle-panel title and post-load score / catalog that no batch
        // variant carries on a reload.
        self.needs_full_populate = true;
    }

    /// Tutorial / campaign puzzle load (Game mode). Ingest entities and
    /// metadata, then tick + snap + apply the puzzle's saved pose.
    fn handle_load_puzzle(&mut self, puzzle_id: u32) {
        self.set_loading_state(LoadingState::LoadingSession);
        // Entity display name for the loaded molecules: the outgoing
        // session title (captured before `reset`, which leaves it intact).
        let title = self.store.title().to_string();
        self.store.reset();
        self.plugin_driver.reset_for_new_structure();
        // Topology swap: `Session::reset` already cleared the selection
        // (entity ids from the outgoing assembly can collide numerically
        // with the incoming ones without referring to the same entities).
        // Emit `SelectionChanged` explicitly so the tick re-pushes the
        // now-empty highlight to viso and raises SELECTION dirty; `reset`
        // itself only emits `HeadMoved`.
        self.store.clear_selection();
        // viso's own per-entity score map has an id-reuse hole:
        // replace_assembly now preserves scores across a swap (so a
        // settling preview doesn't flash the survivors gray), reconciling
        // membership by id. A puzzle reload restarts the entity allocator,
        // so the new puzzle's ids collide with the outgoing ones and would
        // inherit their colors; clear viso scores explicitly here.
        if let Some(engine) = self.engine.as_mut() {
            engine.clear_scores();
        }

        match crate::puzzle::load_puzzle_structure(puzzle_id) {
            Ok(puzzle_data) => {
                // Install the puzzle (title + objective + tutorial bubbles)
                // through the create seam. The tutorial sequence and its
                // cursor move together: a non-empty sequence starts at index
                // 0, an empty sequence is `None`. `start` emits
                // `PuzzleChanged`, which the tick turns into PUZZLE dirty
                // (the PUZZLE arm also pushes the current bubble).
                let bubbles = if puzzle_data.bubbles.is_empty() {
                    None
                } else {
                    Some(puzzle_data.bubbles)
                };
                let current_bubble = bubbles.as_ref().map(|_| 0);
                self.store.start(
                    puzzle_data.name.clone(),
                    Some(Puzzle {
                        id: puzzle_id,
                        start_energy: puzzle_data.start_energy,
                        completion_energy: puzzle_data.completion_score,
                        bubbles,
                        current_bubble,
                    }),
                );

                #[cfg(not(target_arch = "wasm32"))]
                if let Some(preset_name) = &puzzle_data.view_preset {
                    if let Some(dir) = self.host.view_presets_dir() {
                        if let Some(engine) = self.engine.as_mut() {
                            // Read the preset off disk via the engine, then
                            // record it as the active preset on the session.
                            // The tick(0.0) below drains the emitted
                            // `ViewOptionsChanged` and re-applies the options.
                            engine.load_preset(preset_name, dir);
                            let opts = engine.options().clone();
                            self.store.apply_preset(preset_name.clone(), opts);
                        }
                    }
                }

                let ss_override = puzzle_data.ss_override;
                let cam = &puzzle_data.camera;
                let cam_eye =
                    glam::Vec3::new(cam.eye[0] as f32, cam.eye[1] as f32, cam.eye[2] as f32);
                let cam_up = glam::Vec3::new(cam.up[0] as f32, cam.up[1] as f32, cam.up[2] as f32);

                let mut ids: Vec<EntityId> = Vec::new();
                for entity in puzzle_data.entities {
                    if let Some(id) =
                        load_entity_into_history(&mut self.store, entity, title.clone())
                    {
                        ids.push(id);
                    }
                }

                // Topology swap rides the `SessionUpdate` stream — tick's render
                // projector picks `replace_assembly` because the id set
                // differs from the last publish (post-reset = empty).
                self.tick(0.0);

                if let Some(engine) = self.engine.as_mut() {
                    // Snap so bounding_radius reflects molecule extent
                    // (fog driver), then override the pose with the
                    // puzzle's saved eye/up but anchor the orbit
                    // center on the protein centroid.
                    engine.snap_camera_to_focus();
                    if let Some(centroid) = engine.focus_centroid() {
                        engine.set_camera_pose(centroid, cam_eye, cam_up);
                    }

                    if let Some(ss) = ss_override {
                        if let Some(&first_id) = ids.first() {
                            engine.set_ss_override(first_id.raw(), ss);
                        }
                    }
                }

                // Rosetta session init via bridge plugin's `init` +
                // auto-`update_assembly` fan-out lands when the
                // orchestrator's ensure_plugin_registered path is
                // invoked for "rosetta" with the new assembly.
                let _ = puzzle_id;
            }
            Err(e) => log::error!("Failed to load puzzle {}: {}", puzzle_id, e),
        }
        // PUZZLE rides the `start` emit drained by the inner `tick(0.0)`
        // above (its PUZZLE arm also pushes the current bubble), and the
        // topology swap re-pushes the entity list via the batch. A reload
        // does not re-arm the Loading → InSession gate, so signal a one-shot
        // full populate to push the post-load score / catalog the batch does
        // not carry on a reload.
        self.needs_full_populate = true;
    }

    // ── Tutorial-bubble cursor ──

    /// Step the tutorial-bubble cursor on the session. The cursor lives on
    /// `Session` now; this forwards and the emitted `BubbleChanged` is
    /// turned into TEXT_BUBBLE dirty by the tick, which re-pushes the new
    /// head. Forward saturates one past the end (the GUI then clears);
    /// back saturates at 0.
    fn advance_bubble(&mut self, back: bool) {
        self.store.advance_bubble(back);
    }

    // ── History navigation (Undo / Redo / Jump / Pin) ──

    /// Common tail for undo / redo / jump_checkpoint: clear cached
    /// per-residue scores (the values were computed against the
    /// *previous* head and become meaningless on a head move; v1 just
    /// blanks them so the structure renders neutral instead of "gray",
    /// v2 will async-reeval). Score is no longer cached in `App`; the GUI
    /// projection reads it off the new head checkpoint on the next
    /// GUI-consumer pass. The `HeadMoved` emitted by undo/redo/jump rides
    /// the batch, from which the render projector republishes (picking
    /// `replace_assembly` / `set_assembly`) and the GUI consumer derives
    /// SCENE | SCORE | ACTIONS dirty.
    fn after_head_move(&mut self) {
        if let Some(engine) = self.engine.as_mut() {
            let ids: Vec<EntityId> = self.store.ids().collect();
            for eid in ids {
                engine.set_per_residue_scores(eid.raw(), None);
            }
        }
    }

    /// Dispatch a [`HistoryCommand`] from the GUI to the matching
    /// `Session` method. Refusals are logged; the GUI surface
    /// shows the result by virtue of the head not moving (no separate
    /// toast / error channel — `HistoryError::EntityLocked` only
    /// fires while the user's own action is still running, where the
    /// running indicator is the natural feedback). The match is
    /// exhaustive (G10): adding a variant without a handler is a
    /// compile error.
    fn run_history_command(&mut self, cmd: HistoryCommand) {
        if self.engine.is_none() {
            return;
        }
        let result: Result<HistoryOutcome, SessionError> = match cmd {
            HistoryCommand::JumpCheckpoint { id } => self
                .store
                .jump_checkpoint(id.into_inner())
                .map(|_| HistoryOutcome::HeadMoved),
            HistoryCommand::Undo => self.store.undo().map(|opt| match opt {
                Some(_) => HistoryOutcome::HeadMoved,
                None => {
                    log::info!("Undo: already at root");
                    HistoryOutcome::Noop
                }
            }),
            HistoryCommand::Redo { branch } => {
                self.store
                    .redo(branch.map(|w| w.into_inner()))
                    .map(|opt| match opt {
                        Some(_) => HistoryOutcome::HeadMoved,
                        None => {
                            log::info!("Redo: nowhere forward to go");
                            HistoryOutcome::Noop
                        }
                    })
            }
            HistoryCommand::LaneUndo { entity, target } => self
                .store
                .lane_undo(entity, target.into_inner())
                .map(|_| HistoryOutcome::HeadMoved),
            HistoryCommand::LaneRedo { entity, branch } => self
                .store
                .lane_redo(entity, branch.map(|w| w.into_inner()))
                .map(|_| HistoryOutcome::HeadMoved),
            HistoryCommand::PinCheckpoint { id } => self
                .store
                .pin_checkpoint(id.into_inner())
                .map(|_| HistoryOutcome::Curated),
            HistoryCommand::UnpinCheckpoint { id } => self
                .store
                .unpin_checkpoint(id.into_inner())
                .map(|_| HistoryOutcome::Curated),
            HistoryCommand::SetExcludeFromBest { id, exclude } => self
                .store
                .set_exclude_from_best(id.into_inner(), exclude)
                .map(|_| HistoryOutcome::Curated),
            HistoryCommand::AbortAction => {
                // "Discard the running action." Targeting a single edit
                // no-ops once two edits run concurrently, so discard every
                // open edit instead of silently doing nothing.
                let rids: Vec<u64> = self.store.pending_request_ids().collect();
                if rids.is_empty() {
                    Ok(HistoryOutcome::Noop)
                } else {
                    for rid in rids {
                        if let Err(e) = self.store.abort_action(rid) {
                            log::warn!("abort_action({rid}) failed: {e}");
                        }
                        #[cfg(not(target_arch = "wasm32"))]
                        {
                            let _ = self.score_targets.remove(&rid);
                        }
                    }
                    Ok(HistoryOutcome::HeadMoved)
                }
            }
        };

        match result {
            Ok(HistoryOutcome::HeadMoved) => self.after_head_move(),
            Ok(HistoryOutcome::Curated) => {
                // Pin / unpin / exclude mutate the history DAG's curation
                // metadata without moving the head or bumping
                // `topology_version`, so the GUI consumer's cursor-driven
                // history push never re-fires. Push the refreshed history
                // section at-site so the panel reflects the change.
                self.frontend.set_history(project_history(&self.store));
            }
            Ok(HistoryOutcome::Noop) => {}
            Err(e) => log::warn!("history command refused: {e}"),
        }
    }

    // ── Native input (when webview is not ready) ──

    pub fn handle_native_mouse_input(&mut self, button: viso::MouseButton, pressed: bool) {
        let pending_click = if let Some(engine) = &mut self.engine {
            let click = engine.feed_pointer_button(button, pressed);
            update_all_visualizations(engine, None);
            click
        } else {
            None
        };
        if let Some(click) = pending_click {
            self.apply_click_to_selection(&click);
        }
    }

    pub fn handle_native_cursor_moved(&mut self, x: f32, y: f32) {
        if let Some(engine) = &mut self.engine {
            engine.set_cursor_pos(x, y);
            update_all_visualizations(engine, None);
        }
    }

    /// Forward a scroll delta in viso "logical scroll units" (winit
    /// `LineDelta(_, y)` passes `y` directly; `PixelDelta(_, y)` should
    /// pass `y * 0.01`). Conversion lives in the host.
    pub fn handle_native_mouse_wheel(&mut self, scroll_delta: f32) {
        if let Some(engine) = &mut self.engine {
            engine.feed_scroll(scroll_delta);
        }
    }

    pub fn handle_native_modifiers(&mut self, shift: bool) {
        if let Some(engine) = &mut self.engine {
            engine.feed_modifiers(shift);
        }
    }

    // ── Per-frame visual updates ──

    pub fn update_frame_visuals(&mut self) {
        // Pre-snapshot pull info under an immutable borrow so the
        // subsequent `&mut engine` doesn't conflict.
        #[cfg(not(target_arch = "wasm32"))]
        let pull = self.plugin_driver.pull_drag_pull_info();
        #[cfg(target_arch = "wasm32")]
        let pull: Option<viso::PullInfo> = None;
        let Some(engine) = &mut self.engine else {
            return;
        };
        update_all_visualizations(engine, pull);
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
    // builds a `GuiContext` and calls `gui_projector.consume(...)` at the
    // end-of-tick route. There is no `populate_frontend` method anymore:
    // its body moved verbatim onto the consumer, reading named inputs
    // instead of `&mut self`.

    // ── The per-frame drive loop ──

    /// Drive one frame.
    ///
    /// Order:
    /// 1. drain pending plugin updates (apply to `Session`; emits
    ///    `SessionUpdate`s through the funnel).
    /// 2. drain the `SessionUpdate` stream in one go.
    /// 3. route the batch: plugin broadcaster fan-out and render
    ///    projector publish (both no-op on empty batches).
    /// 4. poll plugin scores (refresh head scores; emits `ScoresChanged`).
    /// 5. engine update (camera animation, mesh upload, etc.).
    /// 6. visualization overlay (bands / pull).
    /// 7. InSession gate (one-shot, on first score; raises full populate).
    /// 8. GUI consumer projects the batch (+ one-shot full populate) into
    ///    the frontend so the next `serialize_frontend_dirty` carries the
    ///    latest snapshot.
    pub fn tick(&mut self, dt: f32) {
        // 1. Plugin updates.
        self.apply_backend_updates();

        // 2-3. Drain the SessionUpdate stream once and route to both
        //      projectors. The tick is the sole drain. Handlers used to call
        //      `pump_scene_changes` per-event, but that race-conditioned
        //      against the render projector reading the same update queue, so
        //      the per-handler pumps were removed.
        let changes = self.store.take_updates();
        // A ScoresChanged update signals the GUI score widget and a
        // SelectionChanged update signals the selection mirror + viso
        // highlight, but neither is a scene mutation: the render projector
        // and the rescore trigger below must treat a score-only /
        // selection-only batch as empty. Republishing geometry on such a
        // reply is wasted work (and forces a spurious full rebuild on a
        // topology tick); re-querying scores in response would loop.
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
        // A selection change is not broadcast to plugins and does not
        // republish geometry; the viso highlight push is the only side
        // effect here. The matching GUI dirty (SELECTION + ACTIONS) is
        // derived from this same batch by the GUI consumer below. Source
        // the highlight from the authoritative `Session` selection (disjoint
        // borrows: `&mut self.engine` + `&self.store`).
        if changes
            .iter()
            .any(|c| matches!(c, SessionUpdate::SelectionChanged))
        {
            if let Some(engine) = self.engine.as_mut() {
                engine.set_selection(self.store.selection());
            }
        }
        // A focus change is likewise not broadcast and not a geometry
        // change. Push viso's camera-framing mirror (no reframe), then drive
        // the reframe on the host's cadence. The matching GUI dirty (SCENE +
        // UI + ACTIONS) is derived from this same batch by the GUI consumer.
        if changes
            .iter()
            .any(|c| matches!(c, SessionUpdate::FocusChanged))
        {
            if let Some(engine) = self.engine.as_mut() {
                engine.set_focus(self.store.focus());
                engine.fit_camera_to_focus();
            }
        }
        // A view-options change is not broadcast and not a geometry change.
        // The session is the source of truth; apply the options to the engine
        // here. The matching GUI dirty (VIEW) is derived from this same batch
        // by the GUI consumer below.
        if changes
            .iter()
            .any(|c| matches!(c, SessionUpdate::ViewOptionsChanged))
        {
            if let Some(engine) = self.engine.as_mut() {
                engine.set_options(self.store.view_options().clone());
            }
        }
        // BubbleChanged / PuzzleChanged have no viso side effect; their GUI
        // dirty (TEXT_BUBBLE / PUZZLE) is derived from the batch by the GUI
        // consumer below, so there are no tick arms for them anymore.
        if !changes.is_empty() {
            // The broadcaster self-filters score-only batches (scores are
            // not an observable mutation for plugins); a no-op call is cheap.
            if let Some(orch) = self.plugin_driver.orchestrator.as_mut() {
                self.plugin_driver
                    .broadcaster
                    .consume(&changes, &self.store, orch);
            }
            if render_changes > 0 {
                if let Some(engine) = self.engine.as_mut() {
                    self.render_projector.consume(&changes, &self.store, engine);
                }
            }
        }

        // 4. Plugin score poll. Scores go stale only on an assembly
        //    change (every mutation emits a SessionUpdate, including
        //    those from non-scoring plugins), and the query runs off the
        //    render thread so a slow scorer never stalls rendering. Until
        //    the first score lands the poll is synchronous so the
        //    InSession gate flips promptly.
        #[cfg(not(target_arch = "wasm32"))]
        {
            if !self.has_initial_score() {
                // No score yet: blocking poll each tick until the first
                // one lands. Brief, one-time per load.
                self.poll_plugin_scores();
            } else {
                // Steady state: fire only on an assembly change, never
                // block the render thread; apply replies as they arrive.
                // Always the cheap whole-assembly query against the worker's
                // already-built live pose (no per-frame pose rebuild). With
                // exactly one edit open the live pose IS that edit's
                // composition, so its reply attributes to the edit; per-edit
                // exactness for any other case lands at commit via the
                // commit-stamp.
                if render_changes > 0 {
                    self.request_scores();
                }
                self.poll_async_scores();
                // Drain composition replies: now only commit-time checkpoint
                // stamps, fired at commit regardless of whether an edit is
                // still open.
                self.poll_composition_scores();
            }
        }

        // 5. Engine update + 6. visualization overlay.
        #[cfg(not(target_arch = "wasm32"))]
        let pull = self.plugin_driver.pull_drag_pull_info();
        #[cfg(target_arch = "wasm32")]
        let pull: Option<viso::PullInfo> = None;
        if let Some(engine) = self.engine.as_mut() {
            engine.update(dt);
            update_all_visualizations(engine, pull);
        }

        // 7. State-machine: flip into InSession the first time the plugin
        //    score lands for the just-loaded session. This is the only
        //    phase that routes the frontend to the in-puzzle UI.
        if self.awaiting_initial_score && self.has_initial_score() {
            self.set_loading_state(LoadingState::InSession);
            self.awaiting_initial_score = false;
            self.frontend.set_puzzle_loaded(true);
            self.frontend.set_score_title(self.store.title().to_string());
            self.frontend
                .set_puzzle_scientist(self.store.title().to_string());
            // Session birth: the GUI consumer below does a one-shot full
            // populate (every section once) rather than flooding the
            // transmit layer's dirty bits directly.
            self.needs_full_populate = true;
            log::info!("Initial plugin score received — app_state=InSession");
        }

        // 8. Frontend projection: the GUI consumer derives its dirty set
        //    entirely from this tick's `changes` batch, plus the one-shot
        //    `needs_full_populate` signal (session birth). The signal is
        //    consumed (taken + cleared) only when the engine is present and
        //    the consumer runs; with no engine attached yet it persists to a
        //    later tick, so the birth populate is never dropped.
        if let Some(engine) = self.engine.as_ref() {
            let full_populate = self.needs_full_populate;
            self.needs_full_populate = false;
            let ctx = GuiContext {
                host: self.host.as_ref(),
            };
            self.gui_projector.consume(
                &changes,
                full_populate,
                &self.store,
                engine,
                &self.plugin_driver,
                &ctx,
                &mut self.frontend,
            );
        }
    }

    // ── Complex lifecycle (engine attach + initial load) ──

    /// Attach a host-built `VisoEngine` to this App. Hosts are
    /// responsible for constructing the wgpu `RenderContext` against
    /// their own surface (winit window on desktop, `<canvas>` on web)
    /// and applying any preset / render-scale tweaks they want before
    /// handing it over.
    pub fn attach_engine(&mut self, engine: VisoEngine) {
        self.engine = Some(engine);
    }

    /// Load the initial structure, register entities, and create the
    /// initial Rosetta session. Runs AFTER the webview's loading screen
    /// is visible so the user has feedback during the (potentially
    /// slow) load. Requires `create_render_context` to have run first.
    ///
    /// Bootstrap path comes from the host (`HostResources::initial_structure_path`);
    /// `None` is a no-op (e.g. the web shell loads structures via a
    /// separate flow rather than a startup path).
    pub fn load_initial_structure(&mut self) {
        if self.engine.is_none() {
            log::error!("load_initial_structure called before create_render_context");
            return;
        }

        let Some(path) = self.host.initial_structure_path() else {
            return;
        };

        self.set_loading_state(LoadingState::LoadingSession);

        // Parse entities from file
        match crate::puzzle::load_file_as_entities(&path) {
            Ok((entities, name)) => {
                for entity in entities {
                    let _ = load_entity_into_history(&mut self.store, entity, name.clone());
                }
                // Free-form initial load: set the title and ensure the
                // free-form (no-puzzle) session through the create seam. The
                // scientist puzzle panel + title reach the GUI at the
                // InSession gate's `set_puzzle_scientist` push.
                self.store.start(name.clone(), None);

                // Publish + fit. tick(0.0) drains the `SessionUpdate` stream, hands the
                // assembly to the render projector, and runs
                // engine.update(0.0) so the pending Assembly is drained
                // before fit_camera reads bounding-radius.
                self.tick(0.0);
                if let Some(engine) = self.engine.as_mut() {
                    engine.fit_camera_to_focus();
                }

                log::info!("Loaded structure: {}", name);

                // Install a fresh orchestrator BEFORE `bootstrap_plugins`
                // so discovery + registration run against the handle the
                // plugin driver owns. A fresh orchestrator restarts
                // request ids at 1, so drop any stale composition targets
                // before a new edit can reuse an old id.
                self.plugin_driver.init_orchestrator();
                #[cfg(not(target_arch = "wasm32"))]
                self.score_targets.clear();
                self.bootstrap_plugins();

                // Republish: bootstrap may have committed rosetta's
                // post-Init normalized assembly (full-atom pose) into
                // the store. The HeadMoved emitted by commit_action
                // rides the `SessionUpdate` stream; tick(0.0) flushes it and polls
                // scores, so has_initial_score() flips synchronously.
                self.tick(0.0);
            }
            Err(e) => {
                log::error!("Failed to load structure '{}': {}", path, e);
                self.plugin_driver.init_orchestrator();
                #[cfg(not(target_arch = "wasm32"))]
                self.score_targets.clear();
            }
        }

        // The now-populated state reaches the GUI via the one-shot full
        // populate the Loading → InSession flip raises (VIEW for the engine
        // options, ACTIONS for the catalog, SCORE for the initial number,
        // SCENE for the entity list). The loading screen clears via the
        // `InSession` flip itself, not a dirty flag.
        //
        // Arm the Loading → InSession gate. `tick` flips `app_state` the
        // first frame `has_initial_score()` returns true (plugins may
        // not have replied yet by the time we return here).
        self.awaiting_initial_score = true;
    }

    /// Discover plugins under the runtime plugin root and bring up the
    /// always-on Rosetta session with the just-loaded structure as the
    /// initial assembly. Errors are logged and dropped: a missing plugin
    /// dir / dylib should degrade the app to viewer-only, not crash the
    /// load.
    ///
    /// Caller must have installed a fresh orchestrator on the plugin
    /// driver before calling; this method drives discovery + registration
    /// through the driver and applies any per-plugin post-Init result.
    ///
    /// If Rosetta's Init returns a non-empty normalized assembly (full-atom
    /// pose with hydrogens / terminal O / etc. added), it is committed as
    /// a follow-up `PluginOp` checkpoint and republished so that
    /// `scene.positions` is seeded at the normalized atom count before any
    /// user action runs. Without this, the first user op would cross an
    /// atom-set boundary mid-action and snap.
    #[cfg(not(target_arch = "wasm32"))]
    fn bootstrap_plugins(&mut self) {
        self.set_loading_state(LoadingState::Initializing);
        let Some(plugins_root) = locate_plugins_root() else {
            log::warn!(
                "[App] no plugins root found (set FOLDIT_PLUGINS_ROOT or run \
                 from a workspace checkout); plugins disabled"
            );
            return;
        };
        log::info!("[App] discovering plugins under {}", plugins_root.display());

        // Snapshot the initial assembly under an immutable store borrow so
        // the plugin driver can hand it to `ensure_plugin_registered` for
        // each plugin. Registration uses this one pre-normalization
        // snapshot for every plugin, so applying rosetta's post-Init result
        // afterward (below) does not change what later plugins register
        // against.
        let initial_assembly = {
            let head_before = self.store.head_assembly();
            match molex::ops::wire::serialize_assembly(&head_before) {
                Ok(b) => b,
                Err(e) => {
                    log::warn!(
                        "[App] failed to serialize initial assembly for plugin \
                         registration: {e:?}; plugins disabled"
                    );
                    return;
                }
            }
        };

        let registered = self
            .plugin_driver
            .discover_and_register(&plugins_root, initial_assembly);

        // Apply each registered plugin's post-Init normalization into the
        // store. Only rosetta returns a non-empty normalized assembly
        // today; the empty-bytes guard inside `apply_post_init` makes the
        // call a no-op for plugins that ship none, so the loop stays
        // generic and additional normalizing plugins drop in without
        // host-side wiring changes.
        for (plugin_id, post_init_bytes) in &registered {
            self.apply_post_init(plugin_id, post_init_bytes);
        }
    }

    /// Apply a plugin's post-Init normalized assembly (full-atom pose) so
    /// the host's canonical assembly matches the plugin's internal pose
    /// before any user action runs. Every entity the normalized assembly
    /// touches that has a committed lane in the store is normalized inside
    /// a single multi-lane edit, so a multi-chain session no longer drops
    /// every entity past the first.
    #[cfg(not(target_arch = "wasm32"))]
    fn apply_post_init(&mut self, plugin_id: &str, post_init_bytes: &[u8]) {
        if post_init_bytes.is_empty() {
            log::warn!(
                "[App] {plugin_id} post-Init returned no normalized assembly; \
                 first user action will likely snap because scene.positions \
                 stays at the pre-Init atom count."
            );
            return;
        }
        let normalized = match molex::ops::wire::deserialize_assembly(post_init_bytes) {
            Ok(a) => a,
            Err(e) => {
                log::warn!(
                    "[App] {plugin_id} post-Init assembly decode failed: {e:?}; \
                     skipping normalization apply"
                );
                return;
            }
        };
        // Every entity the normalized assembly names that has a committed
        // lane in the store. A protein has a lane (loaded into history);
        // ambient / zero-residue stubs stay transient and have none, so
        // they're skipped here.
        let target_entities: Vec<EntityId> = normalized
            .entities()
            .iter()
            .map(|e| e.id())
            .filter(|id| self.store.history().lane(*id).is_some())
            .collect();
        if target_entities.is_empty() {
            log::warn!(
                "[App] {plugin_id} post-Init: no store entity matches the \
                 normalized assembly; skipping normalization apply"
            );
            return;
        }
        let kind = CheckpointKind::PluginOp {
            plugin_id: String::from(plugin_id),
            op_id: String::from("_init_normalize"),
            display: String::from("Init"),
        };
        // Host-internal action: no dispatch happened, so draw the edit's
        // request_id straight from the orchestrator (the single id
        // authority).
        let Some(request_id) = self.plugin_driver.alloc_request_id() else {
            log::warn!(
                "[App] {plugin_id} post-Init: no orchestrator to allocate a \
                 request id; skipping normalization apply"
            );
            return;
        };
        if let Err(e) =
            self.store
                .begin_action(target_entities, kind, String::from("Init"), request_id)
        {
            log::warn!(
                "[App] {plugin_id} post-Init begin_action failed: {e}; \
                 skipping normalization apply"
            );
            return;
        }
        let applied = apply_streaming_assembly(&mut self.store, &normalized, None, request_id);
        if !applied {
            log::warn!(
                "[App] {plugin_id} post-Init apply_streaming_assembly did not \
                 update any entity; rolling back tentative. This usually means \
                 the {plugin_id}-returned entity ID does not match any store \
                 entity ID."
            );
            let _ = self.store.commit_action(request_id);
            return;
        }
        if let Err(e) = self.store.commit_action(request_id) {
            log::warn!("[App] {plugin_id} post-Init commit_action failed: {e}");
            return;
        }
        log::info!(
            "[App] {plugin_id} post-Init assembly applied ({} bytes)",
            post_init_bytes.len()
        );
        // Republish is stream-driven: the HeadMoved from commit_action
        // rides through the next tick's render projector.
    }

    /// Shut down backends and scene processor.
    pub fn shutdown(&mut self) {
        self.plugin_driver.shutdown();
        if let Some(engine) = &mut self.engine {
            engine.shutdown();
        }
    }

    /// Apply a viso click-event to the selection store. Empty-area
    /// clicks clear the selection; non-empty expansions either replace
    /// (no modifier) or toggle (shift held) on a per-residue basis.
    /// Targets with an empty expansion (atom picks, non-protein hits)
    /// are no-ops on shift-held click and a clear on plain click; we
    /// follow the same "replace selection with the click's expansion"
    /// rule, which collapses to "clear" when the expansion is empty.
    fn apply_click_to_selection(&mut self, click: &ClickEvent) {
        match classify_click_for_selection(click) {
            ClickSelectionAction::Clear => {
                self.store.clear_selection();
            }
            ClickSelectionAction::Replace(residues) => {
                self.store.clear_selection();
                for (entity, residue) in residues {
                    self.store.select_residue(entity, residue);
                }
            }
            ClickSelectionAction::Toggle(residues) => {
                for (entity, residue) in residues {
                    let _ = self.store.toggle_residue(entity, residue);
                }
            }
        }
    }

    /// Apply a panel-originated selection mutation: wholesale replace
    /// the current selection with `entries`. The wire-side `entity_id`
    /// is a raw `u32`; look it up against the store's existing ids
    /// instead of minting a new one through the allocator (which would
    /// silently advance and break the next genuine allocation).
    /// Entries that don't match any live entity are dropped — panels
    /// can race a structure swap, and a stale id should clear silently
    /// rather than fail loudly. An empty `entries` list clears the
    /// selection entirely. Per-entity residue lists are collected into
    /// `BTreeSet`, so duplicate or out-of-order indices in the wire
    /// payload are silently normalized.
    pub fn handle_set_selection(&mut self, entries: Vec<foldit_gui::EntitySelection>) {
        self.store.clear_selection();
        for entry in entries {
            let Some(entity) = self.store.ids().find(|id| id.raw() == entry.entity_id) else {
                log::trace!(
                    "handle_set_selection: unknown entity_id {} (dropping)",
                    entry.entity_id
                );
                continue;
            };
            self.store.set_residues_on(entity, entry.residues);
        }
    }
}

// ---------------------------------------------------------------------------
// Bridge: Dispatcher trait impl
// ---------------------------------------------------------------------------

impl foldit_gui::Dispatcher for App {
    /// Webview signaled it's ready — mark every section of the owned
    /// `FrontendState` dirty so the next `serialize_frontend_dirty`
    /// emits a full snapshot. App owns the frontend mirror (RX13), so
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
                let filepath = payload
                    .get("filepath")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "missing 'filepath'".to_string())?;
                let bytes = self
                    .host
                    .read_file(filepath)
                    .map_err(|e| format!("read {}: {}", filepath, e))?;
                use base64::Engine;
                let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                Ok(serde_json::json!({ "encoding": "base64", "content": b64 }))
            }
            RequestKind::GetHotkeyText => {
                // Stub: real implementation would look up display strings
                // for hotkey ids. Until that surface lands, return empty so
                // HelpMenuPanel rejects gracefully instead of timing out.
                let hotkey = payload.get("hotkey").and_then(|v| v.as_str()).unwrap_or("");
                Err(format!("hotkey lookup not implemented (hotkey={})", hotkey))
            }
            RequestKind::ServerRequest => {
                // Stub: server requests (news, etc.) require an HTTP client
                // bound here. Defer until a dedicated request handler exists.
                let endpoint = payload
                    .get("endpoint")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                Err(format!(
                    "server request not implemented (endpoint={})",
                    endpoint
                ))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Visualization helpers (free functions for split-borrow friendliness)
// ---------------------------------------------------------------------------

/// Update drag/pull/band visualizations. Bands are still inert (the
/// band state machine is the next item to come back online). The pull
/// capsule + cone arrow renders whenever the caller hands a
/// `Some(PullInfo)` from a live drag; clears otherwise so a finished
/// or cancelled drag leaves no overlay.
fn update_all_visualizations(engine: &mut VisoEngine, pull: Option<viso::PullInfo>) {
    engine.update_bands(vec![]);
    engine.update_pull(pull);
}

/// Get the trajectory path from command-line arguments. CLI/host
/// utility — read once on a hotkey + reused by `LoadTrajectory`.
fn trajectory_path_from_args() -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.windows(2).find_map(|w| {
        if w[0] == "--trajectory" {
            Some(w[1].clone())
        } else {
            None
        }
    })
}

/// Locate the runtime plugins directory.
///
/// Resolution order:
///   1. `FOLDIT_PLUGINS_ROOT` environment override (production /
///      bundled deployments point this at the bundle's plugins dir).
///   2. `<exe_dir>/plugins/` if it exists (bundle layout).
///   3. Walk up from `current_exe()` looking for
///      `crates/foldit-runner/plugins/` (dev workflow under cargo).
///
/// Returns `None` if none of these resolve. The caller logs and skips
/// plugin discovery in that case -- the desktop app degrades to viewer-
/// only mode rather than failing the load.
#[cfg(not(target_arch = "wasm32"))]
pub fn locate_plugins_root() -> Option<std::path::PathBuf> {
    if let Some(env) = std::env::var_os("FOLDIT_PLUGINS_ROOT") {
        let p = std::path::PathBuf::from(env);
        if p.is_dir() {
            return Some(p);
        }
    }
    let exe = std::env::current_exe().ok()?;
    if let Some(dir) = exe.parent() {
        let bundle = dir.join("plugins");
        if bundle.is_dir() {
            return Some(bundle);
        }
    }
    let mut cursor = exe.parent()?.to_path_buf();
    loop {
        let candidate = cursor.join("crates/foldit-runner/plugins");
        if candidate.is_dir() {
            return Some(candidate);
        }
        if !cursor.pop() {
            break;
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[cfg(test)]
mod selection_tests {
    use super::*;
    use molex::entity::molecule::id::EntityIdAllocator;
    use std::io;
    use std::path::Path;

    /// Minimal [`HostResources`] stub. `App` construction needs one;
    /// these tests never touch the filesystem.
    struct TestHost;

    impl crate::HostResources for TestHost {
        fn read_file(&self, _path: &str) -> io::Result<Vec<u8>> {
            Err(io::Error::new(io::ErrorKind::NotFound, "test stub"))
        }
        fn view_presets_dir(&self) -> Option<&Path> {
            None
        }
        fn initial_structure_path(&self) -> Option<String> {
            None
        }
    }

    fn fresh_app() -> App {
        App::new(Box::new(TestHost))
    }

    /// Mint a sequence of distinct entity ids in a test-local order.
    /// `EntityId` is opaque, so we allocate via `EntityIdAllocator` and
    /// hand back the n-th id from a freshly-seeded allocator. The map
    /// keys we care about are just "different ids on the same App",
    /// not specific raw values.
    fn mint_ids(n: usize) -> Vec<EntityId> {
        let mut alloc = EntityIdAllocator::new();
        (0..n).map(|_| alloc.allocate()).collect()
    }

    #[test]
    fn handle_set_selection_clears_on_empty_input() {
        let mut app = fresh_app();
        let ids = mint_ids(1);
        app.store.select_residue(ids[0], 7);
        assert!(!app.store.selection_is_empty());
        // Empty entries: clear (`clear_selection` always runs first; no
        // entry loop body) — independent of whether the empty store
        // could even resolve a raw id.
        app.handle_set_selection(Vec::new());
        assert!(app.store.selection_is_empty());
    }

    #[test]
    fn handle_set_selection_drops_unknown_entity_ids() {
        let mut app = fresh_app();
        let ids = mint_ids(1);
        // Seed a non-empty selection so we can prove the clear ran.
        app.store.select_residue(ids[0], 9);
        // The test stub has no loaded structure, so `self.store.ids()`
        // is empty and every raw id is unresolvable. The mutator clears
        // the existing selection and drops the unknown entries.
        app.handle_set_selection(vec![
            foldit_gui::EntitySelection {
                entity_id: 0,
                residues: vec![1, 2, 3],
            },
            foldit_gui::EntitySelection {
                entity_id: 999,
                residues: vec![5],
            },
        ]);
        assert!(app.store.selection_is_empty());
    }

    #[test]
    fn next_focus_cycles_then_wraps_to_all() {
        // The Tab-cycle step, owned by foldit-core: `All` -> first
        // focusable -> ... -> last -> back to `All`.
        let ids = mint_ids(2);
        assert_eq!(
            next_focus(Focus::All, &ids),
            Focus::Entity(ids[0]),
            "All advances to the first focusable entity",
        );
        assert_eq!(
            next_focus(Focus::Entity(ids[0]), &ids),
            Focus::Entity(ids[1]),
            "a focused entity advances to the next in the list",
        );
        assert_eq!(
            next_focus(Focus::Entity(ids[1]), &ids),
            Focus::All,
            "the last focusable entity wraps back to All",
        );
        // An entity that has left the focusable list (e.g. hidden) also
        // wraps to All rather than getting stuck.
        assert_eq!(next_focus(Focus::Entity(ids[1]), &ids[..1]), Focus::All);
        // No focusable entities: All stays All.
        assert_eq!(next_focus(Focus::All, &[]), Focus::All);
    }

    /// One committed Bulk entity, promoted into history so the store has a
    /// non-root committed head.
    fn mk_bulk() -> molex::MoleculeEntity {
        use molex::entity::molecule::atom::Atom;
        use molex::entity::molecule::bulk::BulkEntity;
        use molex::{Element, MoleculeType};
        let id = EntityIdAllocator::new().allocate();
        let atom = Atom {
            position: glam::Vec3::ZERO,
            occupancy: 1.0,
            b_factor: 0.0,
            element: Element::O,
            name: *b"O   ",
            formal_charge: 0,
        };
        molex::MoleculeEntity::Bulk(BulkEntity::new(id, MoleculeType::Water, vec![atom], *b"HOH", 1))
    }

    /// A composition score for an open edit must land on that edit and be
    /// minted onto its committed checkpoint only at commit; the committed
    /// parent is never overwritten mid-action. This is the write the
    /// composition-score poll performs (`set_edit_scores`), targeted by the
    /// edit's `request_id` rather than "the first open edit".
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn composition_score_routes_to_pending_edit_not_committed_parent() {
        use crate::history::CheckpointKind;
        use crate::session::EntityOrigin;

        let mut app = fresh_app();
        // Commit one entity so the head is a real checkpoint, and stamp a
        // known score on it (the committed parent).
        let id = app
            .store
            .insert_preview(mk_bulk(), "e".to_string(), EntityOrigin::Loaded);
        app.store
            .promote_preview(
                id,
                CheckpointKind::PromotedPreview { entity: id },
                None,
                None,
                "e",
            )
            .expect("promote");
        app.store.set_head_scores(Some(10.0), Some(100.0));
        let parent = app.store.history().checkpoints().head();
        assert_eq!(
            app.store.history().checkpoint(parent).unwrap().raw_score,
            Some(10.0)
        );

        // Open an action on that entity.
        let rid = 1u64;
        app.store
            .begin_action(
                [id],
                CheckpointKind::PluginOp {
                    plugin_id: "rosetta".to_string(),
                    op_id: "wiggle".to_string(),
                    display: "wiggle".to_string(),
                },
                "w",
                rid,
            )
            .expect("begin_action");

        // Drive the composition-score write the poll path performs: stamp
        // the open edit by its request_id.
        let game = ((-42.0_f64 + 800.0) * 10.0).max(0.0);
        app.store.set_edit_scores(rid, Some(42.0), Some(game));

        // Mid-action: the committed parent is untouched; the composition
        // node carries the streamed score.
        assert_eq!(
            app.store.history().checkpoint(parent).unwrap().raw_score,
            Some(10.0),
            "committed parent score must not change mid-action"
        );
        assert_eq!(app.store.current_composition_scores().0, Some(42.0));

        // After commit: the minted checkpoint carries the streamed score;
        // the parent still holds its own.
        let committed = app.store.commit_action(rid).expect("commit");
        assert_eq!(
            app.store.history().checkpoint(committed).unwrap().raw_score,
            Some(42.0)
        );
        assert_eq!(
            app.store.history().checkpoint(committed).unwrap().game_score,
            Some(game)
        );
        assert_eq!(
            app.store.history().checkpoint(parent).unwrap().raw_score,
            Some(10.0)
        );
    }

    /// Post-Init normalization must reach *every* matching entity, not
    /// just the first. Guards the multi-lane path `apply_post_init` opens:
    /// one begin over the whole touched set, `apply_streaming_assembly`
    /// fanning across both lanes, and a single commit. Before the fix the
    /// begin ran on `first_protein_entity` only, so every entity past the
    /// first kept its pre-Init coordinates.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn post_init_normalizes_every_matching_entity_not_just_the_first() {
        use crate::history::CheckpointKind;
        use crate::session::EntityOrigin;
        use std::sync::Arc;

        let mut store = Session::new();
        // Two committed entities.
        let e1 = store.insert_preview(mk_bulk(), "a".to_string(), EntityOrigin::Loaded);
        store
            .promote_preview(e1, CheckpointKind::PromotedPreview { entity: e1 }, None, None, "a")
            .expect("promote a");
        let e2 = store.insert_preview(mk_bulk(), "b".to_string(), EntityOrigin::Loaded);
        store
            .promote_preview(e2, CheckpointKind::PromotedPreview { entity: e2 }, None, None, "b")
            .expect("promote b");
        let ckpts_before = store.history().checkpoints().len();

        // A "normalized" assembly that displaces BOTH entities' atoms,
        // keeping their store ids so `apply_streaming_assembly` id-matches.
        let moved = glam::Vec3::new(7.0, 7.0, 7.0);
        let mut a1 = store.entity(e1).expect("e1").clone();
        for atom in a1.atom_set_mut() {
            atom.position = moved;
        }
        let mut a2 = store.entity(e2).expect("e2").clone();
        for atom in a2.atom_set_mut() {
            atom.position = moved;
        }
        let normalized = molex::Assembly::from_arcs(vec![Arc::new(a1), Arc::new(a2)]);

        // The multi-lane apply path `apply_post_init` runs (sans the
        // orchestrator-driven request_id allocation, which a unit test
        // can't stand up): collect every assembly entity with a committed
        // lane, open ONE edit over the whole set, fan the stream across it,
        // commit once.
        let target_entities: Vec<EntityId> = normalized
            .entities()
            .iter()
            .map(|e| e.id())
            .filter(|id| store.history().lane(*id).is_some())
            .collect();
        assert_eq!(
            target_entities.len(),
            2,
            "both entities must resolve to a committed lane"
        );
        let rid = 99u64;
        store
            .begin_action(
                target_entities,
                CheckpointKind::PluginOp {
                    plugin_id: "rosetta".to_string(),
                    op_id: "_init_normalize".to_string(),
                    display: "Init".to_string(),
                },
                "Init",
                rid,
            )
            .expect("begin multi-lane edit");
        assert!(
            super::apply_streaming_assembly(&mut store, &normalized, None, rid),
            "apply_streaming_assembly must update at least one lane"
        );
        store.commit_action(rid).expect("commit");

        // Exactly one new checkpoint, and BOTH entities carry the moved
        // coordinates — not just the first.
        assert_eq!(store.history().checkpoints().len(), ckpts_before + 1);
        let head = store.head_assembly();
        for e in [e1, e2] {
            let ent = head.entity(e).expect("entity present in head assembly");
            assert!(
                ent.positions().iter().all(|p| *p == moved),
                "entity {} was not normalized",
                e.raw()
            );
        }
    }

    /// A whole-pose dispatch must open its edit over EVERY committed entity,
    /// not the host's single-entity fallback guess. `EditScope::AllEntities`
    /// resolves to all committed lanes (transient previews filtered out), and
    /// a multi-entity streamed frame then updates every lane on commit.
    /// Before the fix the runner's resolved target never reached core, so the
    /// edit opened on one entity and every other entity kept its pre-op
    /// coordinates (which also blew up the committed score).
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn global_scope_opens_edit_over_all_committed_entities() {
        use crate::history::CheckpointKind;
        use crate::session::EntityOrigin;
        use std::sync::Arc;

        let mut app = fresh_app();
        // Two committed entities.
        let e1 = app
            .store
            .insert_preview(mk_bulk(), "a".to_string(), EntityOrigin::Loaded);
        app.store
            .promote_preview(e1, CheckpointKind::PromotedPreview { entity: e1 }, None, None, "a")
            .expect("promote a");
        let e2 = app
            .store
            .insert_preview(mk_bulk(), "b".to_string(), EntityOrigin::Loaded);
        app.store
            .promote_preview(e2, CheckpointKind::PromotedPreview { entity: e2 }, None, None, "b")
            .expect("promote b");
        // A preview that is never promoted: it has no committed lane and so
        // must be filtered out of a whole-pose edit's lane set.
        let e_transient = app
            .store
            .insert_preview(mk_bulk(), "c".to_string(), EntityOrigin::Loaded);

        // AllEntities resolves to exactly the two committed lanes.
        let mut lanes = app.lanes_for_scope(&EditScope::AllEntities);
        lanes.sort_unstable();
        let mut expected = vec![e1, e2];
        expected.sort_unstable();
        assert_eq!(lanes, expected, "global scope spans committed lanes only");
        assert!(!lanes.contains(&e_transient), "transient preview has no lane");

        // Open ONE edit over the whole set, fan a multi-entity frame across
        // it, commit once. Every lane must carry the moved coordinates.
        let moved = glam::Vec3::new(3.0, 3.0, 3.0);
        let mut a1 = app.store.entity(e1).expect("e1").clone();
        for atom in a1.atom_set_mut() {
            atom.position = moved;
        }
        let mut a2 = app.store.entity(e2).expect("e2").clone();
        for atom in a2.atom_set_mut() {
            atom.position = moved;
        }
        let frame = molex::Assembly::from_arcs(vec![Arc::new(a1), Arc::new(a2)]);

        let rid = 7u64;
        app.store
            .begin_action(
                lanes,
                CheckpointKind::PluginOp {
                    plugin_id: "rosetta".to_string(),
                    op_id: "wiggle".to_string(),
                    display: "Wiggle".to_string(),
                },
                "Wiggle",
                rid,
            )
            .expect("begin multi-lane edit");
        assert!(
            super::apply_streaming_assembly(&mut app.store, &frame, None, rid),
            "frame applies across the locked lanes"
        );
        app.store.commit_action(rid).expect("commit");

        let head = app.store.head_assembly();
        for e in [e1, e2] {
            let ent = head.entity(e).expect("entity in head assembly");
            assert!(
                ent.positions().iter().all(|p| *p == moved),
                "entity {} was not updated by the whole-pose edit",
                e.raw()
            );
        }
    }

    /// An entity-scoped dispatch resolves to its named set, filtered to
    /// committed lanes: a resolved id without a lane drops out rather than
    /// refusing the whole multi-lane edit (`begin_action` is all-or-nothing).
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn entity_scope_filters_to_committed_lanes() {
        use crate::history::CheckpointKind;
        use crate::session::EntityOrigin;

        let mut app = fresh_app();
        let e1 = app
            .store
            .insert_preview(mk_bulk(), "a".to_string(), EntityOrigin::Loaded);
        app.store
            .promote_preview(e1, CheckpointKind::PromotedPreview { entity: e1 }, None, None, "a")
            .expect("promote a");
        let e_transient = app
            .store
            .insert_preview(mk_bulk(), "t".to_string(), EntityOrigin::Loaded);

        // The resolved set names a committed entity and a transient one;
        // only the committed lane survives the filter.
        let scope = EditScope::Entities(vec![e1, e_transient]);
        assert_eq!(app.lanes_for_scope(&scope), vec![e1]);
    }
}
