//! Right-button drag sphere selection: press on a residue to anchor a
//! world-space sphere at its neighbor atom, drag to grow the radius, and
//! select every residue whose neighbor atom falls inside. The neighbor atom
//! is CB, or CA when the residue has no CB (glycine), matching Rosetta's
//! `nbr_atom_xyz`. Bare drag replaces the selection; shift-drag unions with
//! the selection as it stood at press.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use glam::Vec3;
use molex::entity::molecule::id::EntityId;
use molex::MoleculeEntity;

use crate::app::App;
use crate::session::Session;

/// Transient state for an in-progress right-drag sphere selection.
#[derive(Default)]
pub(in crate::app) struct SphereSelect {
    /// Sphere centre in world space, angstroms. `Some` iff an anchor resolved.
    center: Option<Vec3>,
    /// Selection captured at pointer-down, unioned into the result. `Some`
    /// only when the drag began with shift held.
    base: Option<BTreeMap<EntityId, BTreeSet<u32>>>,
    /// Set while the right button is held, whether or not an anchor resolved.
    /// Lets the gesture claim every right-button event even when the press
    /// missed a selectable residue.
    right_down: bool,
}

impl App {
    /// Sphere-select interception, run ahead of viso's regular input routing
    /// so a right-button drag never rotates the camera or clears the
    /// selection. The gesture owns the right button outright: a right-button
    /// down claims the button and anchors the sphere when it lands on a
    /// selectable residue; every subsequent move and the release are claimed
    /// while the button is held, growing the radius and rewriting the
    /// selection live once an anchor exists. Returns `true` when it claimed
    /// the input (the caller then returns early).
    pub(in crate::app) fn try_sphere_select_interception(
        &mut self,
        input: &foldit_gui::ViewportInput,
    ) -> bool {
        use foldit_gui::ViewportInput;
        match *input {
            ViewportInput::PointerDown {
                button: 2, shift, ..
            } => {
                self.sphere_select.right_down = true;
                self.begin_sphere_select(shift);
                true
            }
            ViewportInput::PointerMove { x, y, .. } if self.sphere_select.right_down => {
                if self.sphere_select.center.is_some() {
                    self.update_sphere_select(x, y);
                }
                true
            }
            ViewportInput::PointerUp { button: 2, .. } if self.sphere_select.right_down => {
                self.end_sphere_select();
                true
            }
            _ => false,
        }
    }

    /// Anchor the sphere at the neighbor atom of the residue under the cursor,
    /// capturing the current selection when `shift` is held. A no-op that
    /// leaves `center` unset when the hover is not a live residue or the
    /// residue has neither CB nor CA; the button is already claimed by the
    /// caller, so such a press starts no drag and does nothing further.
    fn begin_sphere_select(&mut self, shift: bool) {
        let target = self
            .harness
            .engine
            .as_ref()
            .and_then(|engine| super::pick::hovered_segment_target(engine, &self.store));
        let Some((eid, local_residue)) = target else {
            return;
        };
        let center = self
            .store
            .entity(eid)
            .and_then(|entity| residue_neighbor_pos(entity, local_residue));
        let Some(center) = center else {
            return;
        };
        self.sphere_select.center = Some(center);
        self.sphere_select.base = shift.then(|| self.store.selection().clone());
    }

    /// Reproject the cursor onto the camera-parallel plane through the centre,
    /// take the radius as the distance from the centre to that point, refresh
    /// the preview sphere, and rewrite the selection to the residues inside.
    fn update_sphere_select(&mut self, x: f32, y: f32) {
        let Some(center) = self.sphere_select.center else {
            return;
        };
        let Some(world) = self
            .harness
            .screen_to_world_at_depth(glam::Vec2::new(x, y), center)
        else {
            return;
        };
        let radius = (world - center).length();
        if let Some(engine) = self.harness.engine.as_mut() {
            engine.update_select_sphere(Some(viso::SelectSphereInfo { center, radius }));
        }
        let hits = residues_in_sphere(&self.store, center, radius);
        self.apply_sphere_selection(hits);
    }

    /// Clear the preview sphere and reset the drag state.
    fn end_sphere_select(&mut self) {
        if let Some(engine) = self.harness.engine.as_mut() {
            engine.update_select_sphere(None);
        }
        self.sphere_select = SphereSelect::default();
    }

    /// Commit the sweep result: union it into the captured base (shift-drag)
    /// or replace the whole selection (bare drag). Clearing first lets a
    /// shrinking radius drop residues the previous move had selected.
    fn apply_sphere_selection(&mut self, hits: BTreeMap<EntityId, BTreeSet<u32>>) {
        let mut result = self.sphere_select.base.clone().unwrap_or_default();
        for (eid, set) in hits {
            result.entry(eid).or_default().extend(set);
        }
        self.store.clear_selection();
        for (eid, set) in result {
            self.store.set_residues_on(eid, set);
        }
    }
}

/// World-space position of a residue's neighbor atom: its CB, or CA when
/// there is no CB. `None` for non-polymer entities (their `residues()` is
/// `None`, so small molecules and ligands are never selected) and for a
/// residue carrying neither atom.
fn residue_neighbor_pos(entity: &MoleculeEntity, local_residue: usize) -> Option<Vec3> {
    let residue = entity.residues()?.get(local_residue)?;
    neighbor_pos(
        residue.atom_range.clone(),
        &entity.columns().name,
        entity.positions(),
    )
}

/// Every residue whose neighbor atom lies within `radius` of `center`, as a
/// per-entity residue-index map. Non-polymer entities are skipped.
fn residues_in_sphere(
    store: &Session,
    center: Vec3,
    radius: f32,
) -> BTreeMap<EntityId, BTreeSet<u32>> {
    let radius_sq = radius * radius;
    let mut hits = BTreeMap::new();
    for eid in store.ids() {
        let Some(entity) = store.entity(eid) else {
            continue;
        };
        let Some(residues) = entity.residues() else {
            continue;
        };
        let names = &entity.columns().name;
        let positions = entity.positions();
        let mut set = BTreeSet::new();
        for (idx, residue) in residues.iter().enumerate() {
            let Some(pos) = neighbor_pos(residue.atom_range.clone(), names, positions) else {
                continue;
            };
            if (pos - center).length_squared() <= radius_sq {
                // residue counts are far below u32::MAX.
                #[allow(clippy::cast_possible_truncation)]
                set.insert(idx as u32);
            }
        }
        if !set.is_empty() {
            hits.insert(eid, set);
        }
    }
    hits
}

/// Neighbor-atom position within an atom range: the CB column, else the CA
/// column. molex exposes no name lookup, so scan the range and compare the
/// trimmed 4-byte name.
fn neighbor_pos(range: Range<usize>, names: &[[u8; 4]], positions: &[Vec3]) -> Option<Vec3> {
    named_atom_pos(range.clone(), names, positions, b"CB")
        .or_else(|| named_atom_pos(range, names, positions, b"CA"))
}

/// First atom in `range` whose trimmed name equals `target`, or `None`.
fn named_atom_pos(
    range: Range<usize>,
    names: &[[u8; 4]],
    positions: &[Vec3],
    target: &[u8],
) -> Option<Vec3> {
    for i in range {
        if names.get(i).is_some_and(|name| trimmed_name_eq(*name, target)) {
            return positions.get(i).copied();
        }
    }
    None
}

/// Compare a raw 4-byte atom name to `target`, ignoring the trailing padding.
fn trimmed_name_eq(raw: [u8; 4], target: &[u8]) -> bool {
    crate::atom_name::trimmed_atom_name(&raw) == target
}
