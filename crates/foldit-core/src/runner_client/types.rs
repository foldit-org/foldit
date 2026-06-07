//! Core-shaped data types that cross the runner-client facade: the
//! dispatch request/result shapes, the neutral edit scope, the inbound
//! plugin-event projection, and the native-only stream bookkeeping. None
//! of these name an orchestrator type in their public surface, so `App`
//! drives dispatch without touching runner vocabulary.

#[cfg(not(target_arch = "wasm32"))]
use molex::Assembly;

/// Core-shaped dispatch request handed to [`RunnerClient::dispatch_op`].
/// Carries only molex / gui-wire types so `App` never builds the
/// orchestrator's `DispatchContext` / `ResidueRef` / `ParamValue` shapes;
/// the flatten and conversion happen inside `dispatch_op`.
///
/// [`RunnerClient::dispatch_op`]: super::RunnerClient::dispatch_op
#[cfg(not(target_arch = "wasm32"))]
pub struct DispatchIntent {
    /// The authoritative in-core selection (molex `EntityId`, same type as
    /// `App.selection`), flattened to per-residue refs at dispatch time.
    pub selection: std::collections::BTreeMap<
        molex::entity::molecule::id::EntityId,
        std::collections::BTreeSet<u32>,
    >,
    /// The focused entity, a molex `EntityId` sourced from the session
    /// focus; passed straight through into the orchestrator's
    /// `DispatchContext`, no wrapping needed.
    pub focused_entity_id: Option<molex::EntityId>,
    /// The op to dispatch; resolved against the registry for Invoke vs Stream.
    pub op_id: String,
    /// Op params in gui-wire form, converted to the orchestrator's native
    /// `ParamValue` inside `dispatch_op`.
    pub params: std::collections::HashMap<String, foldit_gui::state::ParamValue>,
}

/// Core-shaped pull-drag start request handed to
/// [`RunnerClient::start_stream`]. Carries only molex / core-native
/// types; the `DispatchContext` / `ResidueRef` / `ParamValue` build all
/// happen inside `start_stream`, so `App`'s pull-drag path names no
/// orchestrator type. Pull-drag is always a stream (no Invoke branch).
///
/// [`RunnerClient::start_stream`]: super::RunnerClient::start_stream
#[cfg(not(target_arch = "wasm32"))]
pub struct StreamStartIntent {
    /// The pull op-id (one of `pull_drag::OP_PULL_*`); resolved against
    /// the registry inside `start_stream` for the plugin id + dispatch.
    pub op_id: &'static str,
    /// The picked entity (already a molex id - no runner-id wrapping).
    /// Becomes both the `DispatchContext` focus and the single
    /// `ResidueRef`'s entity.
    pub focused_entity: molex::EntityId,
    /// 0-based residue index within the entity; the single selection ref
    /// and the start-param 1-indexing both derive from it.
    pub residue_in_entity: u32,
    /// PDB atom name the user picked; only the sidechain op consumes it
    /// (backbone is residue-anchored), inside `build_start_params`.
    pub atom_name: String,
}

/// Core-side reason a dispatch was refused or failed, produced by
/// [`RunnerClient::dispatch_op`]. Deliberately carries no orchestrator
/// type: the lock refusal is reshaped into a raw entity id so `App`
/// distinguishes a busy-entity refusal (advisory, no error log) from a
/// genuine failure without naming any runner error.
///
/// [`RunnerClient::dispatch_op`]: super::RunnerClient::dispatch_op
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug)]
pub enum DispatchError {
    /// A required entity was already locked by another op; carries the raw
    /// id of the locked entity.
    EntityLocked { entity: u64 },
    /// The plugin's backend worker is already running an op; only one op
    /// per backend at a time.
    BackendBusy {
        /// The plugin whose backend is busy.
        plugin_id: String,
    },
    /// Any other dispatch failure, rendered to a string.
    Failed(String),
}

/// Discriminated result of a dispatch - wraps the two return shapes
/// `dispatch_invoke` and `dispatch_start_stream` produce so
/// `App::handle_dispatch_op` can post-process either uniformly. Lives
/// here (rather than in `app.rs`) because [`RunnerClient::dispatch_op`]
/// is the producer and `App` is just one of two consumers.
///
/// [`RunnerClient::dispatch_op`]: super::RunnerClient::dispatch_op
#[cfg(not(target_arch = "wasm32"))]
pub enum OpOutcome {
    /// Synchronous invoke completed. `request_id` is the dispatch id the
    /// caller keys its edit on; `bytes` is the plugin's reply, fed into
    /// `apply_invoke_result`; `scope` is the entity set the op locked, so
    /// the caller opens its edit over every targeted entity.
    Invoke {
        request_id: u64,
        bytes: Vec<u8>,
        scope: EditScope,
    },
    /// Stream dispatch succeeded; the `DispatchHandle` is already stored
    /// in `StreamHost::active_streams` under `request_id` - the same id
    /// the caller opens its edit under, so there is nothing left to
    /// reconcile here. The matching terminal arm in
    /// `apply_backend_updates` performs the cleanup. `scope` is the entity
    /// set the op locked, so the caller opens its edit over every target.
    Stream { request_id: u64, scope: EditScope },
}

/// The entity set a dispatched op resolved to, threaded from the runner's
/// resolved lock target back to `App` so the edit opens over every entity
/// the op operates on (not the host's single-entity fallback guess). A
/// neutral core-owned scope: it names only `molex::EntityId`, so `App`
/// never sees the runner's `LockTargets` / `DispatchHandle`.
#[cfg(not(target_arch = "wasm32"))]
pub enum EditScope {
    /// A whole-pose / global op: the edit opens over the whole document.
    AllEntities,
    /// The op resolved to this specific entity set (focus / selection,
    /// type-filtered by the runner's lock resolution).
    Entities(Vec<molex::EntityId>),
}

/// Map a runner `DispatchHandle`'s resolved target onto the neutral
/// [`EditScope`]: a `global_held` handle is whole-pose, otherwise the
/// handle's locked entity set.
#[cfg(not(target_arch = "wasm32"))]
pub fn edit_scope_from_handle(
    handle: &foldit_runner::orchestrator::DispatchHandle,
) -> EditScope {
    if handle.global_held {
        EditScope::AllEntities
    } else {
        EditScope::Entities(handle.entities.clone())
    }
}

/// Map the runner's resolved [`LockTargets`] (returned by `dispatch_invoke`)
/// onto the neutral [`EditScope`].
///
/// [`LockTargets`]: foldit_runner::orchestrator::LockTargets
#[cfg(not(target_arch = "wasm32"))]
pub fn edit_scope_from_targets(
    targets: foldit_runner::orchestrator::LockTargets,
) -> EditScope {
    use foldit_runner::orchestrator::LockTargets;
    match targets {
        LockTargets::Global => EditScope::AllEntities,
        LockTargets::Entities(set) => EditScope::Entities(set),
    }
}

/// Core-side projection of inbound plugin traffic, produced by
/// [`RunnerClient::drain_op_events`]. Each variant enumerates one of
/// core's edit-lifecycle verbs keyed by the dispatch `request_id` (the
/// single id `App` opened the edit under), and owns its `Assembly` so the
/// returned batch outlives the driver borrow that produced it. `App`
/// applies these without naming any orchestrator type.
///
/// [`RunnerClient::drain_op_events`]: super::RunnerClient::drain_op_events
#[cfg(not(target_arch = "wasm32"))]
pub enum OpEvent {
    /// Mid-stream tentative frame, keyed by the dispatch `request_id`.
    /// `App` applies it into the edit open under that id, or no-ops when
    /// none is open.
    Update { token: u64, assembly: Assembly },
    /// Terminal success. The runner's distinct `Final` and `Cancelled`
    /// terminals collapse here because core commits either identically.
    /// `token` is the dispatch `request_id`; `App` commits the edit open
    /// under it, or accounts for the terminal with nothing to commit when
    /// `is_pending` reports none is open.
    Commit {
        token: Option<u64>,
        assembly: Assembly,
    },
    /// Terminal failure. `token` is the dispatch `request_id`; `App`
    /// aborts the edit open under it (gated on `is_pending`), or accounts
    /// for the terminal with nothing to abort.
    Abort { token: Option<u64>, reason: String },
}

/// Owns the in-flight stream bookkeeping that only exists on native
/// builds: the plugin stream handle table plus the live pull-drag
/// state. Grouped so App's stream lifecycle touches one field.
#[cfg(not(target_arch = "wasm32"))]
pub struct StreamHost {
    /// In-flight stream handles keyed by `request_id`. Populated by
    /// `handle_dispatch_op` on `StartStream`; the matching
    /// `release_dispatch_locks` runs in `drain_op_events` when the
    /// stream's terminal `PluginUpdate` arrives. The stored
    /// `plugin_id` is the dispatch target for `dispatch_cancel_stream`
    /// when the user hits ESC.
    pub(crate) active_streams: std::collections::HashMap<
        u64,
        ActiveStreamEntry,
    >,
    /// Live pull-drag state. `Some(...)` between pointer-down on an
    /// atom and pointer-up / stream-terminal / ESC cancel. The drag's
    /// stream id also lives in `active_streams` so Final/Error
    /// handling flows through the unified stream-cleanup path; this
    /// field carries the extra viso-side bookkeeping needed for
    /// pointer-move (`PullInfo` + op id).
    pub(crate) pull_drag: Option<crate::pull_drag::PullDrag>,
}

/// Bundle stored per running stream so `drain_op_events` /
/// `cancel_operations` can release locks and dispatch cancel against
/// the right plugin worker without re-querying the orchestrator.
#[cfg(not(target_arch = "wasm32"))]
pub struct ActiveStreamEntry {
    pub(crate) handle: foldit_runner::orchestrator::DispatchHandle,
    pub(crate) plugin_id: String,
}
