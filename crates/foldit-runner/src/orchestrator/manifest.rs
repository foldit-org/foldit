//! Plugin manifest schema (`plugin.toml`).
//!
//! Every plugin is a directory containing a `plugin.toml` declaring its
//! identity and runtime kind. Optional per-kind sections override the
//! default conventions.
//!
//! ## Schema
//!
//! ```toml
//! id = "foundry"
//! kind = "python"          # python | native | wasm
//!
//! [python]                 # only meaningful if kind = "python"
//! entry = "model_plugins.foundry"   # default: same as id
//! env = "foundry"                   # default: same as id
//!
//! [native]                 # only meaningful if kind = "native"
//! binary = "bin/foundry"            # default: lib{id}.{dylib|so|dll}
//! args = ["--config", "..."]        # default: []
//!
//! [wasm]                   # only meaningful if kind = "wasm"
//! module = "foundry.wasm"           # default: {id}.wasm
//!
//! [config]                 # plugin-private; passed verbatim to the plugin
//! checkpoint_dir = "..."
//! ```
//!
//! Sections that don't match the declared `kind` are silently ignored.
//! The kind-specific loader is responsible for resolving asset names
//! against the plugin's [`crate::orchestrator::assets::PluginAssets`].

use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;

/// Top-level manifest deserialized from a plugin's `plugin.toml`.
#[derive(Debug, Clone, Deserialize)]
pub struct PluginManifest {
    /// Stable plugin id. Matches `PluginRegistration.id` returned by the
    /// plugin at Init time. Also used to derive default file names
    /// (`{id}.wasm`, `lib{id}.dylib`, etc.).
    pub id: String,

    /// Runtime kind. Selects which spawn primitive the orchestrator uses.
    pub kind: PluginKind,

    /// Human display name for the plugin's button group header. Omitted →
    /// the GUI derives a title from the `id`. Carried to the frontend
    /// alongside the per-button catalog so the group box can be titled.
    #[serde(default)]
    pub name: Option<String>,

    /// Left-to-right sort key for the plugin's button group. Lower sorts
    /// first; absent sorts after every explicit order, with plugin id as
    /// the tie-break. Carried to the frontend to order the group boxes.
    #[serde(default)]
    pub order: Option<u32>,

    /// Whether this plugin fits against an electron-density map. When
    /// `true`, the host includes the loaded density map in the plugin's
    /// init payload; density-agnostic plugins never receive it. Defaults
    /// `false`.
    #[serde(default)]
    pub uses_density: bool,
    /// Whether this plugin produces the electron-density map. The host inits
    /// `provides_density` plugins first, reads their map through the
    /// well-known `density` query, then inits `uses_density` plugins with it.
    /// Defaults `false`.
    #[serde(default)]
    pub provides_density: bool,

    /// Python-specific overrides. Read only when `kind == Python`.
    pub python: Option<PythonSection>,

    /// Native-specific overrides. Read only when `kind == Native`.
    pub native: Option<NativeSection>,

    /// Wasm-specific overrides. Read only when `kind == Wasm`.
    pub wasm: Option<WasmSection>,

    /// Plugin-private config map. Passed verbatim to the plugin's
    /// constructor (Python `Plugin.__init__(config)`, Native plugin
    /// startup args, etc.). The protocol forbids init params on the
    /// wire — this is the host-process knob.
    #[serde(default)]
    pub config: HashMap<String, String>,

    /// User-facing buttons this plugin contributes.
    ///
    /// Each entry binds one of the plugin's bridge-registered op-ids
    /// to a display name + icon path. The icon is shipped
    /// manifest-relative; the frontend builds the
    /// `/plugins/<plugin_id>/<path>` URL. Op-ids the bridge registers
    /// but the manifest omits stay dispatchable -- they just don't
    /// render as buttons.
    #[serde(default)]
    pub buttons: Vec<ButtonEntry>,

    /// Custom UI panels this plugin contributes.
    ///
    /// Each entry names an ES-module entrypoint the frontend dynamically
    /// imports to render a plugin-owned panel. The module + icon paths
    /// are shipped manifest-relative; the frontend builds the
    /// `/plugins/<plugin_id>/<path>` URLs.
    #[serde(default)]
    pub panels: Vec<PanelEntry>,

    /// Settings tabs this plugin contributes.
    ///
    /// Each entry names a JSON schema the frontend renders as a settings
    /// form, bound to an op the plugin invokes when the form is applied.
    /// The schema path is shipped manifest-relative; the frontend builds
    /// the `/plugins/<plugin_id>/<path>` URL.
    #[serde(default)]
    pub settings: Vec<SettingsEntry>,
}

/// A single plugin-contributed settings tab declaration.
///
/// Lives in `plugin.toml` under `[[settings]]`. The frontend renders
/// `schema_asset_path` as a settings form; the path is shipped
/// manifest-relative and the frontend builds the
/// `/plugins/<plugin_id>/<path>` URL. Applying the form invokes
/// `on_update_op` on the plugin.
#[derive(Debug, Clone, Deserialize)]
pub struct SettingsEntry {
    /// Display name for the settings tab.
    pub name: String,
    /// JSON schema path, relative to the plugin directory. Shipped
    /// manifest-relative; the frontend builds the
    /// `/plugins/<plugin_id>/<path>` URL and fetches it to render the
    /// settings form.
    pub schema_asset_path: PathBuf,
    /// Op-id the plugin invokes when the settings form is applied.
    pub on_update_op: String,
}

/// A single plugin-contributed custom UI panel declaration.
///
/// Lives in `plugin.toml` under `[[panels]]`. The frontend dynamically
/// imports `entry` (an ES module) to mount the panel; `entry`/`icon`
/// are shipped manifest-relative and the frontend builds the
/// `/plugins/<plugin_id>/<path>` URLs. All panels are custom
/// (module-rendered); there is no panel kind to select between.
#[derive(Debug, Clone, Deserialize)]
pub struct PanelEntry {
    /// Stable panel id, unique within the plugin. The frontend keys its
    /// panel registry on `(plugin_id, id)`.
    pub id: String,
    /// Display title rendered on the panel's title bar.
    pub title: String,
    /// Panel width in pixels.
    pub width: u32,
    /// ES-module path, relative to the plugin directory. Shipped
    /// manifest-relative; the frontend builds the
    /// `/plugins/<plugin_id>/<path>` URL and dynamically imports it to
    /// mount the panel.
    pub entry: PathBuf,
    /// Icon path, relative to the plugin directory. Shipped
    /// manifest-relative; the frontend builds the
    /// `/plugins/<plugin_id>/<path>` URL.
    pub icon: PathBuf,
    /// Optional hover tooltip for the panel's launcher. Omitted → the
    /// frontend falls back to `title`.
    #[serde(default)]
    pub tooltip: Option<String>,
    /// Default panel x position in pixels (top-left origin). Distinct from
    /// the dragged-position side-table the GUI maintains; this is the
    /// layout default the panel opens at.
    pub position_x: f32,
    /// Default panel y position in pixels (top-left origin).
    pub position_y: f32,
}

/// A parameter value a manifest can declare inline for a button option.
///
/// Untagged so TOML scalars map straight onto a variant: `aa = "ALA"` is a
/// `String`, `count = 3` an `Int`, `scale = 0.5` a `Float`. Variant order
/// matters — bool and int must precede float so `3` never lands as `3.0`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum ManifestParamValue {
    /// TOML boolean.
    Bool(bool),
    /// TOML integer.
    Int(i64),
    /// TOML float.
    Float(f64),
    /// TOML string (also carries ENUM-typed params).
    String(String),
}

/// One entry of a button's option picker.
///
/// Lives in `plugin.toml` under `[[buttons.options]]`. An option is a full
/// dispatch of its parent button's op with `params` bound.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ButtonOption {
    /// Display label rendered on the option.
    pub label: String,
    /// Render color token. Options sharing a color group into one picker
    /// row, in the order colors first appear.
    pub color: String,
    /// Optional icon token: `builtin:<name>` glyph, `game:<path>` for a
    /// foldit-owned asset, or a plugin-relative path. `None` = no icon.
    #[serde(default)]
    pub icon: Option<String>,
    /// Optional hotkey corner badge (winit `KeyCode` debug spelling).
    #[serde(default)]
    pub hotkey: Option<String>,
    /// Parameter values bound into this option's dispatch, keyed by
    /// `ParamSpec.name`.
    #[serde(default)]
    pub params: HashMap<String, ManifestParamValue>,
}

/// A single plugin-contributed user-facing button declaration.
///
/// Lives in `plugin.toml` under `[[buttons]]`; the orchestrator joins
/// the entry with the plugin's bridge-side op registration (typed
/// schema, lock metadata) to produce a flat catalog the GUI consumes.
#[derive(Debug, Clone, Deserialize)]
pub struct ButtonEntry {
    /// Op-id this button dispatches. Must match an op in the plugin's
    /// `PluginRegistration.operations` (enforced at catalog-join time
    /// — the orchestrator drops manifest entries whose op-id isn't
    /// registered, with a warning).
    pub op: String,
    /// Display label rendered on the button.
    pub display: String,
    /// Icon path, relative to the plugin directory. Shipped
    /// manifest-relative; the frontend builds the
    /// `/plugins/<plugin_id>/<path>` URL.
    pub icon: PathBuf,
    /// Optional single-key hotkey, in the `winit::keyboard::KeyCode`
    /// debug spelling the key resolver uses (`"KeyW"`, not `"W"`).
    /// Display-only at this layer: it renders as a corner badge on the
    /// button. Populating it does not wire key->dispatch routing;
    /// pressing the key is a no-op. Omitted = no hotkey.
    #[serde(default)]
    pub hotkey: Option<String>,
    /// Optional hover tooltip — a richer description distinct from the
    /// on-button `display` label. The GUI falls back to `display` when
    /// this is omitted.
    #[serde(default)]
    pub tooltip: Option<String>,
    /// Optional selection requirement for this op. Declared inline, e.g.
    /// `selection_spec = { min_residues = 2, continuity = "contiguous" }`.
    /// Omitted → the op imposes no selection requirement. The
    /// orchestrator carries it onto the [`CatalogEntry`] and disables the
    /// button when the live focus-scoped selection doesn't satisfy it.
    ///
    /// [`CatalogEntry`]: crate::orchestrator::types::CatalogEntry
    #[serde(default)]
    pub selection_spec: Option<crate::orchestrator::types::SelectionSpec>,
    /// Whether this op requires the selected residues to be designable.
    /// `true` gates the button on the host-side design mask (a design op
    /// is enabled only when every focus-scoped selected residue may be
    /// mutated). Defaults `false`. The orchestrator carries it onto the
    /// [`CatalogEntry`] but does NOT evaluate it: the design mask is
    /// foldit-owned and never reaches the orchestrator, so the host
    /// (foldit-core) folds it into `enabled`.
    ///
    /// [`CatalogEntry`]: crate::orchestrator::types::CatalogEntry
    #[serde(default)]
    pub requires_designable: bool,
    /// Whether this op renders its stream as a discardable preview rather
    /// than mutating the entity. Defaults `false`. The orchestrator carries
    /// it onto the [`CatalogEntry`] for the host (foldit-core) to read at
    /// dispatch; the orchestrator itself does not act on it.
    ///
    /// [`CatalogEntry`]: crate::orchestrator::types::CatalogEntry
    #[serde(default)]
    pub preview: bool,
    /// Whether this op's stream reports how far through the user's request it
    /// is. Defaults `false`, which renders an indeterminate bar. Open-ended
    /// streams (wiggle, shake) run until cancelled, so any fraction they emit
    /// measures an internal cycle budget rather than the work the user asked
    /// for; the host discards it rather than draw a bar that fills and sits at
    /// 100% while the op keeps running. Ops with a fixed step count (B-factor
    /// refinement, diffusion) opt in.
    #[serde(default)]
    pub determinate_progress: bool,
    /// Option picker for this button. Non-empty turns the button into a
    /// toggle that opens a picker instead of a click-to-fire dispatch; each
    /// option fires the same op with its own `params`. Empty = click-to-fire.
    #[serde(default)]
    pub options: Vec<ButtonOption>,
}

/// Plugin runtime kind. Drives spawn-primitive selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginKind {
    /// Hosted by `foldit-worker` (PyO3 + Python ABC).
    Python,
    /// The plugin's own binary (Rust/C++/anything that speaks
    /// `proto::plugin` over the local socket).
    Native,
    /// Wasm module loaded by a wasm-capable host. Desktop spawn isn't
    /// implemented yet; web is the natural target.
    Wasm,
}

/// Python-runtime overrides parsed from a manifest's `[python]` table.
#[derive(Debug, Clone, Deserialize)]
pub struct PythonSection {
    /// Importable Python module path. Defaults to `{id}` (i.e. the
    /// plugin's `id` interpreted as the top-level module name).
    pub entry: Option<String>,
    /// Pixi/conda environment name. Defaults to `{id}`. The worker
    /// uses this to find the right Python interpreter.
    pub env: Option<String>,
}

/// Native-runtime overrides parsed from a manifest's `[native]` table.
#[derive(Debug, Clone, Deserialize)]
pub struct NativeSection {
    /// File name of the shared library inside the plugin directory.
    ///
    /// Two forms are accepted:
    ///
    /// - **Literal** — contains a recognized shared-library extension
    ///   (`.dylib`, `.so`, `.dll`) or a path separator. Used as-is. Right form
    ///   when a plugin's build pipeline produces a fixed filename that doesn't
    ///   follow the platform convention.
    ///
    /// - **Basename** — bare identifier like `"rosetta_interactive"`.
    ///   [`PluginManifest::native_binary_name`] resolves it to the
    ///   platform-canonical name (`lib{basename}.dylib` on macOS,
    ///   `lib{basename}.so` on Linux, `{basename}.dll` on Windows). Right form
    ///   when the plugin's build pipeline produces a platform-canonical dylib
    ///   via the standard toolchain (cargo cdylib, cmake
    ///   `add_library(SHARED)`, etc.).
    ///
    /// If omitted, defaults to the basename form using the plugin `id`.
    pub binary: Option<String>,
    /// Command-line arguments passed to the plugin binary BEFORE the
    /// orchestrator-supplied socket name.
    #[serde(default)]
    pub args: Vec<String>,
}

/// Wasm-runtime overrides parsed from a manifest's `[wasm]` table.
#[derive(Debug, Clone, Deserialize)]
pub struct WasmSection {
    /// File name of the wasm module inside the plugin directory.
    /// Defaults to `{id}.wasm`.
    pub module: Option<String>,
}

impl PluginManifest {
    /// Parse a manifest from TOML source.
    ///
    /// # Errors
    ///
    /// Returns an error if the TOML doesn't deserialize as a
    /// `PluginManifest`.
    pub fn parse(s: &str) -> Result<Self, ManifestError> {
        toml::from_str::<Self>(s).map_err(ManifestError::Toml)
    }

    /// Resolved Python entry module — falls back to `id`.
    #[must_use]
    pub fn python_entry(&self) -> &str {
        self.python
            .as_ref()
            .and_then(|p| p.entry.as_deref())
            .unwrap_or(&self.id)
    }

    /// Resolved Python env name — falls back to `id`.
    #[must_use]
    pub fn python_env(&self) -> &str {
        self.python
            .as_ref()
            .and_then(|p| p.env.as_deref())
            .unwrap_or(&self.id)
    }

    /// Resolved native shared-library file name.
    ///
    /// Resolution rules (see [`NativeSection::binary`] for the two
    /// accepted input forms):
    ///
    /// - `[native].binary` is a literal (contains `.dylib`/`.so`/`.dll` or a
    ///   path separator) → returned as-is.
    /// - `[native].binary` is a basename → decorated with platform prefix +
    ///   extension (`lib{basename}.dylib` on macOS, `lib{basename}.so` on
    ///   Linux, `{basename}.dll` on Windows).
    /// - Section absent → basename = plugin `id`, decorated as above.
    #[must_use]
    pub fn native_binary_name(&self) -> String {
        let explicit = self.native.as_ref().and_then(|n| n.binary.as_deref());
        if let Some(b) = explicit {
            if is_literal_binary(b) {
                return String::from(b);
            }
            return decorate_basename(b);
        }
        decorate_basename(&self.id)
    }

    /// Resolved native args. Empty if not declared.
    #[must_use]
    pub fn native_args(&self) -> &[String] {
        self.native.as_ref().map_or(&[][..], |n| n.args.as_slice())
    }

    /// Resolved wasm module name — falls back to `{id}.wasm`.
    pub fn wasm_module_name(&self) -> String {
        self.wasm
            .as_ref()
            .and_then(|w| w.module.as_deref())
            .map_or_else(|| format!("{}.wasm", self.id), str::to_owned)
    }
}

/// True when `s` should be used as-is rather than decorated as a
/// platform basename — i.e. it already carries a recognized
/// shared-library extension or contains a path separator.
fn is_literal_binary(s: &str) -> bool {
    s.contains('/')
        || s.contains('\\')
        || std::path::Path::new(s)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("dylib"))
        || std::path::Path::new(s)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("so"))
        || std::path::Path::new(s)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("dll"))
}

/// Apply platform-canonical shared-library prefix + extension to a
/// basename.
fn decorate_basename(basename: &str) -> String {
    if cfg!(target_os = "macos") {
        format!("lib{basename}.dylib")
    } else if cfg!(target_os = "windows") {
        format!("{basename}.dll")
    } else {
        format!("lib{basename}.so")
    }
}

/// Errors from manifest parsing.
#[derive(Debug)]
pub enum ManifestError {
    /// TOML deserialization failure.
    Toml(toml::de::Error),
    /// I/O failure reading the manifest from disk.
    Io(std::io::Error),
}

impl std::fmt::Display for ManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Toml(e) => write!(f, "manifest TOML error: {e}"),
            Self::Io(e) => write!(f, "manifest I/O error: {e}"),
        }
    }
}

impl std::error::Error for ManifestError {}

impl From<std::io::Error> for ManifestError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_buttons_array() {
        let toml = r#"
            id = "rosetta"
            kind = "native"

            [[buttons]]
            op = "ActionGlobalMinimize"
            display = "Wiggle"
            icon = "icons/wiggle.svg"

            [[buttons]]
            op = "ActionShakeMutate"
            display = "Shake"
            icon = "icons/shake.svg"
        "#;
        let m = PluginManifest::parse(toml).unwrap();
        assert_eq!(m.buttons.len(), 2);
        assert_eq!(m.buttons[0].op, "ActionGlobalMinimize");
        assert_eq!(m.buttons[0].display, "Wiggle");
        assert_eq!(m.buttons[0].icon, PathBuf::from("icons/wiggle.svg"));
        assert_eq!(m.buttons[1].op, "ActionShakeMutate");
    }

    #[test]
    fn parses_button_options_picker() {
        let toml = r#"
            id = "design"
            kind = "native"

            [[buttons]]
            op = "mutate_residue"
            display = "Mutate"
            icon = "builtin:replace"

            [[buttons.options]]
            label = "A"
            color = "orange"
            icon = "game:residue_icons/ALA.png"
            params = { aa = "ALA" }

            [[buttons.options]]
            label = "R"
            color = "blue"
            params = { aa = "ARG" }
        "#;
        let m = PluginManifest::parse(toml).unwrap();
        let options = &m.buttons[0].options;
        assert_eq!(options.len(), 2);
        assert_eq!(options[0].label, "A");
        assert_eq!(options[0].color, "orange");
        assert_eq!(
            options[0].icon.as_deref(),
            Some("game:residue_icons/ALA.png")
        );
        assert_eq!(
            options[0].params.get("aa"),
            Some(&ManifestParamValue::String("ALA".to_owned()))
        );
        // An option may omit its icon entirely.
        assert!(options[1].icon.is_none());
    }

    #[test]
    fn button_options_default_to_empty_and_scalars_keep_their_type() {
        // A click-to-fire button declares no options.
        let toml = r#"
            id = "p"
            kind = "native"

            [[buttons]]
            op = "go"
            display = "Go"
            icon = "builtin:replace"

            [[buttons.options]]
            label = "x"
            color = "blue"
            params = { count = 3, scale = 0.5, on = true, name = "n" }
        "#;
        let m = PluginManifest::parse(toml).unwrap();
        let p = &m.buttons[0].options[0].params;
        // Untagged variant order must keep an integer from landing as a float.
        assert_eq!(p.get("count"), Some(&ManifestParamValue::Int(3)));
        assert_eq!(p.get("scale"), Some(&ManifestParamValue::Float(0.5)));
        assert_eq!(p.get("on"), Some(&ManifestParamValue::Bool(true)));
        assert_eq!(
            p.get("name"),
            Some(&ManifestParamValue::String("n".to_owned()))
        );
    }

    #[test]
    fn parses_button_selection_spec() {
        use crate::orchestrator::types::Continuity;
        let toml = r#"
            id = "rosetta"
            kind = "native"

            [[buttons]]
            op = "ActionIdealize"
            display = "Idealize"
            icon = "i.png"
            selection_spec = { min_residues = 2, continuity = "contiguous" }

            [[buttons]]
            op = "ActionRebuild"
            display = "Rebuild"
            icon = "r.png"
            selection_spec = { min_residues = 1 }

            [[buttons]]
            op = "ActionGlobalMinimize"
            display = "Wiggle"
            icon = "w.png"
        "#;
        let m = PluginManifest::parse(toml).unwrap();

        // Fully-specified spec.
        let spec = m.buttons[0].selection_spec.expect("idealize spec");
        assert_eq!(spec.min_residues, 2);
        assert_eq!(spec.max_residues, 0); // omitted → unbounded
        assert_eq!(spec.continuity, Continuity::Contiguous);

        // Partial spec: continuity defaults to Any, max to unbounded.
        let spec = m.buttons[1].selection_spec.expect("rebuild spec");
        assert_eq!(spec.min_residues, 1);
        assert_eq!(spec.max_residues, 0);
        assert_eq!(spec.continuity, Continuity::Any);

        // No spec declared → None (no requirement).
        assert!(m.buttons[2].selection_spec.is_none());
    }

    #[test]
    fn parses_button_requires_designable() {
        let toml = r#"
            id = "rosetta"
            kind = "native"

            [[buttons]]
            op = "ActionRepackDesign"
            display = "Shake Mutate"
            icon = "m.png"
            requires_designable = true

            [[buttons]]
            op = "ActionRepack"
            display = "Shake"
            icon = "s.png"
        "#;
        let m = PluginManifest::parse(toml).unwrap();
        // Declared true on the design op.
        assert!(m.buttons[0].requires_designable);
        // Omitted → defaults false (the non-design op is ungated).
        assert!(!m.buttons[1].requires_designable);
    }

    #[test]
    fn buttons_default_to_empty() {
        let toml = r#"
            id = "dummy"
            kind = "python"
        "#;
        let m = PluginManifest::parse(toml).unwrap();
        assert!(m.buttons.is_empty());
    }

    #[test]
    fn parses_panels_array() {
        let toml = r#"
            id = "rosetta"
            kind = "native"

            [[panels]]
            id = "controls"
            title = "Controls"
            width = 320
            entry = "ui/controls.mjs"
            icon = "icons/controls.svg"
            position_x = 12.0
            position_y = 48.0

            [[panels]]
            id = "design"
            title = "Design"
            width = 480
            entry = "ui/design.mjs"
            icon = "icons/design.svg"
            tooltip = "Design tools"
            position_x = 360.0
            position_y = 48.0
        "#;
        let m = PluginManifest::parse(toml).unwrap();
        assert_eq!(m.panels.len(), 2);
        assert_eq!(m.panels[0].id, "controls");
        assert_eq!(m.panels[0].title, "Controls");
        assert_eq!(m.panels[0].width, 320);
        assert_eq!(m.panels[0].entry, PathBuf::from("ui/controls.mjs"));
        assert_eq!(m.panels[0].icon, PathBuf::from("icons/controls.svg"));
        assert!(m.panels[0].tooltip.is_none());
        assert!((m.panels[0].position_x - 12.0).abs() < f32::EPSILON);
        assert!((m.panels[0].position_y - 48.0).abs() < f32::EPSILON);
        assert_eq!(m.panels[1].id, "design");
        assert_eq!(m.panels[1].tooltip.as_deref(), Some("Design tools"));
    }

    #[test]
    fn panels_default_to_empty() {
        let toml = r#"
            id = "dummy"
            kind = "python"
        "#;
        let m = PluginManifest::parse(toml).unwrap();
        assert!(m.panels.is_empty());
    }

    #[test]
    fn parses_settings_array() {
        let toml = r#"
            id = "rosetta"
            kind = "native"

            [[settings]]
            name = "General"
            schema_asset_path = "settings/general.json"
            on_update_op = "ApplyGeneralSettings"

            [[settings]]
            name = "Advanced"
            schema_asset_path = "settings/advanced.json"
            on_update_op = "ApplyAdvancedSettings"
        "#;
        let m = PluginManifest::parse(toml).unwrap();
        assert_eq!(m.settings.len(), 2);
        assert_eq!(m.settings[0].name, "General");
        assert_eq!(
            m.settings[0].schema_asset_path,
            PathBuf::from("settings/general.json")
        );
        assert_eq!(m.settings[0].on_update_op, "ApplyGeneralSettings");
        assert_eq!(m.settings[1].name, "Advanced");
        assert_eq!(
            m.settings[1].schema_asset_path,
            PathBuf::from("settings/advanced.json")
        );
        assert_eq!(m.settings[1].on_update_op, "ApplyAdvancedSettings");
    }

    #[test]
    fn settings_default_to_empty() {
        let toml = r#"
            id = "dummy"
            kind = "python"
        "#;
        let m = PluginManifest::parse(toml).unwrap();
        assert!(m.settings.is_empty());
    }

    #[test]
    fn parses_minimal_python_manifest() {
        let toml = r#"
            id = "dummy"
            kind = "python"
        "#;
        let m = PluginManifest::parse(toml).unwrap();
        assert_eq!(m.id, "dummy");
        assert_eq!(m.kind, PluginKind::Python);
        assert_eq!(m.python_entry(), "dummy");
        assert_eq!(m.python_env(), "dummy");
        assert!(m.config.is_empty());
    }

    #[test]
    fn parses_python_with_overrides() {
        let toml = r#"
            id = "foundry"
            kind = "python"

            [python]
            entry = "model_plugins.foundry"
            env = "foundry"

            [config]
            checkpoint_dir = "/tmp/foundry-checkpoints"
        "#;
        let m = PluginManifest::parse(toml).unwrap();
        assert_eq!(m.id, "foundry");
        assert_eq!(m.kind, PluginKind::Python);
        assert_eq!(m.python_entry(), "model_plugins.foundry");
        assert_eq!(m.python_env(), "foundry");
        assert_eq!(
            m.config.get("checkpoint_dir").map(String::as_str),
            Some("/tmp/foundry-checkpoints")
        );
    }

    #[test]
    fn parses_native_manifest() {
        let toml = r#"
            id = "rosetta"
            kind = "native"

            [native]
            binary = "bin/rosetta-plugin"
            args = ["--db-path", "/usr/share/rosetta"]
        "#;
        let m = PluginManifest::parse(toml).unwrap();
        assert_eq!(m.kind, PluginKind::Native);
        assert_eq!(m.native_binary_name(), "bin/rosetta-plugin");
        assert_eq!(m.native_args(), &["--db-path", "/usr/share/rosetta"]);
    }

    #[test]
    fn native_default_binary_uses_platform_extension() {
        let toml = r#"
            id = "myplugin"
            kind = "native"
        "#;
        let m = PluginManifest::parse(toml).unwrap();
        let name = m.native_binary_name();
        assert!(name.contains("myplugin"));
        #[cfg(target_os = "macos")]
        assert_eq!(name, "libmyplugin.dylib");
        #[cfg(target_os = "linux")]
        assert_eq!(name, "libmyplugin.so");
        #[cfg(target_os = "windows")]
        assert_eq!(name, "myplugin.dll");
    }

    #[test]
    fn native_basename_binary_resolves_per_platform() {
        let toml = r#"
            id = "rosetta"
            kind = "native"

            [native]
            binary = "rosetta_interactive"
        "#;
        let m = PluginManifest::parse(toml).unwrap();
        let name = m.native_binary_name();
        #[cfg(target_os = "macos")]
        assert_eq!(name, "librosetta_interactive.dylib");
        #[cfg(target_os = "linux")]
        assert_eq!(name, "librosetta_interactive.so");
        #[cfg(target_os = "windows")]
        assert_eq!(name, "rosetta_interactive.dll");
    }

    #[test]
    fn native_literal_binary_with_extension_preserved() {
        // `.dylib`/`.so`/`.dll` markers all pin the form as literal,
        // regardless of host platform — useful when a plugin's build
        // pipeline emits a fixed filename that doesn't match the host
        // convention.
        for literal in ["libfoo.dylib", "libfoo.so", "foo.dll"] {
            let toml = format!(
                r#"
                    id = "foo"
                    kind = "native"

                    [native]
                    binary = "{literal}"
                "#
            );
            let m = PluginManifest::parse(&toml).unwrap();
            assert_eq!(m.native_binary_name(), literal);
        }
    }

    #[test]
    fn native_literal_binary_with_path_separator_preserved() {
        let toml = r#"
            id = "foo"
            kind = "native"

            [native]
            binary = "bin/foo-plugin"
        "#;
        let m = PluginManifest::parse(toml).unwrap();
        assert_eq!(m.native_binary_name(), "bin/foo-plugin");
    }

    #[test]
    fn parses_wasm_manifest() {
        let toml = r#"
            id = "myplugin"
            kind = "wasm"
        "#;
        let m = PluginManifest::parse(toml).unwrap();
        assert_eq!(m.kind, PluginKind::Wasm);
        assert_eq!(m.wasm_module_name(), "myplugin.wasm");
    }

    #[test]
    fn rejects_unknown_kind() {
        let toml = r#"
            id = "x"
            kind = "magic"
        "#;
        assert!(PluginManifest::parse(toml).is_err());
    }

    #[test]
    fn rejects_missing_id() {
        let toml = r#"
            kind = "python"
        "#;
        assert!(PluginManifest::parse(toml).is_err());
    }

    #[test]
    fn parses_on_disk_rosetta() {
        let toml = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../plugins")
                .join("rosetta")
                .join("plugin.toml"),
        )
        .expect("plugins/rosetta/plugin.toml must exist");
        let m =
            PluginManifest::parse(&toml).expect("rosetta manifest must parse");
        assert_eq!(m.id, "rosetta");
        assert_eq!(m.kind, PluginKind::Native);
        let name = m.native_binary_name();
        #[cfg(target_os = "macos")]
        assert_eq!(name, "librosetta_interactive.dylib");
        #[cfg(target_os = "linux")]
        assert_eq!(name, "librosetta_interactive.so");
        #[cfg(target_os = "windows")]
        assert_eq!(name, "rosetta_interactive.dll");
    }

    #[test]
    fn parses_on_disk_foundry() {
        let toml = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../plugins")
                .join("foundry")
                .join("plugin.toml"),
        )
        .expect("plugins/foundry/plugin.toml must exist");
        let m = PluginManifest::parse(&toml).expect("foundry manifest must parse");
        assert_eq!(m.id, "foundry");
        assert_eq!(m.kind, PluginKind::Python);

        let ops: Vec<&str> = m.buttons.iter().map(|b| b.op.as_str()).collect();
        assert_eq!(ops, ["rfd3_design", "rf3_predict", "mpnn_design"]);

        // RF3's diffusion intermediates are backbone-only, so they must ride a
        // discardable ghost rather than the real lane.
        let rf3 = m.buttons.iter().find(|b| b.op == "rf3_predict").unwrap();
        assert!(rf3.preview, "rf3_predict must be a preview stream");
        assert_eq!(rf3.icon, PathBuf::from("builtin:atom"));

        let mpnn = m.buttons.iter().find(|b| b.op == "mpnn_design").unwrap();
        assert_eq!(mpnn.icon, PathBuf::from("assets/icons/protein_mpnn.png"));
    }

    #[test]
    fn parses_on_disk_design() {
        let toml = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../plugins")
                .join("design")
                .join("plugin.toml"),
        )
        .expect("plugins/design/plugin.toml must exist");
        let m = PluginManifest::parse(&toml).expect("design manifest must parse");
        assert_eq!(m.id, "design");
        assert_eq!(m.kind, PluginKind::Native);

        let button = &m.buttons[0];
        assert_eq!(button.op, "mutate_residue");
        assert!(button.requires_designable);
        // Mutate rewrites exactly one residue's identity.
        let spec = button.selection_spec.expect("mutate declares a selection spec");
        assert_eq!((spec.min_residues, spec.max_residues), (1, 1));

        // One option per proteinogenic amino acid, each binding its own code.
        assert_eq!(button.options.len(), 20);
        let codes: std::collections::HashSet<_> = button
            .options
            .iter()
            .map(|o| match o.params.get("aa") {
                Some(ManifestParamValue::String(s)) => s.clone(),
                other => panic!("option {:?} has a non-string `aa`: {other:?}", o.label),
            })
            .collect();
        assert_eq!(codes.len(), 20, "amino-acid codes must be unique");

        // The frontend rows options by first-seen color, so a hydrophobic
        // option must come first to keep the hydrophobic row on top.
        assert_eq!(button.options[0].color, "orange");
        let orange = button.options.iter().filter(|o| o.color == "orange").count();
        assert_eq!(orange, 11, "molex's hydrophobic set has 11 members");

        #[cfg(target_os = "linux")]
        assert_eq!(m.native_binary_name(), "libdesign.so");
        #[cfg(target_os = "macos")]
        assert_eq!(m.native_binary_name(), "libdesign.dylib");
        #[cfg(target_os = "windows")]
        assert_eq!(m.native_binary_name(), "design.dll");
    }

    /// Determinate progress is opt-in. An open-ended stream that happens to
    /// report a fraction (rosetta's shake counts its own repack cycles) must
    /// not be able to turn that into a filling progress bar just by emitting
    /// it, so a button that says nothing gets an indeterminate bar.
    #[test]
    fn determinate_progress_defaults_off_and_is_opt_in() {
        let toml = r#"
            id = "p"
            kind = "native"

            [[buttons]]
            op = "shake"
            display = "Shake"
            icon = "builtin:replace"

            [[buttons]]
            op = "refine_b"
            display = "Refine B"
            icon = "builtin:cloud"
            determinate_progress = true
        "#;
        let m = PluginManifest::parse(toml).expect("manifest must parse");
        assert!(!m.buttons[0].determinate_progress, "open-ended ops default off");
        assert!(m.buttons[1].determinate_progress, "fixed-step ops opt in");
    }

    /// The on-disk manifests must agree with the rule above: rosetta's
    /// open-ended sampling ops stay indeterminate, and the fixed-step ops
    /// declare themselves.
    #[test]
    fn on_disk_manifests_only_opt_fixed_step_ops_into_determinate_progress() {
        let plugins = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins");
        let determinate = |plugin: &str| -> std::collections::HashSet<String> {
            let path = plugins.join(plugin).join("plugin.toml");
            let toml = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("{} must exist: {e}", path.display()));
            PluginManifest::parse(&toml)
                .unwrap_or_else(|e| panic!("{plugin} manifest must parse: {e}"))
                .buttons
                .into_iter()
                .filter(|b| b.determinate_progress)
                .map(|b| b.op)
                .collect()
        };

        assert!(
            determinate("rosetta").is_empty(),
            "wiggle/shake/rebuild run until cancelled; none may claim a fraction"
        );
        assert_eq!(determinate("xtal"), ["refine_b".to_owned()].into());
        assert_eq!(
            determinate("foundry"),
            ["rfd3_design".to_owned(), "rf3_predict".to_owned()].into(),
        );
    }
}
