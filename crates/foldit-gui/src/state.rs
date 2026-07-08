use serde::{Deserialize, Serialize};

/// Top-level GUI state machine: the App-lifetime lifecycle phase.
///
/// Drives the
/// root-level routing in the frontend (loading screen for every pre-session
/// phase, in-puzzle UI once `InSession`). Owned by the backend; the frontend
/// renders whatever state the backend last set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum AppPhase {
    /// Fetching plugin weights / assets. There is no App-orchestrated
    /// download boundary today (download is plugin/runner-side), so this
    /// phase is defined but never entered; it reserves the slot for when
    /// download becomes App-driven.
    Downloading,
    /// Plugin discovery + bootstrap. Frontend shows the `LoadingScreen`.
    #[default]
    Initializing,
    /// Backend initialized, no session loaded; the user is at the menus.
    Landing,
    /// A structure / file / puzzle load is in progress. Frontend shows the
    /// `LoadingScreen`.
    LoadingSession,
    /// A session is live and the user can interact with it.
    InSession,
}

/// Which score the GUI displays and how it's framed.
///
/// `Game` is for tutorial / campaign / science puzzles where the user sees the
/// Foldit-style game score and a target. `Scientist` is for free-form work
/// (CLI file loads, drag/drop) where raw Rosetta scores are surfaced unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum ScoringMode {
    Game,
    #[default]
    Scientist,
}

/// Active puzzle context. Always populated; in Scientist mode only `mode`
/// and `title` are meaningful (target/starting are 0 and the GUI hides them).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PuzzleSection {
    pub mode: ScoringMode,
    pub puzzle_id: u32,
    pub title: String,
    pub starting_score: f64,
    pub target_score: f64,
    /// Latches true the first time `current_score` crosses `target_score` in
    /// Game mode. Reset to false on the next puzzle load. Frontend opens
    /// the victory modal on the false→true transition.
    pub complete: bool,
}

impl Default for PuzzleSection {
    fn default() -> Self {
        Self {
            mode: ScoringMode::default(),
            puzzle_id: 0,
            title: String::new(),
            starting_score: 0.0,
            target_score: 0.0,
            complete: false,
        }
    }
}

/// Live R-free readout shown under the score bar.
///
/// `value` is the current R-free (e.g. 0.28); `bonus` is the game-score
/// points its objective is currently contributing (e.g. 140). Rust computes
/// both; the frontend only renders.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, specta::Type)]
pub struct RFreeStatus {
    pub value: f32,
    pub bonus: f32,
}

/// Current score and validity state
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, specta::Type)]
pub struct ScoreSection {
    pub value: f64,
    pub invalid: bool,
    pub title: String,
    /// Live R-free readout for the subheader under the score bar, or `None`
    /// when unavailable. Filled by the host projector; the frontend renders
    /// it from the same section the score value rides.
    pub r_free: Option<RFreeStatus>,
}

impl Default for ScoreSection {
    fn default() -> Self {
        Self {
            value: 0.0,
            invalid: true,
            title: String::new(),
            r_free: None,
        }
    }
}

/// Per-residue segment-info panel payload.
///
/// Identity (`residue_number`/`chain`/`aa_three`/`aa_one`) and the
/// `ss_label` are computed once when the target is set and held fixed for
/// the lifetime of the open target; only `term_values` and `weighted` are
/// refreshed as scores stream. `term_values` is aligned to `term_names`
/// and is empty when no per-residue energies are available yet (right
/// after load, or on wasm). `anchor` is the open-time screen position of
/// the residue's CA atom (pixels, origin top-left), `None` when the atom
/// projects off-screen or behind the camera.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, specta::Type)]
pub struct SegmentInfo {
    pub residue_number: i32,
    pub chain: String,
    pub aa_three: String,
    pub aa_one: String,
    pub ss_label: String,
    pub term_names: Vec<String>,
    pub term_values: Vec<f32>,
    pub weighted: f32,
    pub anchor: Option<(f32, f32)>,
}

/// The open segment-info target plus the identity and secondary structure
/// cached at the moment it was set.
///
/// Identity and SS are resolved once (a single `recompute_ss()` over the
/// head assembly) when the target opens and held here for its lifetime; the
/// GUI projection rebuilds only the energies and the screen anchor on each
/// score tick, so a streaming score never re-runs DSSP.
#[derive(Debug, Clone)]
pub struct SegmentTarget {
    pub entity: molex::EntityId,
    pub residue: usize,
    pub residue_number: i32,
    pub chain: String,
    pub aa_three: String,
    pub aa_one: String,
    pub ss_label: String,
}

/// Human-readable secondary-structure label for the segment panel.
#[must_use]
pub fn ss_label(ss: Option<molex::SSType>) -> String {
    match ss {
        Some(molex::SSType::Helix) => "Helix",
        Some(molex::SSType::Sheet) => "Sheet",
        Some(molex::SSType::Coil) | None => "Loop",
    }
    .to_owned()
}

/// A tail-tip change the host should push to the webview this frame.
///
/// Drained by `GuiState::take_tail_update` only when the tip changed
/// since the last push; an unchanged tip yields `None` and the host pushes
/// nothing.
#[derive(Debug, Clone)]
pub enum TailUpdate {
    /// Move the tail tip to this screen position (pixels, origin top-left).
    Position(f32, f32),
    /// Hide the tail (the residue went off-screen, or the panel closed).
    Hide,
}

/// Backend-authoritative panel open/closed state plus per-panel screen
/// positions.
///
/// `open` lists the panels currently shown (by string id); a panel absent
/// from the list is closed. `positions` carries the dragged top-left
/// position of any panel the user has moved; a panel without an entry
/// renders at its layout default. The backend owns this so panel
/// visibility survives a reload and can be driven from either side.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, specta::Type, Default)]
pub struct PanelsSection {
    pub open: Vec<String>,
    pub positions: Vec<PanelPosition>,
}

/// Screen position of a single panel (top-left, pixels, origin top-left).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, specta::Type)]
pub struct PanelPosition {
    pub panel: String,
    pub x: f32,
    pub y: f32,
}

/// Per-entity residue selection state. One entry per entity that
/// currently has at least one residue selected; entities with empty
/// selections are absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type, Default)]
pub struct SelectionSection {
    pub entries: Vec<EntitySelection>,
}

/// Selected residues on a single entity.
///
/// `entity_id` is the raw id of
/// the owning entity (matches `SceneEntityInfo.entity_id`); `residues`
/// is the sorted set of selected residue indices, never empty by
/// invariant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct EntitySelection {
    pub entity_id: u32,
    pub residues: Vec<u32>,
}

/// Backend-authoritative puzzle high-score progress.
///
/// One entry per puzzle the player has ever scored on; `high_score` is the
/// best display score recorded for that puzzle (monotonic max). A puzzle
/// absent from `entries` has never been scored. The menu reads this to gate
/// category unlocks (a puzzle counts as complete once its high score is
/// positive).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, specta::Type, Default)]
pub struct ProgressSection {
    pub entries: Vec<ProgressEntry>,
}

/// One puzzle's best recorded score.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, specta::Type)]
pub struct ProgressEntry {
    pub puzzle_id: u32,
    pub high_score: f64,
}

/// Severity of a host-raised notification. Drives the frontend's toast
/// styling (`Error` renders red; `Warning` / `Info` render neutral).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub enum NotificationLevel {
    Info,
    Warning,
    Error,
}

/// A host-raised, user-facing notification.
///
/// `id` is a monotonic counter assigned by [`crate::GuiState::push_notification`].
/// The frontend dedups on it: it toasts only ids greater than the highest it
/// has already shown, so a reload replaying the retained list never re-toasts
/// an already-seen message. The backend keeps only the most recent entries;
/// dropping older ones is safe because their ids stay below the frontend's
/// high-water mark.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct Notification {
    // A session-monotonic counter; `u32` on the wire (specta forbids u64
    // export) is ample and never wraps in a session's worth of toasts.
    #[specta(type = u32)]
    pub id: u64,
    pub level: NotificationLevel,
    pub text: String,
}

/// View display options — opaque JSON blob serialized from engine Options.
///
/// The engine's `Options` struct (in viso crate) is the single source of truth.
/// This is just a pass-through for serialization to the JS GUI.
pub type ViewOptions = serde_json::Value;

/// Current view state
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct ViewSection {
    #[specta(type = specta_typescript::Unknown)]
    pub options: ViewOptions,
    /// JSON Schema describing UI-exposed options (generated by schemars).
    /// Static; set once at startup, never changes.
    #[specta(type = specta_typescript::Unknown)]
    pub options_schema: serde_json::Value,
    /// JSON Schema for the per-entity `DisplayOverrides` (generated by
    /// schemars). Parallel to `options_schema`; the GUI walks it to render
    /// the per-entity appearance body. Static; set once at startup.
    #[specta(type = specta_typescript::Unknown)]
    pub appearance_schema: serde_json::Value,
    /// Available view preset names (file stems from `assets/view_presets/`).
    pub available_presets: Vec<String>,
    /// Currently active preset name, if any.
    pub active_preset: Option<String>,
}

impl Default for ViewSection {
    fn default() -> Self {
        Self {
            options: serde_json::Value::Object(serde_json::Map::default()),
            options_schema: serde_json::Value::Null,
            appearance_schema: serde_json::Value::Null,
            available_presets: Vec::new(),
            active_preset: None,
        }
    }
}

/// Tutorial bubble payload pushed to the GUI.
///
/// Clean IPC twin of the Rust-side `Bubble`; only carries fields the
/// Tier-1 renderer reads. Anchor/branch/event metadata is intentionally
/// dropped here; Tier-2/3 will widen this as those features land.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextBubblePayload {
    pub text: String,
    pub color: Option<String>,
    pub image: Option<String>,
    pub buttons: Vec<TextBubbleButton>,
}

/// Button shown inside a [`TextBubblePayload`]. Tier-1 buttons close
/// the bubble locally on click; `goto` is reserved for Tier-2 sequence
/// advancement and is `None` until then.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextBubbleButton {
    pub text: String,
    pub goto: Option<i32>,
}

/// Transient UI state pushed from backend
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UISection {
    pub text_bubble: Option<TextBubblePayload>,
    pub fps: f32,
    pub log: String,
    pub selected_count: usize,
    /// Whether the tutorial-hint bubble is shown. Backend-authoritative so
    /// the toggle survives a reload. Defaults true.
    pub hints_visible: bool,
    /// Whether the window is in OS fullscreen. Backend-authoritative; on
    /// desktop the winit window is the source of truth and the host applies
    /// the change, on web the click handler drives the DOM fullscreen API.
    pub fullscreen: bool,
}

impl Default for UISection {
    fn default() -> Self {
        Self {
            text_bubble: None,
            fps: 0.0,
            log: String::new(),
            selected_count: 0,
            hints_visible: true,
            fullscreen: false,
        }
    }
}

/// Wire-side parameter type tag.
///
/// Mirrors the orchestrator's native
/// `ParamType` in shape; lives here because `foldit-gui` does not
/// depend on `foldit-runner` (the dep direction is one-way:
/// foldit-core / runner consumers convert into this shape).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub enum ParamType {
    Int,
    Float,
    Bool,
    String,
    Enum,
    Vec3,
}

/// Wire-side parameter value. Mirrors the orchestrator's native
/// `ParamValue`; serde-default (externally-tagged) encoding produces
/// `{ "Int": 5 }` etc. on the wire, which the TS mirror matches.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, specta::Type)]
pub enum ParamValue {
    Int(i32),
    Float(f32),
    Bool(bool),
    String(String),
    Vec3([f32; 3]),
}

/// Wire-side constraint shape. Mirrors the orchestrator's native
/// `ParamConstraint`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, specta::Type)]
pub enum ParamConstraint {
    /// Inclusive integer range.
    IntRange { min: i32, max: i32 },
    /// Inclusive float range.
    FloatRange { min: f32, max: f32 },
    /// Closed set of allowed string values.
    EnumValues(Vec<String>),
    /// Regex the string value must match.
    StringPattern(String),
}

/// Wire-side parameter schema. Carried on [`ActionInfo`] so the
/// frontend can render typed input forms (sliders, dropdowns, text
/// inputs) without re-walking the registration proto.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, specta::Type)]
pub struct ParamSpec {
    /// Map key in the dispatch envelope's `params` dictionary.
    pub name: String,
    /// Form-field label.
    pub display_name: String,
    /// Tooltip / help text.
    pub description: String,
    /// Value type tag.
    pub param_type: ParamType,
    /// Default value when the user leaves the field unset.
    pub default: Option<ParamValue>,
    /// Optional constraint driving rendering + validation.
    pub constraints: Option<ParamConstraint>,
}

/// One selectable entry in an action's button-list picker.
///
/// Each option is a self-contained dispatch: pressing it fires the op named
/// by `op_id` with `params` as the envelope's parameter map. `params` keys
/// and value tags match [`OpDispatch::params`], so an option dispatches as a
/// normal op envelope with no extra translation. `label`, `color`, `icon`,
/// and `hotkey` drive how the option renders inside the picker.
///
/// [`OpDispatch::params`]: crate::actions::OpDispatch
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, specta::Type)]
pub struct ActionOption {
    /// Display label for this option.
    pub label: String,
    /// Render color (frontend-interpreted CSS color string).
    pub color: String,
    /// Optional icon asset path (manifest-relative). `None` = no icon.
    pub icon: Option<String>,
    /// Optional hotkey corner-badge string (winit `KeyCode` spelling).
    /// `None` = no badge.
    pub hotkey: Option<String>,
    /// Op-id this option dispatches when chosen.
    pub op_id: String,
    /// Parameter values for the dispatch envelope, keyed by `ParamSpec.name`.
    /// Matches [`OpDispatch::params`] so the option fires as a normal op.
    ///
    /// [`OpDispatch::params`]: crate::actions::OpDispatch
    pub params: std::collections::HashMap<String, ParamValue>,
}

/// Information about an available action surfaced to the GUI.
///
/// One entry per row in the orchestrator's
/// [`foldit_runner::orchestrator::CatalogEntry`] join. `display` and
/// `icon_path` come from the plugin manifest's `[[buttons]]` array;
/// `enabled` reflects the current orchestrator lock state (the running
/// state lives in [`ActionsSection::running`], keyed per live instance);
/// `params` carries the typed schema declared on the plugin's
/// `PluginOp.params` array (empty for click-to-fire ops).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, specta::Type)]
pub struct ActionInfo {
    /// Op-id the GUI dispatches when the button is pressed. Matches an
    /// entry in the owning plugin's `PluginRegistration.operations`.
    pub op_id: String,
    /// Owning plugin id (matches `PluginRegistration.id`). Op-ids are
    /// protocol-globally unique, so this is not a routing namespace, but
    /// it is the grouping key: the frontend buckets actions by `plugin_id`
    /// and joins it against [`PluginGroupInfo::plugin_id`] to title and
    /// order each group box.
    pub plugin_id: String,
    /// Display label.
    pub display: String,
    /// Manifest-relative icon asset path (relative to the owning plugin
    /// directory). The GUI builds its fetch URL as
    /// `/plugins/<plugin_id>/<icon_path>`. A value beginning with `builtin:`
    /// instead names a built-in GUI icon resolved by the frontend's icon set,
    /// letting native actions ship a glyph with no plugin asset file.
    pub icon_path: String,
    /// True when the op can be dispatched in the current lock state.
    pub enabled: bool,
    /// Optional hotkey corner-badge string (winit `KeyCode` spelling,
    /// e.g. `"KeyW"`). `None` = no badge. Pressing the key does not
    /// dispatch the op yet.
    pub hotkey: Option<String>,
    /// Optional hover tooltip. The GUI falls back to `display` when
    /// `None`.
    pub tooltip: Option<String>,
    /// Typed parameter schema (empty for click-to-fire ops). Drives
    /// schema-driven panel widgets without an extra round-trip.
    pub params: Vec<ParamSpec>,
    /// Button-list picker entries. Empty => the action renders as a normal
    /// button that dispatches `op_id` directly; non-empty => the action
    /// renders a host-emitted picker where each [`ActionOption`] is a full
    /// dispatch in its own right.
    pub options: Vec<ActionOption>,
}

/// Per-plugin button-group metadata.
///
/// Parallel side-table to [`ActionsSection::available`]: one entry per
/// discovered plugin, joined to the per-button list on `plugin_id`. Carries
/// the manifest-declared group title and left-to-right sort key so the
/// frontend can title and order each plugin's group box. Both are optional;
/// the frontend derives a title from `plugin_id` and sorts unordered groups
/// last when absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type, Default)]
pub struct PluginGroupInfo {
    /// Join key against [`ActionInfo::plugin_id`].
    pub plugin_id: String,
    /// Human display name for the group header. `None` → derive from id.
    pub name: Option<String>,
    /// Left-to-right sort key. `None` → sorts after every explicit order.
    pub order: Option<u32>,
}

/// One plugin-contributed custom panel, served on demand via the
/// `PanelsCatalog` request.
///
/// `entry` is the manifest-relative path (relative to the owning plugin
/// directory) to the panel's ES-module entrypoint the frontend dynamically
/// imports to mount the panel; `icon_path` is the manifest-relative
/// launcher-icon path. The frontend builds both asset URLs as
/// `/plugins/<plugin_id>/<path>`. `position_x` / `position_y` are the
/// panel's layout-default screen position, distinct from the dragged
/// position the frontend tracks in [`PanelsSection`]. All panels are custom
/// (module-rendered); there is no panel kind.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, specta::Type)]
pub struct PanelInfo {
    /// Owning plugin id.
    pub plugin_id: String,
    /// Panel id, unique within the plugin.
    pub id: String,
    /// Display title for the panel's title bar.
    pub title: String,
    /// Panel width in pixels.
    pub width: u32,
    /// Default panel x position in pixels (top-left origin).
    pub position_x: f32,
    /// Default panel y position in pixels (top-left origin).
    pub position_y: f32,
    /// Manifest-relative path to the panel's ES-module entrypoint.
    pub entry: String,
    /// Manifest-relative path to the panel's launcher icon.
    pub icon_path: String,
    /// Optional hover tooltip; consumer falls back to `title`.
    pub tooltip: Option<String>,
}

/// One plugin-contributed settings tab, served on demand via the
/// `SettingsCatalog` request.
///
/// `schema_asset_path` is relative to the owning plugin directory; the
/// frontend fetches the JSON-schema asset at `/plugins/<plugin_id>/<path>`
/// and renders a write-only form from it. Each field edit dispatches
/// `on_update_op` with that single field as a typed parameter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct SettingsTabInfo {
    /// Owning plugin id.
    pub plugin_id: String,
    /// Display title for the tab.
    pub name: String,
    /// Plugin-relative path to the tab's JSON-schema asset.
    pub schema_asset_path: String,
    /// Op-id dispatched on each field edit, carrying the changed field.
    pub on_update_op: String,
}

/// Live download progress for a plugin whose weights are streaming in.
///
/// `fraction` is 0..1 (0 at kick, 1 at completion); `stage` is a
/// human-readable label for the current phase of the download.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, specta::Type)]
pub struct DownloadProgress {
    pub fraction: f32,
    pub stage: String,
}

/// Live progress for an in-flight b-factor refine.
///
/// Determinate: `fraction` is 0..1 (0 at kick, 1 at completion) and `label`
/// is a ready-to-render phrase (e.g. "Refining B-factors (cycle 2/5)"). Rust
/// computes both; the frontend only renders.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, specta::Type)]
pub struct RefineProgress {
    pub fraction: f32,
    pub label: String,
}

/// One currently-running action, projected from a held lock (a live
/// orchestrator stream, or the native refine holding the global lock). The
/// single source of truth for the running UI: the per-instance cancel toasts
/// and each action button's cancel state both derive from this list, so a
/// button is "running" exactly when an entry here is `global` or locks the
/// focused entity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, specta::Type)]
pub struct RunningAction {
    /// Dispatch request-id of the backing stream, used to cancel this one
    /// instance. `None` for the native refine, which is not a stream and is
    /// cancelled through the `refine` flag on [`crate::AppCommand::CancelAction`].
    /// `u32` on the wire (specta forbids u64); request-ids are a monotonic
    /// counter that never approaches the u32 ceiling.
    #[specta(type = Option<u32>)]
    pub request_id: Option<u64>,
    /// Op-id of the running action (joins to [`ActionInfo::op_id`]).
    pub op_id: String,
    /// Display label for the toast.
    pub display: String,
    /// Raw ids of the entities this action's lock holds. Empty when `global`.
    pub entities: Vec<u32>,
    /// True when the action holds the global lock (no specific entity): it
    /// blocks, and is blocked by, every other action.
    pub global: bool,
}

/// Available actions and their current state
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, specta::Type, Default)]
pub struct ActionsSection {
    pub available: Vec<ActionInfo>,
    /// Currently-running actions, one per held lock. Drives the per-instance
    /// cancel toasts and the buttons' running/cancel state.
    pub running: Vec<RunningAction>,
    /// Raw id of the focused entity, or `None` for whole-session focus. Lets
    /// the frontend decide a button's running/cancel state: a button shows
    /// cancel when a `running` entry is `global` or locks this focused entity.
    pub focused_entity_id: Option<u32>,
    /// Per-plugin group metadata, joined to `available` on `plugin_id`.
    pub groups: Vec<PluginGroupInfo>,
    /// The `op_id` of the currently-open action picker, or `None` when no
    /// picker is open. Rides the same `"actions"` wire push as `available`
    /// and `groups`; the frontend renders one picker open at a time from it.
    pub open_picker: Option<String>,
    /// Per-plugin live download progress, keyed by `plugin_id`. Empty when
    /// nothing is downloading; a plugin's host-injected download button reads
    /// its entry to render a progress fill.
    pub download_progress: std::collections::HashMap<String, DownloadProgress>,
    /// Live progress for the single in-flight b-factor refine, or `None` when
    /// nothing is refining. Rides the same `"actions"` wire push as its
    /// siblings; the frontend renders one determinate progress bar from it.
    pub refine_progress: Option<RefineProgress>,
}

/// Information about a single entity in the scene
// `appearance_values` is a `serde_json::Value`, which is not `Eq`, so `Eq`
// cannot be derived alongside `PartialEq` here.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, specta::Type)]
pub struct SceneEntityInfo {
    pub entity_id: u32,
    pub label: String,
    pub molecule_type: String,
    #[specta(type = u32)]
    pub atom_count: usize,
    #[specta(type = u32)]
    pub residue_count: usize,
    /// True when the entity carries any non-empty per-entity appearance
    /// override (resolved display values are surfaced separately).
    pub has_overrides: bool,
    /// The entity's resolved display values: global display options with
    /// this entity's overrides overlaid, serialized flat by field name so a
    /// values-bound appearance panel can read each control's current setting.
    #[specta(type = specta_typescript::Unknown)]
    pub appearance_values: serde_json::Value,
}

/// Scene entity listing for the GUI.
///
/// `focused_entity` mirrors viso's `Focus`: `Some(eid)` when the user
/// has zoomed/cycled to a specific entity, `None` for whole-session
/// focus. Drives the `HistoryPanel` view choice (river vs swim lanes).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SceneSection {
    pub entities: Vec<SceneEntityInfo>,
    pub focused_entity: Option<u32>,
}

/// Loading/progress state
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct LoadingSection {
    pub progress: Option<f32>,
    pub puzzle_loaded: bool,
}

// `HistorySection` and per-checkpoint / per-snapshot wire payloads
// live in [`crate::wire`] and are re-exported from `lib.rs`.
