//! Backend update processing: applies backend results to the render engine.
//!
//! Free functions extracted from ActionRouter to keep the router focused on
//! translating user input into orchestrator/backend commands.

use molex::ops::codec::deserialize as deserialize_coords;
use molex::ops::codec::split_into_entities;
use molex::{Assembly, MoleculeEntity};
use molex::ops::transform::{align_coords_bytes, kabsch_alignment_with_scale};
use foldit_frontend::DirtyFlags;
use foldit_runner::orchestrator::{BackendUpdate, EntityId, OpType};
use foldit_runner::Orchestrator;
use foldit_rs::entity_store::{EntityStore, EntityOrigin, EntityRole};
use foldit_rs::shared_state::SharedState;
use glam::Vec3;
use std::collections::HashMap;
use viso::{Focus, Transition, VisoEngine};

/// Handle a single backend update. Called by the frame loop after draining
/// triple buffers via SharedState.
pub(crate) fn handle_backend_update(
    engine: &mut VisoEngine,
    store: &mut EntityStore,
    shared: &mut SharedState,
    orchestrator: &mut Option<Orchestrator>,
    ui_dirty: &mut DirtyFlags,
    pending_prediction_reference: &mut Option<Vec<Vec3>>,
    latest_score: &mut Option<f64>,
    update: BackendUpdate,
) {
    match update {
        BackendUpdate::RosettaCoords {
            assembly,
            score,
            cycle,
            message,
            converged,
            per_residue_scores,
        } => {
            handle_rosetta_coords(
                engine, store, shared, orchestrator, ui_dirty, latest_score,
                assembly, score, cycle, message, converged, per_residue_scores,
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
                engine, store, ui_dirty,
                pending_prediction_reference.as_deref(),
                coords_bytes, backbone_positions, step, total_steps, confidence,
            );
        }
        BackendUpdate::MLPredictResult {
            coords_bytes,
            confidence,
        } => {
            handle_ml_predict_result(
                engine, store, orchestrator, ui_dirty,
                pending_prediction_reference,
                coords_bytes, confidence,
            );
        }
        BackendUpdate::SequenceDesignResult { sequences, scores } => {
            handle_sequence_design_result(
                engine, store, orchestrator, ui_dirty,
                sequences, scores,
            );
        }
        BackendUpdate::StructureDesignResult {
            assembly,
            confidence,
        } => {
            handle_structure_design_result(
                engine, store, orchestrator, ui_dirty,
                assembly.entities().to_vec(), confidence,
            );
        }
        BackendUpdate::Error { message } => {
            log::error!("Backend error: {}", message);
            // Restore visibility of any locked entities so models don't
            // "disappear" after a failed ML operation.
            if let Some(ref mut orch) = orchestrator {
                for eid in orch.locked_entities() {
                    let id = eid.0 as u32;
                    engine.set_entity_visible(id, true);
                    orch.unlock(eid);
                }
            }
            *ui_dirty |= DirtyFlags::LOADING | DirtyFlags::ACTIONS;
        }
    }
}

fn handle_rosetta_coords(
    engine: &mut VisoEngine,
    store: &mut EntityStore,
    _shared: &mut SharedState,
    orchestrator: &mut Option<Orchestrator>,
    ui_dirty: &mut DirtyFlags,
    latest_score: &mut Option<f64>,
    assembly: Assembly,
    score: f64,
    cycle: u32,
    message: Option<String>,
    converged: bool,
    per_residue_scores: Option<Vec<f64>>,
) {
    *latest_score = Some(score);
    *ui_dirty |= DirtyFlags::SCORE | DirtyFlags::ACTIONS;

    let returned: Vec<MoleculeEntity> = assembly.entities().to_vec();

    // Check if any entity is locked for MLSequenceDesign (replaces mpnn_pending)
    let mpnn_entity = orchestrator.as_ref().and_then(|orch| {
        orch.locked_entities().into_iter().find(|eid| {
            orch.get_op_type(*eid) == Some(OpType::MLSequenceDesign)
        })
    });

    if let Some(mpnn_eid) = mpnn_entity {
        // Apply to the entity that was locked for MPNN
        let apply_target = mpnn_eid.0 as u32;
        let entity_name = store.get(apply_target).map(|te| te.name.clone());
        log::info!(
            "MPNN update received: score: {:.1}, target={} ({})",
            score,
            apply_target,
            entity_name.as_deref().unwrap_or("?"),
        );

        let name = format!("MPNN Design (score {:.1})", score);
        if let Some(mut protein) = returned.into_iter().find(|e| {
            e.molecule_type() == molex::MoleculeType::Protein
        }) {
            protein.set_id(store.mint_id(apply_target));
            store.set_name(apply_target, name);
            store.update_entity_and_publish(
                engine,
                apply_target,
                protein,
                Transition::collapse_expand(
                    std::time::Duration::from_millis(200),
                    std::time::Duration::from_millis(300),
                ),
            );
        }
        cache_per_residue_scores(engine, apply_target, &per_residue_scores);

        if let Some(ref mut orch) = orchestrator {
            orch.unlock(mpnn_eid);
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

    // Match returned entities back to local entity ids by position. The
    // session's structure ids are foldit-rs entity ids minted in the
    // order combined_assembly_for_backend stashed them, so structure
    // order maps 1:1 to local id order.
    let entity_ids: Option<Vec<u32>> = orchestrator
        .as_ref()
        .and_then(|o| o.session())
        .map(|state| state.structures.iter().map(|s| s.0 as u32).collect());

    if let Some(entity_ids) = entity_ids {
        log::info!(
            "Applying full session update ({} structures)",
            entity_ids.len()
        );
        for &eid in &entity_ids {
            cache_per_residue_scores(engine, eid, &per_residue_scores);
        }
        apply_combined_update(engine, store, returned, &entity_ids, Transition::smooth());
    } else {
        let focus = engine.focus();
        if let Some(id) = SharedState::operation_target(&focus)
            .or(store.loaded_entity())
        {
            let name = message.unwrap_or_else(|| {
                format!("Cycle {} (score {:.1})", cycle, score)
            });
            if let Some(mut protein) = returned.into_iter().find(|e| {
                e.molecule_type() == molex::MoleculeType::Protein
            }) {
                protein.set_id(store.mint_id(id));
                if !converged {
                    store.set_name(id, name);
                }
                store.update_entity_and_publish(engine, id, protein, Transition::smooth());
            }
            cache_per_residue_scores(engine, id, &per_residue_scores);
        }
    }
}

fn handle_ml_intermediate(
    engine: &mut VisoEngine,
    store: &mut EntityStore,
    ui_dirty: &mut DirtyFlags,
    pending_prediction_reference: Option<&[Vec3]>,
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
            engine, store,
            pending_prediction_reference,
            coords_bytes, backbone_positions, step, total_steps,
        );
    }
}

fn handle_ml_predict_result(
    engine: &mut VisoEngine,
    store: &mut EntityStore,
    orchestrator: &mut Option<Orchestrator>,
    ui_dirty: &mut DirtyFlags,
    pending_prediction_reference: &mut Option<Vec<Vec3>>,
    coords_bytes: Vec<u8>,
    confidence: f32,
) {
    // Consume the submission-time CA snapshot for the final result so
    // it doesn't leak across predictions. If the snapshot exists it
    // takes priority over the entity's stored `reference_ca`.
    let submitted_reference = pending_prediction_reference.take();
    let is_assem = coords_bytes.len() >= 8
        && &coords_bytes[0..8] == molex::ops::codec::ASSEMBLY_MAGIC;
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

    if let Some(anim_id) = store.animation() {
        store.remove_animation();
        store.publish_to(engine);
        let _ = anim_id;
    }

    if let Some(orig_id) = store.loaded_entity() {
        // Prefer the submission-time CA snapshot (handles multi-entity
        // predictions). Fall back to the entity's stored reference_ca
        // (for predictions where no submission snapshot was recorded).
        let reference_ca: Option<Vec<Vec3>> = submitted_reference
            .or_else(|| store.reference_ca(orig_id).map(<[Vec3]>::to_vec));
        let aligned_coords_bytes = match reference_ca {
            Some(original_ca) => match align_coords_bytes(&coords_bytes, &original_ca) {
                Ok(aligned) => {
                    log::info!(
                        "RF3 final: aligned ({} bytes, {} reference CAs)",
                        aligned.len(),
                        original_ca.len(),
                    );
                    aligned
                }
                Err(e) => {
                    log::warn!(
                        "RF3 final: align failed ({}); using prediction coords as-is — \
                         entity will land at the model's coordinate frame",
                        e,
                    );
                    coords_bytes.clone()
                }
            },
            None => {
                log::warn!(
                    "RF3 final: no submission snapshot or reference_ca for entity {}; using \
                     prediction coords as-is — entity will land at the model's coordinate frame",
                    orig_id,
                );
                coords_bytes.clone()
            }
        };

        match ml_entities_from_bytes(&aligned_coords_bytes) {
            Ok(entities) => {
                let total_atoms: usize = entities.iter().map(|e| e.atom_count()).sum();
                log::info!(
                    "Parsed prediction: {} entities, {} total atoms",
                    entities.len(), total_atoms,
                );
                if total_atoms == 0 {
                    log::error!("Prediction entities have 0 atoms — not updating model");
                    engine.set_entity_visible(orig_id, true);
                } else {
                    let name = format!("RF3 ({:.0}%)", confidence * 100.0);
                    if let Some(mut protein) = entities.into_iter().find(|e| {
                        e.molecule_type() == molex::MoleculeType::Protein
                    }) {
                        protein.set_id(store.mint_id(orig_id));
                        store.set_name(orig_id, name);
                        engine.set_entity_visible(orig_id, true);
                        store.update_entity_and_publish(engine, orig_id, protein, Transition::smooth());
                    }
                }
            }
            Err(e) => {
                log::error!("Failed to parse prediction: {}", e);
                engine.set_entity_visible(orig_id, true);
            }
        }
        if let Some(ref mut orch) = orchestrator {
            orch.unlock(EntityId(u64::from(orig_id)));
        }
    }

    *ui_dirty |= DirtyFlags::LOADING | DirtyFlags::ACTIONS;
}

fn handle_sequence_design_result(
    _engine: &mut VisoEngine,
    store: &mut EntityStore,
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
    let target_entity = orchestrator.as_ref().and_then(|orch| {
        orch.locked_entities().into_iter().find(|eid| {
            orch.get_op_type(*eid) == Some(OpType::MLSequenceDesign)
        })
    }).map(|eid| eid.0 as u32);

    // Store all designed sequences associated with the target
    if let Some(target_id) = target_entity {
        store.add_designed_sequences(target_id, sequences.clone(), scores.clone());
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

        // Get the locked target entity and wrap it in a single-entity
        // Assembly for Rosetta packing.
        let assembly = target_entity.and_then(|id| {
            store.get(id).and_then(|te| {
                if te.entity.molecule_type() == molex::MoleculeType::Protein {
                    Some(Assembly::new(vec![te.entity.clone()]))
                } else {
                    None
                }
            })
        });

        if let Some(assembly) = assembly {
            if let Some(ref mut orch) = orchestrator {
                orch.stop_rosetta();
                orch.clear_session();

                log::info!("Applying designed sequence via Rosetta and packing sidechains...");
                if let Err(e) = orch.apply_sequence_and_pack(assembly, best_seq.clone()) {
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
        if let Some(id) = target_entity {
            if let Some(ref mut orch) = orchestrator {
                orch.unlock(EntityId(u64::from(id)));
            }
        }
    }

    *ui_dirty |= DirtyFlags::LOADING | DirtyFlags::ACTIONS;
}

fn handle_structure_design_result(
    engine: &mut VisoEngine,
    store: &mut EntityStore,
    orchestrator: &mut Option<Orchestrator>,
    ui_dirty: &mut DirtyFlags,
    entities: Vec<molex::MoleculeEntity>,
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

    let had_animation = store.animation().is_some();
    if let Some(anim_id) = store.animation() {
        if let Some(mut protein) = entities.into_iter().find(|e| {
            e.molecule_type() == molex::MoleculeType::Protein
        }) {
            protein.set_id(store.mint_id(anim_id));
            let name = format!("RFD3 Design ({:.0}%)", confidence * 100.0);
            store.set_name(anim_id, name);
            log::info!("Updated animation structure {} to final result", anim_id);
            store.update_entity_and_publish(engine, anim_id, protein, Transition::smooth());
        }
        store.promote_animation_to_design(anim_id, confidence);
    } else {
        // Insert into store first, then push to viso
        let mut ids = Vec::new();
        for entity in entities {
            let id = store.insert(
                entity,
                format!("RFD3 Design ({:.0}%)", confidence * 100.0),
                EntityOrigin::StructureDesign { source: store.loaded_entity().unwrap_or(0), confidence },
                EntityRole { foldable: true, designable: true, ambient: false },
            );
            ids.push(id);
        }
        let design_entity_id = ids.first().copied().unwrap_or(0);
        log::info!("Added designed structure {} to scene", design_entity_id);

        // Push the full Assembly to viso
        store.publish_to(engine);

        if let Some(loaded) = store.loaded_entity() {
            store.register_animation(design_entity_id, loaded);
            store.promote_animation_to_design(design_entity_id, confidence);
        }
    }

    // Unlock the original structure
    if let Some(orig_id) = store.loaded_entity() {
        if let Some(ref mut orch) = orchestrator {
            orch.unlock(EntityId(u64::from(orig_id)));
        }
    }

    // Each update_entity_and_publish call passes its own per-call
    // transition, so we deliberately don't set a persistent behavior
    // override — those win over per-call transitions and would
    // clobber MPNN's collapse_expand and other op-specific animations.
    engine.sync_scene_to_renderers(HashMap::new());

    if !had_animation {
        engine.fit_camera_to_focus();
    }

    *ui_dirty |= DirtyFlags::LOADING | DirtyFlags::ACTIONS;
}

fn update_animation_structure_from_backend(
    engine: &mut VisoEngine,
    store: &mut EntityStore,
    pending_prediction_reference: Option<&[Vec3]>,
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

    // SimpleFold / RF3: update full structure including sidechains, with
    // scale correction. Aligns the predicted structure to the CAs of
    // the entities the user actually submitted to RF3 (captured at
    // submission time) so the predicted structure stays anchored to
    // where the user expects it. Falls back to the loaded entity's
    // stored `reference_ca` if no submission snapshot is available.
    if let Some(ref coords_bytes) = coords_bytes {
        if let Some(orig_id) = store.loaded_entity() {
            match ml_entities_from_bytes(coords_bytes) {
                Ok(mut entities) => {
                    let predicted_ca: Vec<Vec3> = molex::ops::codec::ca_positions(&entities);
                    let original_ca: Option<Vec<Vec3>> = pending_prediction_reference
                        .map(<[Vec3]>::to_vec)
                        .or_else(|| store.reference_ca(orig_id).map(<[Vec3]>::to_vec));
                    let aligned = match original_ca.as_deref() {
                        Some(orig) if orig.len() == predicted_ca.len() => {
                            match kabsch_alignment_with_scale(orig, &predicted_ca) {
                                Some((rotation, translation, scale)) => {
                                    if !scale.is_finite() {
                                        log::warn!(
                                            "RF3 frame {}: Kabsch scale non-finite ({}); leaving \
                                             coords unaligned",
                                            step, scale,
                                        );
                                        false
                                    } else {
                                        for entity in &mut entities {
                                            let mut coords = entity.to_coords();
                                            for atom in &mut coords.atoms {
                                                let pos = Vec3::new(atom.x, atom.y, atom.z);
                                                let t = rotation * (pos * scale) + translation;
                                                atom.x = t.x;
                                                atom.y = t.y;
                                                atom.z = t.z;
                                            }
                                            let mut v = vec![entity.clone()];
                                            molex::ops::codec::update_protein_entities(
                                                &mut v,
                                                &coords,
                                            );
                                            *entity = v.into_iter().next().unwrap();
                                        }
                                        log::debug!(
                                            "RF3 frame {}: aligned (scale {:.3}, {} CAs)",
                                            step, scale, predicted_ca.len(),
                                        );
                                        true
                                    }
                                }
                                None => {
                                    log::warn!(
                                        "RF3 frame {}: Kabsch returned None ({} CAs); skipping \
                                         frame to avoid teleporting the entity",
                                        step, predicted_ca.len(),
                                    );
                                    false
                                }
                            }
                        }
                        Some(orig) => {
                            log::warn!(
                                "RF3 frame {}: CA count mismatch (reference={}, predicted={}); \
                                 skipping frame to avoid teleporting the entity",
                                step, orig.len(), predicted_ca.len(),
                            );
                            false
                        }
                        None => {
                            log::warn!(
                                "RF3 frame {}: no reference_ca for entity {}; skipping frame to \
                                 avoid teleporting the entity",
                                step, orig_id,
                            );
                            false
                        }
                    };
                    if !aligned {
                        return;
                    }

                    let name = format!("Predicting... ({}/{})", step, total_steps);
                    if let Some(mut protein) = entities.into_iter().find(|e| {
                        e.molecule_type() == molex::MoleculeType::Protein
                    }) {
                        protein.set_id(store.mint_id(orig_id));
                        store.set_name(orig_id, name);
                        log::info!("Updated RF3 frame {}/{}", step, total_steps);
                        store.update_entity_and_publish(engine, orig_id, protein, Transition::smooth());
                    }
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
        let num_residues = backbone_positions.len() / 4;
        if num_residues == 0 {
            log::warn!("Empty backbone positions, skipping update");
            return;
        }
        log::info!(
            "RFD3 intermediate: {} backbone positions -> {} residues",
            backbone_positions.len(),
            num_residues,
        );

        if let Some(anim_id) = store.animation() {
            let id = store.mint_id(anim_id);
            if let Some(protein) =
                backbone_positions_to_protein_entity(&backbone_positions, id)
            {
                store.set_name(anim_id, format!("Designing... ({}/{})", step, total_steps));
                log::info!("Updated animation frame {}/{}", step, total_steps);
                store.update_entity_and_publish(engine, anim_id, protein, Transition::smooth());
            }
        } else {
            // Use a placeholder entity id; `EntityStore::insert` overrides
            // it via the allocator and returns the assigned id.
            let placeholder = store.mint_id(0);
            if let Some(protein) =
                backbone_positions_to_protein_entity(&backbone_positions, placeholder)
            {
                let inserted_id = store.insert(
                    protein,
                    format!("Designing... ({}/{})", step, total_steps),
                    EntityOrigin::Animation { source: store.loaded_entity().unwrap_or(0) },
                    EntityRole { foldable: false, designable: false, ambient: false },
                );
                let source = store.loaded_entity().unwrap_or(0);
                store.register_animation(inserted_id, source);
                log::info!("Created animation structure {}", inserted_id);

                store.publish_to(engine);
                engine.fit_camera_to_focus();
            }
        }
        return;
    }

    log::warn!("No coordinates in update, skipping");
}

/// Cache per-residue scores on an entity.
fn cache_per_residue_scores(
    engine: &mut VisoEngine,
    entity_id: u32,
    per_residue_scores: &Option<Vec<f64>>,
) {
    if let Some(scores) = per_residue_scores {
        engine.set_per_residue_scores(entity_id, Some(scores.clone()));
    }
}

// ── Backend coord helpers ──

/// Apply Rosetta-returned entities back to the local store, matching
/// by position against `entity_ids` from the original send.
fn apply_combined_update(
    engine: &mut VisoEngine,
    store: &mut EntityStore,
    returned: Vec<MoleculeEntity>,
    entity_ids: &[u32],
    transition: Transition,
) {
    if returned.len() != entity_ids.len() {
        log::warn!(
            "apply_combined_update: returned {} entities but expected {}; \
             positional matching will skip the mismatched tail",
            returned.len(),
            entity_ids.len(),
        );
    }
    for (entity_id, mut entity) in entity_ids.iter().copied().zip(returned.into_iter()) {
        entity.set_id(store.mint_id(entity_id));
        store.update_entity_and_publish(engine, entity_id, entity, transition.clone());
    }
}

// ── Helpers ──

/// Deserialize ML-worker bytes (ASSEM01 or COORDS) into entities.
/// Used only on cross-process paths where bytes are the wire format.
fn ml_entities_from_bytes(
    bytes: &[u8],
) -> Result<Vec<molex::MoleculeEntity>, String> {
    if bytes.len() >= 8 && &bytes[0..8] == molex::ops::codec::ASSEMBLY_MAGIC {
        molex::ops::codec::deserialize_assembly(bytes)
            .map(|a| a.entities().to_vec())
            .map_err(|e| format!("Failed to parse ASSEM01: {:?}", e))
    } else {
        let coords = deserialize_coords(bytes)
            .map_err(|e| format!("Failed to parse COORDS: {:?}", e))?;
        Ok(split_into_entities(&coords)
            .into_iter()
            .filter(|e| e.molecule_type() == molex::MoleculeType::Protein)
            .collect())
    }
}

/// Build a `ProteinEntity` directly from RFD3-streamed backbone
/// positions (flat `Vec<Vec3>` of N, CA, C, O per residue).
fn backbone_positions_to_protein_entity(
    positions: &[Vec3],
    id: molex::entity::molecule::id::EntityId,
) -> Option<MoleculeEntity> {
    use molex::{Atom, Element};
    use molex::entity::molecule::polymer::Residue;
    use molex::entity::molecule::protein::ProteinEntity;

    let num_residues = positions.len() / 4;
    if num_residues == 0 {
        return None;
    }

    let names = [*b"N   ", *b"CA  ", *b"C   ", *b"O   "];
    let elements = [Element::N, Element::C, Element::C, Element::O];

    let mut atoms = Vec::with_capacity(num_residues * 4);
    let mut residues = Vec::with_capacity(num_residues);

    for res_idx in 0..num_residues {
        let base = res_idx * 4;
        let atom_start = atoms.len();
        for j in 0..4 {
            atoms.push(Atom {
                position: positions[base + j],
                occupancy: 1.0,
                b_factor: 0.0,
                element: elements[j],
                name: names[j],
            });
        }
        residues.push(Residue {
            name: *b"ALA",
            number: (res_idx + 1) as i32,
            atom_range: atom_start..atom_start + 4,
        });
    }

    Some(MoleculeEntity::Protein(ProteinEntity::new_continuous(
        id,
        atoms,
        residues,
        b'A',
    )))
}

/// Collect entities for ML based on current focus.
/// Returns `(entity_id, Vec<MoleculeEntity>)` for locking + serialization.
///
/// - `Focus::Entity(eid)` → returns only that single entity.
/// - `Focus::Session` → falls back to `fallback_entity`.
pub(crate) fn collect_ml_entities(
    store: &EntityStore,
    focus: &Focus,
    fallback_entity: Option<u32>,
) -> Option<(u32, Vec<MoleculeEntity>)> {
    store.collect_ml_entities(focus, fallback_entity)
}

/// Serialize a slice of entities to ASSEM01 bytes.
pub(crate) fn entities_to_assembly_bytes(entities: &[MoleculeEntity]) -> Option<Vec<u8>> {
    molex::ops::codec::assembly_bytes(entities).ok()
}
