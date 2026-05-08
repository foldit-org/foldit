//! Backend update processing: applies backend results to the render engine.
//!
//! Free functions extracted from ActionRouter to keep the router focused on
//! translating user input into orchestrator/backend commands.

use molex::{Assembly, MoleculeEntity};
use molex::entity::molecule::id::EntityId;
use molex::ops::transform::{align_to_reference, kabsch_alignment_with_scale};
use foldit_gui::DirtyFlags;
use foldit_runner::orchestrator::{BackendUpdate, EntityId as RunnerEntityId, OpType};
use foldit_runner::Orchestrator;
use foldit::entity_store::{EntityStore, EntityOrigin, EntityRole};
use foldit::focus;
use foldit::history::CheckpointKind;
use glam::Vec3;
use std::collections::HashMap;
use viso::{Focus, Transition, VisoEngine};

/// Apply a single backend update. Called by the frame loop after draining
/// triple buffers via SharedState.
pub(crate) fn apply_backend_update(
    engine: &mut VisoEngine,
    store: &mut EntityStore,
    orchestrator: &mut Option<Orchestrator>,
    ui_dirty: &mut DirtyFlags,
    pending_prediction_reference: &mut Option<Vec<Vec3>>,
    pending_preview_id: &mut Option<EntityId>,
    scoring_mode: foldit_gui::ScoringMode,
    update: BackendUpdate,
) {
    match update {
        BackendUpdate::RosettaCoords {
            assembly,
            score,
            game_score,
            cycle,
            message,
            converged,
            per_residue_scores,
            per_residue_game_scores,
        } => {
            handle_rosetta_coords(
                engine, store, orchestrator, ui_dirty,
                scoring_mode,
                assembly, score, game_score, cycle, message, converged,
                per_residue_scores, per_residue_game_scores,
            );
        }
        BackendUpdate::MLIntermediate {
            assembly,
            step,
            total_steps,
            confidence,
        } => {
            handle_ml_intermediate(
                engine, store, ui_dirty,
                pending_prediction_reference.as_deref(),
                pending_preview_id,
                assembly, step, total_steps, confidence,
            );
        }
        BackendUpdate::MLPredictResult {
            assembly,
            confidence,
        } => {
            handle_ml_predict_result(
                engine, store, orchestrator, ui_dirty,
                pending_prediction_reference,
                pending_preview_id,
                assembly, confidence,
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
                pending_preview_id,
                assembly
                    .entities()
                    .iter()
                    .map(|e| MoleculeEntity::clone(e))
                    .collect(),
                confidence,
            );
        }
        BackendUpdate::Error { message } => {
            log::error!("Backend error: {}", message);
            // Restore visibility of any locked entities and tear down any
            // active preview so models don't "disappear" after a failed
            // ML operation.
            if let Some(preview_id) = pending_preview_id.take() {
                store.remove_preview(preview_id);
                store.publish_to(engine);
            }
            if let Some(ref mut orch) = orchestrator {
                for eid in orch.locked_entities() {
                    let id = eid.0 as u32;
                    engine.set_entity_visible(id, true);
                    orch.unlock(eid);
                }
            }
            // If an action was in flight when the backend errored,
            // discard it: the tentative coords are stale.
            if store.has_ongoing_action() {
                if let Err(e) = store.abort_action() {
                    log::warn!("Backend error: abort_action refused: {e}");
                }
            }
            *ui_dirty |= DirtyFlags::LOADING | DirtyFlags::ACTIONS;
        }
    }
}

fn handle_rosetta_coords(
    engine: &mut VisoEngine,
    store: &mut EntityStore,
    orchestrator: &mut Option<Orchestrator>,
    ui_dirty: &mut DirtyFlags,
    scoring_mode: foldit_gui::ScoringMode,
    assembly: Assembly,
    score: f64,
    game_score: f64,
    cycle: u32,
    _message: Option<String>,
    converged: bool,
    per_residue_scores: Option<Vec<f64>>,
    per_residue_game_scores: Option<Vec<f64>>,
) {
    // G7: write path stamps BOTH raw + game scores onto the checkpoint;
    // it never reads `scoring_mode`. The GUI projection picks one at
    // read time. `scoring_mode` is still threaded through here for the
    // per-residue-score cache (viso displays one mode at a time).
    *ui_dirty |= DirtyFlags::SCORE | DirtyFlags::ACTIONS;

    let returned: Vec<MoleculeEntity> = assembly
        .entities()
        .iter()
        .map(|e| MoleculeEntity::clone(e))
        .collect();

    // Check if any entity is locked for MLSequenceDesign (MPNN result
    // packing path).
    let mpnn_entity = orchestrator.as_ref().and_then(|orch| {
        orch.locked_entities().into_iter().find(|eid| {
            orch.get_op_type(*eid) == Some(OpType::MLSequenceDesign)
        })
    });

    if let Some(mpnn_eid) = mpnn_entity {
        let apply_target_raw = mpnn_eid.0 as u32;
        let apply_target = store.mint_id(apply_target_raw);
        let entity_name = store.metadata(apply_target).map(|m| m.name.clone());
        log::info!(
            "MPNN update received: score: {:.1}, target={} ({})",
            score,
            apply_target_raw,
            entity_name.as_deref().unwrap_or("?"),
        );

        let name = format!("MPNN Design (score {:.1})", score);
        if let Some(mut protein) = returned.into_iter().find(|e| {
            e.molecule_type() == molex::MoleculeType::Protein
        }) {
            protein.set_id(apply_target);
            store.set_entity_name(apply_target, name.clone());
            // Stash the produced sequence at most as a string-derived
            // marker for now; the actual MPNN sequence wiring is
            // handled by `add_designed_sequences` in the design-result
            // arm.
            let kind = CheckpointKind::Mpnn {
                entity: apply_target,
                sequence: String::new(),
            };
            if let Err(e) = store.record_entity_update(
                kind,
                apply_target,
                protein,
                name,
                Some(score),
                Some(game_score),
            ) {
                log::warn!("MPNN record_entity_update refused: {e}");
            } else {
                publish_with_transition(
                    engine,
                    store,
                    apply_target,
                    Transition::collapse_expand(
                        std::time::Duration::from_millis(200),
                        std::time::Duration::from_millis(300),
                    ),
                );
            }
        }
        cache_per_residue_scores(
            engine,
            apply_target_raw,
            scoring_mode,
            &per_residue_scores,
            &per_residue_game_scores,
        );

        if let Some(ref mut orch) = orchestrator {
            orch.unlock(mpnn_eid);
        }

        return;
    }

    // Normal wiggle/shake update — flows into the in-flight action's
    // tentative checkpoint via `action_update` (no per-cycle history
    // push). `begin_action` at toggle-on installed the tentative; the
    // tentative becomes permanent at `commit_action` on toggle-off.
    log::info!(
        "Rosetta update: cycle {}, score {:.2}, converged: {}",
        cycle,
        score,
        converged
    );

    let entity_ids: Option<Vec<EntityId>> = orchestrator
        .as_ref()
        .and_then(|o| o.session())
        .map(|state| {
            state
                .structures
                .iter()
                .map(|s| store.mint_id(s.0 as u32))
                .collect()
        });

    if let Some(entity_ids) = entity_ids {
        log::info!(
            "Applying full session update ({} structures)",
            entity_ids.len()
        );
        for eid in &entity_ids {
            cache_per_residue_scores(
                engine,
                eid.raw(),
                scoring_mode,
                &per_residue_scores,
                &per_residue_game_scores,
            );
        }
        apply_ongoing_update(
            engine,
            store,
            returned,
            &entity_ids,
            Transition::smooth(),
            Some(score),
            Some(game_score),
        );
    } else {
        let focus = engine.focus();
        let target = focus::operation_target(&focus)
            .map(|raw| store.mint_id(raw))
            .or_else(|| store.loaded_entity());
        if let Some(id) = target {
            if let Some(mut protein) = returned.into_iter().find(|e| {
                e.molecule_type() == molex::MoleculeType::Protein
            }) {
                protein.set_id(id);
                apply_action_update_one(
                    engine,
                    store,
                    id,
                    protein,
                    Transition::smooth(),
                    Some(score),
                    Some(game_score),
                );
            }
            cache_per_residue_scores(
                engine,
                id.raw(),
                scoring_mode,
                &per_residue_scores,
                &per_residue_game_scores,
            );
        }
    }
}

fn handle_ml_intermediate(
    engine: &mut VisoEngine,
    store: &mut EntityStore,
    ui_dirty: &mut DirtyFlags,
    pending_prediction_reference: Option<&[Vec3]>,
    pending_preview_id: &mut Option<EntityId>,
    assembly: Option<Assembly>,
    step: u32,
    total_steps: u32,
    confidence: f32,
) {
    log::info!(
        "ML update: step {}/{}, confidence {:.2}, has_assembly={}",
        step,
        total_steps,
        confidence,
        assembly.is_some(),
    );

    *ui_dirty |= DirtyFlags::LOADING;

    if assembly.is_some() {
        update_animation_structure_from_backend(
            engine, store,
            pending_prediction_reference,
            pending_preview_id,
            assembly, step, total_steps,
        );
    }
}

fn handle_ml_predict_result(
    engine: &mut VisoEngine,
    store: &mut EntityStore,
    orchestrator: &mut Option<Orchestrator>,
    ui_dirty: &mut DirtyFlags,
    pending_prediction_reference: &mut Option<Vec<Vec3>>,
    pending_preview_id: &mut Option<EntityId>,
    assembly: Assembly,
    confidence: f32,
) {
    // Consume the submission-time CA snapshot for the final result so
    // it doesn't leak across predictions. If the snapshot exists it
    // takes priority over the entity's stored `reference_ca`.
    let submitted_reference = pending_prediction_reference.take();
    let total_atoms: usize = assembly.entities().iter().map(|e| e.atom_count()).sum();
    log::info!(
        "Prediction complete! Confidence: {:.2}, {} entities, {} atoms",
        confidence, assembly.entities().len(), total_atoms,
    );

    // Tear down the streaming preview mirror regardless of outcome — its
    // lifetime ends with the final result.
    let preview_id = pending_preview_id.take();
    if let Some(pid) = preview_id {
        store.remove_preview(pid);
    }

    if total_atoms == 0 {
        log::error!("Prediction returned empty assembly — not updating model");
        if let Some(ref mut orch) = orchestrator {
            for eid in orch.locked_entities() {
                let id = eid.0 as u32;
                engine.set_entity_visible(id, true);
                orch.unlock(eid);
            }
        }
        if preview_id.is_some() {
            store.publish_to(engine);
        }
        *ui_dirty |= DirtyFlags::LOADING | DirtyFlags::ACTIONS;
        return;
    }

    if let Some(orig_id) = store.loaded_entity() {
        // Prefer the submission-time CA snapshot (handles multi-entity
        // predictions). Fall back to the entity's stored reference_ca
        // (for predictions where no submission snapshot was recorded).
        let reference_ca: Option<Vec<Vec3>> = submitted_reference
            .or_else(|| store.reference_ca(orig_id).map(<[Vec3]>::to_vec));

        let mut entities: Vec<MoleculeEntity> = assembly
            .entities()
            .iter()
            .map(|e| MoleculeEntity::clone(e))
            .collect();

        match reference_ca {
            Some(original_ca) => match align_to_reference(&mut entities, &original_ca) {
                Ok(()) => log::info!(
                    "RF3 final: aligned ({} entities, {} reference CAs)",
                    entities.len(),
                    original_ca.len(),
                ),
                Err(e) => log::warn!(
                    "RF3 final: align failed ({:?}); using prediction coords as-is — \
                     entity will land at the model's coordinate frame",
                    e,
                ),
            },
            None => log::warn!(
                "RF3 final: no submission snapshot or reference_ca for entity {}; using \
                 prediction coords as-is — entity will land at the model's coordinate frame",
                orig_id.raw(),
            ),
        }

        let name = format!("RF3 ({:.0}%)", confidence * 100.0);
        if let Some(mut protein) = entities.into_iter().find(|e| {
            e.molecule_type() == molex::MoleculeType::Protein
        }) {
            protein.set_id(orig_id);
            store.set_entity_name(orig_id, name.clone());
            engine.set_entity_visible(orig_id.raw(), true);
            if let Err(e) = store.record_entity_update(
                CheckpointKind::Rfd3 {
                    entity: orig_id,
                    confidence,
                },
                orig_id,
                protein,
                name,
                None,
                None,
            ) {
                log::warn!("RF3 record_entity_update refused: {e}");
            } else {
                publish_with_transition(engine, store, orig_id, Transition::smooth());
            }
        }
        if let Some(ref mut orch) = orchestrator {
            orch.unlock(RunnerEntityId(u64::from(orig_id.raw())));
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
    let target_raw = orchestrator.as_ref().and_then(|orch| {
        orch.locked_entities().into_iter().find(|eid| {
            orch.get_op_type(*eid) == Some(OpType::MLSequenceDesign)
        })
    }).map(|eid| eid.0 as u32);
    let target_entity: Option<EntityId> = target_raw.map(|raw| store.mint_id(raw));

    // Store all designed sequences associated with the target
    if let Some(target_id) = target_entity {
        let designed: Vec<foldit::entity_store::DesignedSequence> = sequences
            .iter()
            .zip(scores.iter())
            .map(|(seq, score)| foldit::entity_store::DesignedSequence {
                sequence: seq.clone(),
                score: *score,
                designed_for: target_id,
            })
            .collect();
        store.add_designed_sequences(target_id, designed);
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
            let entity = store.entity(id)?;
            if entity.molecule_type() == molex::MoleculeType::Protein {
                Some(Assembly::new(vec![entity.clone()]))
            } else {
                None
            }
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
                orch.unlock(RunnerEntityId(u64::from(id.raw())));
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
    pending_preview_id: &mut Option<EntityId>,
    entities: Vec<molex::MoleculeEntity>,
    confidence: f32,
) {
    log::info!(
        "Structure design complete! {} entities, confidence: {:.2}",
        entities.len(),
        confidence
    );

    // Invalidate Rosetta session — topology has changed.
    if let Some(ref mut orch) = orchestrator {
        orch.clear_session();
    }

    let preview_id = pending_preview_id.take();
    let had_preview = preview_id.is_some();
    let source = store.loaded_entity();
    let final_role = EntityRole { foldable: true, designable: true, ambient: false };
    let final_name = format!("RFD3 Design ({:.0}%)", confidence * 100.0);

    if let Some(pid) = preview_id {
        // Apply final body to the preview, then promote — promote_preview
        // moves the entity from `transient` into history via the same
        // recorded path as the loaded-puzzle promotion.
        if let Some(mut protein) = entities.into_iter().find(|e| {
            e.molecule_type() == molex::MoleculeType::Protein
        }) {
            protein.set_id(pid);
            store.update_preview(pid, protein);
        }
        let new_origin = source.map(|src| EntityOrigin::StructureDesign {
            source: src,
            confidence,
        });
        if let Err(e) = store.promote_preview(
            pid,
            CheckpointKind::Rfd3 {
                entity: pid,
                confidence,
            },
            new_origin,
            Some(final_role),
            Some(final_name.clone()),
            final_name,
        ) {
            log::warn!("RFD3 promote_preview refused: {e}");
        } else {
            engine.queue_entity_transition(pid.raw(), Transition::smooth());
            store.publish_to(engine);
            log::info!("Promoted RFD3 preview {} to design", pid.raw());
        }
    } else {
        // No streaming preview — design landed in one shot. Stage it
        // through the same preview→promote pipeline used by the puzzle
        // load path; section 5 may grow a direct multi-add primitive.
        let mut ids = Vec::new();
        for entity in entities {
            let new_origin = match source {
                Some(src) => EntityOrigin::StructureDesign { source: src, confidence },
                None => EntityOrigin::Loaded,
            };
            let pid = store.insert_preview(
                entity,
                final_name.clone(),
                new_origin.clone(),
                final_role.clone(),
            );
            if let Err(e) = store.promote_preview(
                pid,
                CheckpointKind::Rfd3 {
                    entity: pid,
                    confidence,
                },
                None,
                None,
                None,
                final_name.clone(),
            ) {
                log::warn!("RFD3 promote_preview refused: {e}");
            }
            ids.push(pid);
        }
        let design_entity_id = ids.first().copied().map(|id| id.raw()).unwrap_or(0);
        log::info!("Added designed structure {} to scene", design_entity_id);
        store.publish_to(engine);
    }

    // Unlock the original structure
    if let Some(orig_id) = store.loaded_entity() {
        if let Some(ref mut orch) = orchestrator {
            orch.unlock(RunnerEntityId(u64::from(orig_id.raw())));
        }
    }

    engine.sync_scene_to_renderers(HashMap::new());

    if !had_preview {
        engine.fit_camera_to_focus();
    }

    *ui_dirty |= DirtyFlags::LOADING | DirtyFlags::ACTIONS;
}

fn update_animation_structure_from_backend(
    engine: &mut VisoEngine,
    store: &mut EntityStore,
    pending_prediction_reference: Option<&[Vec3]>,
    pending_preview_id: &mut Option<EntityId>,
    assembly: Option<Assembly>,
    step: u32,
    total_steps: u32,
) {
    let Some(assembly) = assembly else {
        log::warn!("ML frame {}: no assembly, skipping", step);
        return;
    };

    let mut entities: Vec<MoleculeEntity> = assembly
        .entities()
        .iter()
        .map(|e| MoleculeEntity::clone(e))
        .collect();

    // Two cases:
    //   1. Predict (RF3 / SimpleFold): a preview was mirrored at op
    //      kickoff and a reference frame is available. Align the
    //      streamed assembly to the reference and write into the
    //      preview, leaving the loaded entity untouched so undo
    //      doesn't capture intermediate frames.
    //   2. Structure design (RFD3): no preview exists on the first
    //      frame. The runner emits a placeholder Assembly built from
    //      streamed backbone positions; we materialize a preview
    //      entity from it (StructureDesign origin) and reuse the same
    //      preview on subsequent frames.

    if let Some(preview_id) = *pending_preview_id {
        // Case (a) update OR case (b) subsequent frame.
        if let Some(orig_id) = store.loaded_entity() {
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
                                    for atom in entity.atom_set_mut() {
                                        atom.position =
                                            rotation * (atom.position * scale) + translation;
                                    }
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
                    // No reference CA: case (b) RFD3 subsequent frame —
                    // backbone is generated fresh, no alignment needed.
                    true
                }
            };
            if !aligned {
                return;
            }
        }

        let name = format!("Predicting... ({}/{})", step, total_steps);
        if let Some(mut protein) = entities.into_iter().find(|e| {
            e.molecule_type() == molex::MoleculeType::Protein
        }) {
            protein.set_id(preview_id);
            store.set_entity_name(preview_id, name);
            log::info!("Updated ML frame {}/{} (preview {})", step, total_steps, preview_id.raw());
            store.update_preview(preview_id, protein);
            engine.queue_entity_transition(preview_id.raw(), Transition::smooth());
            store.publish_to(engine);
        }
    } else {
        // Case (b) first frame — runner-supplied placeholder Assembly,
        // no kickoff-time mirror exists. Mint a real preview id from
        // the store and adopt the streamed protein.
        let placeholder = store.mint_id(0);
        if let Some(mut protein) = entities.into_iter().find(|e| {
            e.molecule_type() == molex::MoleculeType::Protein
        }) {
            protein.set_id(placeholder);
            let source = store.loaded_entity();
            let new_origin = match source {
                Some(src) => EntityOrigin::StructureDesign { source: src, confidence: 0.0 },
                None => EntityOrigin::Loaded,
            };
            let inserted_id = store.insert_preview(
                protein,
                format!("Designing... ({}/{})", step, total_steps),
                new_origin,
                EntityRole { foldable: false, designable: false, ambient: false },
            );
            *pending_preview_id = Some(inserted_id);
            log::info!("Created RFD3 preview entity {}", inserted_id.raw());

            store.publish_to(engine);
            engine.fit_camera_to_focus();
        }
    }
}

/// Cache per-residue scores on an entity, picking the rosetta or game array
/// based on the active scoring mode.
fn cache_per_residue_scores(
    engine: &mut VisoEngine,
    entity_id: u32,
    scoring_mode: foldit_gui::ScoringMode,
    per_residue_scores: &Option<Vec<f64>>,
    per_residue_game_scores: &Option<Vec<f64>>,
) {
    let scores = match scoring_mode {
        foldit_gui::ScoringMode::Game => per_residue_game_scores,
        foldit_gui::ScoringMode::Scientist => per_residue_scores,
    };
    if let Some(s) = scores {
        engine.set_per_residue_scores(entity_id, Some(s.clone()));
    }
}

// ── Backend coord helpers ──

/// Live multi-entity update from a Rosetta cycle. If an action is in
/// flight, mutates the tentative snapshot's payload for the action's
/// entity (the one named in the kind) and stamps both raw + game
/// scores onto the tentative checkpoint. If no action is in flight
/// (cycle-0 init score after session-create / `ScorePose`), the
/// returned coords are written *in place* onto the current head's
/// lane snapshots via `set_head_entity` and the score is stamped on
/// the head checkpoint — no per-cycle history push, but the
/// idealized coords are absorbed so the next wiggle / shake doesn't
/// re-clash from a stale pre-idealization pose.
fn apply_ongoing_update(
    engine: &mut VisoEngine,
    store: &mut EntityStore,
    returned: Vec<MoleculeEntity>,
    entity_ids: &[EntityId],
    transition: Transition,
    raw_score: Option<f64>,
    game_score: Option<f64>,
) {
    use foldit::history::OngoingState;

    let active_entity = match store.history().ongoing() {
        OngoingState::Active { entity, .. } => Some(*entity),
        OngoingState::Idle => None,
    };

    for (entity_id, mut entity) in entity_ids.iter().copied().zip(returned.into_iter()) {
        entity.set_id(entity_id);
        if Some(entity_id) == active_entity {
            // Active path: write the tentative snapshot's payload via
            // `action_update`; stamp both scores on the tentative
            // checkpoint.
            let _ = store.action_update(raw_score, game_score, None, |payload| {
                *payload = entity;
            });
        } else if active_entity.is_none() {
            // Idle path (cycle outside any action — session-init or a
            // post-head-move `ScorePose`). Absorb the returned payload
            // onto the current head's lane snapshot. Without this, the
            // next wiggle's `update_session_in_place` would overwrite
            // Rosetta's idealized pose with our pre-idealized coords
            // and fa_rep would blow up on the clash-heavy starting
            // state — the "huge energy jump" symptom.
            let _ = store.set_head_entity(entity_id, entity);
        }
        // Else: another entity in a multi-entity session, mid-action.
        // Bytes don't get written to history (only the active lane
        // moves under an action) but the transition is queued so
        // viso renders the publish below.
        engine.queue_entity_transition(entity_id.raw(), transition.clone());
    }

    // Idle-branch score stamp: outside any action, the converged score
    // for the current head belongs on the head checkpoint itself, not
    // on a fresh checkpoint per cycle.
    if active_entity.is_none() {
        store.set_head_scores(raw_score, game_score);
    }

    store.publish_to(engine);
}

/// Single-entity convenience for the `else` branch of
/// `handle_rosetta_coords` (focus-target update without a session).
fn apply_action_update_one(
    engine: &mut VisoEngine,
    store: &mut EntityStore,
    entity_id: EntityId,
    entity: MoleculeEntity,
    transition: Transition,
    raw_score: Option<f64>,
    game_score: Option<f64>,
) {
    apply_ongoing_update(
        engine,
        store,
        vec![entity],
        std::slice::from_ref(&entity_id),
        transition,
        raw_score,
        game_score,
    );
}

/// Queue a viso transition for `id` and publish the head assembly.
fn publish_with_transition(
    engine: &mut viso::VisoEngine,
    store: &mut EntityStore,
    id: EntityId,
    transition: Transition,
) {
    engine.queue_entity_transition(id.raw(), transition);
    store.publish_to(engine);
}

// ── Helpers ──

/// Collect entities for ML based on current focus.
/// Returns `(entity_id, Vec<MoleculeEntity>)` for locking + serialization.
///
/// - `Focus::Entity(eid)` → returns only that single entity.
/// - `Focus::Session` → falls back to `fallback_entity`.
pub(crate) fn collect_ml_entities(
    store: &EntityStore,
    focus: &Focus,
    fallback_entity: Option<EntityId>,
) -> Option<(EntityId, Vec<MoleculeEntity>)> {
    store.collect_ml_entities(focus, fallback_entity)
}
