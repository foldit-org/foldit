//! Backend update processing: applies backend results to the render engine.
//!
//! Free functions extracted from ActionRouter to keep the router focused on
//! translating user input into orchestrator commands.

use foldit_conv::coords::binary::{deserialize as deserialize_coords, serialize as serialize_coords};
use foldit_conv::coords::entity::MoleculeEntity;
use foldit_conv::coords::types::Coords;
use foldit_conv::coords::{
    align_coords_bytes, kabsch_alignment_with_scale, split_into_entities,
};
use foldit_frontend::DirtyFlags;
use foldit_runner::orchestrator::{BackendUpdate, EntityId, OpType};
use foldit_runner::Orchestrator;
use foldit_rs::shared_state::SharedState;
use glam::Vec3;
use std::collections::HashMap;
use viso::animation::AnimationAction;
use viso::engine::ProteinRenderEngine;
use viso::scene::{Focus, GroupId};

/// Handle a single backend update. Called by the frame loop after draining
/// triple buffers via SharedState.
pub(crate) fn handle_backend_update(
    engine: &mut ProteinRenderEngine,
    shared: &mut SharedState,
    orchestrator: &mut Option<Orchestrator>,
    ui_dirty: &mut DirtyFlags,
    latest_score: &mut Option<f64>,
    update: BackendUpdate,
) {
    match update {
        BackendUpdate::RosettaCoords {
            coords_bytes,
            score,
            cycle,
            message,
            converged,
            per_residue_scores,
        } => {
            handle_rosetta_coords(
                engine, shared, orchestrator, ui_dirty, latest_score,
                coords_bytes, score, cycle, message, converged, per_residue_scores,
            );
        }
        BackendUpdate::MLIntermediate {
            coords_bytes,
            backbone_positions,
            step,
            total_steps,
            confidence,
        } => {
            handle_ml_intermediate(
                engine, shared, ui_dirty,
                coords_bytes, backbone_positions, step, total_steps, confidence,
            );
        }
        BackendUpdate::MLPredictResult {
            coords_bytes,
            confidence,
        } => {
            handle_ml_predict_result(
                engine, shared, orchestrator, ui_dirty,
                coords_bytes, confidence,
            );
        }
        BackendUpdate::SequenceDesignResult { sequences, scores } => {
            handle_sequence_design_result(
                engine, shared, orchestrator, ui_dirty,
                sequences, scores,
            );
        }
        BackendUpdate::StructureDesignResult {
            entities,
            confidence,
        } => {
            handle_structure_design_result(
                engine, shared, orchestrator, ui_dirty,
                entities, confidence,
            );
        }
        BackendUpdate::Error { message } => {
            log::error!("Backend error: {}", message);
            // Restore visibility of any locked entity groups so models don't
            // "disappear" after a failed ML operation.
            if let Some(ref mut orch) = orchestrator {
                for eid in orch.locked_entities() {
                    let gid = GroupId(eid.0);
                    engine.scene.set_visible(gid, true);
                    orch.unlock(eid);
                }
            }
            *ui_dirty |= DirtyFlags::LOADING | DirtyFlags::ACTIONS;
        }
    }
}

fn handle_rosetta_coords(
    engine: &mut ProteinRenderEngine,
    shared: &mut SharedState,
    orchestrator: &mut Option<Orchestrator>,
    ui_dirty: &mut DirtyFlags,
    latest_score: &mut Option<f64>,
    coords_bytes: Vec<u8>,
    score: f64,
    cycle: u32,
    message: Option<String>,
    converged: bool,
    per_residue_scores: Option<Vec<f64>>,
) {
    *latest_score = Some(score);
    *ui_dirty |= DirtyFlags::SCORE | DirtyFlags::ACTIONS;

    // Check if any entity is locked for MLSequenceDesign (replaces mpnn_pending)
    let mpnn_entity = orchestrator.as_ref().and_then(|orch| {
        orch.locked_entities().into_iter().find(|eid| {
            orch.get_op_type(*eid) == Some(OpType::MLSequenceDesign)
        })
    });

    if let Some(mpnn_eid) = mpnn_entity {
        // Apply to the group that was locked for MPNN
        let apply_target = GroupId(mpnn_eid.0);
        let group_name = engine.scene.group(apply_target).map(|g| g.name().to_string());
        log::info!(
            "MPNN update received: {} bytes, score: {:.1}, target={:?} ({})",
            coords_bytes.len(),
            score,
            apply_target,
            group_name.as_deref().unwrap_or("?"),
        );

        match entities_from_coords_bytes(&coords_bytes) {
            Ok(entities) => {
                log::info!("MPNN structure parsed: {} entities", entities.len());
                let name = format!("MPNN Design (score {:.1})", score);
                if let Some(group) = engine.scene.group_mut(apply_target) {
                    group.set_entities(entities);
                    group.name = name;
                    group.invalidate_render_cache();
                }
                cache_per_residue_scores(engine, apply_target, &per_residue_scores);

                // Only animate the MPNN target entity with CollapseExpand;
                // all other entities snap.
                let mut entity_actions = HashMap::new();
                entity_actions.insert(apply_target, AnimationAction::Mutation);
                engine.sync_scene_to_renderers_targeted(entity_actions);

                if let Some(ref mut orch) = orchestrator {
                    orch.unlock(mpnn_eid);
                }
            }
            Err(e) => {
                log::error!("Failed to parse MPNN structure: {}", e);
                if let Some(ref mut orch) = orchestrator {
                    orch.unlock(mpnn_eid);
                }
            }
        }

        return;
    }

    // Normal wiggle/shake update
    log::info!(
        "Rosetta update: cycle {}, score {:.2}, converged: {}",
        cycle,
        score,
        converged
    );

    if let Some(ref state) = orchestrator.as_ref().and_then(|o| o.session().cloned()) {
        log::info!(
            "Applying full session update ({} structures, {} bytes)",
            state.structure_count(),
            coords_bytes.len()
        );

        let chain_ids: Vec<(GroupId, Vec<u8>)> = state
            .chain_ids_per_structure
            .iter()
            .map(|(id, chains)| (GroupId(id.0), chains.clone()))
            .collect();

        // Cache scores on all groups BEFORE sync so they're in PerGroupData
        for (group_id, _) in &chain_ids {
            cache_per_residue_scores(engine, *group_id, &per_residue_scores);
        }
        match engine.apply_combined_update(
            &coords_bytes,
            &chain_ids,
            AnimationAction::Wiggle,
        ) {
            Ok(()) => {
                log::info!("Successfully updated all structures in session");
            }
            Err(e) => log::warn!("Failed to apply combined update: {}", e),
        }
    } else {
        let focus = *engine.scene.focus();
        if let Some(id) = SharedState::operation_target(&focus)
            .or(shared.loaded_entity())
        {
            match entities_from_coords_bytes(&coords_bytes) {
                Ok(entities) => {
                    let name = message.unwrap_or_else(|| {
                        format!("Cycle {} (score {:.1})", cycle, score)
                    });
                    if let Some(group) = engine.scene.group_mut(id) {
                        group.set_entities(entities);
                        if !converged {
                            group.name = name;
                        }
                        group.invalidate_render_cache();
                    }
                    cache_per_residue_scores(engine, id, &per_residue_scores);
                    engine.sync_scene_to_renderers(Some(AnimationAction::Wiggle));
                }
                Err(e) => log::warn!("Failed to update structure from Rosetta: {}", e),
            }
        }
    }

}

fn handle_ml_intermediate(
    engine: &mut ProteinRenderEngine,
    shared: &mut SharedState,
    ui_dirty: &mut DirtyFlags,
    coords_bytes: Option<Vec<u8>>,
    backbone_positions: Vec<Vec3>,
    step: u32,
    total_steps: u32,
    confidence: f32,
) {
    let has_data = coords_bytes.is_some() || !backbone_positions.is_empty();
    log::info!(
        "ML update: step {}/{}, confidence {:.2}, has_coords={}, backbone_positions={}",
        step,
        total_steps,
        confidence,
        coords_bytes.is_some(),
        backbone_positions.len()
    );

    *ui_dirty |= DirtyFlags::LOADING;

    if has_data {
        update_animation_structure_from_backend(
            engine, shared,
            coords_bytes, backbone_positions, step, total_steps,
        );
    }
}

fn handle_ml_predict_result(
    engine: &mut ProteinRenderEngine,
    shared: &mut SharedState,
    orchestrator: &mut Option<Orchestrator>,
    ui_dirty: &mut DirtyFlags,
    coords_bytes: Vec<u8>,
    confidence: f32,
) {
    let is_assem = coords_bytes.len() >= 8
        && &coords_bytes[0..8] == foldit_conv::types::coords::ASSEMBLY_MAGIC;
    log::info!(
        "Prediction complete! Confidence: {:.2}, {} bytes, is_assem={}",
        confidence, coords_bytes.len(), is_assem,
    );

    if coords_bytes.is_empty() {
        log::error!("Prediction returned empty coords — not updating model");
        if let Some(ref mut orch) = orchestrator {
            for eid in orch.locked_entities() {
                orch.unlock(eid);
            }
        }
        *ui_dirty |= DirtyFlags::LOADING | DirtyFlags::ACTIONS;
        return;
    }

    if let Some(anim_id) = shared.animation() {
        engine.scene.remove_group(anim_id);
        shared.remove_animation();
    }

    if let Some(orig_id) = shared.loaded_entity() {
        let aligned_coords_bytes =
            if let Some(original_ca) = shared.reference_ca(orig_id) {
                match align_coords_bytes(&coords_bytes, original_ca) {
                    Ok(aligned) => {
                        log::info!("Applied Kabsch alignment to prediction ({} bytes)", aligned.len());
                        aligned
                    }
                    Err(e) => {
                        log::warn!("Failed to align coords: {}, using original", e);
                        coords_bytes.clone()
                    }
                }
            } else {
                log::info!("No reference CA — using prediction coords as-is");
                coords_bytes.clone()
            };

        match entities_from_coords_bytes(&aligned_coords_bytes) {
            Ok(entities) => {
                let total_atoms: usize = entities.iter().map(|e| e.coords.num_atoms).sum();
                log::info!(
                    "Parsed prediction: {} entities, {} total atoms",
                    entities.len(), total_atoms,
                );
                if total_atoms == 0 {
                    log::error!("Prediction entities have 0 atoms — not updating model");
                    engine.scene.set_visible(orig_id, true);
                } else {
                    let name = format!("RF3 ({:.0}%)", confidence * 100.0);
                    if let Some(group) = engine.scene.group_mut(orig_id) {
                        group.set_entities(entities);
                        group.name = name;
                        group.visible = true;
                        group.invalidate_render_cache();
                    }
                    engine.sync_scene_to_renderers(Some(AnimationAction::Diffusion));
                }
            }
            Err(e) => {
                log::error!("Failed to parse prediction: {}", e);
                engine.scene.set_visible(orig_id, true);
            }
        }
        if let Some(ref mut orch) = orchestrator {
            orch.unlock(EntityId(orig_id.0));
        }
    }

    *ui_dirty |= DirtyFlags::LOADING | DirtyFlags::ACTIONS;
}

fn handle_sequence_design_result(
    engine: &mut ProteinRenderEngine,
    shared: &mut SharedState,
    orchestrator: &mut Option<Orchestrator>,
    ui_dirty: &mut DirtyFlags,
    sequences: Vec<String>,
    scores: Vec<f32>,
) {
    log::info!("Sequence design complete!");
    for (i, (seq, score)) in sequences.iter().zip(scores.iter()).enumerate() {
        log::info!("  {}: {} (score: {:.3})", i + 1, seq, score);
    }

    // Derive target from the MPNN lock (not focus — focus may have changed
    // while MPNN was running).
    let target_group = orchestrator.as_ref().and_then(|orch| {
        orch.locked_entities().into_iter().find(|eid| {
            orch.get_op_type(*eid) == Some(OpType::MLSequenceDesign)
        })
    }).map(|eid| GroupId(eid.0));

    // Store all designed sequences associated with the target
    if let Some(target_id) = target_group {
        shared.add_designed_sequences(target_id, sequences.clone(), scores.clone());
    }

    let best_idx = scores
        .iter()
        .enumerate()
        .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(i, _)| i)
        .unwrap_or(0);

    let mut unlock_needed = true;

    if let Some(best_seq) = sequences.get(best_idx) {
        log::info!(
            "Using sequence {} (score: {:.3})",
            best_idx + 1,
            scores[best_idx]
        );

        // Get coords from the locked target group for Rosetta packing
        let coords = target_group.and_then(|id| get_group_coords_bytes(engine, id));

        if let Some(coords) = coords {
            if let Some(ref mut orch) = orchestrator {
                orch.stop_rosetta();
                orch.clear_session();

                log::info!("Applying designed sequence via Rosetta and packing sidechains...");
                if let Err(e) = orch.apply_sequence_and_pack(coords, best_seq.clone()) {
                    log::error!("Failed to apply sequence: {}", e);
                } else {
                    // Lock is held through packing — will be released when
                    // handle_rosetta_coords sees the MLSequenceDesign lock
                    unlock_needed = false;
                }
            }
        } else {
            log::warn!("No coords available from original structure");
        }
    }

    if unlock_needed {
        if let Some(id) = target_group {
            if let Some(ref mut orch) = orchestrator {
                orch.unlock(EntityId(id.0));
            }
        }
    }

    *ui_dirty |= DirtyFlags::LOADING | DirtyFlags::ACTIONS;
}

fn handle_structure_design_result(
    engine: &mut ProteinRenderEngine,
    shared: &mut SharedState,
    orchestrator: &mut Option<Orchestrator>,
    ui_dirty: &mut DirtyFlags,
    entities: Vec<foldit_conv::coords::entity::MoleculeEntity>,
    confidence: f32,
) {
    log::info!(
        "Structure design complete! {} entities, confidence: {:.2}",
        entities.len(),
        confidence
    );

    // Invalidate Rosetta session - topology has changed with new structure
    if let Some(ref mut orch) = orchestrator {
        orch.clear_session();
    }

    let had_animation = shared.animation().is_some();
    let design_group_id;
    if let Some(anim_id) = shared.animation() {
        if let Some(group) = engine.scene.group_mut(anim_id) {
            group.set_entities(entities);
            group.name = format!("RFD3 Design ({:.0}%)", confidence * 100.0);
            group.invalidate_render_cache();
            log::info!("Updated animation structure {:?} to final result", anim_id);
        }
        shared.promote_animation_to_design(anim_id, confidence);
        design_group_id = anim_id;
    } else {
        let name = format!("RFD3 Design ({:.0}%)", confidence * 100.0);
        let id = engine.load_entities(entities, &name, false);
        log::info!("Added designed structure {:?} to scene", id);
        if let Some(loaded) = shared.loaded_entity() {
            shared.register_animation(id, loaded);
            shared.promote_animation_to_design(id, confidence);
        }
        design_group_id = id;
    }

    // Unlock the original structure
    if let Some(orig_id) = shared.loaded_entity() {
        if let Some(ref mut orch) = orchestrator {
            orch.unlock(EntityId(orig_id.0));
        }
    }

    // Only animate the design group — not the input structure.
    engine.sync_scene_to_renderers_targeted(
        HashMap::from([(design_group_id, AnimationAction::DiffusionFinalize)]),
    );
    if !had_animation {
        engine.fit_camera_to_focus();
    }

    *ui_dirty |= DirtyFlags::LOADING | DirtyFlags::ACTIONS;
}

fn update_animation_structure_from_backend(
    engine: &mut ProteinRenderEngine,
    shared: &mut SharedState,
    coords_bytes: Option<Vec<u8>>,
    backbone_positions: Vec<Vec3>,
    step: u32,
    total_steps: u32,
) {
    log::debug!(
        "update_animation_structure: step {}/{}, has_coords={}, backbone_positions={}",
        step,
        total_steps,
        coords_bytes.is_some(),
        backbone_positions.len()
    );

    // SimpleFold: update full structure including sidechains, with scale correction
    if let Some(ref coords_bytes) = coords_bytes {
        if let Some(orig_id) = shared.loaded_entity() {
            match entities_from_coords_bytes(coords_bytes) {
                Ok(mut entities) => {
                    if let Some(original_ca) = shared.reference_ca(orig_id) {
                        let predicted_ca: Vec<Vec3> = entities
                            .iter()
                            .flat_map(|e| {
                                let mut cas = Vec::new();
                                for i in 0..e.coords.num_atoms {
                                    let name = std::str::from_utf8(&e.coords.atom_names[i])
                                        .unwrap_or("")
                                        .trim();
                                    if name == "CA" {
                                        cas.push(Vec3::new(
                                            e.coords.atoms[i].x,
                                            e.coords.atoms[i].y,
                                            e.coords.atoms[i].z,
                                        ));
                                    }
                                }
                                cas
                            })
                            .collect();

                        if let Some((rotation, translation, scale)) =
                            kabsch_alignment_with_scale(original_ca, &predicted_ca)
                        {
                            for entity in &mut entities {
                                for atom in &mut entity.coords.atoms {
                                    let pos = Vec3::new(atom.x, atom.y, atom.z);
                                    let transformed = rotation * (pos * scale) + translation;
                                    atom.x = transformed.x;
                                    atom.y = transformed.y;
                                    atom.z = transformed.z;
                                }
                            }
                            log::debug!(
                                "Applied Kabsch+scale ({:.3}) for frame {}",
                                scale,
                                step
                            );
                        }
                    }

                    let name = format!("Predicting... ({}/{})", step, total_steps);
                    if let Some(group) = engine.scene.group_mut(orig_id) {
                        group.set_entities(entities);
                        group.name = name;
                        group.invalidate_render_cache();
                        log::info!("Updated frame {}/{}", step, total_steps);
                    }
                    engine.sync_scene_to_renderers(Some(AnimationAction::Diffusion));
                }
                Err(e) => {
                    log::warn!("Failed to parse intermediate: {}", e);
                }
            }
        }
        return;
    }

    // RFD3: uses backbone_positions and needs animation structure (new design)
    if !backbone_positions.is_empty() {
        let backbone_chains = positions_to_backbone_chains(&backbone_positions);
        if backbone_chains.is_empty() || backbone_chains[0].is_empty() {
            log::warn!("Empty backbone chains, skipping update");
            return;
        }
        log::info!(
            "RFD3 intermediate: {} backbone positions -> {} backbone chains, {} residues",
            backbone_positions.len(),
            backbone_chains.len(),
            backbone_chains.iter().map(|c| c.len() / 3).sum::<usize>(),
        );

        if let Some(anim_id) = shared.animation() {
            if let Some(group) = engine.scene.group_mut(anim_id) {
                let coords = backbone_chains_to_coords(&backbone_chains);
                let entities = split_into_entities(&coords);
                group.set_entities(entities);
                group.name =
                    format!("Designing... ({}/{})", step, total_steps);
                group.invalidate_render_cache();
                log::info!(
                    "Updated animation frame {}/{}",
                    step,
                    total_steps
                );
            }
            // Only animate the design group — not the input structure.
            engine.sync_scene_to_renderers_targeted(
                HashMap::from([(anim_id, AnimationAction::Diffusion)]),
            );
        } else {
            let coords = backbone_chains_to_coords(&backbone_chains);
            let entities = split_into_entities(&coords);
            let name = format!("Designing... ({}/{})", step, total_steps);
            let id = engine.load_entities(entities, &name, false);
            let source = shared.loaded_entity().unwrap_or(GroupId(0));
            shared.register_animation(id, source);
            log::info!("Created animation structure {:?}", id);
            engine.fit_camera_to_focus();
        }
        return;
    }

    log::warn!("No coordinates in update, skipping");
}

/// Cache per-residue scores on a group (scores are stored as raw data;
/// the scene processor derives and caches colors from them).
fn cache_per_residue_scores(
    engine: &mut ProteinRenderEngine,
    group_id: GroupId,
    per_residue_scores: &Option<Vec<f64>>,
) {
    if let Some(scores) = per_residue_scores {
        if let Some(group) = engine.scene.group_mut(group_id) {
            group.set_per_residue_scores(Some(scores.clone()));
        }
    }
}

// ── Helpers ──

fn entities_from_coords_bytes(
    coords_bytes: &[u8],
) -> Result<Vec<foldit_conv::coords::entity::MoleculeEntity>, String> {
    // Auto-detect ASSEM01 (entity-aware) vs COORDS (flat) format
    if coords_bytes.len() >= 8
        && &coords_bytes[0..8] == foldit_conv::types::coords::ASSEMBLY_MAGIC
    {
        foldit_conv::types::coords::deserialize_assembly(coords_bytes)
            .map_err(|e| format!("Failed to parse ASSEM01: {:?}", e))
    } else {
        let coords = deserialize_coords(coords_bytes)
            .map_err(|e| format!("Failed to parse COORDS: {:?}", e))?;
        let coords = foldit_conv::coords::protein_only(&coords);
        Ok(split_into_entities(&coords))
    }
}

fn positions_to_backbone_chains(positions: &[Vec3]) -> Vec<Vec<Vec3>> {
    if positions.is_empty() {
        return vec![];
    }

    let mut chain: Vec<Vec3> = Vec::new();

    for chunk in positions.chunks(4) {
        if chunk.len() < 3 {
            continue;
        }
        chain.push(chunk[0]); // N
        chain.push(chunk[1]); // CA
        chain.push(chunk[2]); // C
    }

    if chain.is_empty() {
        vec![]
    } else {
        vec![chain]
    }
}

/// Serialize a group's protein coordinates to COORDS bytes.
pub(crate) fn get_group_coords_bytes(engine: &ProteinRenderEngine, id: GroupId) -> Option<Vec<u8>> {
    let group = engine.scene.group(id)?;
    let protein_coords = group.protein_coords()?;
    serialize_coords(&protein_coords).ok()
}

/// Serialize all entities in a group to ASSEM01 bytes (preserves all entity types).
pub(crate) fn get_group_assembly_bytes(engine: &ProteinRenderEngine, id: GroupId) -> Option<Vec<u8>> {
    let group = engine.scene.group(id)?;
    foldit_conv::types::assembly::assembly_bytes(group.entities()).ok()
}

/// Serialize a single entity's coordinates to COORDS bytes.
/// Returns `(GroupId, bytes)` so the caller can lock the containing group.
pub(crate) fn get_entity_coords_bytes(engine: &ProteinRenderEngine, entity_id: u32) -> Option<(GroupId, Vec<u8>)> {
    for gid in engine.scene.group_ids() {
        if let Some(group) = engine.scene.group(gid) {
            for entity in group.entities() {
                if entity.entity_id == entity_id {
                    if entity.coords.num_atoms == 0 {
                        return None;
                    }
                    let bytes = serialize_coords(&entity.coords).ok()?;
                    return Some((gid, bytes));
                }
            }
        }
    }
    None
}

/// Convert backbone chains (N,CA,C per residue) to minimal Coords.
fn backbone_chains_to_coords(backbone_chains: &[Vec<Vec3>]) -> Coords {
    use foldit_conv::coords::types::{CoordsAtom, Element};

    let mut atoms = Vec::new();
    let mut chain_ids = Vec::new();
    let mut res_names = Vec::new();
    let mut res_nums = Vec::new();
    let mut atom_names = Vec::new();

    for (chain_idx, chain) in backbone_chains.iter().enumerate() {
        let chain_id = b'A' + (chain_idx as u8 % 26);
        let num_residues = chain.len() / 3;

        for res_idx in 0..num_residues {
            let base = res_idx * 3;
            let names = [*b"N   ", *b"CA  ", *b"C   "];
            for (j, &atom_name) in names.iter().enumerate() {
                if let Some(&pos) = chain.get(base + j) {
                    atoms.push(CoordsAtom {
                        x: pos.x,
                        y: pos.y,
                        z: pos.z,
                        occupancy: 1.0,
                        b_factor: 0.0,
                    });
                    chain_ids.push(chain_id);
                    res_names.push(*b"ALA");
                    res_nums.push((res_idx + 1) as i32);
                    atom_names.push(atom_name);
                }
            }
        }
    }

    let elements = atom_names
        .iter()
        .map(|n| {
            let s = std::str::from_utf8(n).unwrap_or("");
            Element::from_atom_name(s)
        })
        .collect();

    Coords {
        num_atoms: atoms.len(),
        atoms,
        chain_ids,
        res_names,
        res_nums,
        atom_names,
        elements,
    }
}

/// Collect entities for ML based on current focus.
/// Returns `(GroupId, Vec<MoleculeEntity>)` for locking + serialization.
///
/// - `Focus::Entity(eid)` → returns only that single entity (and its group id).
/// - `Focus::Group(gid)` → returns all entities in the group.
/// - `Focus::Session` → falls back to `fallback_group`.
pub(crate) fn collect_ml_entities(
    engine: &ProteinRenderEngine,
    focus: &Focus,
    fallback_group: Option<GroupId>,
) -> Option<(GroupId, Vec<MoleculeEntity>)> {
    match focus {
        Focus::Entity(eid) => {
            for gid in engine.scene.group_ids() {
                if let Some(group) = engine.scene.group(gid) {
                    for entity in group.entities() {
                        if entity.entity_id == *eid {
                            return Some((gid, vec![entity.clone()]));
                        }
                    }
                }
            }
            None
        }
        Focus::Group(gid) => {
            let group = engine.scene.group(*gid)?;
            Some((*gid, group.entities().to_vec()))
        }
        Focus::Session => {
            let gid = fallback_group?;
            let group = engine.scene.group(gid)?;
            Some((gid, group.entities().to_vec()))
        }
    }
}

/// Serialize a slice of entities to ASSEM01 bytes.
pub(crate) fn entities_to_assembly_bytes(entities: &[MoleculeEntity]) -> Option<Vec<u8>> {
    foldit_conv::types::assembly::assembly_bytes(entities).ok()
}
