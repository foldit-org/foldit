//! Undo / redo history: per-entity timelines + a unified checkpoint graph.
//!
//! Two cooperating DAGs.
//!
//! 1. **Entity timelines** ("swim lanes") — each [`EntityId`] has its own
//!    [`EntityHistory`], a slotmap of [`EntitySnapshot`]s linked by
//!    parent/children. Snapshots own the entity payload (an
//!    `Arc<MoleculeEntity>`).
//!
//! 2. **Checkpoint graph** (the unified "river") — a slotmap of
//!    [`Checkpoint`]s linked by parent/children. Each checkpoint carries an
//!    `IndexMap<EntityId, EntitySnapshotId>` plus per-checkpoint state
//!    (score, filter status). It does **not** own snapshot payloads.
//!
//! **Cross-DAG invariant.** For every `e ∈ keys(checkpoint_head.entity_heads)`,
//! the lane head is either that committed snapshot, or a tentative
//! snapshot whose parent is it (an in-flight action's open tentative sits
//! one step past the committed head on its lane). Asserted at the tail of
//! every DAG-bearing event under `debug_assertions`.
//!
//! **Single record root.** Every checkpoint-bearing event funnels through
//! the private [`History::record`]; the public methods are thin shims
//! that build a [`HistoryEvent`] variant and delegate. Per-cycle byte
//! mutation (`action_update`) and curation (pin / unpin / exclude / budget)
//! do not change DAG topology and do not go through the root.
//!
//! **Lock layering.** `History` enforces only the structural *action
//! lock* — one open tentative per lane, and a frozen committed graph head
//! (no navigation / immediate-commit mutation) while any action is in
//! flight. Multi-client locking (the case where the runner is
//! server-side and clients are remote) is owned by the runner's
//! orchestrator (its `EntityLockTable`), not by foldit-core; do not push it
//! into this module.

use std::borrow::Cow;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use web_time::SystemTime;

use indexmap::IndexMap;
use molex::entity::molecule::id::EntityId;
use molex::MoleculeEntity;
use slotmap::SlotMap;
use smallvec::SmallVec;

// Slotmap key types and the wire-side `WireId<K>` newtype both live in
// `foldit_gui::wire` so the GUI crate can build wire payloads without
// inverting the dependency direction (foldit → foldit_gui, never
// the reverse). Re-exported through this module for ergonomic
// `foldit::history::CheckpointId` use sites.
pub use foldit_gui::wire::{CheckpointId, EntitySnapshotId, WireId};

mod types;
pub use types::{CheckpointKind, FilterStatus};

mod storage;
pub use storage::{Checkpoint, CheckpointGraph, EntityHistory, EntitySnapshot, HistoryBudget};

mod pending;
use pending::PendingEdit;

mod error;
pub use error::HistoryError;

// ── History (the public type) ──────────────────────────────────────────

/// Two-layer history: per-entity timelines plus a unified checkpoint
/// graph. Owns both DAGs; enforces the cross-DAG invariant; funnels every
/// checkpoint-bearing event through a single private root.
///
/// See module docs for the design contract; see `record` for the
/// single event funnel.
#[derive(Debug, Clone)]
pub struct History {
    /// Per-entity swim lanes. IndexMap so insertion order is the
    /// canonical entity order across the assembly.
    pub(super) lanes: IndexMap<EntityId, EntityHistory>,
    /// Unified checkpoint graph + cursors.
    pub(super) checkpoints: CheckpointGraph,
    /// In-flight actions keyed by request id. Empty in the resting
    /// state; one entry per open action (0 or 1 until concurrent
    /// dispatch lands, but the map and per-lane keying support fan-out).
    /// Replaces the old ambient single-action flag.
    pub(super) pending: IndexMap<u64, PendingEdit>,
    /// Bumped on push / move / evict — full reproject on the wire.
    pub(super) topology_version: u64,
    /// Bumped on tentative in-place byte mutation — small live-update
    /// payload on the wire.
    pub(super) live_version: u64,
}

/// One operation passed into the private [`History::record`] root.
///
/// Every checkpoint-bearing event is one of these variants. Public
/// methods are thin shims that build a variant and delegate. New
/// operations land here, not on a sibling root (G3).
#[derive(Debug, Clone)]
enum HistoryEvent {
    Begin {
        entities: SmallVec<[EntityId; 1]>,
        kind: CheckpointKind,
        label: Cow<'static, str>,
        request_id: u64,
    },
    Commit {
        request_id: u64,
    },
    Abort {
        request_id: u64,
    },
    RecordEntityUpdate {
        entity: EntityId,
        kind: CheckpointKind,
        payload: Arc<MoleculeEntity>,
        label: Cow<'static, str>,
        raw_score: Option<f64>,
        game_score: Option<f64>,
    },
    LaneUndo {
        entity: EntityId,
        target: EntitySnapshotId,
    },
    LaneRedo {
        entity: EntityId,
        branch: Option<EntitySnapshotId>,
    },
    Undo,
    Redo {
        branch: Option<CheckpointId>,
    },
    JumpCheckpoint {
        id: CheckpointId,
    },
    AddEntity {
        entity_id: EntityId,
        payload: Arc<MoleculeEntity>,
        kind: CheckpointKind,
        label: Cow<'static, str>,
    },
}

/// Outcome of [`History::record`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HistoryEventOutcome {
    Pushed(CheckpointId),
    HeadMoved {
        from: CheckpointId,
        to: CheckpointId,
    },
    Aborted,
    /// A `Begin` registered a new pending edit. Under the open-action
    /// model a begin mints no checkpoint.
    Began,
}

impl History {
    /// Build a new history seeded with one root checkpoint and one root
    /// snapshot per entity in `seed`. The root checkpoint's
    /// `entity_heads` indexes those root snapshots in the supplied
    /// order; head and root cursors point at the new root.
    #[must_use]
    pub fn new(
        seed: impl IntoIterator<Item = (EntityId, MoleculeEntity)>,
        source: PathBuf,
    ) -> Self {
        let now = SystemTime::now();
        let mut lanes: IndexMap<EntityId, EntityHistory> = IndexMap::new();
        let mut entity_heads: IndexMap<EntityId, EntitySnapshotId> = IndexMap::new();

        for (eid, entity) in seed {
            let mut snapshots: SlotMap<EntitySnapshotId, EntitySnapshot> = SlotMap::with_key();
            let snap_id = snapshots.insert(EntitySnapshot {
                parent: None,
                children: SmallVec::new(),
                payload: Arc::new(entity),
                label: Cow::Borrowed("Loaded"),
                timestamp: now,
                tentative: false,
                checkpoint_refs: 1,
            });
            lanes.insert(
                eid,
                EntityHistory {
                    snapshots,
                    head: snap_id,
                    root: snap_id,
                },
            );
            entity_heads.insert(eid, snap_id);
        }

        let mut checkpoints: SlotMap<CheckpointId, Checkpoint> = SlotMap::with_key();
        let root_ckpt = checkpoints.insert(Checkpoint {
            parent: None,
            children: SmallVec::new(),
            entity_heads,
            kind: CheckpointKind::Loaded { source },
            label: Cow::Borrowed("Initial state"),
            timestamp: now,
            raw_score: None,
            game_score: None,
            filter_status: FilterStatus::NotEvaluated,
            exclude_from_best: false,
        });

        let history = Self {
            lanes,
            checkpoints: CheckpointGraph {
                checkpoints,
                head: root_ckpt,
                root: root_ckpt,
                best: None,
                best_that_counts: None,
                pinned: HashSet::new(),
                budget: HistoryBudget::default(),
            },
            pending: IndexMap::new(),
            topology_version: 0,
            live_version: 0,
        };

        if cfg!(debug_assertions) {
            history.assert_invariant();
        }
        history
    }

    // ── Read accessors ─────────────────────────────────────────────────

    /// Read access to the checkpoint graph.
    #[must_use]
    pub fn checkpoints(&self) -> &CheckpointGraph {
        &self.checkpoints
    }

    /// Read access to one entity's lane.
    #[must_use]
    pub fn lane(&self, entity: EntityId) -> Option<&EntityHistory> {
        self.lanes.get(&entity)
    }

    /// Iterate (entity_id, lane) pairs in canonical order.
    pub fn lanes(&self) -> impl Iterator<Item = (EntityId, &EntityHistory)> {
        self.lanes.iter().map(|(eid, lane)| (*eid, lane))
    }

    /// Whether any action is in flight.
    #[must_use]
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Whether the action identified by `request_id` is in flight.
    #[must_use]
    pub fn is_pending(&self, request_id: u64) -> bool {
        self.pending.contains_key(&request_id)
    }

    /// Topology version — bumped on push / move / evict. Triggers full
    /// reproject on the wire.
    #[must_use]
    pub fn topology_version(&self) -> u64 {
        self.topology_version
    }

    /// Live version — bumped on per-cycle in-place mutation. Triggers
    /// the small live-update payload on the wire.
    #[must_use]
    pub fn live_version(&self) -> u64 {
        self.live_version
    }

    /// Lookup helper.
    #[must_use]
    pub fn checkpoint(&self, id: CheckpointId) -> Option<&Checkpoint> {
        self.checkpoints.checkpoint(id)
    }

    /// Lookup helper.
    #[must_use]
    pub fn snapshot(&self, entity: EntityId, id: EntitySnapshotId) -> Option<&EntitySnapshot> {
        self.lanes.get(&entity)?.snapshot(id)
    }

    // ── Public surface — thin shims over record ───────────────────────

    /// Begin a streaming action over `entities` under the caller-supplied
    /// `request_id` (allocated by the orchestrator, the single id
    /// authority). Opens one tentative lane per entity, each forked from
    /// its own committed lane head (parent = current lane head), and
    /// advances those lane heads, but mints no checkpoint and does not
    /// move the committed graph head: the checkpoint is composed and
    /// minted, already committed, at commit. Registers one multi-lane
    /// pending edit under `request_id`. Refused if any named lane already
    /// has an open tentative. A single-entity action passes a one-element
    /// set.
    pub fn begin_action(
        &mut self,
        entities: impl IntoIterator<Item = EntityId>,
        kind: CheckpointKind,
        label: Cow<'static, str>,
        request_id: u64,
    ) -> Result<(), HistoryError> {
        match self.record(HistoryEvent::Begin {
            entities: entities.into_iter().collect(),
            kind,
            label,
            request_id,
        })? {
            HistoryEventOutcome::Began => Ok(()),
            _ => unreachable!("Begin always returns Began"),
        }
    }

    /// Per-cycle update of the action identified by `request_id`. Mutates
    /// each held lane's tentative snapshot payload via `Arc::make_mut`
    /// (fanned across every lane in the edit), accumulates the live
    /// composition score on the pending edit (no tentative checkpoint
    /// exists to hold it), and bumps `live_version` only. Does NOT change
    /// DAG topology and is not routed through `record`.
    ///
    /// Returns `NoOngoingAction` if `request_id` names no in-flight action.
    pub fn action_update(
        &mut self,
        request_id: u64,
        raw_score: Option<f64>,
        game_score: Option<f64>,
        filter_status: Option<FilterStatus>,
        mut mutate: impl FnMut(&mut MoleculeEntity),
    ) -> Result<(), HistoryError> {
        // Snapshot the lane set up front so the per-lane mutation loop
        // doesn't hold a borrow of `self.pending` while it borrows
        // `self.lanes` mutably.
        let lanes: SmallVec<[(EntityId, EntitySnapshotId); 1]> = self
            .pending
            .get(&request_id)
            .ok_or(HistoryError::NoOngoingAction)?
            .lanes
            .clone();

        for (entity, snap_id) in &lanes {
            let lane = self
                .lanes
                .get_mut(entity)
                .expect("pending lane must exist (G8)");
            let snap = lane
                .snapshots
                .get_mut(*snap_id)
                .expect("tentative snapshot must exist (G8)");
            let payload = Arc::make_mut(&mut snap.payload);
            mutate(payload);
        }

        let edit = self
            .pending
            .get_mut(&request_id)
            .expect("checked above");
        if let Some(s) = raw_score {
            edit.raw_score = Some(s);
        }
        if let Some(s) = game_score {
            edit.game_score = Some(s);
        }
        if let Some(fs) = filter_status {
            edit.filter_status = fs;
        }

        self.live_version = self.live_version.saturating_add(1);

        if cfg!(debug_assertions) {
            self.assert_invariant();
        }
        Ok(())
    }

    /// Commit the action identified by `request_id`. Flips each held
    /// lane's tentative snapshot to committed, composes a new checkpoint
    /// from the current committed graph head plus the edit's lanes (so
    /// the committed node never references a peer's open tentative),
    /// stamps the edit's accumulated score onto it, advances the graph
    /// head, recomputes best cursors, and drops the pending edit.
    pub fn commit_action(&mut self, request_id: u64) -> Result<CheckpointId, HistoryError> {
        match self.record(HistoryEvent::Commit { request_id })? {
            HistoryEventOutcome::Pushed(id) => Ok(id),
            _ => unreachable!("Commit returns the now-real checkpoint"),
        }
    }

    /// Abort the action identified by `request_id`. Removes each held
    /// lane's tentative snapshot; lane heads fall back to their parents.
    /// No checkpoint is removed (a begin mints none) and the committed
    /// graph head does not move.
    pub fn abort_action(&mut self, request_id: u64) -> Result<(), HistoryError> {
        match self.record(HistoryEvent::Abort { request_id })? {
            HistoryEventOutcome::Aborted => Ok(()),
            _ => unreachable!("Abort returns Aborted"),
        }
    }

    /// Stamp scores on the open edit identified by `request_id`. The edit's
    /// accumulated score is what `do_commit` transfers onto the checkpoint
    /// it mints, so this is how a per-edit composition score reaches the
    /// committed node. Targeting the named edit (not "the first open one")
    /// keeps two concurrent edits' scores from colliding. Bumps
    /// `live_version` only; no DAG topology change. No-op when `request_id`
    /// names no open edit, or on `(None, None)`.
    pub fn set_edit_scores(
        &mut self,
        request_id: u64,
        raw_score: Option<f64>,
        game_score: Option<f64>,
    ) {
        if raw_score.is_none() && game_score.is_none() {
            return;
        }
        if let Some(edit) = self.pending.get_mut(&request_id) {
            if let Some(s) = raw_score {
                edit.raw_score = Some(s);
            }
            if let Some(s) = game_score {
                edit.game_score = Some(s);
            }
            self.live_version = self.live_version.saturating_add(1);
        }
    }

    /// Stamp scores on the committed checkpoint `id` in place. Used by the
    /// commit-time composition score: the checkpoint composes the committed
    /// union at commit, the score lands once the reply returns, and this
    /// stamps the now-immutable node it was scored for. Bumps `live_version`
    /// only. No-op on unknown `id` or `(None, None)`.
    pub fn set_checkpoint_scores(
        &mut self,
        id: CheckpointId,
        raw_score: Option<f64>,
        game_score: Option<f64>,
    ) {
        if raw_score.is_none() && game_score.is_none() {
            return;
        }
        if let Some(ckpt) = self.checkpoints.checkpoints.get_mut(id) {
            if let Some(s) = raw_score {
                ckpt.raw_score = Some(s);
            }
            if let Some(s) = game_score {
                ckpt.game_score = Some(s);
            }
            self.live_version = self.live_version.saturating_add(1);
        }
    }

    /// Read the `(raw, game)` score of the current composition node: the
    /// first open pending edit if one exists, else the committed head
    /// checkpoint. The live-score read surface for the score widget; with
    /// per-edit composition scores each open edit holds its own correctly
    /// attributed score, so the first one is a meaningful display value.
    #[must_use]
    pub fn current_composition_scores(&self) -> (Option<f64>, Option<f64>) {
        if let Some(edit) = self.pending.values().next() {
            (edit.raw_score, edit.game_score)
        } else {
            let head = &self.checkpoints.checkpoints[self.checkpoints.head];
            (head.raw_score, head.game_score)
        }
    }

    /// The request ids of every open edit, in insertion order. Used by the
    /// host to fire one composition score per open edit.
    pub fn pending_request_ids(&self) -> impl Iterator<Item = u64> + '_ {
        self.pending.keys().copied()
    }

    /// The lone open edit's request id, or `None` if zero or >1 edits are
    /// open. When exactly one edit is open the worker's live pose IS that
    /// edit's composition, so a whole-assembly score attributes to it.
    #[must_use]
    pub fn sole_pending_request_id(&self) -> Option<u64> {
        let mut it = self.pending_request_ids();
        let first = it.next()?;
        if it.next().is_some() {
            None
        } else {
            Some(first)
        }
    }

    /// The entities composing the open edit `request_id`: the committed
    /// graph head's membership, reading the edit's tentative lane for each
    /// entity the edit holds and the committed head snapshot for every peer.
    /// This is exactly the composition `do_commit` will mint, so scoring it
    /// attributes correctly even while peer edits are open. `None` when
    /// `request_id` names no open edit.
    #[must_use]
    pub fn edit_composition_entities(
        &self,
        request_id: u64,
    ) -> Option<Vec<Arc<MoleculeEntity>>> {
        let edit = self.pending.get(&request_id)?;
        let head = &self.checkpoints.checkpoints[self.checkpoints.head];
        let mut out = Vec::with_capacity(head.entity_heads.len());
        for (eid, committed_snap) in &head.entity_heads {
            // Overlay this edit's tentative for entities it holds; read the
            // committed head snapshot for peers (so a peer's open edit never
            // leaks into this composition).
            let snap_id = edit
                .lanes
                .iter()
                .find(|(e, _)| e == eid)
                .map_or(*committed_snap, |(_, s)| *s);
            if let Some(lane) = self.lanes.get(eid) {
                if let Some(snap) = lane.snapshot(snap_id) {
                    out.push(Arc::clone(&snap.payload));
                }
            }
        }
        Some(out)
    }

    /// The entities composing committed checkpoint `id` (its `entity_heads`
    /// snapshots), in canonical order. `None` when `id` is unknown.
    #[must_use]
    pub fn checkpoint_composition_entities(
        &self,
        id: CheckpointId,
    ) -> Option<Vec<Arc<MoleculeEntity>>> {
        let ckpt = self.checkpoints.checkpoint(id)?;
        let mut out = Vec::with_capacity(ckpt.entity_heads.len());
        for (eid, snap_id) in &ckpt.entity_heads {
            if let Some(lane) = self.lanes.get(eid) {
                if let Some(snap) = lane.snapshot(*snap_id) {
                    out.push(Arc::clone(&snap.payload));
                }
            }
        }
        Some(out)
    }

    /// Atomic non-streaming entity update — used by RFD3-final / MPNN
    /// results, manual moves, etc. Pushes one snapshot + one checkpoint
    /// with `tentative = false` immediately. Refused while `Active`.
    /// Optional `raw_score` / `game_score` are stamped on the new
    /// checkpoint (G7: caller carries both; projection picks at read).
    pub fn record_entity_update(
        &mut self,
        entity: EntityId,
        kind: CheckpointKind,
        payload: Arc<MoleculeEntity>,
        label: Cow<'static, str>,
        raw_score: Option<f64>,
        game_score: Option<f64>,
    ) -> Result<CheckpointId, HistoryError> {
        match self.record(HistoryEvent::RecordEntityUpdate {
            entity,
            kind,
            payload,
            label,
            raw_score,
            game_score,
        })? {
            HistoryEventOutcome::Pushed(id) => Ok(id),
            _ => unreachable!("RecordEntityUpdate always pushes"),
        }
    }

    /// Move `entity`'s lane head to `target`. Pushes a `LaneUndo`
    /// checkpoint with `entity_heads` mirroring the new lane head;
    /// no new snapshot.
    pub fn lane_undo(
        &mut self,
        entity: EntityId,
        target: EntitySnapshotId,
    ) -> Result<CheckpointId, HistoryError> {
        match self.record(HistoryEvent::LaneUndo { entity, target })? {
            HistoryEventOutcome::Pushed(id) => Ok(id),
            _ => unreachable!("LaneUndo always pushes a LaneUndo checkpoint"),
        }
    }

    /// Move `entity`'s lane head to a child of the current lane head;
    /// `branch` picks among multiple children. Pushes a `LaneUndo`
    /// checkpoint (the kind covers redo too — both directions move the
    /// lane head along the lane DAG).
    pub fn lane_redo(
        &mut self,
        entity: EntityId,
        branch: Option<EntitySnapshotId>,
    ) -> Result<CheckpointId, HistoryError> {
        match self.record(HistoryEvent::LaneRedo { entity, branch })? {
            HistoryEventOutcome::Pushed(id) => Ok(id),
            _ => unreachable!("LaneRedo always pushes a LaneUndo checkpoint"),
        }
    }

    /// Move checkpoint head to its parent. Mirror lane heads to match
    /// the new head's `entity_heads`. Returns `None` at the root.
    pub fn undo(&mut self) -> Result<Option<CheckpointId>, HistoryError> {
        match self.record(HistoryEvent::Undo) {
            Ok(HistoryEventOutcome::HeadMoved { to, .. }) => Ok(Some(to)),
            Err(HistoryError::AlreadyAtRoot) => Ok(None),
            Err(e) => Err(e),
            Ok(_) => unreachable!("Undo returns HeadMoved or AlreadyAtRoot"),
        }
    }

    /// Move checkpoint head to a child. `branch` picks among multiple
    /// children. Returns `None` at a leaf.
    pub fn redo(
        &mut self,
        branch: Option<CheckpointId>,
    ) -> Result<Option<CheckpointId>, HistoryError> {
        match self.record(HistoryEvent::Redo { branch }) {
            Ok(HistoryEventOutcome::HeadMoved { to, .. }) => Ok(Some(to)),
            Err(HistoryError::NoChildren) => Ok(None),
            Err(e) => Err(e),
            Ok(_) => unreachable!("Redo returns HeadMoved or NoChildren"),
        }
    }

    /// Introduce a new entity (and its lane) into history. Pushes a
    /// fresh checkpoint whose `entity_heads` is the parent's plus
    /// `entity_id → root_snapshot`. Refused while `Active` (the running
    /// action freezes the assembly per § Lock semantics).
    ///
    /// Used by [`crate::session::Session::promote_preview`] to
    /// move a previously-transient entity into history.
    pub fn add_entity(
        &mut self,
        entity_id: EntityId,
        payload: Arc<MoleculeEntity>,
        kind: CheckpointKind,
        label: Cow<'static, str>,
    ) -> Result<CheckpointId, HistoryError> {
        match self.record(HistoryEvent::AddEntity {
            entity_id,
            payload,
            kind,
            label,
        })? {
            HistoryEventOutcome::Pushed(id) => Ok(id),
            _ => unreachable!("AddEntity always pushes"),
        }
    }

    /// Move checkpoint head to `id`. Mirror lane heads to match.
    pub fn jump_checkpoint(&mut self, id: CheckpointId) -> Result<CheckpointId, HistoryError> {
        match self.record(HistoryEvent::JumpCheckpoint { id })? {
            HistoryEventOutcome::HeadMoved { to, .. } => Ok(to),
            _ => unreachable!("JumpCheckpoint returns HeadMoved"),
        }
    }

    // ── Curation (no DAG topology change — direct field writes) ───────

    /// Pin a checkpoint as user-marked best.
    pub fn pin_checkpoint(&mut self, id: CheckpointId) -> Result<(), HistoryError> {
        if !self.checkpoints.checkpoints.contains_key(id) {
            return Err(HistoryError::UnknownCheckpoint { id });
        }
        let _ = self.checkpoints.pinned.insert(id);
        Ok(())
    }

    /// Unpin a checkpoint.
    pub fn unpin_checkpoint(&mut self, id: CheckpointId) -> Result<(), HistoryError> {
        if !self.checkpoints.checkpoints.contains_key(id) {
            return Err(HistoryError::UnknownCheckpoint { id });
        }
        let _ = self.checkpoints.pinned.remove(&id);
        Ok(())
    }

    /// Set the "exclude from best" flag.
    pub fn set_exclude_from_best(
        &mut self,
        id: CheckpointId,
        exclude: bool,
    ) -> Result<(), HistoryError> {
        let ckpt = self
            .checkpoints
            .checkpoints
            .get_mut(id)
            .ok_or(HistoryError::UnknownCheckpoint { id })?;
        ckpt.exclude_from_best = exclude;
        Ok(())
    }

    /// Replace the eviction budget.
    pub fn set_budget(&mut self, budget: HistoryBudget) {
        self.checkpoints.budget = budget;
    }

    /// Stamp `raw_score` / `game_score` on the current head checkpoint
    /// in place. Bumps `live_version` only — DAG topology unchanged, no
    /// new checkpoint, no new snapshot. Idempotent on `(None, None)`.
    ///
    /// This is the right call for cycle-zero scoring during session init
    /// (Rosetta streams a score before the user takes any action). It
    /// avoids the pre-fix behavior where every init cycle pushed a fresh
    /// checkpoint on top of root + AddEntity.
    pub fn set_head_scores(&mut self, raw_score: Option<f64>, game_score: Option<f64>) {
        if raw_score.is_none() && game_score.is_none() {
            return;
        }
        let head_id = self.checkpoints.head;
        let ckpt = self
            .checkpoints
            .checkpoints
            .get_mut(head_id)
            .expect("head checkpoint must exist (G8)");
        if let Some(s) = raw_score {
            ckpt.raw_score = Some(s);
        }
        if let Some(s) = game_score {
            ckpt.game_score = Some(s);
        }
        self.live_version = self.live_version.saturating_add(1);

        if cfg!(debug_assertions) {
            self.assert_invariant();
        }
    }

    // ── Action-lock helpers ───────────────────────────────────────────

    /// Whether `entity`'s lane head snapshot is an open tentative (i.e.
    /// the lane already belongs to an in-flight action). `false` for an
    /// unknown entity (`do_begin` reports `UnknownEntity` for that).
    fn lane_head_is_tentative(&self, entity: EntityId) -> bool {
        self.lanes
            .get(&entity)
            .and_then(|l| l.snapshot(l.head()))
            .is_some_and(|s| s.tentative)
    }

    /// A representative locked entity to name in an `EntityLocked`
    /// refusal: the first lane of the first pending edit. Callers gate on
    /// a non-empty pending map.
    fn first_pending_entity(&self) -> EntityId {
        self.pending
            .values()
            .next()
            .and_then(|e| e.lanes.first())
            .map(|(eid, _)| *eid)
            .expect("first_pending_entity called with empty pending map")
    }

    // ── Private root: every DAG-bearing event funnels here (G3) ──────

    /// The single root through which every checkpoint- or lane-DAG-
    /// bearing event passes. Validates the action-lock
    /// preconditions, performs the mutation, updates `checkpoint_refs`,
    /// runs eviction, bumps `topology_version`, and asserts the cross-
    /// DAG invariant (G8).
    ///
    /// New events land here as a new [`HistoryEvent`] variant. A
    /// sibling root would carry state this function doesn't know about
    /// and is therefore illegal (G3).
    fn record(&mut self, event: HistoryEvent) -> Result<HistoryEventOutcome, HistoryError> {
        // ── Action-lock pre-check ─────────────────────────────────────
        // Reframed off the pending-edit map. While any action is open
        // the committed graph head is frozen (each commit composes from
        // it), so navigation and immediate-commit mutations are refused.
        // A new action may still begin on a *free* lane (multi-lane
        // fan-out); only a lane that already has an open tentative
        // refuses begin.
        if !self.pending.is_empty() {
            match &event {
                // Commit / Abort resolve their own request_id in the arm.
                HistoryEvent::Commit { .. } | HistoryEvent::Abort { .. } => {}
                HistoryEvent::Begin { entities, .. } => {
                    if entities
                        .iter()
                        .any(|e| self.lane_head_is_tentative(*e))
                    {
                        return Err(HistoryError::ActiveActionInProgress);
                    }
                }
                // Both move the committed head, which would strand an
                // open edit's commit composition.
                HistoryEvent::RecordEntityUpdate { .. } | HistoryEvent::AddEntity { .. } => {
                    return Err(HistoryError::ActiveActionInProgress)
                }
                HistoryEvent::LaneUndo { .. }
                | HistoryEvent::LaneRedo { .. }
                | HistoryEvent::Undo
                | HistoryEvent::Redo { .. }
                | HistoryEvent::JumpCheckpoint { .. } => {
                    return Err(HistoryError::EntityLocked {
                        entity: self.first_pending_entity(),
                    })
                }
            }
        }

        // Linear-undo invariant: after a push, every checkpoint must
        // lie on the root → head path, and every snapshot must lie on
        // its lane's root → head path. Navigation-only events
        // (Undo / Redo / JumpCheckpoint) skip the prune so the user
        // can move the cursor without losing the redo path; the next
        // *push* drops it (classic editor undo).
        let is_push = matches!(
            event,
            HistoryEvent::Begin { .. }
                | HistoryEvent::RecordEntityUpdate { .. }
                | HistoryEvent::LaneUndo { .. }
                | HistoryEvent::LaneRedo { .. }
                | HistoryEvent::AddEntity { .. }
        );

        let result = match event {
            HistoryEvent::Begin {
                entities,
                kind,
                label,
                request_id,
            } => self.do_begin(entities, kind, label, request_id)?,
            HistoryEvent::Commit { request_id } => self.do_commit(request_id)?,
            HistoryEvent::Abort { request_id } => self.do_abort(request_id)?,
            HistoryEvent::RecordEntityUpdate {
                entity,
                kind,
                payload,
                label,
                raw_score,
                game_score,
            } => {
                self.do_record_entity_update(entity, kind, payload, label, raw_score, game_score)?
            }
            HistoryEvent::LaneUndo { entity, target } => self.do_lane_undo(entity, target)?,
            HistoryEvent::LaneRedo { entity, branch } => self.do_lane_redo(entity, branch)?,
            HistoryEvent::Undo => self.do_undo()?,
            HistoryEvent::Redo { branch } => self.do_redo(branch)?,
            HistoryEvent::JumpCheckpoint { id } => self.do_jump(id)?,
            HistoryEvent::AddEntity {
                entity_id,
                payload,
                kind,
                label,
            } => self.do_add_entity(entity_id, payload, kind, label)?,
        };

        if is_push {
            self.prune_to_head_path();
        }

        self.evict_to_budget();
        self.topology_version = self.topology_version.saturating_add(1);

        if cfg!(debug_assertions) {
            self.assert_invariant();
        }

        Ok(result)
    }
}

mod eviction;
mod invariant;
mod record;

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests;
