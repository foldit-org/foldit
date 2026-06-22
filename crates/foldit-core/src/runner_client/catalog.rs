//! Action-catalog projection: turn the orchestrator's op catalog into the
//! GUI's `ActionInfo` shape and resolve manifest hotkeys / display labels
//! off the static op catalog. Read-only against the orchestrator.

#[cfg(not(target_arch = "wasm32"))]
use super::RunnerClient;

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
        selection: &std::collections::BTreeMap<
            molex::EntityId,
            std::collections::BTreeSet<u32>,
        >,
        selection_designable: bool,
        entity_type_of: F,
    ) -> Vec<foldit_gui::state::ActionInfo>
    where
        F: Fn(molex::EntityId) -> Option<molex::EntityKind>,
    {
        use foldit_gui::state::ActionInfo;
        use foldit_runner::orchestrator::{DispatchContext, ResidueRef};

        let Some(orch) = self.orchestrator.as_ref() else {
            return vec![];
        };

        // Same flatten dispatch_op does: molex ids -> wire-shape refs.
        let flat: Vec<ResidueRef> = selection
            .iter()
            .flat_map(|(entity, residues)| {
                let id = *entity;
                residues.iter().map(move |&residue_index| ResidueRef {
                    entity_id: id,
                    residue_index,
                })
            })
            .collect();
        let ctx = DispatchContext {
            focused_entity_id: focus,
            selection: flat,
            // Availability resolution never reaches a plugin, so the design
            // mask is not transmitted here; the host-side design gate is
            // folded into `enabled` below instead.
            designable: Vec::new(),
        };

        orch.actions_catalog(&ctx, entity_type_of)
            .into_iter()
            .map(|entry| ActionInfo {
                op_id: entry.op_id,
                plugin_id: entry.plugin_id,
                display: entry.display,
                icon_path: entry.icon_path.to_string_lossy().into_owned(),
                // Fold the host-side design gate into the orchestrator's
                // lock/selection `enabled` for design-gated ops only.
                enabled: entry.enabled
                    && (!entry.requires_designable || selection_designable),
                active: false,
                hotkey: entry.hotkey,
                tooltip: entry.tooltip,
                params: entry
                    .params
                    .into_iter()
                    .map(crate::wire_params::param_spec_to_wire)
                    .collect(),
            })
            .collect()
    }

    /// Project the orchestrator's per-plugin group metadata into the GUI's
    /// [`PluginGroupInfo`] shape. One entry per discovered plugin (the
    /// orchestrator emits a row whether or not the plugin has buttons; the
    /// frontend joins on `plugin_id` and ignores rows with no buttons).
    /// Empty when no orchestrator is wired up. The `App` names no runner
    /// catalog type — the forward lives here, like [`Self::actions_catalog`].
    ///
    /// [`PluginGroupInfo`]: foldit_gui::state::PluginGroupInfo
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn plugin_groups(&self) -> Vec<foldit_gui::state::PluginGroupInfo> {
        use foldit_gui::state::PluginGroupInfo;

        let Some(orch) = self.orchestrator.as_ref() else {
            return vec![];
        };
        orch.plugin_groups()
            .into_iter()
            .map(|entry| PluginGroupInfo {
                plugin_id: entry.plugin_id,
                name: entry.name,
                order: entry.order,
            })
            .collect()
    }

    /// Project the orchestrator's custom-panel catalog into the GUI's
    /// [`PanelInfo`] shape. One entry per plugin-declared `[[panels]]`
    /// row; empty when no orchestrator is wired up. Served on demand
    /// through the one-shot request path, not pushed via the projector.
    /// The `App` names no runner catalog type — the forward lives here,
    /// like [`Self::plugin_groups`].
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
    /// [`Self::panels_catalog`]. The `App` names no runner catalog type.
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

    /// Resolve a manifest hotkey string to its `(plugin_id, op_id)` via the
    /// static op catalog. `None` when no catalog button binds the key.
    /// Static identity only: no focus/selection/lock state involved.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn hotkey_to_op(&self, key: &str) -> Option<(String, String)> {
        let orch = self.orchestrator.as_ref()?;
        orch.ops_catalog()
            .into_iter()
            .find(|e| e.hotkey.as_deref() == Some(key))
            .map(|e| (e.plugin_id, e.op_id))
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
