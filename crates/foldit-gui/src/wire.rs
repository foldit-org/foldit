//! Wire payloads exchanged between the foldit backend and the
//! webview frontend.
//!
//! Two read-direction shapes (`HistorySection` for full reproject,
//! `HistoryLiveUpdate` for in-flight tentative score patches) and one
//! write-direction shape (`HistoryCommand`, the navigation surface that
//! crosses the IPC envelope as `AppCommand::History(cmd)`).
//!
//! All identifiers carry the `WireId<K>` newtype around their slotmap
//! keys. `WireId` serializes via `Display` / `FromStr` so JS holds them
//! as opaque strings; the alternative — a numeric `u64` — silently
//! truncates the slotmap version (upper 32 bits) past JS's 53-bit
//! safe-integer range.
//!
//! The slotmap key types themselves (`EntitySnapshotId`,
//! `CheckpointId`) live here rather than in `foldit::history` so the
//! GUI crate can build wire payloads without depending on the foldit
//! lib (the dependency direction is foldit → `foldit_gui`, never the
//! reverse). The history module re-imports the key types from here.

use std::str::FromStr;

use indexmap::IndexMap;
use molex::entity::molecule::id::EntityId;
use serde::{Deserialize, Serialize};
use slotmap::{new_key_type, Key, KeyData};

new_key_type! {
    /// Stable handle into an `EntityHistory`'s snapshot arena.
    /// Defined in `foldit-gui` so wire payloads can name the type
    /// without inverting the dependency direction.
    pub struct EntitySnapshotId;
    /// Stable handle into the `CheckpointGraph`'s checkpoint arena.
    /// See [`EntitySnapshotId`] for the rationale on the home crate.
    pub struct CheckpointId;
}

// WireId<K>

/// Wire-friendly newtype around a slotmap key. Encodes as a decimal
/// string of `KeyData::as_ffi()`. JS holds these as opaque strings;
/// round-trip preserves both index and version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WireId<K: Key>(K);

impl<K: Key> WireId<K> {
    /// Wrap a key for wire serialization.
    #[must_use]
    pub const fn new(key: K) -> Self {
        Self(key)
    }

    /// Unwrap into the underlying slotmap key.
    #[must_use]
    pub const fn into_inner(self) -> K {
        self.0
    }

    /// Borrow the underlying slotmap key.
    #[must_use]
    pub const fn as_inner(&self) -> &K {
        &self.0
    }
}

impl<K: Key> std::fmt::Display for WireId<K> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0.data().as_ffi())
    }
}

impl<K: Key> FromStr for WireId<K> {
    type Err = std::num::ParseIntError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let raw: u64 = s.parse()?;
        Ok(Self(KeyData::from_ffi(raw).into()))
    }
}

impl<K: Key> serde::Serialize for WireId<K> {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.collect_str(self)
    }
}

impl<'de, K: Key> serde::Deserialize<'de> for WireId<K> {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        struct V<K: Key>(std::marker::PhantomData<K>);
        impl<K: Key> serde::de::Visitor<'_> for V<K> {
            type Value = WireId<K>;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a decimal-encoded slotmap key")
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                v.parse().map_err(serde::de::Error::custom)
            }
        }
        de.deserialize_str(V::<K>(std::marker::PhantomData))
    }
}

/// Specta `Type` impl: `WireId<K>` is opaque to the wire as `string`.
/// The `K` parameter is a Rust-side brand only; every `WireId<K>` is
/// the same `string` shape in TypeScript.
impl<K: Key> specta::Type for WireId<K> {
    fn definition(_: &mut specta::Types) -> specta::datatype::DataType {
        specta::datatype::DataType::Primitive(specta::datatype::Primitive::str)
    }
}

// Read direction: HistorySection (full reproject)

/// Filter evaluation status as seen on the wire. Strict subset of the
/// backend [`foldit::history::FilterStatus`] — the wire side only needs
/// the discriminant for rendering. Failure reasons can join later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum FilterStatus {
    /// Every filter passed.
    Pass,
    /// One or more filters failed.
    Fail,
    /// Not yet evaluated.
    #[default]
    NotEvaluated,
}

/// Compact display string for a [`CheckpointKind`] discriminant. Lives
/// on the wire as a fixed enum tag string (`"wiggle"`, `"shake"`, ...);
/// the panel reads it as a typed kind for icons / tooltips.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointKindTag {
    /// Initial puzzle / file load.
    Load,
    /// Promoted from a transient preview.
    PromotedPreview,
    /// New entity introduced.
    AddEntity,
    /// Entity removed.
    RemoveEntity,
    /// Per-entity revert (lane head moved).
    LaneUndo,
    /// Generic plugin-dispatched op. Display label rides on
    /// `CheckpointInfo::label`; icon lookup is up to the frontend
    /// (catalog join keyed by plugin id + op id, falling back to a
    /// generic icon).
    PluginOp,
}

/// One node in the unified checkpoint graph.
#[derive(Debug, Clone, Serialize, Deserialize, specta::Type)]
pub struct CheckpointInfo {
    /// Wire-encoded checkpoint id.
    pub id: WireId<CheckpointId>,
    /// Parent in the DAG. `None` only for the root.
    pub parent: Option<WireId<CheckpointId>>,
    /// Direct children for branch indicators.
    pub children: Vec<WireId<CheckpointId>>,
    /// Tuple of entity snapshot ids that this checkpoint pins. Insertion
    /// order is the canonical entity order.
    pub entity_heads: IndexMap<EntityId, WireId<EntitySnapshotId>>,
    /// Entity this checkpoint primarily targets, if any. `None` for
    /// non-entity-targeted kinds (`Loaded`). Drives the `HistoryPanel`'s
    /// focus filter — when a user focuses entity X, the panel keeps
    /// checkpoints with `entity == Some(X)` plus those with
    /// `entity == None` (root) for context.
    pub entity: Option<EntityId>,
    /// Action discriminant (e.g., `wiggle`, `lane_undo`).
    pub kind: CheckpointKindTag,
    /// Display label for tooltip / context menu.
    pub label: String,
    /// Milliseconds since UNIX epoch. Encoded as `f64` so JS reads it
    /// as a normal `number` — well within safe-integer range for any
    /// practical clock (year ~285,000 AD before f64 mantissa breaks).
    pub timestamp_ms: f64,
    /// Rosetta REU. Mode-independent; the GUI picks raw vs. game.
    pub raw_score: Option<f64>,
    /// Game score. Mode-independent.
    pub game_score: Option<f64>,
    /// Filter evaluation summary.
    pub filter_status: FilterStatus,
    /// True iff this checkpoint is the running tentative.
    pub tentative: bool,
    /// User-pinned as best.
    pub pinned: bool,
    /// User-flagged "exclude from best".
    pub exclude_from_best: bool,
}

/// Full history payload pushed when topology changes (push / move /
/// evict). Compare with [`HistoryLiveUpdate`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, specta::Type)]
pub struct HistorySection {
    /// All live checkpoints.
    pub checkpoints: Vec<CheckpointInfo>,
    /// Current checkpoint head. Wire-default is empty (cannot fail in
    /// real payloads — the backend always seeds at least one root).
    pub checkpoint_head: Option<WireId<CheckpointId>>,
    /// Root checkpoint.
    pub checkpoint_root: Option<WireId<CheckpointId>>,
    /// Highest raw-score live checkpoint.
    pub best: Option<WireId<CheckpointId>>,
    /// Highest filter-passing live checkpoint.
    pub best_that_counts: Option<WireId<CheckpointId>>,
    /// Bumped on every topology mutation. JS uses this to reconcile.
    /// `f64` for the same reason as `timestamp_ms` — JS reads it as a
    /// plain `number`.
    pub topology_version: f64,
}

/// Small payload pushed during in-flight actions: the tentative
/// checkpoint's score moves between cycles. JS patches just that
/// checkpoint's fields rather than re-rendering the whole history.
#[derive(Debug, Clone, Serialize, Deserialize, specta::Type)]
pub struct HistoryLiveUpdate {
    /// Which checkpoint to patch. Almost always the current head /
    /// tentative.
    pub checkpoint_id: WireId<CheckpointId>,
    /// Updated raw score.
    pub raw_score: Option<f64>,
    /// Updated game score.
    pub game_score: Option<f64>,
    /// Updated label (mid-action labels can change as cycles tick).
    pub label: String,
    /// Updated filter status.
    pub filter_status: FilterStatus,
}

// Write direction: HistoryCommand

/// Navigation commands sent from the frontend. Routed through
/// `AppCommand::History(cmd)` and dispatched to the
/// `EntityStore` methods.
///
/// New variants are handled in `App::run_history_command`.
#[derive(Debug, Clone, Serialize, Deserialize, specta::Type)]
#[serde(tag = "kind")]
pub enum HistoryCommand {
    /// Move checkpoint head to the given checkpoint.
    JumpCheckpoint {
        /// Target checkpoint id.
        id: WireId<CheckpointId>,
    },
    /// Move checkpoint head to its parent.
    Undo,
    /// Move checkpoint head to a child. Branch optional.
    Redo {
        /// Optional child to disambiguate when multiple children exist.
        branch: Option<WireId<CheckpointId>>,
    },
    /// Move an entity's lane head to a specific snapshot. Pushes a
    /// `LaneUndo` checkpoint at the new head.
    LaneUndo {
        /// Entity whose lane head to move.
        entity: EntityId,
        /// Target snapshot id.
        target: WireId<EntitySnapshotId>,
    },
    /// Move an entity's lane head to a child of its current lane head.
    LaneRedo {
        /// Entity whose lane head to move.
        entity: EntityId,
        /// Optional child to disambiguate when multiple children exist.
        branch: Option<WireId<EntitySnapshotId>>,
    },
    /// Pin the given checkpoint (user marks it as best).
    PinCheckpoint {
        /// Target checkpoint id.
        id: WireId<CheckpointId>,
    },
    /// Unpin the given checkpoint.
    UnpinCheckpoint {
        /// Target checkpoint id.
        id: WireId<CheckpointId>,
    },
    /// Toggle the `exclude_from_best` flag on a checkpoint.
    SetExcludeFromBest {
        /// Target checkpoint id.
        id: WireId<CheckpointId>,
        /// New flag value.
        exclude: bool,
    },
    /// Discard the running tentative action (Shift+Esc / "Discard"
    /// button).
    AbortAction,
}

