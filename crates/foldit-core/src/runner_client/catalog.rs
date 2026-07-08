//! Action-catalog projection: turn the orchestrator's op catalog into the
//! GUI's `ActionInfo` shape and resolve manifest hotkeys / display labels
//! off the static op catalog. Read-only against the orchestrator.

#[cfg(not(target_arch = "wasm32"))]
use super::{RunnerClient, WeightsState};

/// Which action, if any, owns a hotkey once press-time precedence is applied.
/// Plugin catalog bindings win over the native picker-toggle table; a key
/// bound by neither is `None`. Resolved in one place so the GUI badge and the
/// key press agree on the winner.
#[cfg(not(target_arch = "wasm32"))]
pub enum HotkeyOwner {
    Plugin { plugin_id: String, op_id: String },
    Native { op_id: String },
    None,
}

#[cfg(not(target_arch = "wasm32"))]
impl RunnerClient {
    /// Project the orchestrator's op catalog into the GUI's [`ActionInfo`]
    /// shape, resolving each entry's `enabled` flag against the current
    /// lock state plus the supplied focus/selection. Read-only: no lock is
    /// taken. Empty when no orchestrator is wired up.
    ///
    /// The selection flatten mirrors [`Self::dispatch_op`]'s, so the
    /// availability reasoning sees the exact target set a real dispatch
    /// would. The `CatalogEntry -> ActionInfo` forward lives here so `App`
    /// names no runner catalog type.
    ///
    /// `selection_designable` is the host-computed design gate (every
    /// focus-scoped selected residue is designable). The orchestrator never
    /// sees the design mask, so the gate is folded in here: an entry whose
    /// manifest set `requires_designable` is forced disabled when the
    /// selection is not fully designable. Ungated ops ignore the flag, and
    /// `requires_designable` is internal gating metadata that never reaches
    /// the frontend `ActionInfo`.
    ///
    /// [`ActionInfo`]: foldit_gui::state::ActionInfo
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn actions_catalog<F>(
        &self,
        focus: Option<molex::EntityId>,
        selection: &std::collections::BTreeMap<molex::EntityId, std::collections::BTreeSet<u32>>,
        selection_designable: bool,
        entity_type_of: F,
    ) -> Vec<foldit_gui::state::ActionInfo>
    where
        F: Fn(molex::EntityId) -> Option<molex::EntityKind>,
    {
        use super::types::build_dispatch_context;
        use foldit_gui::state::{ActionInfo, ActionOption, ParamValue};

        let Some(orch) = self.orchestrator.as_ref() else {
            return vec![];
        };

        // Availability resolution never reaches a plugin, so the design mask
        // is not transmitted here (empty designable map); the host-side design
        // gate is folded into `enabled` below instead.
        let ctx = build_dispatch_context(focus, selection, &std::collections::BTreeMap::new());

        // The running/cancel state is NOT on `ActionInfo`: it lives in the
        // separate `running` list (one entry per held lock), which the
        // frontend matches against the focused entity. That keeps a button's
        // cancel state per-focus instead of global-by-op-id.
        let mut rows: Vec<ActionInfo> = orch
            .actions_catalog(&ctx, entity_type_of)
            .into_iter()
            .map(|entry| ActionInfo {
                op_id: entry.op_id,
                plugin_id: entry.plugin_id,
                display: entry.display,
                icon_path: entry.icon_path.to_string_lossy().into_owned(),
                enabled: entry.enabled && (!entry.requires_designable || selection_designable),
                hotkey: entry.hotkey,
                tooltip: entry.tooltip,
                params: entry
                    .params
                    .into_iter()
                    .map(crate::wire_params::param_spec_to_wire)
                    .collect(),
                options: Vec::new(),
            })
            .collect();

        // Host-declared (non-plugin) action. Its non-empty `options` make it a
        // 20-button amino-acid picker rather than a click-to-fire button: each
        // option is a full dispatch in its own right. `color` encodes
        // hydrophobicity, and the 3-letter `aa` param is what the dispatch
        // reads back (there is no 1-letter residue constructor). Enabled only
        // when exactly one designable residue is selected: it must pass the
        // same design mask the plugin gate uses and be a single-residue
        // selection, which is what the dispatch will accept.
        let options = molex::chemistry::AminoAcid::ALL
            .iter()
            .map(|&aa| {
                // `code()` is statically uppercase ASCII, so each byte maps
                // straight to a char without a fallible UTF-8 decode.
                let three_letter: String = aa.code().iter().map(|&b| b as char).collect();
                // Foldit-owned residue icon, served under `/game-assets/`; the
                // 3-letter code matches the icon filename (e.g. ALA.png).
                let icon = format!("residue_icons/{three_letter}.png");
                let mut params = std::collections::HashMap::new();
                params.insert("aa".to_owned(), ParamValue::String(three_letter));
                ActionOption {
                    label: (aa.one_letter() as char).to_string(),
                    color: if aa.is_hydrophobic() {
                        "orange"
                    } else {
                        "blue"
                    }
                    .to_owned(),
                    icon: Some(icon),
                    hotkey: None,
                    op_id: "mutate_residue".to_owned(),
                    params,
                }
            })
            .collect();
        rows.push(ActionInfo {
            op_id: "mutate_residue".to_owned(),
            plugin_id: "native".to_owned(),
            display: "Mutate".to_owned(),
            // `builtin:<name>` selects a built-in GUI icon rather than a
            // plugin-relative asset path, so this native action ships a glyph
            // with no asset file.
            icon_path: "builtin:replace".to_owned(),
            enabled: selection_designable
                && selection
                    .values()
                    .map(std::collections::BTreeSet::len)
                    .sum::<usize>()
                    == 1,
            // Badge wire: the friendly glyph the frontend prettifies to "M".
            // The trailing dedup pass reconciles this against `resolve_hotkey`,
            // so the badge only survives if the key actually dispatches here.
            hotkey: Some("KeyM".to_owned()),
            tooltip: Some("Mutate the selected residue".to_owned()),
            params: Vec::new(),
            options,
        });

        // Host-declared crystallographic B-factor refine: a click-to-fire
        // button (no options). `enabled` reads the driver-side availability
        // `App` syncs each tick (a loaded density, a GPU device, and no refine
        // already running), so the button matches what a dispatch would accept.
        rows.push(ActionInfo {
            op_id: "refine_b".to_owned(),
            plugin_id: "native".to_owned(),
            display: "B-factor Refine".to_owned(),
            icon_path: "builtin:cloud".to_owned(),
            enabled: self.refine_available,
            hotkey: None,
            tooltip: Some("Refine per-atom B-factors against the loaded density".to_owned()),
            params: Vec::new(),
            options: Vec::new(),
        });

        // Weights gate: an ML plugin whose model weights are not ready
        // surfaces a single "Download weights" button in place of its normal
        // buttons. This holds for every non-`Ready` state (a `Missing` or
        // `Failed` retry, and while `Downloading` is in flight); only `Ready`
        // restores the normal rows. Plugins absent from the map (readiness not
        // yet reported, or non-ML plugins that never advertise
        // `weights_status`) and the native host row keep their normal rows.
        // The injected `download_weights` op is a registered stream op, so the
        // button dispatches through the ordinary path with no special-casing.
        // The button is disabled while `Downloading` so a second click cannot
        // start a duplicate download; a `Missing` / `Failed` retry stays live.
        for (plugin_id, state) in &self.weights {
            if matches!(state, WeightsState::Ready) {
                continue;
            }
            rows.retain(|r| &r.plugin_id != plugin_id);
            rows.push(ActionInfo {
                op_id: "download_weights".to_owned(),
                plugin_id: plugin_id.clone(),
                display: "Download weights".to_owned(),
                icon_path: "builtin:download".to_owned(),
                enabled: !matches!(state, WeightsState::Downloading { .. }),
                hotkey: None,
                tooltip: Some("Download this plugin's model weights".to_owned()),
                params: Vec::new(),
                options: Vec::new(),
            });
        }

        self.reconcile_hotkey_badges(&mut rows);
        rows
    }

    /// Project the live streams into the GUI's [`RunningAction`] list: one
    /// entry per held lock, carrying the request-id (to cancel just that
    /// instance), the display label, and the locked entity set / global flag
    /// (so the frontend can match a running action against the focused
    /// entity). Weight downloads are excluded - they surface through their own
    /// download toast, not the running-action list. The native refine is not
    /// a stream and is appended by the projector, not here.
    ///
    /// [`RunningAction`]: foldit_gui::state::RunningAction
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn running_actions(&self) -> Vec<foldit_gui::state::RunningAction> {
        let mut running: Vec<foldit_gui::state::RunningAction> = self
            .stream_host
            .active_streams
            .iter()
            .filter(|(_, e)| e.op_id != "download_weights")
            .map(|(rid, e)| foldit_gui::state::RunningAction {
                request_id: Some(*rid),
                display: self
                    .op_display(&e.plugin_id, &e.op_id)
                    .unwrap_or_else(|| e.op_id.clone()),
                op_id: e.op_id.clone(),
                entities: e.handle.entities.iter().map(|id| id.raw()).collect(),
                global: e.handle.global_held,
            })
            .collect();

        // The native refine is not an orchestrator stream, so it never appears
        // in `active_streams`; surface it as a synthetic global running action
        // (it holds the global lock). `request_id: None` routes its cancel
        // through the `refine` flag on `CancelAction`, not a stream teardown.
        // Its progress toast rides `refine_progress`, so the frontend renders
        // only its button cancel-state from this entry, not a second toast.
        if self.refine_running {
            running.push(foldit_gui::state::RunningAction {
                request_id: None,
                op_id: "refine_b".to_owned(),
                display: "B-factor Refine".to_owned(),
                entities: Vec::new(),
                global: true,
            });
        }
        running
    }

    /// Set whether the native B-factor-refine action is available, returning
    /// `true` when the value changed (so the caller re-projects the actions
    /// section only on a real transition).
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) const fn set_refine_available(&mut self, available: bool) -> bool {
        if self.refine_available == available {
            return false;
        }
        self.refine_available = available;
        true
    }

    /// Drop the hotkey badge from any action whose key is actually owned by a
    /// higher-precedence action, so the badge matches what the key dispatches
    /// (resolved once in [`Self::resolve_hotkey`], the same order `handle_key`
    /// applies).
    #[cfg(not(target_arch = "wasm32"))]
    fn reconcile_hotkey_badges(&self, rows: &mut [foldit_gui::state::ActionInfo]) {
        for row in rows {
            if let Some(key) = row.hotkey.clone() {
                let owns = match self.resolve_hotkey(&key) {
                    HotkeyOwner::Plugin { plugin_id, op_id } => {
                        plugin_id == row.plugin_id && op_id == row.op_id
                    }
                    HotkeyOwner::Native { op_id } => {
                        row.plugin_id == "native" && op_id == row.op_id
                    }
                    HotkeyOwner::None => false,
                };
                if !owns {
                    row.hotkey = None;
                }
            }
        }
    }

    /// Project the orchestrator's per-plugin group metadata into the GUI's
    /// [`PluginGroupInfo`] shape. One entry per discovered plugin (the
    /// orchestrator emits a row whether or not the plugin has buttons; the
    /// frontend joins on `plugin_id` and ignores rows with no buttons).
    /// Empty when no orchestrator is wired up.
    ///
    /// [`PluginGroupInfo`]: foldit_gui::state::PluginGroupInfo
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn plugin_groups(&self) -> Vec<foldit_gui::state::PluginGroupInfo> {
        use foldit_gui::state::PluginGroupInfo;

        let Some(orch) = self.orchestrator.as_ref() else {
            return vec![];
        };
        let mut groups: Vec<PluginGroupInfo> = orch
            .plugin_groups()
            .into_iter()
            .map(|entry| PluginGroupInfo {
                plugin_id: entry.plugin_id,
                name: entry.name,
                order: entry.order,
            })
            .collect();

        // Group box titling the host-declared actions (joined on `plugin_id`
        // against `ActionInfo`). Ordered last via a max sort key so it sits
        // after every manifest plugin group deterministically.
        groups.push(PluginGroupInfo {
            plugin_id: "native".to_owned(),
            name: Some("Design".to_owned()),
            order: Some(u32::MAX),
        });
        groups
    }

    /// Project the orchestrator's custom-panel catalog into the GUI's
    /// [`PanelInfo`] shape. One entry per plugin-declared `[[panels]]`
    /// row; empty when no orchestrator is wired up. Served on demand
    /// through the one-shot request path, not pushed via the projector.
    ///
    /// [`PanelInfo`]: foldit_gui::state::PanelInfo
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn panels_catalog(&self) -> Vec<foldit_gui::state::PanelInfo> {
        use foldit_gui::state::PanelInfo;

        let Some(orch) = self.orchestrator.as_ref() else {
            return vec![];
        };
        orch.panels_catalog()
            .into_iter()
            .map(|e| PanelInfo {
                plugin_id: e.plugin_id,
                id: e.id,
                title: e.title,
                width: e.width,
                position_x: e.position_x,
                position_y: e.position_y,
                entry: e.entry.to_string_lossy().into_owned(),
                icon_path: e.icon_path.to_string_lossy().into_owned(),
                tooltip: e.tooltip,
            })
            .collect()
    }

    /// Project the orchestrator's settings-tab catalog into the GUI's
    /// [`SettingsTabInfo`] shape. One entry per plugin-declared
    /// `[[settings]]` row; empty when no orchestrator is wired up. Served
    /// on demand through the one-shot request path, like
    /// [`Self::panels_catalog`].
    ///
    /// [`SettingsTabInfo`]: foldit_gui::state::SettingsTabInfo
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn settings_catalog(&self) -> Vec<foldit_gui::state::SettingsTabInfo> {
        use foldit_gui::state::SettingsTabInfo;

        let Some(orch) = self.orchestrator.as_ref() else {
            return vec![];
        };
        orch.settings_catalog()
            .into_iter()
            .map(|e| SettingsTabInfo {
                plugin_id: e.plugin_id,
                name: e.name,
                schema_asset_path: e.schema_asset_path.to_string_lossy().into_owned(),
                on_update_op: e.on_update_op,
            })
            .collect()
    }

    /// Resolve a key to the single action that owns it, applying the same
    /// precedence `handle_key` does: plugin catalog binding first, then the
    /// native picker-toggle table. The one authority the GUI badge and the
    /// key press both read, so the badge cannot advertise a shadowed action.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn resolve_hotkey(&self, key: &str) -> HotkeyOwner {
        if let Some((plugin_id, op_id)) = self.hotkey_to_op(key) {
            return HotkeyOwner::Plugin { plugin_id, op_id };
        }
        if let Some(op_id) = Self::native_hotkey_toggle(key) {
            return HotkeyOwner::Native { op_id };
        }
        HotkeyOwner::None
    }

    /// Resolve a manifest hotkey string to its `(plugin_id, op_id)` via the
    /// static op catalog. `None` when no catalog button binds the key.
    /// Static identity only: no focus/selection/lock state involved.
    #[cfg(not(target_arch = "wasm32"))]
    fn hotkey_to_op(&self, key: &str) -> Option<(String, String)> {
        let orch = self.orchestrator.as_ref()?;
        orch.ops_catalog()
            .into_iter()
            .find(|e| e.hotkey.as_deref() == Some(key))
            .map(|e| (e.plugin_id, e.op_id))
    }

    /// Resolve a native (non-plugin) key to the `op_id` whose picker it
    /// toggles. Separate from [`Self::hotkey_to_op`], which scans the plugin
    /// `ops_catalog`: the native Mutate action is host-declared and never
    /// appears there. Toggling a picker is not an op dispatch; the Mutate
    /// dispatch needs an `aa` param, so this key must never route through the
    /// op path.
    #[cfg(not(target_arch = "wasm32"))]
    fn native_hotkey_toggle(key: &str) -> Option<String> {
        match key {
            "KeyM" => Some("mutate_residue".to_owned()),
            _ => None,
        }
    }

    /// Static display label for `(plugin_id, op_id)` from the op catalog.
    /// `None` when the op isn't surfaced as a manifest button (the caller
    /// falls back to the op id). Static identity only.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn op_display(&self, plugin_id: &str, op_id: &str) -> Option<String> {
        let orch = self.orchestrator.as_ref()?;
        orch.ops_catalog()
            .into_iter()
            .find(|e| e.plugin_id == plugin_id && e.op_id == op_id)
            .map(|e| e.display)
    }

    /// Whether `(plugin_id, op_id)` declares `preview`: the op's stream is a
    /// discardable ghost rather than a mutation of the target lane. Reads the
    /// op catalog (manifest-authored), like [`Self::op_display`], not the
    /// lock-meta the create-entities flag rides. `false` when no orchestrator
    /// is wired up or the op isn't a manifest button.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn op_preview(&self, plugin_id: &str, op_id: &str) -> bool {
        self.orchestrator.as_ref().is_some_and(|orch| {
            orch.ops_catalog()
                .into_iter()
                .any(|e| e.plugin_id == plugin_id && e.op_id == op_id && e.preview)
        })
    }
}
