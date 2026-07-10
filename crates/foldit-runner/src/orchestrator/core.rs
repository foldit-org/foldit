//! Orchestrator struct — lifecycle, locking, plugin worker pool.

use std::collections::HashMap;
#[cfg(not(target_arch = "wasm32"))]
use std::path::PathBuf;
#[cfg(not(target_arch = "wasm32"))]
use std::sync::mpsc as std_mpsc;

#[cfg(not(target_arch = "wasm32"))]
use super::pump::PluginWorkerHandle;
use super::types::{EntityLockTable, PluginRegistry, PluginUpdate};
#[cfg(not(target_arch = "wasm32"))]
use crate::error::RunnerError;

/// Receiver for one score provider's async `score` reply. Carries the raw
/// query reply bytes; the score pollers decode each into a
/// `proto::ScoreReport`.
#[cfg(not(target_arch = "wasm32"))]
pub(super) type ScoreReplyRx = std_mpsc::Receiver<Result<Vec<u8>, RunnerError>>;

/// Decoded `Init` reply payload: `(session_id, registration,
/// initial_assembly)`. Mirrors the worker-side `InitReply`;
/// `initial_assembly` is the plugin's post-Init normalized assembly
/// bytes (empty when the plugin made no changes).
#[cfg(not(target_arch = "wasm32"))]
pub(super) type InitReplyPayload =
    (u64, crate::proto::plugin::PluginRegistration, Vec<u8>);

/// Receiver for one plugin's async `Init` reply.
///
/// [`Orchestrator::poll_init_sessions`] drains it, caches the
/// registration, and stores the session.
#[cfg(not(target_arch = "wasm32"))]
type InitReplyRx = std_mpsc::Receiver<Result<InitReplyPayload, RunnerError>>;

/// One plugin's in-flight async invoke: the reply receiver plus the lock
/// handle and originating plugin id the poll half needs to finish the
/// round-trip (capture the resolved targets, release the locks, fan the
/// post-op assembly out to peers).
///
/// The [`super::lock_check::DispatchHandle`] is held HERE across frames and
/// released only when [`Orchestrator::poll_invokes`] drains the reply.
/// That differs from the streaming twins (which release on the terminal
/// event) and from the Init twin (which holds no lock at all); it is safe
/// for the intended startup use, where the async invoke is the sole
/// dispatcher and nothing else contends the backend between kick and poll.
#[cfg(not(target_arch = "wasm32"))]
pub(super) struct PendingInvoke {
    /// Receiver for the worker's `Invoke` reply (the post-op assembly
    /// bytes, or a plugin error).
    pub(super) reply: std_mpsc::Receiver<Result<Vec<u8>, RunnerError>>,
    /// Lock handle acquired at kick; released when the reply is drained.
    pub(super) lock: super::lock_check::DispatchHandle,
    /// Owning plugin id, cached so the post-op fan-out skips the
    /// originator without re-resolving the op.
    pub(super) plugin_id: String,
}

/// Unified backend orchestrator.
///
/// Owns the entity-lock table and the plugin worker pool. Plugin
/// streaming updates flow through `plugin_update_rx`. Rosetta is a
/// plugin under `plugins/rosetta/` — dispatch flows through the unified
/// plugin path.
pub struct Orchestrator {
    pub(super) locks: EntityLockTable,

    /// Path to `foldit-worker`. `None` if the binary couldn't be
    /// located at construction; `register_plugin` errors in that case.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) worker_binary: Option<PathBuf>,
    /// Spawn descriptors per plugin id, built from discovery. Used by
    /// `ensure_plugin_registered` to spawn a worker on demand.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) plugin_descriptors:
        HashMap<String, super::spawn::PluginSpawnDescriptor>,
    /// Op-id and query-id → owning-plugin lookup. Populated by
    /// `register_plugin` from the plugin's `PluginRegistration`.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) plugin_registry: PluginRegistry,
    /// Per-plugin worker handle (process + task channel + thread).
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) plugin_workers: HashMap<String, PluginWorkerHandle>,
    /// Per-plugin session id (returned by Init).
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) plugin_sessions: HashMap<String, u64>,
    /// Sender end of the plugin streaming-update channel; cloned into
    /// each spawned worker thread.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) plugin_update_tx: std_mpsc::Sender<PluginUpdate>,
    /// Receiver drained by `pump_updates`; carries `Pending` / `Final` /
    /// `Error` from streaming polls (and Assembly fan-out happens on
    /// `Final`).
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) plugin_update_rx: std_mpsc::Receiver<PluginUpdate>,
    /// Host-side Assembly broadcast generation counter. Bumped on every
    /// `UpdateAssembly` fan-out (post-op or host-originated); stamped on
    /// `UpdateAssemblyRequest.{from_gen,to_gen}` so plugins can detect
    /// dropped broadcasts via the STALE_GEN error path.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) broadcast_gen: u64,
    /// Last full assembly broadcast, used for STALE_GEN recovery (the
    /// orchestrator re-sends this to a plugin that reports it lost
    /// state). `None` before any broadcast has flown.
    ///
    /// Delta broadcasts do NOT overwrite this; foldit-core is
    /// responsible for periodically refreshing the cache via an
    /// explicit full snapshot, otherwise STALE_GEN recovery can only
    /// roll back to the last known full state.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) last_full_broadcast: Option<Vec<u8>>,
    /// Stream request_id → owning plugin_id. Populated by
    /// `dispatch_start_stream`; consumed by `drain_plugin_updates`'s
    /// fan-out + cleanup paths so a Final / Error knows which plugin
    /// originated the stream (required to skip self-broadcast in
    /// stream-side fan-out).
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) stream_plugins: HashMap<u64, String>,
    /// Sole dispatch-id authority. Allocated per dispatch (stream and
    /// invoke) and per host-internal action that opens an edit; flows
    /// down to the plugin for streams and keys the caller's edit. Single
    /// counter, never reused within a session. Starts at 1 so 0 is never
    /// a live id.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) next_request_id: u64,
    /// In-flight async `score` queries, keyed by plugin id. The host
    /// fires `request_scores` (non-blocking submit; the reply lands here)
    /// and drains with `poll_score_results`, so the render thread never
    /// blocks on a score round-trip. One entry per provider with a query
    /// outstanding; this coalesces a fast pose stream against a slow
    /// scorer (a provider already in flight is skipped).
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) pending_score_queries: HashMap<String, ScoreReplyRx>,
    /// In-flight async `weights_status` queries, keyed by plugin id. The
    /// `weights_status` query id is shared across every ML plugin, so the
    /// first-provider-only generic query path can't reach them all; the host
    /// fires `request_weights_status` to fan out to each provider (the reply
    /// lands here) and drains with `poll_weights_status`. One entry per
    /// provider with a query outstanding; a provider already in flight is
    /// skipped. The receiver carries the raw query reply bytes (opaque
    /// `{ready,present,missing}` JSON), decoded downstream, not here.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) pending_weights_queries: HashMap<String, ScoreReplyRx>,
    /// In-flight composition-score requests, keyed by the caller's
    /// correlation `request_id` (an open edit's id, or a fresh id for a
    /// commit-time checkpoint stamp). Each value holds one receiver per
    /// score provider the request fanned out to. The receiver carries the
    /// raw `score` query reply bytes; `poll_composition_scores` decodes
    /// each into a `ScoreReport` and emits `(request_id, ScoreReport)` so
    /// the host routes each reply to its exact target. A request_id already
    /// in flight is skipped, coalescing a fast edit stream against a slow
    /// scorer.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) pending_composition_scores: HashMap<u64, Vec<ScoreReplyRx>>,
    /// In-flight async queries on the generic path, keyed by query id. The
    /// host fires `request_query` (non-blocking submit; the reply lands
    /// here) and drains with `poll_query_results`, so the caller's thread
    /// never blocks on the round-trip. The receiver carries the raw query
    /// reply bytes, which the caller decodes for the specific query. One
    /// entry per query id with a reply outstanding; a second `request_query`
    /// for the same id is skipped (coalesced) while one is in flight.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) pending_queries: HashMap<String, ScoreReplyRx>,
    /// In-flight async queries fired on behalf of a specific caller request
    /// (a webview wish), keyed by that caller's correlation id rather than by
    /// query id. Unlike `pending_queries` these are not coalesced: two wishes
    /// for the same query id are two entries, because each owes its own reply.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) pending_keyed_queries: HashMap<String, ScoreReplyRx>,
    /// Plugins whose worker is being brought up (warmed) without blocking
    /// the caller. `kick_warm_plugin` binds the listener + spawns the
    /// child and stashes the un-accepted [`super::spawn::PendingWorker`]
    /// here, keyed by plugin id; `poll_warm_plugins` retries the accept
    /// each frame and promotes the entry to `plugin_workers` once the
    /// worker connects. Keeping the entry here owns the listener across
    /// frames (dropping it would unlink the socket). One entry per
    /// plugin warming; a second kick for the same id is a no-op.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) pending_warms: HashMap<String, super::spawn::PendingWorker>,
    /// Plugins whose `Init` has been submitted but whose reply has not
    /// arrived. `kick_init_session` submits the Init and stashes the
    /// reply receiver here, keyed by plugin id; `poll_init_sessions`
    /// drains it, caches the registration, and stores the session id.
    /// One entry per plugin with an Init outstanding; a second kick for
    /// the same id is a no-op (the in-flight Init coalesces).
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) pending_inits: HashMap<String, InitReplyRx>,
    /// Plugins whose async invoke has been submitted but whose reply has
    /// not arrived. `kick_invoke` acquires the dispatch lock, submits the
    /// Invoke, and stashes the receiver + held lock handle + plugin id
    /// here, keyed by plugin id; `poll_invokes` drains it, captures the
    /// resolved lock targets, releases the lock, fans the post-op assembly
    /// out, and allocates the dispatch id. The held lock lives across
    /// frames (see [`PendingInvoke`]). One entry per plugin with an invoke
    /// outstanding; a second kick for the same id is a no-op (the in-flight
    /// invoke coalesces).
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) pending_invokes: HashMap<String, PendingInvoke>,
}

impl Default for Orchestrator {
    fn default() -> Self {
        Self::new()
    }
}

impl Orchestrator {
    /// Create a new Orchestrator.
    #[must_use]
    pub fn new() -> Self {
        // Locate the plugin worker binary; tolerate missing — `register_plugin`
        // will surface a clear error if a caller actually tries to spawn.
        #[cfg(not(target_arch = "wasm32"))]
        let worker_binary = match crate::runtime::find_worker_binary(None) {
            Ok(p) => Some(p),
            Err(e) => {
                log::warn!(
                    "[Orchestrator] worker binary not found: {e}; plugin \
                     registration will fail"
                );
                None
            }
        };

        #[cfg(not(target_arch = "wasm32"))]
        let (plugin_update_tx, plugin_update_rx) =
            std_mpsc::channel::<PluginUpdate>();

        Self {
            locks: EntityLockTable::new(),
            #[cfg(not(target_arch = "wasm32"))]
            worker_binary,
            #[cfg(not(target_arch = "wasm32"))]
            plugin_descriptors: HashMap::new(),
            #[cfg(not(target_arch = "wasm32"))]
            plugin_registry: PluginRegistry::new(),
            #[cfg(not(target_arch = "wasm32"))]
            plugin_workers: HashMap::new(),
            #[cfg(not(target_arch = "wasm32"))]
            plugin_sessions: HashMap::new(),
            #[cfg(not(target_arch = "wasm32"))]
            plugin_update_tx,
            #[cfg(not(target_arch = "wasm32"))]
            plugin_update_rx,
            #[cfg(not(target_arch = "wasm32"))]
            broadcast_gen: 0,
            #[cfg(not(target_arch = "wasm32"))]
            last_full_broadcast: None,
            #[cfg(not(target_arch = "wasm32"))]
            stream_plugins: HashMap::new(),
            #[cfg(not(target_arch = "wasm32"))]
            next_request_id: 1,
            #[cfg(not(target_arch = "wasm32"))]
            pending_score_queries: HashMap::new(),
            #[cfg(not(target_arch = "wasm32"))]
            pending_weights_queries: HashMap::new(),
            #[cfg(not(target_arch = "wasm32"))]
            pending_composition_scores: HashMap::new(),
            #[cfg(not(target_arch = "wasm32"))]
            pending_queries: HashMap::new(),
            pending_keyed_queries: HashMap::new(),
            #[cfg(not(target_arch = "wasm32"))]
            pending_warms: HashMap::new(),
            #[cfg(not(target_arch = "wasm32"))]
            pending_inits: HashMap::new(),
            #[cfg(not(target_arch = "wasm32"))]
            pending_invokes: HashMap::new(),
        }
    }

    /// Current broadcast generation. Returns 0 before the first
    /// `UpdateAssembly` broadcast.
    #[cfg(not(target_arch = "wasm32"))]
    #[must_use]
    pub fn broadcast_gen(&self) -> u64 {
        self.broadcast_gen
    }

    /// Override the worker binary path. Useful for tests where the worker
    /// binary lives at `target/<profile>/foldit-worker` rather
    /// than next to the test binary in `target/<profile>/deps/`.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn set_worker_binary(&mut self, path: PathBuf) {
        self.worker_binary = Some(path);
    }

    /// Scan `plugins_root` for `*/plugin.toml`, parse each, and store
    /// the resulting [`super::spawn::PluginSpawnDescriptor`] under its
    /// plugin id. Re-running discovery replaces the descriptor map.
    /// Workers are NOT spawned by this call; that happens in
    /// [`Self::ensure_plugin_registered`] (or
    /// [`Self::register_plugin`] called explicitly).
    ///
    /// # Errors
    ///
    /// Returns an error if `plugins_root` can't be read as a directory.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn discover_plugins(
        &mut self,
        plugins_root: &std::path::Path,
    ) -> anyhow::Result<Vec<String>> {
        let descriptors = super::spawn::discover_plugins(plugins_root)?;
        let mut ids = Vec::with_capacity(descriptors.len());
        self.plugin_descriptors.clear();
        for d in descriptors {
            ids.push(String::from(d.id()));
            let _ = self.plugin_descriptors.insert(String::from(d.id()), d);
        }
        Ok(ids)
    }

    /// All discovered plugin ids in deterministic order. Empty until
    /// [`Self::discover_plugins`] has been called.
    #[cfg(not(target_arch = "wasm32"))]
    #[must_use]
    pub fn discovered_plugin_ids(&self) -> Vec<String> {
        let mut v: Vec<String> =
            self.plugin_descriptors.keys().cloned().collect();
        v.sort();
        v
    }

    /// Look up a discovered descriptor by plugin id. Used by callers
    /// (and the smoke test) to drive `register_plugin` lazily.
    #[cfg(not(target_arch = "wasm32"))]
    #[must_use]
    pub fn plugin_descriptor(
        &self,
        plugin_id: &str,
    ) -> Option<&super::spawn::PluginSpawnDescriptor> {
        self.plugin_descriptors.get(plugin_id)
    }

    /// Read-only handle to the plugin op/query registry. Built up by
    /// `register_plugin` from each plugin's `PluginRegistration`. Used
    /// by foldit-core to resolve op-ids before dispatch (kind lookup,
    /// existence check) without re-serializing through a closure.
    #[cfg(not(target_arch = "wasm32"))]
    #[must_use]
    pub fn plugin_registry(&self) -> &PluginRegistry {
        &self.plugin_registry
    }

    /// Build the per-frame catalog of user-facing buttons. Joins each
    /// discovered plugin's manifest `[[buttons]]` array with the
    /// bridge-side [`super::types::PluginRegistry`] (populated by
    /// `register_plugin`). Manifest button entries whose op-id isn't
    /// registered are dropped with a warning -- the manifest declared
    /// a button for an op the bridge doesn't know about. Op-ids the
    /// bridge registers but the manifest omits stay dispatchable but
    /// don't surface as buttons.
    ///
    /// Returns entries sorted by `(plugin_id, op_id)` so the GUI gets a
    /// stable iteration order across frames.
    #[cfg(not(target_arch = "wasm32"))]
    #[must_use]
    pub fn ops_catalog(&self) -> Vec<super::types::CatalogEntry> {
        let mut entries: Vec<super::types::CatalogEntry> = Vec::new();
        for (plugin_id, descriptor) in &self.plugin_descriptors {
            for button in descriptor.buttons() {
                let Some(cached_op) = self.plugin_registry.get_op(&button.op)
                else {
                    // Ops register at `Init`, so a plugin that has not been
                    // initialized yet -- or that was deactivated for this
                    // structure -- contributes no buttons.
                    log::debug!(
                        "[Orchestrator] manifest button for op {:?} on plugin \
                         {plugin_id} skipped: op-id not registered",
                        button.op
                    );
                    continue;
                };
                let icon_path = button.icon.clone();
                entries.push(super::types::CatalogEntry {
                    plugin_id: plugin_id.clone(),
                    op_id: button.op.clone(),
                    display: button.display.clone(),
                    icon_path,
                    hotkey: button.hotkey.clone(),
                    tooltip: button.tooltip.clone(),
                    params: cached_op.params.clone(),
                    // Neutral default; static-identity consumers ignore
                    // it, and `actions_catalog` overwrites it per-op.
                    enabled: true,
                    // Selection requirement is manifest-authored.
                    selection_spec: button.selection_spec,
                    // Design gate is manifest-authored; the host evaluates
                    // it (the orchestrator holds no design mask).
                    requires_designable: button.requires_designable,
                    determinate_progress: button.determinate_progress,
                    // Preview flag is manifest-authored; the host reads it
                    // to treat the stream as a discardable preview.
                    preview: button.preview,
                    // Option picker is manifest-authored; the host turns each
                    // entry into a full dispatch of this op.
                    options: button.options.clone(),
                });
            }
        }
        entries.sort_by(|a, b| {
            a.plugin_id
                .cmp(&b.plugin_id)
                .then_with(|| a.op_id.cmp(&b.op_id))
        });
        entries
    }

    /// Per-plugin button-group metadata: one row per discovered plugin,
    /// carrying its manifest-declared display `name` and group `order`.
    ///
    /// Emitted for every plugin regardless of whether it contributes
    /// buttons; the GUI joins on `plugin_id` and only renders metadata for
    /// groups that actually have buttons, so extra rows are harmless.
    /// Sorted by `plugin_id` for a stable iteration order.
    #[cfg(not(target_arch = "wasm32"))]
    #[must_use]
    pub fn plugin_groups(&self) -> Vec<PluginGroupEntry> {
        let mut entries: Vec<PluginGroupEntry> = self
            .plugin_descriptors
            .iter()
            .map(|(plugin_id, descriptor)| PluginGroupEntry {
                plugin_id: plugin_id.clone(),
                name: descriptor.name().map(String::from),
                order: descriptor.order(),
            })
            .collect();
        entries.sort_by(|a, b| a.plugin_id.cmp(&b.plugin_id));
        entries
    }

    /// One-shot catalog of plugin-contributed custom panels. One
    /// [`PanelInfo`] row per declared `[[panels]]` entry across every
    /// discovered plugin, with `entry`/`icon_path` shipped manifest-relative
    /// (relative to the owning plugin directory). Sorted by `plugin_id`
    /// for a stable iteration order.
    #[cfg(not(target_arch = "wasm32"))]
    #[must_use]
    pub fn panels_catalog(&self) -> Vec<PanelInfo> {
        let mut entries: Vec<PanelInfo> = Vec::new();
        for (plugin_id, descriptor) in &self.plugin_descriptors {
            for panel in descriptor.panels() {
                entries.push(PanelInfo {
                    plugin_id: plugin_id.clone(),
                    id: panel.id.clone(),
                    title: panel.title.clone(),
                    width: panel.width,
                    entry: panel.entry.clone(),
                    icon_path: panel.icon.clone(),
                    tooltip: panel.tooltip.clone(),
                    position_x: panel.position_x,
                    position_y: panel.position_y,
                });
            }
        }
        entries.sort_by(|a, b| a.plugin_id.cmp(&b.plugin_id));
        entries
    }

    /// One-shot catalog of plugin-contributed settings tabs. One
    /// [`SettingsTabInfo`] row per declared `[[settings]]` entry across
    /// every discovered plugin, with `schema_asset_path` shipped
    /// manifest-relative (relative to the owning plugin directory, mirroring
    /// [`Self::panels_catalog`]). Sorted by `plugin_id` for a stable
    /// iteration order.
    #[cfg(not(target_arch = "wasm32"))]
    #[must_use]
    pub fn settings_catalog(&self) -> Vec<SettingsTabInfo> {
        let mut entries: Vec<SettingsTabInfo> = Vec::new();
        for (plugin_id, descriptor) in &self.plugin_descriptors {
            for entry in descriptor.settings() {
                entries.push(SettingsTabInfo {
                    plugin_id: plugin_id.clone(),
                    name: entry.name.clone(),
                    schema_asset_path: entry.schema_asset_path.clone(),
                    on_update_op: entry.on_update_op.clone(),
                });
            }
        }
        entries.sort_by(|a, b| a.plugin_id.cmp(&b.plugin_id));
        entries
    }

    /// GUI-ready action catalog: every [`Self::ops_catalog`] entry with
    /// its `enabled` flag resolved against the current lock state and the
    /// supplied dispatch context (focus + selection). Reuses the same
    /// focus/selection target resolution dispatch uses
    /// ([`super::types::LockTargets::resolve`]), but READ-ONLY: no lock is
    /// acquired.
    ///
    /// `enabled` rule, per resolved [`super::types::LockTargets`]:
    /// - `Global`: no entity locked and no global lock held.
    /// - `Entities(set)` empty: `false` (no compatible target in scope).
    /// - `Entities(set)`: no global lock held and every target entity free.
    ///
    /// An op whose lock metadata is missing from the registry is disabled
    /// (defensive; it can't be dispatched anyway).
    #[cfg(not(target_arch = "wasm32"))]
    #[must_use]
    pub fn actions_catalog<F>(
        &self,
        ctx: &super::types::DispatchContext,
        entity_type_of: F,
    ) -> Vec<super::types::CatalogEntry>
    where
        F: Fn(molex::EntityId) -> Option<molex::EntityKind>,
    {
        use super::types::LockTargets;
        let mut entries = self.ops_catalog();
        for entry in &mut entries {
            let Some(cached_op) = self.plugin_registry.get_op(&entry.op_id)
            else {
                // Missing from the registry — can't be dispatched anyway.
                entry.enabled = false;
                continue;
            };
            // Lock + focus availability (unchanged from the lock rule).
            let lock_ok = match LockTargets::resolve(
                &cached_op.lock_meta,
                ctx,
                &entity_type_of,
            ) {
                LockTargets::Global => {
                    self.locks.locked_entities().is_empty()
                        && !self.locks.is_global_locked()
                }
                LockTargets::Entities(set) if set.is_empty() => false,
                LockTargets::Entities(set) => {
                    !self.locks.is_global_locked()
                        && set.iter().all(|e| !self.locks.is_locked(*e))
                }
            };
            // Selection requirement, if the manifest declared one. An op
            // with no `selection_spec` is unaffected.
            let sel_ok = entry
                .selection_spec
                .as_ref()
                .is_none_or(|spec| selection_meets_spec(spec, ctx));
            entry.enabled = lock_ok && sel_ok;
        }
        entries
    }

    /// Unlock an entity.
    pub fn unlock(&mut self, entity: molex::EntityId) {
        self.locks.unlock(entity);
    }

    /// All currently locked entities.
    #[must_use]
    pub fn locked_entities(&self) -> Vec<molex::EntityId> {
        self.locks.locked_entities()
    }

    /// Acquire the global lock for a host-native op (the B-factor refine),
    /// returning `true` on success. Fails if any entity lock or the global
    /// lock is already held, so a native global op needs everything free -
    /// the same mutual exclusion a plugin global op gets. `label` is kept for
    /// lock-conflict diagnostics. Release with [`Self::unlock_global`].
    pub fn try_lock_global(&mut self, label: &str) -> bool {
        self.locks.try_lock_global(label).is_some()
    }

    /// Release the global lock if held. Idempotent.
    pub fn unlock_global(&mut self) {
        self.locks.unlock_global();
    }

    /// Whether the global lock is currently held.
    #[must_use]
    pub fn is_global_locked(&self) -> bool {
        self.locks.is_global_locked()
    }

    /// Plugin id that originated a running stream, or `None` if the
    /// stream has already terminated or never existed. Lookup is into
    /// the `stream_plugins` map maintained by `dispatch_start_stream`
    /// and cleaned up in `drain_plugin_updates` on terminal events.
    #[must_use]
    pub fn plugin_id_for_stream(&self, request_id: u64) -> Option<&str> {
        self.stream_plugins.get(&request_id).map(String::as_str)
    }

    /// Allocate the next dispatch `request_id`. The orchestrator is the
    /// single id authority: dispatch (stream and invoke) draws from here,
    /// and so does any host-internal action that opens an edit without a
    /// dispatch (e.g. seeding a post-Init normalized assembly).
    #[cfg(not(target_arch = "wasm32"))]
    pub fn alloc_request_id(&mut self) -> u64 {
        let id = self.next_request_id;
        self.next_request_id = self.next_request_id.saturating_add(1);
        id
    }

    /// Shut down. Plugin workers are torn down by `unregister_plugin`
    /// (called individually per plugin id).
    pub fn shutdown(&self) {}
}

impl Drop for Orchestrator {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Per-plugin button-group metadata the GUI uses to title and order each
/// group box.
///
/// Built by [`Orchestrator::plugin_groups`], one row per discovered plugin
/// (independent of whether the plugin contributes buttons). `plugin_id` is
/// the join key against [`super::types::CatalogEntry::plugin_id`];
/// `name`/`order` are manifest-authored and optional.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone)]
pub struct PluginGroupEntry {
    /// Owning plugin id (matches [`super::types::CatalogEntry::plugin_id`]).
    pub plugin_id: String,
    /// Human display name for the group header. `None` → consumer derives
    /// a title from the id.
    pub name: Option<String>,
    /// Left-to-right sort key. `None` → consumer sorts the group after
    /// every explicit order.
    pub order: Option<u32>,
}

/// One plugin-contributed custom panel, resolved for the GUI.
///
/// Built by [`Orchestrator::panels_catalog`], one row per declared
/// `[[panels]]` entry. `entry`/`icon_path` are manifest-relative (relative
/// to the owning plugin directory); the frontend builds its asset URLs as
/// `/plugins/<plugin_id>/<path>`. `position_x` / `position_y` are the
/// panel's layout-default screen position, distinct from the
/// dragged-position state the GUI tracks per panel.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone)]
pub struct PanelInfo {
    /// Owning plugin id.
    pub plugin_id: String,
    /// Panel id, unique within the plugin.
    pub id: String,
    /// Display title for the panel's title bar.
    pub title: String,
    /// Panel width in pixels.
    pub width: u32,
    /// Manifest-relative path to the panel's ES-module entrypoint.
    pub entry: PathBuf,
    /// Manifest-relative path to the panel's launcher icon.
    pub icon_path: PathBuf,
    /// Optional hover tooltip; consumer falls back to `title`.
    pub tooltip: Option<String>,
    /// Default panel x position in pixels (top-left origin).
    pub position_x: f32,
    /// Default panel y position in pixels (top-left origin).
    pub position_y: f32,
}

/// One plugin-contributed settings tab, resolved for the GUI.
///
/// Built by [`Orchestrator::settings_catalog`], one row per declared
/// `[[settings]]` entry. `schema_asset_path` is manifest-relative (relative
/// to the owning plugin directory); the frontend builds its asset URL as
/// `/plugins/<plugin_id>/<path>`.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone)]
pub struct SettingsTabInfo {
    /// Owning plugin id.
    pub plugin_id: String,
    /// Display name for the settings tab.
    pub name: String,
    /// Manifest-relative path to the settings form's JSON schema.
    pub schema_asset_path: PathBuf,
    /// Op-id the plugin invokes when the settings form is applied.
    pub on_update_op: String,
}

/// Evaluate an op's declared [`super::types::SelectionSpec`] against the
/// live focus-scoped selection in `ctx`.
///
/// "Focus-scoped" mirrors [`super::types::LockTargets::resolve`]: with a
/// focused entity `E` only `E`'s selected residues count; with no focus,
/// every selected residue counts. Returns `true` when the effective
/// selection satisfies the spec's `min_residues` / `max_residues`
/// (`0` = unbounded) and, when `Contiguous`, forms a single unbroken
/// residue run within one entity.
#[cfg(not(target_arch = "wasm32"))]
fn selection_meets_spec(
    spec: &super::types::SelectionSpec,
    ctx: &super::types::DispatchContext,
) -> bool {
    use super::types::Continuity;

    // Effective selection (entity, residue_index) pairs, focus-scoped:
    // with a focused entity keep only its residues; with no focus keep
    // every selected residue.
    let focus = ctx.focused_entity_id;
    let effective: Vec<(molex::EntityId, u32)> = ctx
        .selection
        .iter()
        .filter(|r| focus.is_none_or(|e| r.entity_id == e))
        .map(|r| (r.entity_id, r.residue_index))
        .collect();

    // Counts stay in `usize`; the spec's u32 bounds widen losslessly.
    let count = effective.len();
    let min = spec.min_residues as usize;
    let max = spec.max_residues as usize;
    if count < min {
        return false;
    }
    if max != 0 && count > max {
        return false;
    }

    if spec.continuity == Continuity::Contiguous {
        // Single entity only.
        let mut entities: Vec<molex::EntityId> =
            effective.iter().map(|(e, _)| *e).collect();
        entities.sort_unstable();
        entities.dedup();
        if entities.len() != 1 {
            return false;
        }
        // Contiguous index run: distinct indices spanning exactly `n`.
        let mut idx: Vec<u32> = effective.iter().map(|(_, i)| *i).collect();
        idx.sort_unstable();
        idx.dedup();
        let n = idx.len();
        if n == 0 {
            return false;
        }
        // Index span widens to usize for the comparison (no truncation).
        let span = (idx[idx.len() - 1] - idx[0]) as usize + 1;
        if span != n {
            return false;
        }
    }

    true
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod selection_spec_tests {
    use super::super::types::{
        Continuity, DispatchContext, ResidueRef, SelectionSpec,
    };
    use super::selection_meets_spec;

    fn eid(raw: u32) -> molex::EntityId {
        molex::EntityId::from_raw(raw)
    }

    fn ctx(focus: Option<u32>, sel: &[(u32, u32)]) -> DispatchContext {
        DispatchContext {
            focused_entity_id: focus.map(eid),
            selection: sel
                .iter()
                .map(|&(e, r)| ResidueRef {
                    entity_id: eid(e),
                    residue_index: r,
                })
                .collect(),
            ..Default::default()
        }
    }

    fn spec(min: u32, max: u32, c: Continuity) -> SelectionSpec {
        SelectionSpec {
            min_residues: min,
            max_residues: max,
            continuity: c,
        }
    }

    #[test]
    fn rebuild_min1_enables_on_one_residue_no_focus() {
        let s = spec(1, 0, Continuity::Any);
        assert!(!selection_meets_spec(&s, &ctx(None, &[])));
        assert!(selection_meets_spec(&s, &ctx(None, &[(0, 5)])));
    }

    #[test]
    fn idealize_min2_contiguous() {
        let s = spec(2, 0, Continuity::Contiguous);
        assert!(!selection_meets_spec(&s, &ctx(None, &[(0, 5)]))); // < 2
        assert!(selection_meets_spec(&s, &ctx(None, &[(0, 5), (0, 6)]))); // run
        assert!(!selection_meets_spec(&s, &ctx(None, &[(0, 5), (0, 8)]))); // gap
        assert!(!selection_meets_spec(&s, &ctx(None, &[(0, 5), (1, 6)]))); // 2 entities
    }

    #[test]
    fn remix_3_to_9_contiguous() {
        let s = spec(3, 9, Continuity::Contiguous);
        assert!(!selection_meets_spec(&s, &ctx(None, &[(0, 1), (0, 2)]))); // < 3
        assert!(selection_meets_spec(
            &s,
            &ctx(None, &[(0, 1), (0, 2), (0, 3)])
        ));
        let ten: Vec<(u32, u32)> = (0..10).map(|i| (0, i)).collect();
        assert!(!selection_meets_spec(&s, &ctx(None, &ten))); // > 9
    }

    #[test]
    fn focus_scopes_out_other_entities() {
        // The failure mode: focus on E1 while the selection is on E0 ->
        // effective is empty -> even a min-1 spec is unmet.
        let s = spec(1, 0, Continuity::Any);
        assert!(selection_meets_spec(&s, &ctx(Some(0), &[(0, 5)])));
        assert!(!selection_meets_spec(&s, &ctx(Some(1), &[(0, 5)])));
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod actions_catalog_focus_tests {
    use std::path::PathBuf;

    use super::super::manifest::ButtonEntry;
    use super::super::spawn::PluginSpawnDescriptor;
    use super::super::types::{
        CachedPluginOp, DispatchContext, LockTargets, OpKind, OpLockMeta,
    };
    use super::Orchestrator;

    const PLUGIN: &str = "p";
    const OP: &str = "test_op";

    fn eid(raw: u32) -> molex::EntityId {
        molex::EntityId::from_raw(raw)
    }

    fn meta(
        focus_types: Vec<molex::EntityKind>,
        requires_focus: bool,
    ) -> OpLockMeta {
        OpLockMeta {
            compatible_focus_types: focus_types,
            creates_entities: false,
            requires_focus,
        }
    }

    /// Orchestrator with one registered op and a single manifest button that
    /// dispatches it, so `actions_catalog` surfaces exactly one entry.
    fn orch_with_op(lock_meta: OpLockMeta) -> Orchestrator {
        let mut o = Orchestrator::new();
        o.plugin_registry.register_op(CachedPluginOp {
            plugin_id: String::from(PLUGIN),
            op_id: String::from(OP),
            display_name: String::from("Test Op"),
            kind: OpKind::Invoke,
            lock_meta,
            params: vec![],
        });
        let button = ButtonEntry {
            determinate_progress: false,
            op: String::from(OP),
            display: String::from("Test Op"),
            icon: PathBuf::new(),
            hotkey: None,
            tooltip: None,
            selection_spec: None,
            requires_designable: false,
            preview: false,
            options: Vec::new(),
        };
        let _ = o.plugin_descriptors.insert(
            String::from(PLUGIN),
            PluginSpawnDescriptor::Native {
                id: String::from(PLUGIN),
                plugin_dir: PathBuf::new(),
                name: None,
                order: None,
                uses_density: false,
                buttons: vec![button],
                panels: vec![],
                settings: vec![],
                provides_density: false,
            },
        );
        o
    }

    #[allow(clippy::unnecessary_wraps)]
    fn always_protein(_: molex::EntityId) -> Option<molex::EntityKind> {
        Some(molex::EntityKind::Protein)
    }

    // rfd3-class: genuinely focus-required (requires_focus=true). No focus →
    // empty target, button disabled.
    #[test]
    fn focus_required_no_focus_resolves_empty_and_disables() {
        let m = meta(vec![molex::EntityKind::Protein], true);
        let ctx = DispatchContext::default();
        match LockTargets::resolve(&m, &ctx, always_protein) {
            LockTargets::Entities(set) => assert!(set.is_empty()),
            LockTargets::Global => {
                panic!("focus-required op must not go Global")
            }
        }

        let o = orch_with_op(meta(vec![molex::EntityKind::Protein], true));
        let entries = o.actions_catalog(&ctx, always_protein);
        assert_eq!(entries.len(), 1);
        assert!(!entries[0].enabled);
    }

    #[test]
    fn focus_required_with_focus_resolves_nonempty_and_enables() {
        let m = meta(vec![molex::EntityKind::Protein], true);
        let ctx = DispatchContext {
            focused_entity_id: Some(eid(42)),
            ..Default::default()
        };
        match LockTargets::resolve(&m, &ctx, always_protein) {
            LockTargets::Entities(set) => assert_eq!(set, vec![eid(42)]),
            LockTargets::Global => {
                panic!("focused op must resolve to entities")
            }
        }

        let o = orch_with_op(meta(vec![molex::EntityKind::Protein], true));
        let entries = o.actions_catalog(&ctx, always_protein);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].enabled);
    }

    // Shake-class (the regression fix): non-empty compatible types are only a
    // type filter (requires_focus=false). No focus → global fallback, button
    // stays enabled.
    #[test]
    fn type_restricted_no_focus_resolves_global_and_enables() {
        let m = meta(vec![molex::EntityKind::Protein], false);
        let ctx = DispatchContext::default();
        assert!(matches!(
            LockTargets::resolve(&m, &ctx, always_protein),
            LockTargets::Global
        ));

        let o = orch_with_op(meta(vec![molex::EntityKind::Protein], false));
        let entries = o.actions_catalog(&ctx, always_protein);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].enabled);
    }

    #[test]
    fn global_op_no_focus_resolves_global_and_enables() {
        let m = meta(vec![], false);
        let ctx = DispatchContext::default();
        assert!(matches!(
            LockTargets::resolve(&m, &ctx, always_protein),
            LockTargets::Global
        ));

        let o = orch_with_op(meta(vec![], false));
        let entries = o.actions_catalog(&ctx, always_protein);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].enabled);
    }
}
