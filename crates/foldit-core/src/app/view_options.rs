use crate::app::App;

impl App {
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

    /// Load a named view preset's options off disk and install them as the
    /// App-owned active options + active preset.
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
        // Record the faithful (sparse) form as the round-trip source, keeping
        // the dense `opts` for the eager engine sync below.
        let faithful = serde_json::to_value(&opts).unwrap_or_default();
        self.gui.set_view_preset(faithful, name.to_owned());

        // Eager engine sync
        if let Some(engine) = self.harness.engine.as_mut() {
            engine.set_options(opts);
        }
        self.store.note_view_options_changed();
    }

    /// Push the persisted view options to a freshly-reset engine, rebuilt from
    /// the frontend-held faithful form.
    #[cfg(not(target_arch = "wasm32"))]
    pub(in crate::app) fn reapply_view_options_to_engine(&mut self) {
        let opts = self.view_options();
        if let Some(engine) = self.harness.engine.as_mut() {
            engine.set_options(opts);
        }
        self.store.note_view_options_changed();
    }
}

#[cfg(test)]
#[cfg(not(target_arch = "wasm32"))]
mod preset_tests {
    use crate::app::App;
    use crate::HostResources;
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
    /// Entity) and record it as the active preset.
    #[test]
    fn default_preset_seeds_view_options() {
        let presets_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/view_presets");
        let mut app = App::new(Box::new(PresetHost { presets_dir }));

        assert_eq!(
            app.view_options().display.backbone_color_scheme(),
            ColorScheme::Entity,
        );
        assert!(app.active_preset().is_none());
        assert!(!app.gui.view_touched());

        app.apply_view_preset_to_session("Default");

        assert_eq!(
            app.view_options().display.backbone_color_scheme(),
            ColorScheme::Score,
            "Default preset colors by Score, not bare-default Entity",
        );
        assert_eq!(app.active_preset(), Some("Default"));
    }

    /// The funnel records the preset name AND notes a single
    /// `ViewOptionsChanged` on the `SessionUpdate` stream
    #[test]
    fn funnel_records_preset_and_notes_one_change() {
        let presets_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/view_presets");
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

    /// The view options + active preset live on the frontend and survive
    /// `Session::reset`.
    #[test]
    fn view_options_persist_across_session_reset() {
        let presets_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/view_presets");
        let mut app = App::new(Box::new(PresetHost { presets_dir }));

        let faithful = serde_json::to_value(mk_non_default_options()).unwrap();
        app.gui.set_view_preset(faithful, "warm".to_owned());

        app.store.reset();

        assert_eq!(
            app.view_options(),
            mk_non_default_options(),
            "view options survive a topology swap",
        );
        assert_eq!(
            app.active_preset(),
            Some("warm"),
            "App active preset survives a topology swap",
        );
    }
}
