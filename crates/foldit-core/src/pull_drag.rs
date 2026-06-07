//! Mouse-drag → Rosetta pull plumbing.
//!
//! Owns the routing decision (backbone vs sidechain, hydrogen/non-protein
//! rejection) and bridges the GUI's pointer lifecycle to the unified
//! plugin op-id surface plus viso's pull-geom feed. Lives outside
//! `app.rs` because the routing is non-trivial and worth isolating from
//! the rest of the app lifecycle.
//!
//! The lifecycle is:
//!   - pointer-down on an atom: `begin` classifies the pick into one of
//!     two op ids, dispatches `StartStream`, and returns the viso
//!     `PullInfo` plus a `Pending` entry the host attaches to its
//!     `active_streams` map.
//!   - pointer-move: `update_endpoint` re-resolves the world-space drag
//!     target through viso (it owns the camera + atom positions) and
//!     hands it to the orchestrator as a single-key `endpoint` Vec3
//!     update.
//!   - pointer-up / ESC / stream Final/Error: caller drops the
//!     `PullDrag` and dispatches `CancelStream` (host already cancels
//!     other streams the same way).
//!
//! Single-stream invariant: only one `PullDrag` is alive at a time. The
//! caller enforces this by checking `Option<PullDrag>` before
//! constructing a new one.

#[cfg(not(target_arch = "wasm32"))]
use std::collections::HashMap;

use molex::chemistry::is_protein_backbone_atom_name;
use molex::entity::molecule::id::EntityId as MolexEntityId;
use molex::{Element, MoleculeEntity};
#[cfg(not(target_arch = "wasm32"))]
use viso::{AtomRef, PullInfo};

use crate::session::Session;

/// Op id for the residue-anchored cart-pull (backbone pull).
pub const OP_PULL_BACKBONE: &str = "ActionLocalMinimizePull";
/// Op id for the atom-anchored sidechain pull.
pub const OP_PULL_SIDECHAIN: &str = "ActionPullSidechain";

/// Resolved pull-route decision. The two variants map 1:1 to the two op
/// ids registered with the rosetta bridge; param shape differs (backbone
/// needs the residue only, sidechain needs residue + atom name) so the
/// caller dispatches accordingly.
#[cfg(not(target_arch = "wasm32"))]
pub struct PullRoute {
    /// Which op-id to dispatch (one of [`OP_PULL_BACKBONE`] /
    /// [`OP_PULL_SIDECHAIN`]).
    pub op_id: &'static str,
    /// PDB atom name the user picked (`"CA"`, `"CB"`, etc.). For
    /// backbone pulls the bridge ignores the name (residue-anchored);
    /// for sidechain pulls it's the dispatch key.
    pub atom_name: String,
    /// 0-based residue index within the entity. Bridge expects
    /// 1-indexed pose residue, so the caller converts at dispatch.
    pub residue_in_entity: u32,
    /// Entity-flat 0-based residue index (matches `viso::AtomRef`).
    pub flat_residue: u32,
    /// Molex entity id of the picked entity. Used both for the
    /// `DispatchContext` focus and to compute the rosetta-pose residue
    /// from `residue_in_entity` once multi-entity routing lands.
    pub entity_id: MolexEntityId,
}

/// Live pull-drag state. One per active drag at most (single-stream
/// invariant). Owned by the host alongside its `active_streams` map.
#[cfg(not(target_arch = "wasm32"))]
pub struct PullDrag {
    /// The stream's request id from `dispatch_start_stream`.
    pub request_id: u64,
    /// Plugin id that owns the stream (always `"rosetta"` today).
    pub plugin_id: String,
    /// viso pull-geom spec; the host re-feeds this to
    /// `engine.update_pull` every frame so the capsule + arrow render.
    pub pull_info: PullInfo,
}

/// Classify a `PickTarget::Atom { entity_id, atom_idx }` against the
/// host policy and produce a pull route, or `None` to reject (no pull
/// dispatched).
///
/// Rejection cases:
///   - entity not found in the store
///   - entity is not a protein (no pull on NA / small molecules / bulk)
///   - picked atom is a hydrogen (atom name starts with `H`; covers
///     PDB v3 `HA`, `H`, `HB1`, `HG21`, …)
#[cfg(not(target_arch = "wasm32"))]
#[must_use]
pub fn route_atom_pick(
    store: &Session,
    entity_id: u32,
    atom_idx: u32,
) -> Option<PullRoute> {
    let molex_id = store
        .ids()
        .find(|id| id.raw() == entity_id)?;
    let MoleculeEntity::Protein(protein) = store.entity(molex_id)? else {
        return None;
    };
    let atom = protein.atoms.get(atom_idx as usize)?;
    // Hydrogen reject - element field is the authoritative check; the
    // atom_name prefix gate in the cartoon-pick path is a fallback for
    // when we don't have the Atom in hand.
    if atom.element == Element::H {
        return None;
    }
    let atom_name = trim_atom_name(atom.name);
    // residue counts << u32::MAX.
    #[allow(clippy::cast_possible_truncation)]
    let residue_in_entity = protein
        .residues
        .iter()
        .position(|r| r.atom_range.contains(&(atom_idx as usize)))?
        as u32;

    let op_id = if is_protein_backbone_atom_name(&atom_name) {
        OP_PULL_BACKBONE
    } else {
        OP_PULL_SIDECHAIN
    };
    Some(PullRoute {
        op_id,
        atom_name,
        residue_in_entity,
        flat_residue: residue_in_entity,
        entity_id: molex_id,
    })
}

/// Classify a `PickTarget::Residue(flat_idx)` (cartoon-mode pick)
/// against the host policy. Defers to viso for the "closest atom by
/// screen distance" lookup, then applies the same hydrogen / non-protein
/// gating as [`route_atom_pick`].
///
/// `entity_for_flat_residue` is the host's view of which entity owns a
/// given flat residue index; the host has the multi-entity layout, this
/// module does not.
#[cfg(not(target_arch = "wasm32"))]
#[must_use]
pub fn route_residue_pick(
    store: &Session,
    flat_residue: u32,
    atom_name: &str,
    entity_id: MolexEntityId,
    residue_in_entity: u32,
) -> Option<PullRoute> {
    if atom_name.starts_with('H') {
        return None;
    }
    if !matches!(store.entity(entity_id)?, MoleculeEntity::Protein(_)) {
        return None;
    }
    let op_id = if is_protein_backbone_atom_name(atom_name) {
        OP_PULL_BACKBONE
    } else {
        OP_PULL_SIDECHAIN
    };
    Some(PullRoute {
        op_id,
        atom_name: atom_name.to_owned(),
        residue_in_entity,
        flat_residue,
        entity_id,
    })
}

/// Build the `StartStream` `params` map for a pull route. Backbone pulls
/// carry only the 1-indexed pose residue; sidechain pulls add the
/// `atom_name` so the bridge can resolve `name → atomno` against the
/// live pose.
///
/// The conversion to 1-indexed rosetta-pose residue assumes a
/// single-protein-entity layout (the common Foldit case). Multi-entity
/// support requires the bridge to expose its entity → pose-offset map
/// to the host; left as a v1 limitation.
#[cfg(not(target_arch = "wasm32"))]
#[must_use]
pub fn build_start_params(
    op_id: &str,
    residue_in_entity: u32,
    atom_name: &str,
) -> HashMap<String, foldit_runner::orchestrator::ParamValue> {
    use foldit_runner::orchestrator::ParamValue;
    // ParamValue::Int is i32; rosetta-pose residue is 1-indexed
    // core::Size on the bridge side. `as i32` is safe for any
    // realistic foldit pose (max residues ≪ i32::MAX).
    #[allow(clippy::cast_possible_wrap)]
    let pose_residue = (residue_in_entity as i32) + 1;
    let mut params = HashMap::new();
    let _ = params.insert(String::from("residue"), ParamValue::Int(pose_residue));
    if op_id == OP_PULL_SIDECHAIN {
        let _ = params.insert(
            String::from("atom_name"),
            ParamValue::String(atom_name.to_owned()),
        );
    }
    params
}

/// Build the viso `PullInfo` spec used to drive the pull-geom capsule
/// and arrow render. The atom reference must match what viso resolves
/// on its side; for protein picks the flat residue + PDB atom name
/// land in viso's `ConstraintContext::resolve_atom_ref`.
#[cfg(not(target_arch = "wasm32"))]
#[must_use]
pub fn build_pull_info(
    route: &PullRoute,
    screen_target: (f32, f32),
) -> PullInfo {
    PullInfo {
        atom: AtomRef {
            residue: route.flat_residue,
            atom_name: route.atom_name.clone(),
        },
        screen_target,
    }
}

#[cfg(not(target_arch = "wasm32"))]
use crate::app::input::update_all_visualizations;
#[cfg(not(target_arch = "wasm32"))]
use crate::app::App;
#[cfg(not(target_arch = "wasm32"))]
use crate::history::CheckpointKind;
#[cfg(not(target_arch = "wasm32"))]
use crate::runner_client::StreamStartIntent;

#[cfg(not(target_arch = "wasm32"))]
impl App {
    /// Pull-drag interception, run ahead of viso's regular input routing so
    /// an active drag suppresses camera rotation/pan. Handles the three
    /// gesture phases (pull-start on the first held move after a resolved
    /// down-target, an active drag's move, and the active drag's release),
    /// each finalizing the viewport and reporting the gesture consumed.
    /// Returns `true` when it claimed the input (the caller then returns
    /// early), `false` on fall-through to the regular routing below.
    // The match-arm guard checks `pending_pull_origin.is_some()`, so the
    // `take().expect()` below is provably `Some`.
    #[allow(clippy::expect_used)]
    pub(crate) fn try_pull_drag_interception(&mut self, input: &foldit_gui::ViewportInput) -> bool {
        use foldit_gui::ViewportInput;
        match input {
            ViewportInput::PointerMove { x, y, .. }
                if self
                    .engine
                    .as_ref()
                    .is_some_and(viso::VisoEngine::mouse_pressed)
                    && !self.runner_client.has_active_pull_drag()
                    && self.pending_pull_origin.is_some() =>
            {
                // The pull intent was locked at button-down; the move
                // only supplies the drag endpoint. Take the route so
                // this gesture makes at most one start attempt - a
                // failed stream start falls through to camera for the
                // rest of the drag rather than retrying mid-gesture.
                let route = self
                    .pending_pull_origin
                    .take()
                    .expect("guard guarantees Some");
                if self.begin_pull_drag_from_route(&route, *x, *y) {
                    // viso recorded the press; drop its mouse
                    // state so the now-suppressed pointer-up
                    // can't fire a stray click → selection.
                    if let Some(engine) = self.engine.as_mut() {
                        engine.release_mouse_state();
                    }
                    self.update_pull_drag(*x, *y);
                    self.finalize_viewport_input();
                    return true;
                }
            }
            ViewportInput::PointerMove { x, y, .. } if self.runner_client.has_active_pull_drag() => {
                self.update_pull_drag(*x, *y);
                self.finalize_viewport_input();
                return true;
            }
            ViewportInput::PointerUp { .. } if self.runner_client.has_active_pull_drag() => {
                self.end_pull_drag();
                self.finalize_viewport_input();
                return true;
            }
            _ => {}
        }
        false
    }

    /// Called after the pull-drag interception path. Mirrors the
    /// trailing visualization update the regular `handle_viewport_input`
    /// flow does (the `SessionUpdate` drain itself is tick-driven now).
    /// Pre-snapshots the pull info so the engine borrow doesn't overlap
    /// with the live pull-drag state held in the plugin driver.
    fn finalize_viewport_input(&mut self) {
        let pull = self.runner_client.pull_drag_pull_info();
        if let Some(engine) = self.engine.as_mut() {
            update_all_visualizations(engine, pull);
        }
    }

    /// Classify the pick under `(x, y)` into a pull route, or `None` if
    /// the target is empty / non-pullable (non-protein entity, hydrogen
    /// atom, no atom under the cursor). Pure resolution: reads the engine
    /// pick + store but mutates nothing, so it can run at `PointerDown`
    /// against the just-fed down-cursor to lock the pull's anchor. Takes
    /// `engine` + `store` as borrows rather than `&self` so the caller can
    /// invoke it while holding a disjoint `&mut self.engine` borrow.
    pub(crate) fn resolve_pull_route(
        engine: &viso::VisoEngine,
        store: &Session,
        x: f32,
        y: f32,
    ) -> Option<crate::pull_drag::PullRoute> {
        match engine.hovered_target() {
            viso::PickTarget::Atom {
                entity_id,
                atom_idx,
            } => crate::pull_drag::route_atom_pick(store, entity_id, atom_idx),
            viso::PickTarget::Residue(flat) => {
                engine.picked_residue_atom(flat, (x, y)).and_then(|picked| {
                    let molex_id = store.ids().find(|id| id.raw() == picked.entity_id)?;
                    crate::pull_drag::route_residue_pick(
                        store,
                        flat,
                        &picked.atom_name,
                        molex_id,
                        picked.local_residue,
                    )
                })
            }
            viso::PickTarget::None => None,
        }
    }

    /// Open a pull-drag stream from a pre-resolved `route` (locked at
    /// button-down) with `(x, y)` as the current drag endpoint: dispatch
    /// the matching pull op-id, open the history edit, and install drag
    /// state. Returns true if the stream started (so the caller suppresses
    /// the regular viso input flow), false if `start_stream` failed (the
    /// gesture then falls through to camera handling).
    fn begin_pull_drag_from_route(
        &mut self,
        route: &crate::pull_drag::PullRoute,
        x: f32,
        y: f32,
    ) -> bool {
        let pull_info = crate::pull_drag::build_pull_info(route, (x, y));

        let store = &self.store;
        let intent = StreamStartIntent {
            op_id: route.op_id,
            focused_entity: route.entity_id,
            residue_in_entity: route.residue_in_entity,
            atom_name: route.atom_name.clone(),
        };
        let (rid, plugin_id) =
            match self.runner_client.start_stream(&intent, |id| store.entity_type(id)) {
                Ok(v) => v,
                Err(e) => {
                    log::warn!(
                        "begin_pull_drag_from_route: start_stream {:?} failed: {e:?}",
                        route.op_id,
                    );
                    return false;
                }
            };

        // History side-effect - same shape as button-driven dispatch
        // so the drag's eventual commit_action lands as a regular
        // PluginOp entry. Failure is non-fatal (commit_action becomes
        // a no-op on an idle store).
        let action_entity = self
            .store
            .ids()
            .find(|id| id.raw() == route.entity_id.raw());
        if let Some(entity) = action_entity {
            let kind = CheckpointKind::PluginOp {
                plugin_id: plugin_id.clone(),
                op_id: String::from(route.op_id),
                display: String::from("Pull"),
            };
            // Open the edit under the dispatch's request_id; the stream
            // table is keyed by the same id, so the terminal commit lands
            // on this edit.
            if let Err(e) =
                self.store.begin_action([entity], kind, String::from("Pull"), rid)
            {
                log::trace!("begin_pull_drag_from_route: begin_action skipped: {e}");
            }
        }

        self.runner_client.set_pull_drag(crate::pull_drag::PullDrag {
            request_id: rid,
            plugin_id,
            pull_info,
        });
        true
    }

    /// Pointer-move during an active drag: re-resolve the world-space
    /// drag target through the camera, and push a single-key
    /// `endpoint` Vec3 update to the running stream. Also refreshes
    /// `pull_info.screen_target` so the next visualization pass moves
    /// the cone tip with the cursor.
    fn update_pull_drag(&mut self, x: f32, y: f32) {
        let Some(drag) = self.runner_client.pull_drag_mut() else {
            return;
        };
        drag.pull_info.screen_target = (x, y);
        let (residue, atom_name, plugin_id, request_id) = (
            drag.pull_info.atom.residue,
            drag.pull_info.atom.atom_name.clone(),
            drag.plugin_id.clone(),
            drag.request_id,
        );

        let Some(engine) = self.engine.as_ref() else {
            return;
        };
        let Some(atom_pos) = engine.resolve_atom_position(residue, &atom_name) else {
            return;
        };
        let target = engine.screen_to_world_at_depth(glam::Vec2::new(x, y), atom_pos);

        self.runner_client.update_stream(request_id, &plugin_id, target);
    }

    /// Pointer-up (or any cancel signal): tear down the drag state
    /// and ask the orchestrator to cancel the stream. The stream's
    /// terminal `PluginUpdate::Cancelled` flows through
    /// `apply_backend_updates` → `commit_action`, so the partial pull
    /// becomes a permanent undo entry.
    fn end_pull_drag(&mut self) {
        let Some(drag) = self.runner_client.take_pull_drag() else {
            return;
        };
        self.runner_client.end_stream(drag.request_id, &drag.plugin_id);
    }
}

/// Trim the trailing zero / space padding off a PDB atom name. Atoms
/// in molex carry the raw 4-byte buffer; the on-wire / classifier
/// representation drops the padding.
fn trim_atom_name(raw: [u8; 4]) -> String {
    let end = raw
        .iter()
        .position(|b| *b == 0 || *b == b' ')
        .unwrap_or(raw.len());
    String::from_utf8_lossy(&raw[..end]).into_owned()
}
