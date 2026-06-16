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
}
