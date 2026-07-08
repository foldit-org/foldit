//! Plugin `Init`-payload assembly: flatten the loaded puzzle's weight patch
//! and objective filters into the generic config-param channel, and adopt a
//! plugin's post-`Init` normalized pose back onto the host's canonical
//! assembly.

use super::App;
use crate::history::CheckpointKind;
use molex::entity::molecule::id::EntityId;

impl App {
    /// Build the loaded puzzle's generic config-param channel for the Init
    /// payload: the scorefunction weight patch as `weight.<scoretype>` ->
    /// `Float(weight)` entries, plus the rosetta-targeted objective filters
    /// as `filter.<i>.*` String entries. Empty when the session carries no
    /// puzzle (a free-form structure load) or the puzzle declares neither.
    ///
    /// Weight patch: one entry per patched term, so weight-zero terms (e.g.
    /// `envsmooth`) ship and are optimized against.
    ///
    /// Filters: only those naming `plugin = "rosetta"` are forwarded; each
    /// takes a contiguous index `i` over the forwarded filters, and the
    /// bridge decodes `filter.<i>.type` for the filter kind and
    /// `filter.<i>.<key>` for each flattened param, all String-typed. A
    /// non-rosetta plugin is unsupported: warn (naming it) and skip rather
    /// than forward to a bridge that cannot score it.
    pub(super) fn build_init_config_params(
        &self,
    ) -> std::collections::HashMap<String, foldit_gui::state::ParamValue> {
        let mut params: std::collections::HashMap<String, foldit_gui::state::ParamValue> = self
            .store
            .puzzle()
            .and_then(|p| p.weight_patch.as_ref())
            .map(|patch| {
                patch
                    .iter()
                    .map(|(name, &w)| {
                        (
                            format!("weight.{name}"),
                            foldit_gui::state::ParamValue::Float(w),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();

        if let Some(filters) = self.store.puzzle().map(|p| p.filters.clone()) {
            let mut i = 0;
            for spec in &filters {
                match spec.plugin.as_deref() {
                    None => {}
                    Some("rosetta") => {
                        params.insert(
                            format!("filter.{i}.type"),
                            foldit_gui::state::ParamValue::String(spec.kind.clone()),
                        );
                        for (key, value) in &spec.params {
                            params.insert(
                                format!("filter.{i}.{key}"),
                                foldit_gui::state::ParamValue::String(toml_value_to_plain_string(
                                    value,
                                )),
                            );
                        }
                        i += 1;
                    }
                    Some(other) => {
                        log::warn!(
                            "[App] puzzle filter '{}' names unknown plugin '{other}'; \
                             skipping (only 'rosetta' is forwarded)",
                            spec.kind,
                        );
                    }
                }
            }
        }

        params
    }

    /// Apply a plugin's post-Init normalized assembly (full-atom pose) so
    /// the host's canonical assembly matches the plugin's internal pose
    /// before any user action runs. Every entity the normalized assembly
    /// touches that has a committed lane in the store is normalized inside
    /// a single multi-lane edit.
    pub(super) fn apply_post_init(
        &mut self,
        plugin_id: &str,
        post_init_bytes: &[u8],
        op_id: &str,
        display: &str,
    ) {
        if post_init_bytes.is_empty() {
            log::warn!(
                "[App] {plugin_id} post-Init returned no normalized assembly; \
                 first user action will likely snap because scene.positions \
                 stays at the pre-Init atom count."
            );
            return;
        }
        let normalized = match molex::Assembly::from_bytes(post_init_bytes) {
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
            op_id: String::from(op_id),
            display: String::from(display),
        };
        // Host-internal action: no dispatch happened, so draw the edit's
        // request_id straight from the orchestrator (the single id
        // authority).
        let Some(request_id) = self.runner_client.alloc_request_id() else {
            log::warn!(
                "[App] {plugin_id} post-Init: no orchestrator to allocate a \
                 request id; skipping normalization apply"
            );
            return;
        };
        if let Err(e) = self.store.begin_action(
            target_entities,
            kind,
            String::from(display),
            request_id,
            std::collections::BTreeMap::new(),
        ) {
            log::warn!(
                "[App] {plugin_id} post-Init begin_action failed: {e}; \
                 skipping normalization apply"
            );
            return;
        }
        let applied = self
            .store
            .apply_streaming_assembly(&normalized, None, request_id);
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
}

/// Render a `toml::Value` as the bare string the forwarded-filter param
/// convention expects: an `Integer(-100)` becomes `"-100"`, a `Float` its
/// decimal string, and a `String` its bare contents (no surrounding quotes,
/// unlike `Value::to_string`). Other variants fall back to their `Display`
/// form. Used to flatten a `FilterSpec.params` entry into a `filter.<i>.<key>`
/// String param.
fn toml_value_to_plain_string(value: &toml::Value) -> String {
    match value {
        toml::Value::String(s) => s.clone(),
        toml::Value::Integer(n) => n.to_string(),
        toml::Value::Float(f) => f.to_string(),
        toml::Value::Boolean(b) => b.to_string(),
        other => other.to_string(),
    }
}
