//! Action Router: translates user input into orchestrator/backend commands.
//!
//! Owns routing state (orchestrator handle) and dispatches user actions
//! to the appropriate backend operations. Does NOT handle backend output
//! processing, rendering, or frontend state sync.

use foldit_gui::DirtyFlags;
use crate::entity_store::{EntityRole, EntityStore};
use viso::{InputEvent, InputProcessor, MouseButton, VisoCommand, VisoEngine};
use foldit_runner::orchestrator::{
    EntityId as RunnerEntityId, OpType, ParamValue, SessionContext,
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

/// Central mediator for action dispatch, owning all routing state.
///
/// Rosetta-specific UI scaffolding (pull-drag, band-drag, active_bands
/// mirror) was gutted alongside the in-process executor — it'll come
/// back wired through the unified plugin protocol once bridge/ implements
/// the pull / band_add handlers (items 54–61).
pub struct ActionRouter {
    pub orchestrator: Option<Orchestrator>,
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
            orchestrator: None,
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

    /// Release any lock state when puzzle topology changes.
    pub fn reset_for_new_structure(&mut self) {
        if let Some(ref mut orch) = self.orchestrator {
            for eid in orch.locked_entities() {
                orch.unlock(eid);
            }
        }
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
        _store: &mut EntityStore,
        action: foldit_gui::ActionId,
    ) -> Option<foldit_gui::ParameterizedAction> {
        log::error!(
            "handle_trigger_action({:?}) reached router; built-ins are \
             intercepted at App level",
            action
        );
        None
    }

    pub fn cancel_operations(&mut self, engine: &mut VisoEngine, store: &mut EntityStore) {
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
    pub fn start_op(
        &mut self,
        request: BackendOpRequest,
        engine: &mut VisoEngine,
        store: &mut EntityStore,
    ) {
        let Some(ref mut orch) = self.orchestrator else {
            log::warn!("Orchestrator not initialized");
            return;
        };

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
                        EntityRole { foldable: false, designable: false, ambient: false },
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

    #[cfg(not(target_arch = "wasm32"))]
    #[allow(dead_code)]
    fn run_prediction(&mut self, engine: &mut VisoEngine, store: &mut EntityStore) {
        let focus = engine.focus();
        let fallback = store.loaded_entity();
        let Some((target_id, entities)) =
            store.collect_ml_entities(&focus, fallback)
        else {
            log::warn!("No structure available for prediction");
            return;
        };

        let total_atoms: usize = entities.iter().map(|e| e.atom_count()).sum();
        log::info!(
            "RF3 prediction: focus={:?}, {} entities, {} total atoms",
            focus, entities.len(), total_atoms,
        );

        let pending_ca = molex::ops::codec::ca_positions(&entities);
        let entity_context = build_session_context(target_id, None);
        let num_recycles: i32 = 3;

        self.start_op(
            BackendOpRequest {
                target: RunnerEntityId(u64::from(target_id.raw())),
                op_type: OpType::MLPredict,
                entity_context,
                create_preview_mirror: true,
                pending_reference_ca: Some(pending_ca),
                kickoff: Box::new(move |orch, ctx| {
                    let mut params = std::collections::HashMap::new();
                    params.insert(
                        "num_recycles".to_string(),
                        ParamValue::Int(num_recycles),
                    );
                    orch.dispatch_invoke("predict", ctx, params, |_| None)
                        .map(|_| ())
                        .map_err(|e| e.to_string())
                }),
            },
            engine,
            store,
        );
    }

    // ── Mouse / input handlers ──

    pub fn handle_native_mouse_input(
        &mut self,
        engine: &mut VisoEngine,
        input: &mut InputProcessor,
        _store: &mut EntityStore,
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
        _store: &mut EntityStore,
        x: f32,
        y: f32,
    ) {
        engine.set_cursor_pos(x, y);
        self.last_mouse_pos = (x, y);
    }

    // ── Shutdown ──

    pub fn shutdown(&self) {
        if let Some(ref orch) = self.orchestrator {
            orch.shutdown();
        }
    }
}

// ---------------------------------------------------------------------------
// Free functions (used by App)
// ---------------------------------------------------------------------------

/// Extract CA positions from entities (for Kabsch alignment).
pub fn entities_backbone_ca(
    entities: &[molex::MoleculeEntity],
) -> Vec<Vec3> {
    molex::ops::codec::ca_positions(entities)
}

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
        })
        .collect()
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
/// selection. Entity-role hints (designable / foldable / ambient) are
/// not plumbed through — plugins read their own state. Selection
/// capture is not yet wired; the field is left empty for now.
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
