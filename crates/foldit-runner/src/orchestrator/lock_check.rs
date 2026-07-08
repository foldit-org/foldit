//! Plugin-protocol dispatch boundary — lock check.
//!
//! Lock model:
//!
//! - Per-op metadata (`OpLockMeta`) declares `compatible_focus_types` and
//!   `creates_entities`.
//! - At dispatch time, the orchestrator resolves the SET of entities the op
//!   operates on from the request's `DispatchContext` (focus + selection):
//!   empty `compatible_focus_types` → global lock; focused entity present →
//!   that entity (type-filtered); no focus but a selection → the distinct
//!   selected entities (type-filtered); no focus and no selection → global
//!   lock. The resolved set is locked atomically (all-or-nothing). An empty
//!   type-filtered set (e.g. a ligand focused under a protein-only op) is
//!   refused.
//! - If `creates_entities=true`, an additional global create barrier is
//!   acquired alongside the set / global lock.
//!
//! Plugins never see locks; by the time a request reaches a plugin,
//! the check here has already passed.
//!
//! The implementation lives on `EntityLockTable` so it's testable in
//! isolation; `Orchestrator::dispatch_lock_check` is a thin forwarder.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use super::core::Orchestrator;
use super::types::{DispatchContext, EntityLockTable, LockTargets, OpLockMeta};

/// Successful dispatch — holds onto the locks the op acquired. The caller
/// MUST return this to `release_dispatch_locks` once the op finishes
/// (natural end, cancel, or error) so the locks are freed.
#[derive(Debug)]
pub struct DispatchHandle {
    /// Per-entity locks held. Empty if the op locked globally.
    pub entities: Vec<molex::EntityId>,
    /// True if the op also holds a global lock.
    pub global_held: bool,
    /// True if the op also holds the create barrier (for `creates_entities`
    /// ops).
    pub create_barrier_held: bool,
    /// Cancel flag the op should monitor. Aliased into the lock table so
    /// any cancel-by-entity / cancel-by-session call flips it.
    pub cancel_flag: Arc<AtomicBool>,
    /// The per-backend lock this op holds, if any. Released by
    /// `release_dispatch_locks`.
    pub backend_lock: Option<String>,
}

/// Reason a dispatch was refused.
#[derive(Debug, PartialEq, Eq)]
pub enum DispatchError {
    /// A required entity is already locked by another op.
    EntityLocked {
        /// The entity whose lock is held.
        entity: molex::EntityId,
        /// Manifest display label of the holding op, if recorded.
        current_op: Option<String>,
    },
    /// The op declares compatible focus types but no entity in scope
    /// (focus + selection, after type-filtering) matches — e.g. a ligand
    /// is focused under a protein-only op. There is nothing to operate on.
    NoCompatibleTarget,
    /// A global lock was requested, but other ops are running on
    /// individual entities, we can't take a global lock while they hold
    /// per-entity locks.
    Busy {
        /// Entities holding per-entity locks at the time of refusal.
        active: Vec<molex::EntityId>,
    },
    /// The global lock is already held — only one global op
    /// at a time.
    AlreadyLocked,
    /// Another create-op is in flight; only one create-op at a time.
    CreateBarrierBusy,
    /// Another op is already running on this plugin's backend worker;
    /// only one op per backend at a time.
    BackendBusy {
        /// The plugin whose backend is busy.
        plugin_id: String,
    },
}

impl EntityLockTable {
    /// Resolve lock targets per `OpLockMeta` and `DispatchContext`, acquire
    /// the corresponding locks, and return a handle.
    ///
    /// See module docs for the resolution rules.
    ///
    /// `entity_type_of` is the caller-provided lookup mapping entity id
    /// to entity type. The lock table doesn't hold an Assembly; the
    /// caller (typically `EntityStore` in foldit-core, or
    /// `RosettaSessionState`) supplies the resolution.
    ///
    /// # Errors
    ///
    /// Returns a [`DispatchError`] variant if a required lock is
    /// already held.
    pub fn dispatch_lock_check<F>(
        &mut self,
        meta: &OpLockMeta,
        ctx: &DispatchContext,
        op_label: &str,
        entity_type_of: F,
    ) -> Result<DispatchHandle, DispatchError>
    where
        F: Fn(molex::EntityId) -> Option<molex::EntityKind>,
    {
        let targets = LockTargets::resolve(meta, ctx, &entity_type_of);

        // Acquire the create barrier first if needed. If we fail to
        // acquire the focus / global lock below, release this on the
        // way out.
        let create_barrier_held = if meta.creates_entities {
            if self.try_acquire_create_barrier().is_none() {
                return Err(DispatchError::CreateBarrierBusy);
            }
            true
        } else {
            false
        };

        match targets {
            LockTargets::Entities(set) => {
                self.dispatch_set_lock(set, op_label, create_barrier_held)
            }
            LockTargets::Global => {
                self.dispatch_global_lock(op_label, create_barrier_held)
            }
        }
    }

    fn dispatch_set_lock(
        &mut self,
        set: Vec<molex::EntityId>,
        op_label: &str,
        create_barrier_held: bool,
    ) -> Result<DispatchHandle, DispatchError> {
        // No compatible entity in scope: nothing to operate on. Release the
        // create barrier if we took one, then refuse.
        if set.is_empty() {
            if create_barrier_held {
                self.release_create_barrier();
            }
            return Err(DispatchError::NoCompatibleTarget);
        }

        let Some(cancel_flag) = self.try_lock_set(&set, op_label) else {
            if create_barrier_held {
                self.release_create_barrier();
            }
            // Name the first member that's individually locked. If the
            // refusal was instead the global lock (no member locked), fall
            // back to set[0] with no op label — matching the prior
            // single-entity behavior where a global lock blocked an entity
            // op as `EntityLocked { current_op: None }`.
            let conflict = set
                .iter()
                .copied()
                .find(|&e| self.is_locked(e))
                .unwrap_or(set[0]);
            return Err(DispatchError::EntityLocked {
                entity: conflict,
                current_op: self.get_op_label(conflict),
            });
        };
        Ok(DispatchHandle {
            entities: set,
            global_held: false,
            create_barrier_held,
            cancel_flag,
            backend_lock: None,
        })
    }

    fn dispatch_global_lock(
        &mut self,
        op_label: &str,
        create_barrier_held: bool,
    ) -> Result<DispatchHandle, DispatchError> {
        if self.is_global_locked() {
            if create_barrier_held {
                self.release_create_barrier();
            }
            return Err(DispatchError::AlreadyLocked);
        }
        let active = self.locked_entities();
        if !active.is_empty() {
            if create_barrier_held {
                self.release_create_barrier();
            }
            return Err(DispatchError::Busy { active });
        }
        self.try_lock_global(op_label).map_or_else(
            || {
                if create_barrier_held {
                    self.release_create_barrier();
                }
                Err(DispatchError::AlreadyLocked)
            },
            |cancel_flag| {
                Ok(DispatchHandle {
                    entities: vec![],
                    global_held: true,
                    create_barrier_held,
                    cancel_flag,
                    backend_lock: None,
                })
            },
        )
    }

    /// Release the locks held by a dispatch handle.
    pub fn release_dispatch_locks(&mut self, handle: DispatchHandle) {
        for eid in handle.entities {
            self.unlock(eid);
        }
        if handle.global_held {
            self.unlock_global();
        }
        if handle.create_barrier_held {
            self.release_create_barrier();
        }
        if let Some(plugin_id) = handle.backend_lock {
            self.unlock_backend(&plugin_id);
        }
    }
}

impl Orchestrator {
    /// Forward to `EntityLockTable::dispatch_lock_check`. See that method's
    /// docs for the lock-resolution rules.
    ///
    /// # Errors
    ///
    /// Returns a [`DispatchError`] variant if a required lock is
    /// already held.
    pub fn dispatch_lock_check<F>(
        &mut self,
        meta: &OpLockMeta,
        ctx: &DispatchContext,
        op_label: &str,
        entity_type_of: F,
    ) -> Result<DispatchHandle, DispatchError>
    where
        F: Fn(molex::EntityId) -> Option<molex::EntityKind>,
    {
        self.locks
            .dispatch_lock_check(meta, ctx, op_label, entity_type_of)
    }

    /// Release the locks held by a dispatch handle.
    pub fn release_dispatch_locks(&mut self, handle: DispatchHandle) {
        self.locks.release_dispatch_locks(handle);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestrator::types::ResidueRef;

    fn meta(focus_types: Vec<molex::EntityKind>, creates: bool) -> OpLockMeta {
        OpLockMeta {
            compatible_focus_types: focus_types,
            creates_entities: creates,
            requires_focus: false,
        }
    }

    /// Genuinely focus-required variant (rfd3-class): refuses an unfocused,
    /// unselected dispatch instead of falling back to a global run.
    fn meta_focus_required(focus_types: Vec<molex::EntityKind>) -> OpLockMeta {
        OpLockMeta {
            compatible_focus_types: focus_types,
            creates_entities: false,
            requires_focus: true,
        }
    }

    fn eid(raw: u32) -> molex::EntityId {
        molex::EntityId::from_raw(raw)
    }

    fn ctx_focused(id: u64) -> DispatchContext {
        DispatchContext {
            focused_entity_id: Some(eid(id as u32)),
            selection: vec![],
            ..Default::default()
        }
    }

    fn ctx_unfocused() -> DispatchContext {
        DispatchContext::default()
    }

    /// No focus; a residue selection spanning the given entities (one ref
    /// per entity is enough — `resolve` distinct-dedups by entity id).
    fn ctx_selected(entities: &[u32]) -> DispatchContext {
        DispatchContext {
            focused_entity_id: None,
            selection: entities
                .iter()
                .map(|&e| ResidueRef {
                    entity_id: eid(e),
                    residue_index: 0,
                })
                .collect(),
            ..Default::default()
        }
    }

    #[allow(clippy::unnecessary_wraps)]
    fn always_protein(_: molex::EntityId) -> Option<molex::EntityKind> {
        Some(molex::EntityKind::Protein)
    }

    fn always_none(_: molex::EntityId) -> Option<molex::EntityKind> {
        None
    }

    /// Entity 2 is a small molecule (ligand); everything else is protein.
    /// Used to exercise the type-filter across a heterogeneous selection.
    #[allow(clippy::unnecessary_wraps)]
    fn protein_except_2(e: molex::EntityId) -> Option<molex::EntityKind> {
        if e == eid(2) {
            Some(molex::EntityKind::SmallMolecule)
        } else {
            Some(molex::EntityKind::Protein)
        }
    }

    #[test]
    fn focused_compatible_locks_just_that_entity() {
        let mut t = EntityLockTable::new();
        let h = t
            .dispatch_lock_check(
                &meta(vec![molex::EntityKind::Protein], false),
                &ctx_focused(42),
                "Wiggle",
                always_protein,
            )
            .expect("should lock focused entity");
        assert_eq!(h.entities, vec![molex::EntityId::from_raw(42)]);
        assert!(!h.global_held);
        assert!(!h.create_barrier_held);
    }

    #[test]
    fn no_focus_focus_required_op_refuses() {
        // A genuinely focus-required op (requires_focus=true) with no focus and
        // no selection resolves to an empty entity set, NOT a global lock:
        // dispatch refuses rather than operating on the whole structure (and
        // the button disables upstream).
        let mut t = EntityLockTable::new();
        let r = t.dispatch_lock_check(
            &meta_focus_required(vec![molex::EntityKind::Protein]),
            &ctx_unfocused(),
            "rfd3_design",
            always_protein,
        );
        assert!(matches!(r, Err(DispatchError::NoCompatibleTarget)));
    }

    #[test]
    fn no_focus_type_restricted_op_takes_global_lock() {
        // The regression fix: a type-restricted op (non-empty compatible types
        // but requires_focus=false) with no focus/selection falls back to a
        // global lock rather than refusing.
        let mut t = EntityLockTable::new();
        let h = t
            .dispatch_lock_check(
                &meta(vec![molex::EntityKind::Protein], false),
                &ctx_unfocused(),
                "Shake",
                always_protein,
            )
            .expect("type-restricted op should fall back to a global lock");
        assert!(h.global_held);
        assert!(h.entities.is_empty());
    }

    #[test]
    fn focused_type_mismatch_refused() {
        // A protein is focused under a small-molecule-only op: the focused
        // entity is type-filtered out, leaving an empty set → refusal (no
        // global fallback; we don't widen scope past the focused entity).
        let mut t = EntityLockTable::new();
        let r = t.dispatch_lock_check(
            &meta(vec![molex::EntityKind::SmallMolecule], false),
            &ctx_focused(42),
            "Pull",
            always_protein,
        );
        assert!(matches!(r, Err(DispatchError::NoCompatibleTarget)));
    }

    #[test]
    fn empty_compatible_types_always_global() {
        let mut t = EntityLockTable::new();
        let h = t
            .dispatch_lock_check(
                &meta(vec![], false),
                &ctx_focused(42),
                "StructureDesign",
                always_protein,
            )
            .expect("should take global lock");
        assert!(h.global_held);
    }

    #[test]
    fn focused_unknown_entity_type_refused() {
        // Focused entity has no resolvable type → type-filtered out →
        // empty set → refusal.
        let mut t = EntityLockTable::new();
        let r = t.dispatch_lock_check(
            &meta(vec![molex::EntityKind::Protein], false),
            &ctx_focused(99),
            "Shake",
            always_none,
        );
        assert!(matches!(r, Err(DispatchError::NoCompatibleTarget)));
    }

    #[test]
    fn creates_entities_acquires_barrier() {
        let mut t = EntityLockTable::new();
        let h = t
            .dispatch_lock_check(
                &meta(vec![molex::EntityKind::Protein], true),
                &ctx_focused(7),
                "StructureDesign",
                always_protein,
            )
            .expect("should acquire barrier + entity lock");
        assert_eq!(h.entities, vec![molex::EntityId::from_raw(7)]);
        assert!(h.create_barrier_held);
    }

    #[test]
    fn second_create_op_blocked_by_barrier() {
        let mut t = EntityLockTable::new();
        let _h1 = t
            .dispatch_lock_check(
                &meta(vec![molex::EntityKind::Protein], true),
                &ctx_focused(1),
                "StructureDesign",
                always_protein,
            )
            .expect("first should succeed");
        let r2 = t.dispatch_lock_check(
            &meta(vec![molex::EntityKind::Protein], true),
            &ctx_focused(2),
            "StructureDesign",
            always_protein,
        );
        assert!(matches!(r2, Err(DispatchError::CreateBarrierBusy)));
    }

    #[test]
    fn entity_lock_blocks_global() {
        let mut t = EntityLockTable::new();
        let _h1 = t
            .dispatch_lock_check(
                &meta(vec![molex::EntityKind::Protein], false),
                &ctx_focused(1),
                "Wiggle",
                always_protein,
            )
            .expect("entity-locked op succeeds");
        let r2 = t.dispatch_lock_check(
            &meta(vec![], false),
            &ctx_unfocused(),
            "Predict",
            always_protein,
        );
        assert!(matches!(r2, Err(DispatchError::Busy { .. })));
    }

    #[test]
    fn global_blocks_subsequent_entity_lock() {
        let mut t = EntityLockTable::new();
        let _h1 = t
            .dispatch_lock_check(
                &meta(vec![], false),
                &ctx_unfocused(),
                "Predict",
                always_protein,
            )
            .expect("global succeeds");
        let r2 = t.dispatch_lock_check(
            &meta(vec![molex::EntityKind::Protein], false),
            &ctx_focused(1),
            "Wiggle",
            always_protein,
        );
        assert!(matches!(r2, Err(DispatchError::EntityLocked { .. })));
    }

    #[test]
    fn release_frees_all_locks() {
        let mut t = EntityLockTable::new();
        let h = t
            .dispatch_lock_check(
                &meta(vec![molex::EntityKind::Protein], true),
                &ctx_focused(5),
                "StructureDesign",
                always_protein,
            )
            .unwrap();
        t.release_dispatch_locks(h);
        // Same shape op should now succeed again.
        let h2 = t
            .dispatch_lock_check(
                &meta(vec![molex::EntityKind::Protein], true),
                &ctx_focused(5),
                "StructureDesign",
                always_protein,
            )
            .expect("locks released, should re-acquire");
        t.release_dispatch_locks(h2);
    }

    #[test]
    fn create_barrier_does_not_block_unrelated_entity_op() {
        // Binder design (creates) on entity 1, plus wiggle (no create) on
        // entity 2, can run concurrently.
        let mut t = EntityLockTable::new();
        let _h1 = t
            .dispatch_lock_check(
                &meta(vec![molex::EntityKind::Protein], true),
                &ctx_focused(1),
                "StructureDesign",
                always_protein,
            )
            .expect("create-op succeeds");
        let h2 = t
            .dispatch_lock_check(
                &meta(vec![molex::EntityKind::Protein], false),
                &ctx_focused(2),
                "Wiggle",
                always_protein,
            )
            .expect("non-create op on different entity should succeed");
        assert_eq!(h2.entities, vec![molex::EntityId::from_raw(2)]);
        assert!(!h2.create_barrier_held);
    }

    #[test]
    fn selection_without_focus_locks_the_selected_set() {
        // No focus, residues selected on entities 1 and 2 (both protein),
        // protein-only op → the whole set {1, 2} is locked atomically.
        let mut t = EntityLockTable::new();
        let h = t
            .dispatch_lock_check(
                &meta(vec![molex::EntityKind::Protein], false),
                &ctx_selected(&[1, 2]),
                "Wiggle",
                always_protein,
            )
            .expect("should lock the selected set");
        assert!(!h.global_held);
        let mut locked = h.entities;
        locked.sort_unstable();
        assert_eq!(locked, vec![eid(1), eid(2)]);

        // A second op on a member (1) is refused while the set is held.
        let r2 = t.dispatch_lock_check(
            &meta(vec![molex::EntityKind::Protein], false),
            &ctx_focused(1),
            "Shake",
            always_protein,
        );
        assert!(matches!(r2, Err(DispatchError::EntityLocked { .. })));

        // An op on an unrelated entity (3) still succeeds.
        let h3 = t
            .dispatch_lock_check(
                &meta(vec![molex::EntityKind::Protein], false),
                &ctx_focused(3),
                "Shake",
                always_protein,
            )
            .expect("unrelated entity should lock");
        assert_eq!(h3.entities, vec![eid(3)]);
    }

    #[test]
    fn selection_type_filter_drops_incompatible_members() {
        // No focus; residues selected on protein 1 and ligand 2 under a
        // protein-only op → only {1} is locked (2 is filtered out).
        let mut t = EntityLockTable::new();
        let h = t
            .dispatch_lock_check(
                &meta(vec![molex::EntityKind::Protein], false),
                &ctx_selected(&[1, 2]),
                "Wiggle",
                protein_except_2,
            )
            .expect("should lock the protein member only");
        assert_eq!(h.entities, vec![eid(1)]);
        assert!(!t.is_locked(eid(2)));
    }

    #[test]
    fn try_lock_set_is_atomic_on_partial_conflict() {
        // B already locked; locking {A, B} must acquire nothing (A stays
        // free) and return None.
        let mut t = EntityLockTable::new();
        let _b = t.try_lock(eid(2), "Wiggle").expect("B locks");
        let r = t.try_lock_set(&[eid(1), eid(2)], "Shake");
        assert!(r.is_none());
        assert!(!t.is_locked(eid(1)), "A must remain unlocked (no partial)");
        assert!(t.is_locked(eid(2)));
    }

    #[test]
    fn set_members_share_the_handle_cancel_flag() {
        // After locking {1, 2}, a cancel requested on member 1 flips the
        // very flag the handle monitors (one shared Arc across the set).
        let mut t = EntityLockTable::new();
        let h = t
            .dispatch_lock_check(
                &meta(vec![molex::EntityKind::Protein], false),
                &ctx_selected(&[1, 2]),
                "Wiggle",
                always_protein,
            )
            .expect("set locks");
        assert!(!h.cancel_flag.load(std::sync::atomic::Ordering::SeqCst));
        assert!(t.request_cancel(eid(1)), "member 1 is locked");
        assert!(
            h.cancel_flag.load(std::sync::atomic::Ordering::SeqCst),
            "handle flag observes a cancel on any member"
        );
        // The other member shares the same flag too.
        assert!(t.request_cancel(eid(2)));
    }

    #[test]
    fn backend_lock_serializes_same_plugin() {
        let mut t = EntityLockTable::new();
        assert!(t.try_lock_backend("rosetta"));
        assert!(!t.try_lock_backend("rosetta"));
        assert!(t.try_lock_backend("ml"));
    }

    #[test]
    fn backend_lock_released_allows_reacquire() {
        let mut t = EntityLockTable::new();
        assert!(t.try_lock_backend("rosetta"));
        t.unlock_backend("rosetta");
        assert!(t.try_lock_backend("rosetta"));
    }

    #[test]
    fn release_dispatch_locks_frees_backend_lock() {
        let mut t = EntityLockTable::new();
        assert!(t.try_lock_backend("rosetta"));
        let handle = DispatchHandle {
            entities: vec![],
            global_held: false,
            create_barrier_held: false,
            cancel_flag: Arc::new(AtomicBool::new(false)),
            backend_lock: Some(String::from("rosetta")),
        };
        t.release_dispatch_locks(handle);
        assert!(t.try_lock_backend("rosetta"));
    }
}
