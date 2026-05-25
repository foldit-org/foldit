//! Action Router: translates user input into orchestrator/backend commands.
//!
//! Holds the per-input routing state (dirty flags, last cursor pos, the
//! pending ML-op bookkeeping) and dispatches user actions to the
//! appropriate backend operations. The orchestrator handle itself now
//! lives on `PluginDriver`; routing methods that need it take it as a
//! parameter. Does NOT handle backend output processing, rendering, or
//! frontend state sync.

use foldit_gui::DirtyFlags;
use foldit_gui::state::{
    ParamConstraint as WireParamConstraint, ParamType as WireParamType,
    ParamValue as WireParamValue,
};
use crate::document::Document;
use viso::{InputEvent, InputProcessor, MouseButton, VisoCommand, VisoEngine};
use foldit_runner::orchestrator::{
    EntityId as RunnerEntityId, OpType, ParamConstraint as RunnerParamConstraint,
    ParamSpec as RunnerParamSpec, ParamType as RunnerParamType, ParamValue,
    SessionContext,
};
use foldit_runner::Orchestrator;
use glam::Vec3;

/// Request to start a backend ML operation.
///
/// Centralizes the protocol shared by predict / sequence-design /
/// structure-design: lock check → optional rosetta-stop → try_lock →
/// op-specific kickoff → optional preview-mirror → ui_dirty.
pub struct BackendOpRequest {
    /// Entity to lock.
    pub target: RunnerEntityId,
    /// Op type for the lock.
    pub op_type: OpType,
    /// Session context, consumed by the kickoff closure.
    pub entity_context: SessionContext,
    /// True for Predict: mirror the loaded entity into a preview
    /// (`Predicting...`), hide the loaded entity, and store the
    /// preview id on the router so the streaming/result paths know
    /// where to write.
    pub create_preview_mirror: bool,
    /// CA positions to remember for result alignment (Predict only).
    pub pending_reference_ca: Option<Vec<Vec3>>,
    /// Op-specific submission. Receives the orchestrator + the
    /// session context. The closure builds its own typed params map and
    /// dispatches via `Orchestrator::dispatch_invoke` /
    /// `dispatch_start_stream` / `dispatch_query`.
    pub kickoff: Box<
        dyn FnOnce(&mut Orchestrator, SessionContext) -> Result<(), String>,
    >,
}

/// Central mediator for action dispatch. Holds the per-input routing
/// state (dirty flags, cursor pos, pending ML-op bookkeeping); the
/// orchestrator handle lives on `PluginDriver`.
///
/// Rosetta-specific UI scaffolding (pull-drag, band-drag, active_bands
/// mirror) was gutted alongside the in-process executor — it'll come
/// back wired through the unified plugin protocol once bridge/ implements
/// the pull / band_add handlers (items 54–61).
pub struct ActionRouter {
    pub ui_dirty: DirtyFlags,
    pub last_mouse_pos: (f32, f32),
    /// CA positions of the protein chains submitted to the most recent
    /// RF3 prediction, in submission (chain) order. Used by
    /// `handle_ml_predict_result` / `handle_ml_intermediate` as the
    /// alignment reference so the predicted structure stays anchored to
    /// the entities the user actually sent — works for multi-entity
    /// predictions where the loaded entity's stored `reference_ca` no
    /// longer matches. Cleared once the final result has been applied.
    pub pending_prediction_reference: Option<Vec<Vec3>>,
    /// ID of the mirror preview entity backing the active ML op, if any.
    ///
    /// RF3: created at op kickoff (clone of the loaded entity, loaded
    /// hidden); streaming and final paths mutate the preview instead of
    /// the loaded entity, so undo never sees intermediate frames.
    /// RFD3: created lazily on the first streaming frame.
    /// Cleared on final result / cancel.
    pub pending_preview_id: Option<molex::entity::molecule::id::EntityId>,
}

impl ActionRouter {
    pub fn new() -> Self {
        Self {
            ui_dirty: DirtyFlags::empty(),
            last_mouse_pos: (0.0, 0.0),
            pending_prediction_reference: None,
            pending_preview_id: None,
        }
    }

    // ── Helpers ──

    pub fn take_ui_dirty(&mut self) -> DirtyFlags {
        let flags = self.ui_dirty;
        self.ui_dirty = DirtyFlags::empty();
        flags
    }

    // ── Action dispatch ──

    /// Reserved for non-plugin built-ins routed through ActionId.
    /// Currently a no-op -- Undo/Redo are intercepted at the App level
    /// because they need direct access to `store` + `engine`. Plugin
    /// ops dispatch through `App::handle_dispatch_op` (op-id keyed),
    /// not this path.
    pub fn handle_trigger_action(
        &mut self,
        _engine: &mut VisoEngine,
        _store: &mut Document,
        action: foldit_gui::ActionId,
    ) -> Option<foldit_gui::ParameterizedAction> {
        log::error!(
            "handle_trigger_action({:?}) reached router; built-ins are \
             intercepted at App level",
            action
        );
        None
    }

    pub fn cancel_operations(&mut self, engine: &mut VisoEngine, store: &mut Document) {
        log::info!("Cancelling current operation");
        engine.execute(VisoCommand::ClearSelection);
        // Stream lock release + commit live in apply_backend_updates'
        // terminal arms; doing them here races a follow-up dispatch
        // that's quick enough to slip in before the terminal drains.
        let preview_ids: Vec<molex::entity::molecule::id::EntityId> =
            store.preview_ids().collect();
        if !preview_ids.is_empty() {
            for id in &preview_ids {
                store.remove_preview(*id);
            }
            store.publish_to(engine);
            log::info!("Removed {} in-progress preview entities", preview_ids.len());
        }
        self.ui_dirty |= DirtyFlags::ACTIONS | DirtyFlags::SELECTION | DirtyFlags::LOADING;
    }

    // ── ML operations ──

    /// Run the shared kickoff protocol for an ML backend op.
    ///
    /// Order: lock check → (optional) stop rosetta + clear session →
    /// try_lock → kickoff → (optional) preview mirror → ui_dirty.
    // reserved for plugin migration (REMAINING_WORK T8: SessionContext)
    #[allow(dead_code)]
    pub fn start_op(
        &mut self,
        orch: &mut Orchestrator,
        request: BackendOpRequest,
        engine: &mut VisoEngine,
        store: &mut Document,
    ) {
        if orch.is_locked(request.target) {
            let op = orch.get_op_type(request.target);
            log::warn!(
                "Structure is locked by {:?}, cannot start {:?}",
                op, request.op_type,
            );
            return;
        }

        if orch.try_lock(request.target, request.op_type).is_none() {
            log::warn!("Failed to acquire lock for {:?}", request.op_type);
            return;
        }

        if let Some(ca) = request.pending_reference_ca {
            self.pending_prediction_reference = Some(ca);
        }

        if let Err(e) = (request.kickoff)(orch, request.entity_context) {
            log::error!("Failed to submit {:?}: {}", request.op_type, e);
            orch.unlock(request.target);
            return;
        }

        if request.create_preview_mirror {
            if let Some(loaded_id) = store.loaded_entity() {
                let mirror = store.entity(loaded_id).cloned().zip(
                    store.metadata(loaded_id).map(|m| m.origin.clone()),
                );
                if let Some((mirror, origin)) = mirror {
                    let preview_id = store.insert_preview(
                        mirror,
                        "Predicting...".to_string(),
                        origin,
                    );
                    engine.set_entity_visible(loaded_id.raw(), false);
                    self.pending_preview_id = Some(preview_id);
                    store.publish_to(engine);
                    log::info!(
                        "Created RF3 preview {} mirroring loaded entity {}",
                        preview_id.raw(), loaded_id.raw(),
                    );
                }
            }
        }

        self.ui_dirty |= DirtyFlags::ACTIONS | DirtyFlags::LOADING;
    }

    // ── Mouse / input handlers ──

    pub fn handle_native_mouse_input(
        &mut self,
        engine: &mut VisoEngine,
        input: &mut InputProcessor,
        _store: &mut Document,
        button: MouseButton,
        pressed: bool,
    ) {
        let hovered = engine.hovered_target();
        if let Some(cmd) =
            input.handle_event(InputEvent::MouseButton { button, pressed }, hovered)
        {
            engine.execute(cmd);
        }
    }

    pub fn handle_native_cursor_moved(
        &mut self,
        engine: &mut VisoEngine,
        _input: &InputProcessor,
        _store: &mut Document,
        x: f32,
        y: f32,
    ) {
        engine.set_cursor_pos(x, y);
        self.last_mouse_pos = (x, y);
    }
}

// ---------------------------------------------------------------------------
// Free functions (used by App)
// ---------------------------------------------------------------------------

/// Build the GUI's actions list by joining each plugin's manifest
/// `[[buttons]]` array with its bridge-side op registration. The
/// orchestrator's `ops_catalog()` already does the join; this wrapper
/// just maps each `CatalogEntry` to the GUI's `ActionInfo` shape and
/// gates `enabled` on the current orchestrator lock state.
///
/// `enabled` policy: any active lock disables every catalog entry --
/// the wave-1 round-trip only runs one op at a time. Per-op
/// compatibility (wiggle while predict is running, etc.) lights up
/// when `LockTargets`-style focus reasoning lands per-button, on top
/// of the catalog.
///
/// `active` policy: not yet wired (the bridge doesn't expose
/// op-instance metadata back through the orchestrator). Always false
/// for now; the GUI renders no "currently running" state.
pub fn build_actions_list(
    orchestrator: &Option<Orchestrator>,
) -> Vec<foldit_gui::state::ActionInfo> {
    use foldit_gui::state::ActionInfo;

    let Some(orch) = orchestrator else {
        return vec![];
    };

    let any_lock_held = !orch.locked_entities().is_empty();

    orch.ops_catalog()
        .into_iter()
        .map(|entry| ActionInfo {
            op_id: entry.op_id,
            plugin_id: entry.plugin_id,
            display: entry.display,
            icon_path: entry.icon_path.to_string_lossy().into_owned(),
            enabled: !any_lock_held,
            active: false,
            hotkey: entry.hotkey,
            tooltip: entry.tooltip,
            params: entry
                .params
                .into_iter()
                .map(param_spec_to_wire)
                .collect(),
        })
        .collect()
}

/// Generate a structural conversion `fn` between two enums that mirror
/// each other variant-for-variant. The orchestrator-native `Param*`
/// types (`foldit_runner`) and the gui-wire ones (`foldit_gui`) are kept
/// as separate definitions because `foldit-gui` deliberately does not
/// depend on `foldit-runner`, and the orphan rule forbids a `From` impl
/// here (both types are foreign to this crate) — so the conversions must
/// be free functions.
///
/// Each variant is named once with its field shape; the same `(bindings)`
/// / `{ bindings }` token tree both destructures the source and
/// reconstructs the destination. Adding a `Param*` variant becomes a
/// one-line edit per direction instead of an easy-to-desync match arm.
/// Mirrors the spirit of viso's `shader_registry!`.
macro_rules! mirror_enum {
    // Entry: function signature + braced variant list. Hands off to the
    // `@build` muncher with an empty match-arm accumulator.
    (
        $(#[$meta:meta])*
        $vis:vis fn $name:ident($arg:ident: $From:ident) -> $To:ident
        { $($variants:tt)+ }
    ) => {
        $(#[$meta])*
        $vis fn $name($arg: $From) -> $To {
            mirror_enum!(@build $From, $To, $arg, { } $($variants)+)
        }
    };

    // No variants left: emit the accumulated match. (A bare `:tt`-optional
    // shape would be ambiguous with the `,` separator, so we munch instead,
    // dispatching on the delimiter that follows each variant ident.)
    (@build $From:ident, $To:ident, $arg:ident, { $($arms:tt)* }) => {
        match $arg { $($arms)* }
    };
    // `Variant { fields.. }` — struct-shaped.
    (@build $From:ident, $To:ident, $arg:ident, { $($arms:tt)* }
        $variant:ident { $($field:ident),+ $(,)? } $(, $($rest:tt)+)?) => {
        mirror_enum!(@build $From, $To, $arg,
            { $($arms)* $From::$variant { $($field),+ } => $To::$variant { $($field),+ }, }
            $($($rest)+)?)
    };
    // `Variant(bindings..)` — tuple-shaped.
    (@build $From:ident, $To:ident, $arg:ident, { $($arms:tt)* }
        $variant:ident ( $($bind:ident),+ $(,)? ) $(, $($rest:tt)+)?) => {
        mirror_enum!(@build $From, $To, $arg,
            { $($arms)* $From::$variant ( $($bind),+ ) => $To::$variant ( $($bind),+ ), }
            $($($rest)+)?)
    };
    // `Variant` — unit.
    (@build $From:ident, $To:ident, $arg:ident, { $($arms:tt)* }
        $variant:ident $(, $($rest:tt)+)?) => {
        mirror_enum!(@build $From, $To, $arg,
            { $($arms)* $From::$variant => $To::$variant, }
            $($($rest)+)?)
    };
}

fn param_spec_to_wire(
    spec: RunnerParamSpec,
) -> foldit_gui::state::ParamSpec {
    foldit_gui::state::ParamSpec {
        name: spec.name,
        display_name: spec.display_name,
        description: spec.description,
        param_type: param_type_to_wire(spec.param_type),
        default: spec.default.map(param_value_to_wire),
        constraints: spec.constraints.map(param_constraint_to_wire),
    }
}

mirror_enum! {
    fn param_type_to_wire(t: RunnerParamType) -> WireParamType {
        Int, Float, Bool, String, Enum, Vec3
    }
}

mirror_enum! {
    fn param_value_to_wire(v: ParamValue) -> WireParamValue {
        Int(x), Float(x), Bool(x), String(x), Vec3(x)
    }
}

mirror_enum! {
    /// Convert a wire-side `ParamValue` (deserialized from an `OpDispatch`
    /// envelope) into the orchestrator-native form the dispatch calls
    /// expect. Inverse of [`param_value_to_wire`].
    pub(crate) fn param_value_from_wire(v: WireParamValue) -> ParamValue {
        Int(x), Float(x), Bool(x), String(x), Vec3(x)
    }
}

mirror_enum! {
    fn param_constraint_to_wire(c: RunnerParamConstraint) -> WireParamConstraint {
        IntRange { min, max }, FloatRange { min, max },
        EnumValues(x), StringPattern(x)
    }
}

/// Get the trajectory path from command-line arguments.
pub fn trajectory_path_from_args() -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.windows(2).find_map(|w| {
        if w[0] == "--trajectory" {
            Some(w[1].clone())
        } else {
            None
        }
    })
}

/// Build the per-entity context bundle that ML-side ops (predict, MPNN,
/// RFD3) require. Pure helper — no router/app state.
///
/// Builds a `SessionContext` carrying the orchestrator-facing focus +
/// selection. Plugins read their own per-entity state. Selection
/// capture is not yet wired; the field is left empty for now.
// reserved for plugin migration (REMAINING_WORK T8: SessionContext)
#[allow(dead_code)]
pub fn build_session_context(
    target_id: molex::entity::molecule::id::EntityId,
    focused_entity_id: Option<u32>,
) -> SessionContext {
    SessionContext {
        focused_entity_id: focused_entity_id
            .map(|id| RunnerEntityId(u64::from(id)))
            .or_else(|| Some(RunnerEntityId(u64::from(target_id.raw())))),
        selection: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `param_value_from_wire` must be the exact inverse of
    /// `param_value_to_wire` across every variant, so values the GUI
    /// posts back through `OpDispatch.params` reach plugins unchanged.
    #[test]
    fn param_value_wire_roundtrip() {
        let cases = [
            ParamValue::Int(7),
            ParamValue::Float(0.25),
            ParamValue::Bool(true),
            ParamValue::String("alpha".to_string()),
            ParamValue::Vec3([1.0, -2.0, 3.5]),
        ];
        for native in cases {
            let back = param_value_from_wire(param_value_to_wire(native.clone()));
            assert_eq!(back, native);
        }
    }
}
