//! App-free crystallography domain layer (parallels [`molex::xtal`]): turn
//! structure-factor cif text and a model into an experimental-weighted density.
//! Every function here takes what it needs as parameters and touches no `App`
//! state; the App/session/engine lane-feed lives in `crate::app::density`.

/// Build [`molex::xtal::ExperimentalData`] from structure-factor cif text and
/// a resolved space group, seeding the R-free flag set deterministically from
/// `name`. Shared by the `--with-density` load and the `[puzzle.reflns]` path
/// so the from-sf-cif / free-flag-seed core lives in one place. `None` when the
/// reflections are unusable.
pub fn build_experimental_data(
    sf_text: &str,
    sg_number: u16,
    name: &str,
) -> Option<std::sync::Arc<molex::xtal::ExperimentalData>> {
    let seed = molex::xtal::deterministic_free_flag_seed(name);
    molex::ExperimentalData::from_sf_cif_with_spacegroup(sf_text, sg_number, 0.05, seed)
        .map(std::sync::Arc::new)
}

/// Read a coordinate cif at `path` and map its Hermann-Mauguin space-group
/// name to an International Tables number over the xtal module's supported
/// groups. Reads `_symmetry.space_group_name_H-M`, falling back to
/// `_space_group.name_H-M_full`. Returns `None` if the file is unreadable, the
/// tag is absent, or the group is unsupported.
pub fn read_space_group_number(path: &std::path::Path) -> Option<u16> {
    let text = std::fs::read_to_string(path).ok()?;
    let doc = molex::adapters::cif::parse(&text).ok()?;
    let block = doc.blocks.first()?;
    let hm = block
        .get("_symmetry.space_group_name_H-M")
        .and_then(molex::adapters::cif::Value::as_str)
        .or_else(|| {
            block
                .get("_space_group.name_H-M_full")
                .and_then(molex::adapters::cif::Value::as_str)
        })?;
    molex::xtal::space_group_number_from_name(hm)
}

/// Compute the experimental-weighted density from `entities` and `data`,
/// returning both lanes' payloads: the scoring-lane mrc
/// [`crate::puzzle_load::DensityAsset`] (the full-cell map) and the render-lane
/// cropped [`molex::entity::surface::Density`] (a sub-block around the model so
/// symmetry-mate blobs drop out). `device` routes the FFT to the shared GPU
/// device when present, else the CPU. `map_name` names the mrc asset. `None`
/// when the density computation fails, so the caller keeps its prior map.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    reason = "map resolution narrows f64 Å -> f32 for the rosetta density asset"
)]
pub fn compute_density(
    entities: &[molex::MoleculeEntity],
    data: &molex::xtal::ExperimentalData,
    device: Option<&molex::xtal::WgpuDevice>,
    map_name: &str,
) -> Option<(crate::puzzle_load::DensityAsset, molex::entity::surface::Density)> {
    let table = molex::adapters::table::AtomTable::from_entities(entities);
    let grid = device.map_or_else(
        || {
            log::info!("[App] density on CPU (no shared device)");
            molex::xtal::density_from_atom_table(data, &table)
        },
        |dev| {
            log::info!("[App] density on GPU (shared device)");
            molex::xtal::density_from_atom_table_gpu(data, &table, dev)
        },
    )?;
    let density = molex::xtal::density_from_grid(&grid, &data.unit_cell, data.space_group_number());

    // Scoring lane: encode the map to mrc bytes. Takes `density` by reference
    // so it survives for the render-lane crop below.
    let asset = crate::puzzle_load::DensityAsset {
        name: map_name.to_owned(),
        bytes: molex::adapters::mrc::density_to_mrc_bytes(&density),
        resolution: data.d_min() as f32,
        grid_spacing: None,
    };

    // Render lane: crop the whole-cell map to a sub-block around the model so
    // symmetry-mate blobs drop out. The scoring lane keeps the full map above;
    // rosetta re-crops internally.
    let positions: Vec<[f32; 3]> = table.position.iter().map(glam::Vec3::to_array).collect();
    let cropped = density.crop_to_points(&positions, 3);
    Some((asset, cropped))
}
