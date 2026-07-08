//! Shared types for the unified backend orchestrator.

use std::collections::{HashMap, HashSet};
#[cfg(not(target_arch = "wasm32"))]
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

// Protocol types owned by foldit-plugin-sdk. Re-exported so the
// orchestrator-internal `crate::orchestrator::{DispatchContext,
// ParamValue, ResidueRef}` paths resolve against the one source of truth.
pub use foldit_plugin_sdk::{DispatchContext, ParamValue, ResidueRef};

pub use super::stream_update::{PluginUpdate, PollOutcome};

// Operation types & locking

/// A lock held on an entity while an operation runs.
#[derive(Debug)]
pub struct EntityLock {
    /// The entity this lock applies to, or `None` for the global lock
    /// (no specific entity targeted).
    pub entity: Option<molex::EntityId>,
    /// Human-readable label of the op holding this lock (the op's
    /// manifest display name), kept for lock-conflict diagnostics.
    pub op_label: String,
    /// Shared cancel flag the running op should poll. Set to `true` by
    /// `request_cancel` or session-level cancellation paths.
    pub cancel_flag: Arc<AtomicBool>,
}

/// Per-entity operation lock table.
///
/// At most one operation can run on an entity at a time. Two extra lock
/// modes layer on top for the unified plugin protocol:
///
/// - **Global lock**: held by ops with no compatible focused entity. Blocks all
///   other lock attempts (entity-specific or global).
/// - **Create barrier**: held by ops that produce new entities
///   (`creates_entities=true`). Blocks other create-ops only — does not block
///   non-create entity-specific ops on unrelated entities.
#[derive(Debug, Default)]
pub struct EntityLockTable {
    active: HashMap<molex::EntityId, EntityLock>,
    /// Global lock. Mutually exclusive with all other locks.
    global_lock: Option<EntityLock>,
    /// Create barrier — see struct doc.
    create_barrier: Option<Arc<AtomicBool>>,
    /// Per-backend (per-plugin) serialization locks. Holds the plugin
    /// ids whose backend worker is currently busy.
    backend_locks: HashSet<String>,
}

impl EntityLockTable {
    /// Build an empty lock table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Try to acquire a lock on an entity.
    /// Returns the cancel flag if successful, `None` if the entity is already
    /// locked.
    pub fn try_lock(
        &mut self,
        entity: molex::EntityId,
        op_label: &str,
    ) -> Option<Arc<AtomicBool>> {
        if self.global_lock.is_some() || self.active.contains_key(&entity) {
            return None;
        }

        let cancel_flag = Arc::new(AtomicBool::new(false));
        let _ = self.active.insert(
            entity,
            EntityLock {
                entity: Some(entity),
                op_label: op_label.to_owned(),
                cancel_flag: cancel_flag.clone(),
            },
        );

        Some(cancel_flag)
    }

    /// Atomically lock a SET of entities. Returns the shared cancel flag if
    /// the WHOLE set is acquirable, `None` (with no mutation) if the global
    /// lock is held or ANY member is already locked.
    ///
    /// All members share ONE cancel flag — the same `Arc` returned to the
    /// dispatch handle — so a cancel that flips the handle's flag (or any
    /// member's, via [`Self::request_cancel`]) cancels the whole set
    /// coherently. Locking members under distinct flags would let a
    /// per-entity cancel flip only one while the stream monitors the
    /// handle's, silently breaking cancel coherence.
    pub fn try_lock_set(
        &mut self,
        entities: &[molex::EntityId],
        op_label: &str,
    ) -> Option<Arc<AtomicBool>> {
        if self.global_lock.is_some() {
            return None;
        }
        // Pre-scan: acquire nothing unless the entire set is free.
        if entities.iter().any(|e| self.active.contains_key(e)) {
            return None;
        }
        let flag = Arc::new(AtomicBool::new(false));
        for &e in entities {
            let _ = self.active.insert(
                e,
                EntityLock {
                    entity: Some(e),
                    op_label: op_label.to_owned(),
                    cancel_flag: flag.clone(),
                },
            );
        }
        Some(flag)
    }

    /// Try to acquire a global lock. Fails if any entity-specific
    /// lock or the global lock itself is held.
    pub fn try_lock_global(
        &mut self,
        op_label: &str,
    ) -> Option<Arc<AtomicBool>> {
        if self.global_lock.is_some() || !self.active.is_empty() {
            return None;
        }
        let cancel_flag = Arc::new(AtomicBool::new(false));
        self.global_lock = Some(EntityLock {
            entity: None,
            op_label: op_label.to_owned(),
            cancel_flag: cancel_flag.clone(),
        });
        Some(cancel_flag)
    }

    /// Release the global lock if held.
    pub fn unlock_global(&mut self) {
        self.global_lock = None;
    }

    /// True if the global lock is currently held.
    #[must_use]
    pub fn is_global_locked(&self) -> bool {
        self.global_lock.is_some()
    }

    /// Try to acquire the create barrier. Fails if it's already held.
    pub fn try_acquire_create_barrier(&mut self) -> Option<Arc<AtomicBool>> {
        if self.create_barrier.is_some() {
            return None;
        }
        let flag = Arc::new(AtomicBool::new(false));
        self.create_barrier = Some(flag.clone());
        Some(flag)
    }

    /// Release the create barrier if held.
    pub fn release_create_barrier(&mut self) {
        self.create_barrier = None;
    }

    /// True if the create barrier is currently held.
    #[must_use]
    pub fn is_create_barrier_held(&self) -> bool {
        self.create_barrier.is_some()
    }

    /// Release the lock on an entity.
    pub fn unlock(&mut self, entity: molex::EntityId) {
        let _ = self.active.remove(&entity);
    }

    /// Check if an entity is currently locked.
    #[must_use]
    pub fn is_locked(&self, entity: molex::EntityId) -> bool {
        self.active.contains_key(&entity)
    }

    /// Get the operation label for a locked entity.
    #[must_use]
    pub fn get_op_label(&self, entity: molex::EntityId) -> Option<String> {
        self.active.get(&entity).map(|lock| lock.op_label.clone())
    }

    /// Request cancellation of the operation on an entity.
    #[must_use]
    pub fn request_cancel(&self, entity: molex::EntityId) -> bool {
        self.active.get(&entity).is_some_and(|lock| {
            lock.cancel_flag.store(true, Ordering::SeqCst);
            true
        })
    }

    /// Get all currently locked entity IDs.
    #[must_use]
    pub fn locked_entities(&self) -> Vec<molex::EntityId> {
        self.active
            .values()
            .filter_map(|lock| lock.entity)
            .collect()
    }

    /// Try to acquire the per-backend (per-plugin) serialization lock.
    /// Returns true if it was free and is now held; false if already held.
    pub fn try_lock_backend(&mut self, plugin_id: &str) -> bool {
        self.backend_locks.insert(plugin_id.to_owned())
    }

    /// Release the per-backend lock for `plugin_id`. Idempotent.
    pub fn unlock_backend(&mut self, plugin_id: &str) {
        let _ = self.backend_locks.remove(plugin_id);
    }
}

/// Puzzle-specific payload that rides the session-init path alongside the
/// assembly bytes: ligand asset files and typed catalytic constraints.
///
/// Built by the host (foldit-core) and handed to
/// [`crate::Orchestrator::kick_init_session`]; the IPC boundary
/// ([`super::client`]) converts it to the proto `InitRequest` fields. Empty
/// (both `Vec`s empty) for a protein-only puzzle or a free-form structure
/// load.
#[derive(Debug, Clone, Default)]
pub struct InitPayload {
    /// Ligand / conformer asset files: `(file_name, bytes)`.
    pub assets: Vec<PuzzleAsset>,
    /// Typed catalytic constraints.
    pub constraints: Vec<Constraint>,
    /// Generic puzzle-config channel: weight-patch entries (keyed
    /// `weight.<scoretype>`) and objective filters (`filter.<i>.*`),
    /// same value shape as the Invoke path. Empty for a protein-only
    /// puzzle; the host (foldit-core) populates it at session-load.
    pub params: HashMap<String, ParamValue>,
}

/// One puzzle asset file delivered at session-init. Mirror of
/// proto::plugin::PuzzleAsset.
#[derive(Debug, Clone)]
pub struct PuzzleAsset {
    /// Asset file name (e.g. "LG1.params").
    pub name: String,
    /// Raw asset bytes.
    pub data: Vec<u8>,
}

/// One atom reference inside a catalytic constraint. Mirror of
/// proto::plugin::ConstraintAtom (chain as a single-char string).
#[derive(Debug, Clone)]
pub struct ConstraintAtom {
    /// PDB atom name, e.g. "OE2".
    pub atom_name: String,
    /// Residue number the atom belongs to.
    pub res_num: i32,
    /// Single chain character, e.g. "A".
    pub chain: String,
}

/// Geometric relation a constraint pins. Mirror of
/// proto::plugin::ConstraintKind (the `Unspecified` sentinel is filtered at
/// the IPC boundary, so consumer code never sees it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConstraintKind {
    /// Distance between 2 atoms.
    AtomPair,
    /// Angle across 3 atoms.
    Angle,
    /// Dihedral across 4 atoms.
    Dihedral,
}

/// Penalty function for a constraint. Mirror of
/// proto::plugin::ConstraintFunc (a oneof on the wire).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ConstraintFunc {
    /// Flat-bottomed harmonic: zero penalty within `tol` of `x0`, harmonic
    /// outside with standard deviation `sd`.
    FlatHarmonic {
        /// Center of the flat-bottom well.
        x0: f64,
        /// Standard deviation of the harmonic walls.
        sd: f64,
        /// Half-width of the zero-penalty flat bottom.
        tol: f64,
    },
    /// Circular (periodic) harmonic about `x0` with standard deviation `sd`.
    CircularHarmonic {
        /// Center of the periodic well.
        x0: f64,
        /// Standard deviation.
        sd: f64,
    },
}

/// One catalytic constraint. Mirror of proto::plugin::Constraint.
#[derive(Debug, Clone)]
pub struct Constraint {
    /// Geometric relation pinned.
    pub kind: ConstraintKind,
    /// Atom references; count follows from `kind`.
    pub atoms: Vec<ConstraintAtom>,
    /// Penalty function.
    pub func: ConstraintFunc,
}

/// Native-Rust parameter type tag. Mirrors `proto::plugin::ParamType`.
/// The proto `Unspecified` sentinel is filtered out at the IPC
/// boundary, so consumer code never sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamType {
    /// 32-bit signed integer.
    Int,
    /// 32-bit float.
    Float,
    /// Boolean.
    Bool,
    /// Free-form UTF-8 string.
    String,
    /// String drawn from a closed set; pair with
    /// [`ParamConstraint::EnumValues`].
    Enum,
    /// 3-component float vector.
    Vec3,
}

/// Native-Rust constraint shape. Mirrors
/// `proto::plugin::ParamConstraints` (a oneof on the wire).
///
/// Drives form rendering by convention: numeric + range as slider,
/// string + enum as dropdown, string + pattern as text input with
/// validation, bool as checkbox.
#[derive(Debug, Clone, PartialEq)]
pub enum ParamConstraint {
    /// Closed integer interval, inclusive on both ends.
    IntRange {
        /// Inclusive lower bound.
        min: i32,
        /// Inclusive upper bound.
        max: i32,
    },
    /// Closed float interval, inclusive on both ends.
    FloatRange {
        /// Inclusive lower bound.
        min: f32,
        /// Inclusive upper bound.
        max: f32,
    },
    /// Closed set of allowed string values.
    EnumValues(Vec<String>),
    /// Regex the string value must match.
    StringPattern(String),
}

/// Native-Rust parameter schema. Mirrors `proto::plugin::ParamSpec`.
///
/// Carried on [`CachedPluginOp`] / [`CachedPluginQuery`] so the GUI's
/// op catalog can render typed input forms without re-reading the proto.
#[derive(Debug, Clone, PartialEq)]
pub struct ParamSpec {
    /// Map key in the `InvokeRequest.params` / `QueryRequest.params`
    /// dictionary.
    pub name: String,
    /// Form-field label shown to the user.
    pub display_name: String,
    /// Tooltip / help text.
    pub description: String,
    /// Value type tag.
    pub param_type: ParamType,
    /// Default value if the user leaves the field unset.
    pub default: Option<ParamValue>,
    /// Optional constraint driving rendering + validation.
    pub constraints: Option<ParamConstraint>,
}

/// Contiguity requirement on an op's effective residue selection.
/// Manifest-authored, so it deserializes from `"any"` / `"contiguous"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Continuity {
    /// No contiguity requirement.
    #[default]
    Any,
    /// One unbroken residue run within a single entity.
    Contiguous,
}

/// Declared selection requirement for a user-facing op button, authored
/// in the manifest `[[buttons]]` table and carried onto [`CatalogEntry`].
/// `actions_catalog` disables a button whose live focus-scoped selection
/// doesn't satisfy it. Fields default, so a manifest can declare only
/// what it constrains (e.g. `selection_spec = { min_residues = 1 }`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
pub struct SelectionSpec {
    /// Minimum number of effective selected residues.
    #[serde(default)]
    pub min_residues: u32,
    /// Maximum effective selected residues. `0` means unbounded.
    #[serde(default)]
    pub max_residues: u32,
    /// Contiguity requirement.
    #[serde(default)]
    pub continuity: Continuity,
}

/// Lock-relevant op metadata, mirroring the lock-related fields of
/// `proto::plugin::PluginOp`. The orchestrator reads this off the
/// `PluginRegistration` cached at plugin Init time.
#[derive(Debug, Clone)]
pub struct OpLockMeta {
    /// Entity types this op accepts as the focused target. Empty list →
    /// the op is global-scoped; locks globally regardless of focus.
    pub compatible_focus_types: Vec<molex::EntityKind>,
    /// True if the op produces new entities (predict, design, binder
    /// design). Adds a global "create barrier" alongside the focus
    /// lock, preventing concurrent create-ops from racing.
    pub creates_entities: bool,
    /// True only for ops that genuinely require a focused target (e.g.
    /// binder design). When set, an unfocused, unselected dispatch refuses
    /// rather than falling back to a global run. Distinct from
    /// `compatible_focus_types`, which only type-restricts an optional focus.
    pub requires_focus: bool,
}

/// Lock targets resolved from `OpLockMeta` + `DispatchContext` at dispatch
/// time. The orchestrator acquires the corresponding locks atomically
/// before forwarding the request to the plugin worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockTargets {
    /// Lock this SET of entities atomically (the entities the op actually
    /// operates on, derived from focus + selection and type-filtered
    /// against `compatible_focus_types`). May be empty when no compatible
    /// entity is in scope (e.g. a ligand is focused but the op is
    /// protein-only); dispatch refuses an empty set.
    Entities(Vec<molex::EntityId>),
    /// Lock globally — op is global-scoped by metadata, or global focus
    /// with nothing selected (operate on the whole structure).
    Global,
}

impl LockTargets {
    /// Resolve the SET of entities an op targets from its `OpLockMeta` and
    /// the per-dispatch `DispatchContext` (focus + selection):
    ///
    /// 1. `compatible_focus_types` empty → `Global` (global-scoped op).
    /// 2. Gather `focus = ctx.focused_entity_id` and `selected` = the distinct
    ///    entity ids appearing in `ctx.selection`.
    /// 3. Pre-filter target:
    ///    - `focus = Some(E)` → `[E]` (only the focused entity; E's own residue
    ///      selection scopes residues at the op level, not entity targeting,
    ///      and other entities' selection is ignored).
    ///    - `focus = None`, `selected` non-empty → `selected`.
    ///    - `focus = None`, `selected` empty → split on `requires_focus`:
    ///        - `requires_focus = true` → `Entities([])` (genuinely
    ///          focus-required op with nothing in scope; button disables and
    ///          dispatch refuses).
    ///        - `requires_focus = false` → `Global` (the documented
    ///          type-restricted-but-optional behavior: a non-empty
    ///          `compatible_focus_types` only filters an optional focus, so an
    ///          unfocused run falls back to a whole-pose / global run).
    /// 4. Type-filter: keep only entities whose `entity_type_of` is in
    ///    `compatible_focus_types`.
    /// 5. `Entities(filtered)` — `filtered` may be empty, in which case
    ///    dispatch refuses.
    ///
    /// `creates_entities` is handled separately by the caller via a
    /// global create barrier.
    pub fn resolve<F>(
        meta: &OpLockMeta,
        ctx: &DispatchContext,
        entity_type_of: F,
    ) -> Self
    where
        F: Fn(molex::EntityId) -> Option<molex::EntityKind>,
    {
        // 1. Global-scoped op: locks globally regardless of focus/selection.
        if meta.compatible_focus_types.is_empty() {
            return LockTargets::Global;
        }

        // 2/3. Pre-filter target set from focus, falling back to the
        // distinct selected entities, falling back to an empty target.
        let pre: Vec<molex::EntityId> = if let Some(e) = ctx.focused_entity_id {
            vec![e]
        } else {
            let mut selected: Vec<molex::EntityId> = Vec::new();
            for r in &ctx.selection {
                if !selected.contains(&r.entity_id) {
                    selected.push(r.entity_id);
                }
            }
            if selected.is_empty() {
                // `compatible_focus_types` is non-empty (step 1 returned for
                // the empty case) but nothing is in scope. Only genuinely
                // focus-required ops refuse here; for the rest a non-empty
                // type list merely filters an optional focus, so an unfocused
                // run falls back to a global / whole-pose run.
                if meta.requires_focus {
                    return LockTargets::Entities(Vec::new());
                }
                return LockTargets::Global;
            }
            selected
        };

        // 4. Type-filter against the op's compatible focus types.
        let filtered: Vec<molex::EntityId> = pre
            .into_iter()
            .filter(|&e| {
                entity_type_of(e)
                    .is_some_and(|t| meta.compatible_focus_types.contains(&t))
            })
            .collect();

        // 5. May be empty; dispatch refuses an empty set.
        LockTargets::Entities(filtered)
    }
}

/// Per-op metadata cached at plugin Init time. Mirrors the `PluginOp`
/// shape from `proto::plugin` but in Rust-native form, with a back-pointer
/// to the owning plugin (for routing).
#[derive(Debug, Clone)]
pub struct CachedPluginOp {
    /// Owning plugin id (matches `PluginRegistration.id`).
    pub plugin_id: String,
    /// Op id, unique within `plugin_id`.
    pub op_id: String,
    /// Display name for UI surfaces.
    pub display_name: String,
    /// Single-shot vs. streaming.
    pub kind: OpKind,
    /// Lock requirements for dispatch.
    pub lock_meta: OpLockMeta,
    /// Typed parameter schema the GUI renders into a form. Empty for
    /// click-to-fire ops.
    pub params: Vec<ParamSpec>,
}

/// Per-query metadata cached at plugin Init time. Mirrors the `PluginQuery`
/// shape from `proto::plugin`. No `lock_meta` (queries don't lock) and
/// no `kind` (queries are single-shot only).
#[derive(Debug, Clone)]
pub struct CachedPluginQuery {
    /// Owning plugin id (matches `PluginRegistration.id`).
    pub plugin_id: String,
    /// Query id, unique within `plugin_id`.
    pub query_id: String,
    /// Display name for UI surfaces.
    pub display_name: String,
    /// Typed parameter schema the GUI renders into a form. Empty for
    /// param-less queries.
    pub params: Vec<ParamSpec>,
}

/// Classification of a plugin op: single-shot vs. streaming.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpKind {
    /// Single-shot op (`Invoke` endpoint).
    Invoke,
    /// Long-running op (`StartStream` / `PollStream` / ...).
    Stream,
}

/// Op-and-query to owning-plugin lookup table.
///
/// Populated as plugins register at Init time; consulted by the
/// orchestrator's dispatch boundary to route requests and look up lock
/// metadata for ops. Query lookup is symmetric but distinct: queries
/// don't carry lock metadata and don't go through
/// `dispatch_lock_check`.
#[derive(Debug, Default)]
pub struct PluginRegistry {
    ops: HashMap<String, CachedPluginOp>,
    /// Queries indexed by id. Multiple plugins can register the same
    /// query id (e.g. "score") -- the host iterates the inner Vec to
    /// gather per-plugin reports and aggregates them. `get_query`
    /// returns the first registered provider (preserves single-plugin
    /// dispatch semantics for queries that aren't aggregated).
    queries: HashMap<String, Vec<CachedPluginQuery>>,
}

impl PluginRegistry {
    /// Build an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a single op. Replaces any prior registration of the same
    /// op id (last-writer-wins on plugin reload).
    pub fn register_op(&mut self, op: CachedPluginOp) {
        let _ = self.ops.insert(op.op_id.clone(), op);
    }

    /// Register a single query. Multiple plugins may register the same
    /// `query_id`; later registrations from the *same* plugin replace
    /// earlier ones for that plugin. Aggregation across plugins
    /// happens at dispatch time via [`Self::query_providers`].
    pub fn register_query(&mut self, query: CachedPluginQuery) {
        let entries = self.queries.entry(query.query_id.clone()).or_default();
        if let Some(slot) =
            entries.iter_mut().find(|q| q.plugin_id == query.plugin_id)
        {
            *slot = query;
        } else {
            entries.push(query);
        }
    }

    /// Bulk-register all ops a plugin owns. Plugin id is taken from each
    /// op's `plugin_id` field.
    pub fn register_ops<I>(&mut self, ops: I)
    where
        I: IntoIterator<Item = CachedPluginOp>,
    {
        for op in ops {
            self.register_op(op);
        }
    }

    /// Bulk-register all queries a plugin owns.
    pub fn register_queries<I>(&mut self, queries: I)
    where
        I: IntoIterator<Item = CachedPluginQuery>,
    {
        for q in queries {
            self.register_query(q);
        }
    }

    /// Drop everything (ops + queries) belonging to a given plugin. Used
    /// when a plugin session is dropped or the worker dies.
    pub fn drop_plugin(&mut self, plugin_id: &str) {
        self.ops.retain(|_, op| op.plugin_id != plugin_id);
        for entries in self.queries.values_mut() {
            entries.retain(|q| q.plugin_id != plugin_id);
        }
        self.queries.retain(|_, entries| !entries.is_empty());
    }

    /// Look up an op by id.
    #[must_use]
    pub fn get_op(&self, op_id: &str) -> Option<&CachedPluginOp> {
        self.ops.get(op_id)
    }

    /// First registered provider of `query_id`. Use
    /// [`Self::query_providers`] when you need to fan out to every
    /// plugin that registered the same query (e.g. score
    /// aggregation).
    #[must_use]
    pub fn get_query(&self, query_id: &str) -> Option<&CachedPluginQuery> {
        self.queries.get(query_id).and_then(|v| v.first())
    }

    /// Every plugin that registered the named query, in registration
    /// order. Empty if no plugin registered the id.
    #[must_use]
    pub fn query_providers(&self, query_id: &str) -> &[CachedPluginQuery] {
        self.queries.get(query_id).map_or(&[], Vec::as_slice)
    }

    /// All op ids belonging to a given plugin.
    #[must_use]
    pub fn ops_for_plugin(&self, plugin_id: &str) -> Vec<&CachedPluginOp> {
        self.ops
            .values()
            .filter(|op| op.plugin_id == plugin_id)
            .collect()
    }

    /// All queries this plugin owns. Each entry is a (query_id,
    /// CachedPluginQuery) pair flattened from the multi-provider
    /// index.
    #[must_use]
    pub fn queries_for_plugin(
        &self,
        plugin_id: &str,
    ) -> Vec<&CachedPluginQuery> {
        self.queries
            .values()
            .flat_map(|v| v.iter())
            .filter(|q| q.plugin_id == plugin_id)
            .collect()
    }

    /// True when neither ops nor queries are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty() && self.queries.is_empty()
    }

    /// Total registered ops.
    #[must_use]
    pub fn op_count(&self) -> usize {
        self.ops.len()
    }

    /// Total registered queries (counts every (plugin, query_id)
    /// pair, so two plugins both registering "score" count as 2).
    #[must_use]
    pub fn query_count(&self) -> usize {
        self.queries.values().map(Vec::len).sum()
    }
}

/// Flat row in the per-frame ops catalog the GUI consumes.
///
/// Built by [`crate::orchestrator::Orchestrator::ops_catalog`] from
/// the intersection of each plugin's manifest `[[buttons]]` array and
/// its bridge-side [`PluginRegistry`] entries. Op-ids are
/// protocol-globally unique (one plugin owns each id); `plugin_id`
/// rides as metadata for diagnostics, not as a namespace prefix. The
/// icon path is manifest-relative (relative to the owning plugin
/// directory); the frontend builds its asset URL as
/// `/plugins/<plugin_id>/<path>`.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone)]
pub struct CatalogEntry {
    /// Owning plugin id (matches `PluginRegistration.id`).
    pub plugin_id: String,
    /// Op id, dispatch key.
    pub op_id: String,
    /// User-facing display label (from the manifest).
    pub display: String,
    /// Manifest-relative path to the icon asset (relative to the owning
    /// plugin directory).
    pub icon_path: PathBuf,
    /// Optional hotkey (winit `KeyCode` debug spelling, e.g. `"KeyW"`),
    /// copied verbatim from the manifest. Rendered as a corner badge;
    /// routing not yet wired. `None` if the manifest omits it or the
    /// catalog dropped it on a collision.
    pub hotkey: Option<String>,
    /// Optional hover tooltip, copied verbatim from the manifest. The
    /// GUI falls back to `display` when `None`.
    pub tooltip: Option<String>,
    /// Typed parameter schema, propagated from the joined
    /// [`CachedPluginOp`]. Empty for click-to-fire ops. Surfaces on
    /// the GUI as `ActionInfo.params` so schema-driven panel widgets
    /// can render typed input forms without an extra round-trip.
    pub params: Vec<ParamSpec>,
    /// Whether the op can be dispatched in the current lock + focus
    /// state. [`Orchestrator::ops_catalog`] leaves this at a neutral
    /// `true` (its static-identity consumers ignore it);
    /// [`Orchestrator::actions_catalog`] overwrites it per the lock rule.
    ///
    /// [`Orchestrator::ops_catalog`]: crate::Orchestrator::ops_catalog
    /// [`Orchestrator::actions_catalog`]: crate::Orchestrator::actions_catalog
    pub enabled: bool,
    /// Optional manifest-authored selection requirement. `None` → no
    /// requirement; `actions_catalog` folds it into `enabled`.
    pub selection_spec: Option<SelectionSpec>,
    /// Whether the op requires the focus-scoped selection to be fully
    /// designable. Manifest-authored. The orchestrator carries it through
    /// but never evaluates it (it holds no design mask); the host
    /// (foldit-core) ANDs the design gate into `enabled`.
    pub requires_designable: bool,
    /// Whether the op renders its stream as a discardable preview rather
    /// than mutating the entity. Manifest-authored. The orchestrator carries
    /// it through but never acts on it; the host (foldit-core) reads it at
    /// dispatch to decide whether the stream is a throwaway preview.
    pub preview: bool,
}
