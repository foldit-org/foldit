//! The `Session::load_puzzle` constructor.

use std::collections::BTreeMap;

use glam::Vec3;
use molex::entity::molecule::id::EntityId;

use crate::puzzle_load::PuzzleData;

use super::{Puzzle, Session};

/// Render-side leftovers the host applies after the document is built.
pub struct PuzzleRender {
    pub cam_eye: Vec3,
    pub cam_up: Vec3,
    pub view_preset: Option<String>,
    /// The SS override resolved onto the first ingested entity id.
    pub ss_override: Option<(u32, Vec<molex::SSType>)>,
}

impl Session {
    /// Reset the store, install the puzzle add-on from `data`, ingest its
    /// entities (resolving per-chain design gating), and return the render
    /// leftovers. The per-entity metadata name is the outgoing (pre-reset)
    /// title.
    pub(crate) fn load_puzzle(&mut self, id: u32, data: PuzzleData) -> PuzzleRender {
        let outgoing_title = self.title().to_owned();
        self.reset();

        let bubbles = if data.bubbles.is_empty() {
            None
        } else {
            Some(data.bubbles)
        };
        let current_bubble = bubbles.as_ref().map(|_| 0);
        // Structure factors are session state, not puzzle state: a free-form
        // `--with-density` load has no puzzle but still supplies them.
        let reflns = data.reflns;

        self.start(
            data.name,
            Some(Puzzle {
                id,
                start_energy: data.start_energy,
                completion_energy: data.completion_score,
                weight_patch: data.weights,
                filters: data.filters,
                bubbles,
                current_bubble,
                constraints: data.constraints,
                ligands: data.ligands,
                density: data.density,
                design_gating: None,
            }),
        );
        self.set_session_reflns(reflns);

        let chain_keys: Vec<Option<String>> = data
            .entities
            .iter()
            .map(|e| e.pdb_chain_id().map(str::to_owned))
            .collect();
        let ids = self.seed_history_with_entities(
            data.entities,
            std::path::PathBuf::new(),
            &outgoing_title,
        );

        let mut gating: BTreeMap<EntityId, crate::puzzle_setup::DesignMask> = BTreeMap::new();
        for (eid, chain_key) in ids.iter().zip(chain_keys) {
            if let Some(key) = chain_key {
                if let Some(mask) = data.design_masks.get(&key) {
                    gating.insert(*eid, mask.clone());
                }
            }
        }
        let design_gating = if data.design_masks.is_empty() {
            None
        } else {
            Some(gating)
        };
        self.set_puzzle_design_gating(design_gating);

        let cam = &data.camera;
        #[allow(clippy::cast_possible_truncation)]
        let cam_eye = Vec3::new(cam.eye[0] as f32, cam.eye[1] as f32, cam.eye[2] as f32);
        #[allow(clippy::cast_possible_truncation)]
        let cam_up = Vec3::new(cam.up[0] as f32, cam.up[1] as f32, cam.up[2] as f32);

        let ss_override = data
            .ss_override
            .and_then(|ss| ids.first().map(|&first_id| (first_id.raw(), ss)));

        PuzzleRender {
            cam_eye,
            cam_up,
            view_preset: data.view_preset,
            ss_override,
        }
    }
}
