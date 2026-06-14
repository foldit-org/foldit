use foldit_gui::AppPhase;
use molex::entity::molecule::id::EntityId;

use crate::app::App;
use crate::session::Puzzle;
#[cfg(not(target_arch = "wasm32"))]
use foldit_gui::DirtyFlags;

impl App {
    pub fn handle_app_command(&mut self, command: foldit_gui::AppCommand) {
        use foldit_gui::AppCommand;

        // History-side commands take &mut self (no engine borrow held).
        if let AppCommand::History { cmd } = command {
            self.run_history_command(&cmd);
            return;
        }

        // Bubble cursor advance is engine-independent.
        if let AppCommand::AdvanceBubble { back } = command {
            self.advance_bubble(back);
            return;
        }

        // Focus is pure session state; no engine borrow needed.
        if let AppCommand::SetFocus { entity_id } = command {
            let focus =
                entity_id.map_or(viso::Focus::All, |raw| viso::Focus::Entity(EntityId::from_raw(raw)));
            self.store.set_focus(focus);
            return;
        }

        // Per-entity appearance is authoritative on the session; the render
        // projector pushes it into the engine working copy on the emitted
        // `EntityAppearanceChanged`. No engine borrow needed here, so it is
        // handled before the engine-presence guard like focus.
        if let AppCommand::SetEntityAppearance {
            entity_id,
            field,
            value,
        } = command
        {
            self.store
                .set_entity_appearance_field(EntityId::from_raw(entity_id), &field, &value);
            return;
        }

        // Clearing an entity's whole appearance override is likewise pure
        // session state; handled before the engine-presence guard for the
        // same reason as the per-field merge above.
        if let AppCommand::ClearEntityAppearance { entity_id } = command {
            self.store
                .clear_entity_appearance(EntityId::from_raw(entity_id));
            return;
        }

        if self.engine.is_none() {
            return;
        }

        // Engine borrow is taken per-arm now (LoadStructure / LoadPuzzle
        // need to release the borrow before `self.tick(0.0)`, which is
        // how the render projector republishes after a load).
        match command {
            AppCommand::LoadStructure { path } => self.handle_load_structure(&path),
            AppCommand::LoadPuzzle { puzzle_id } => self.handle_load_puzzle(puzzle_id),
            AppCommand::SetViewOptions { options } => {
                // A manual edit: store the App-owned options, clear the active
                // preset (manually-set options no longer match a named preset),
                // and latch the player-touched flag so future loads keep this
                // choice instead of re-seeding from the puzzle/Default preset.
                // The tick applies the options to the engine (+ raises VIEW) off
                // the `ViewOptionsChanged` we note when something actually
                // changed; an idempotent set is silent.
                match serde_json::from_value::<viso::options::VisoOptions>(options) {
                    Ok(opts) => {
                        let changed = self.view_options != opts || self.active_preset.is_some();
                        self.view_options = opts;
                        self.active_preset = None;
                        self.view_settings_touched = true;
                        if changed {
                            self.store.note_view_options_changed();
                        }
                    }
                    Err(e) => log::error!("Failed to deserialize view options: {e}"),
                }
            }
            AppCommand::LoadViewPreset { name } => {
                // An explicit player preset pick: latch the touched flag (so it
                // persists across later loads) and apply the preset now.
                self.view_settings_touched = true;
                #[cfg(not(target_arch = "wasm32"))]
                self.apply_view_preset_to_session(&name);
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
            AppCommand::History { .. }
            | AppCommand::AdvanceBubble { .. }
            | AppCommand::SetFocus { .. }
            | AppCommand::SetEntityAppearance { .. }
            | AppCommand::ClearEntityAppearance { .. } => {
                // Handled in the early-return block above. The match is
                // exhaustive over `AppCommand`: a new variant
                // without a handler is a compile error.
            }
        }
    }

    /// Free-form file load (Scientist mode). Ingest entities, set
    /// metadata, then tick + fit the camera (tick is how the render
    /// projector republishes - the `SessionUpdate` stream carries `PreviewAdded`s and
    /// `HeadMoved`s from `load_entity_into_history`).
    fn handle_load_structure(&mut self, path: &str) {
        self.set_app_phase(AppPhase::LoadingSession);
        // Drop any prior plugin sessions (warm workers stay up) so the
        // bring-up armed below re-`Init`s every plugin against this load.
        self.runner_client.reset_for_new_structure();
        let loaded = match crate::puzzle::load_file_as_entities(path) {
            Ok((entities, name)) => {
                log::info!("Loaded structure via IPC: {name}");
                for entity in entities {
                    let _ = self.store.load_entity_into_history(entity, &name);
                }
                // Free-form load: set the title and drop any puzzle
                // objective + tutorial bubbles through the create seam.
                // `start` emits `PuzzleChanged` (via `clear_puzzle`) when
                // there was a puzzle to clear, which the tick turns into
                // PUZZLE dirty. A scientist→scientist reload where
                // `clear_puzzle` is a no-op emits nothing, so the puzzle
                // panel's title refresh rides the full populate below.
                self.store.start(name, None);
                // Seed the view options from the Default preset on a fresh app
                // (so the panel reflects the true coloring and the whole-blob
                // option emit is faithful); once the player has touched any view
                // setting, skip the seed and keep their persisted choice. Either
                // way, push the persisted-or-seeded options to the freshly-reset
                // engine. The funnel does the eager set + note itself when it
                // seeds; the touched branch does it from the App-owned options.
                #[cfg(not(target_arch = "wasm32"))]
                if self.view_settings_touched {
                    self.reapply_view_options_to_engine();
                } else {
                    self.apply_view_preset_to_session("Default");
                }
                // Camera frames on the focused geometry; the default
                // `StartupCamera::Fit` is what the bring-up terminal applies.
                true
            }
            Err(e) => {
                log::error!("Failed to load structure '{path}': {e}");
                false
            }
        };
        // Hand bring-up to the startup state-machine: it `Init`s every warm
        // plugin against the just-loaded structure, adopts each normalized
        // pose, runs the first score, then flips into the session (clearing
        // the loading screen and raising the full populate). Without this the
        // plugins stay session-less, the op registry is empty (no actions, no
        // clashes/voids), and the backbone never scores (renders gray).
        self.arm_session_bringup(loaded);
    }

    /// Tutorial / campaign puzzle load (Game mode). Ingest entities and
    /// metadata, then tick + snap + apply the puzzle's saved pose.
    fn handle_load_puzzle(&mut self, puzzle_id: u32) {
        self.set_app_phase(AppPhase::LoadingSession);
        // Entity display name for the loaded molecules: the outgoing
        // session title (captured before `reset`, which leaves it intact).
        let title = self.store.title().to_owned();
        self.store.reset();
        self.runner_client.reset_for_new_structure();
        // Topology swap: `Session::reset` already cleared the selection
        // (entity ids from the outgoing assembly can collide numerically
        // with the incoming ones without referring to the same entities).
        // Emit `SelectionChanged` explicitly so the tick re-pushes the
        // now-empty highlight to viso and raises SELECTION dirty; `reset`
        // itself only emits `HeadMoved`.
        self.store.clear_selection();
        // viso's own per-entity score and appearance maps have an id-reuse
        // hole: replace_assembly now preserves both across a swap (so a
        // settling preview doesn't flash the survivors gray and user-authored
        // per-entity appearance persists), reconciling membership by id. A
        // puzzle reload restarts the entity allocator, so the new puzzle's ids
        // collide with the outgoing ones and would inherit their colors and
        // overrides; clear both viso maps explicitly here.
        if let Some(engine) = self.engine.as_mut() {
            engine.clear_scores();
            engine.clear_all_appearance();
        }

        let loaded = match crate::puzzle::load_puzzle_structure(puzzle_id) {
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
                let weight_patch = puzzle_data.weights.clone();
                let objectives = puzzle_data.objectives.clone();
                self.store.start(
                    puzzle_data.name.clone(),
                    Some(Puzzle {
                        id: puzzle_id,
                        start_energy: puzzle_data.start_energy,
                        completion_energy: puzzle_data.completion_score,
                        weight_patch,
                        objectives,
                        bubbles,
                        current_bubble,
                    }),
                );

                // Overlay the puzzle's scorefunction weight patch onto the
                // host's display weight map so `weighted_total` includes the
                // patched terms (e.g. `envsmooth`, weight-zero in stock
                // ref2015_cart). Rebuild from the default base each puzzle load
                // rather than insert-in-place: the base survives `reset`, so an
                // earlier puzzle's patch would otherwise persist into a puzzle
                // that declares none. On a base-load failure, fall back to
                // overlaying the patch onto whatever weights are currently held
                // (degraded, but the patched term still weights).
                #[cfg(not(target_arch = "wasm32"))]
                {
                    let mut weights = match crate::scores::load_default_term_weights()
                    {
                        Ok(base) => base,
                        Err(e) => {
                            log::error!(
                                "[App] puzzle {puzzle_id}: failed to reload base \
                                 score-term weights for patch overlay: {e}"
                            );
                            self.store.term_weights().clone()
                        }
                    };
                    if let Some(patch) = puzzle_data.weights.as_ref() {
                        for (name, &w) in patch {
                            weights.insert(name.clone(), w);
                        }
                        log::info!(
                            "[App] puzzle {puzzle_id}: applied {} score-term \
                             weight patch entr{}",
                            patch.len(),
                            if patch.len() == 1 { "y" } else { "ies" }
                        );
                    }
                    self.store.set_term_weights(weights);
                }

                // A puzzle may pin its own view preset; otherwise fall back to
                // the Default preset so the view panel reflects the true state.
                // Seed only on a fresh app; once the player has touched any view
                // setting, skip the seed and keep their persisted choice. Either
                // way the freshly-reset engine is given the persisted-or-seeded
                // options. The tick(0.0) below drains the noted
                // `ViewOptionsChanged` and re-applies them to the engine.
                #[cfg(not(target_arch = "wasm32"))]
                if self.view_settings_touched {
                    self.reapply_view_options_to_engine();
                } else {
                    self.apply_view_preset_to_session(
                        puzzle_data.view_preset.as_deref().unwrap_or("Default"),
                    );
                }

                let ss_override = puzzle_data.ss_override;
                let cam = &puzzle_data.camera;
                // GPU camera is f32; puzzle coords are f64.
                #[allow(clippy::cast_possible_truncation)]
                let cam_eye =
                    glam::Vec3::new(cam.eye[0] as f32, cam.eye[1] as f32, cam.eye[2] as f32);
                // GPU camera is f32; puzzle coords are f64.
                #[allow(clippy::cast_possible_truncation)]
                let cam_up = glam::Vec3::new(cam.up[0] as f32, cam.up[1] as f32, cam.up[2] as f32);

                let mut ids: Vec<EntityId> = Vec::new();
                for entity in puzzle_data.entities {
                    if let Some(id) =
                        self.store.load_entity_into_history(entity, &title)
                    {
                        ids.push(id);
                    }
                }

                // Defer the camera + SS to the bring-up terminal so they apply
                // to the settled (post-normalize) geometry: the puzzle's saved
                // pose instead of a focus fit, and its pinned SS after the
                // cartoon rebake. The topology swap itself rides the
                // `SessionUpdate` stream (tick's render projector picks
                // `replace_assembly` because the id set differs from the
                // post-reset empty publish).
                self.startup_camera =
                    StartupCamera::PuzzlePose { eye: cam_eye, up: cam_up };
                self.startup_ss_override = ss_override
                    .and_then(|ss| ids.first().map(|&first_id| (first_id.raw(), ss)));
                true
            }
            Err(e) => {
                log::error!("Failed to load puzzle {puzzle_id}: {e}");
                false
            }
        };
        // Hand bring-up to the startup state-machine (see handle_load_structure):
        // it `Init`s + normalizes + scores every warm plugin against the loaded
        // puzzle, then flips into the session. The puzzle's saved camera + SS
        // stashed above are applied by the terminal once the geometry settles.
        self.arm_session_bringup(loaded);
    }

    /// Load a named view preset's options off disk and install them as the
    /// App-owned active options + active preset. App owns the view state (so
    /// it persists across a topology swap); the seed paths route through here
    /// so the view panel reflects the true coloring rather than bare defaults
    /// while the engine renders the boot preset, and the panel's whole-blob
    /// option emit does not clobber the engine's coloring. When an engine is
    /// attached it is synced immediately; the tick also re-applies the options
    /// off the noted `ViewOptionsChanged`. A missing presets dir or an
    /// unreadable preset file is logged and left as a no-op (the App keeps its
    /// current options).
    #[cfg(not(target_arch = "wasm32"))]
    pub(in crate::app) fn apply_view_preset_to_session(&mut self, name: &str) {
        let Some(dir) = self.host.view_presets_dir() else {
            return;
        };
        let path = dir.join(format!("{name}.toml"));
        let opts = match viso::options::VisoOptions::load(&path) {
            Ok(opts) => opts,
            Err(e) => {
                log::error!("Failed to load view preset '{name}': {e}");
                return;
            }
        };
        self.view_options = opts.clone();
        self.active_preset = Some(name.to_owned());
        // Eager engine sync so the engine has the options before this load's
        // `tick(0.0)`; the noted `ViewOptionsChanged` re-applies them through
        // the tick seam too (idempotent).
        if let Some(engine) = self.engine.as_mut() {
            engine.set_options(opts);
        }
        self.store.note_view_options_changed();
    }

    /// Push the persisted App-owned view options to the (freshly-reset) engine
    /// and note the change so the tick re-applies them. Used by the load paths
    /// when the player has already touched a view setting, so the preset seed
    /// is skipped but the engine still receives the persisted options on every
    /// load. The eager set mirrors the funnel's own engine sync.
    #[cfg(not(target_arch = "wasm32"))]
    pub(in crate::app) fn reapply_view_options_to_engine(&mut self) {
        let opts = self.view_options.clone();
        if let Some(engine) = self.engine.as_mut() {
            engine.set_options(opts);
        }
        self.store.note_view_options_changed();
    }

    // ── Tutorial-bubble cursor ──

    /// Step the tutorial-bubble cursor on the session. The cursor lives on
    /// `Session` now; this forwards and the emitted `BubbleChanged` is
    /// turned into `TEXT_BUBBLE` dirty by the tick, which re-pushes the new
    /// head. Forward saturates one past the end (the GUI then clears);
    /// back saturates at 0.
    fn advance_bubble(&mut self, back: bool) {
        self.store.advance_bubble(back);
    }

    /// Shut down backends and scene processor.
    pub fn shutdown(&mut self) {
        self.runner_client.shutdown();
        if let Some(engine) = &mut self.engine {
            engine.shutdown();
        }
    }
}

/// Non-blocking startup phase, advanced once per frame by
/// [`App::advance_startup`]. The machine accumulates per-plugin worker
/// completions across frames (the polls are stateless: each returns only
/// what completed THIS frame) against an `expected` set, so the host
/// renders the loading screen while bring-up proceeds.
#[cfg(not(target_arch = "wasm32"))]
pub(in crate::app) enum StartupPhase {
    /// Not started; the host has not called `begin_startup` yet.
    Idle,
    /// Full session bring-up: warming plugins, with the bootstrap structure
    /// path stashed for the parse once every warm has connected.
    Warming {
        expected: std::collections::BTreeSet<String>,
        connected: std::collections::BTreeSet<String>,
        path: String,
    },
    /// Plugin `Init` sessions in flight; accumulating adopted plugin ids.
    Initializing {
        expected: std::collections::BTreeSet<String>,
        adopted: std::collections::BTreeSet<String>,
    },
    /// Per-plugin load-time normalize invokes in flight; accumulating
    /// applied plugin ids.
    Normalizing {
        expected: std::collections::BTreeSet<String>,
        applied: std::collections::BTreeSet<String>,
    },
    /// First score requested; waiting for the head breakdown to stamp.
    Scoring,
    /// No bootstrap structure: Landing is already shown and the warms are
    /// finishing in the background so a later file-load finds connected
    /// workers.
    WarmingForLanding {
        expected: std::collections::BTreeSet<String>,
        connected: std::collections::BTreeSet<String>,
    },
    /// Bring-up complete; the machine is inert.
    Done,
}

/// How the startup-machine terminal ([`App::enter_session_from_startup`])
/// frames the camera once the geometry has settled (post-normalize). The
/// launch and free-form structure paths fit on the focused geometry; a
/// puzzle load stashes its saved pose so the terminal honors it instead of
/// fitting. Consumed (reset to [`StartupCamera::Fit`]) when the terminal runs.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Default)]
pub(in crate::app) enum StartupCamera {
    /// Frame on the focused geometry (launch + free-form structure load).
    #[default]
    Fit,
    /// Apply a puzzle's saved eye/up anchored on the post-normalize
    /// centroid; falls back to a focus fit when no centroid is available.
    PuzzlePose { eye: glam::Vec3, up: glam::Vec3 },
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
#[must_use]
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

#[cfg(test)]
#[cfg(not(target_arch = "wasm32"))]
mod preset_tests {
    use crate::HostResources;
    use crate::app::App;
    use std::io;
    use std::path::{Path, PathBuf};
    use viso::options::ColorScheme;

    /// Host stub whose `view_presets_dir` points at the repository's shipped
    /// `assets/view_presets`, so the helper reads the real Default preset.
    struct PresetHost {
        presets_dir: PathBuf,
    }

    impl HostResources for PresetHost {
        fn read_file(&self, _path: &str) -> io::Result<Vec<u8>> {
            Err(io::Error::new(io::ErrorKind::NotFound, "test stub"))
        }
        fn view_presets_dir(&self) -> Option<&Path> {
            Some(&self.presets_dir)
        }
        fn initial_structure_path(&self) -> Option<String> {
            None
        }
    }

    /// A non-default `VisoOptions`, distinguishable from the default by a
    /// single toggle. Used to exercise the change-guard on the App writers.
    fn mk_non_default_options() -> viso::options::VisoOptions {
        let mut opts = viso::options::VisoOptions::default();
        opts.debug.show_normals = true;
        opts
    }

    /// After applying the Default preset through the funnel, the App-owned
    /// view options carry the preset's coloring (Score, not the bare-default
    /// Entity) and record it as the active preset. The App is what the view
    /// panel binds, so the menu shows the true state rather than a stale
    /// default. The engine-attached re-sync and the full GPU load path are
    /// exercised by the parent's runtime confirmation.
    #[test]
    fn default_preset_seeds_view_options() {
        let presets_dir =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/view_presets");
        let mut app = App::new(Box::new(PresetHost { presets_dir }));

        // A fresh app is at the bare default (Entity coloring, no active
        // preset, untouched) - the state the bug left the menu in while the
        // engine rendered the boot preset.
        assert_eq!(
            app.view_options().display.backbone_color_scheme(),
            ColorScheme::Entity,
        );
        assert!(app.active_preset().is_none());
        assert!(!app.view_settings_touched);

        app.apply_view_preset_to_session("Default");

        assert_eq!(
            app.view_options().display.backbone_color_scheme(),
            ColorScheme::Score,
            "Default preset colors by Score, not bare-default Entity",
        );
        assert_eq!(app.active_preset(), Some("Default"));
    }

    /// The funnel records the preset name AND notes a single
    /// `ViewOptionsChanged` on the `SessionUpdate` stream so the tick applies
    /// the options to the engine and refreshes the VIEW panel. Coverage that
    /// moved off the deleted `Session::apply_preset`.
    #[test]
    fn funnel_records_preset_and_notes_one_change() {
        let presets_dir =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/view_presets");
        let mut app = App::new(Box::new(PresetHost { presets_dir }));
        let _ = app.store.take_updates();

        app.apply_view_preset_to_session("Default");

        assert_eq!(app.active_preset(), Some("Default"));
        assert!(
            matches!(
                app.store.take_updates().as_slice(),
                [crate::session::SessionUpdate::ViewOptionsChanged]
            ),
            "the funnel notes exactly one ViewOptionsChanged",
        );
    }

    /// The view options + active preset live on `App` and survive
    /// `Session::reset` (a topology swap). This is the inverted reset
    /// semantics: a player's display choices carry from one structure to the
    /// next instead of zeroing per session. Coverage that moved off the
    /// deleted `Session::reset` view-options block.
    #[test]
    fn view_options_persist_across_session_reset() {
        let presets_dir =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/view_presets");
        let mut app = App::new(Box::new(PresetHost { presets_dir }));

        app.view_options = mk_non_default_options();
        app.active_preset = Some("warm".to_owned());

        app.store.reset();

        assert_eq!(
            app.view_options(),
            &mk_non_default_options(),
            "App view options survive a topology swap",
        );
        assert_eq!(
            app.active_preset(),
            Some("warm"),
            "App active preset survives a topology swap",
        );
    }
}
