//! Foldit design plugin: sequence-design ops.
//!
//! One op today, `mutate_residue`: change the identity of the single selected
//! residue. The 20-amino-acid picker is declared in `plugin.toml` under
//! `[[buttons.options]]`; this crate validates and applies the edit and hands
//! back its working assembly.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use foldit_plugin_sdk::proto::plugin as proto;
use foldit_plugin_sdk::{
    export_plugin, AssemblyPayload, DispatchContext, ParamValue, Plugin, PluginError, ResidueRef,
    Result,
};
use molex::chemistry::AminoAcid;
use molex::{Assembly, MoleculeEntity};

/// Op id. Must match `[[buttons]].op` in `plugin.toml`.
const OP_MUTATE: &str = "mutate_residue";

fn op_err(code: &str, message: impl Into<String>) -> PluginError {
    PluginError::Op {
        code: code.to_owned(),
        message: message.into(),
    }
}

/// `AminoAcid::code()` is statically uppercase ASCII, so each byte maps
/// straight to a char without a fallible UTF-8 decode.
fn code_string(aa: AminoAcid) -> String {
    aa.code().iter().map(|&b| b as char).collect()
}

#[derive(Default)]
struct DesignPlugin {
    /// The host serializes calls into one instance, but `Plugin` takes
    /// `&self`, so session state sits behind a mutex.
    sessions: Mutex<HashMap<u64, Assembly>>,
    next_session: AtomicU64,
}

impl DesignPlugin {
    fn sessions(&self) -> Result<MutexGuard<'_, HashMap<u64, Assembly>>> {
        self.sessions
            .lock()
            .map_err(|_| op_err("POISONED", "design session state was poisoned by a panic"))
    }
}

/// Re-checks the gate the manifest declares (`selection_spec = { min_residues
/// = 1, max_residues = 1 }`, `requires_designable = true`): a stale catalog or
/// a scripted dispatch can still arrive with a selection that fails it.
fn single_selected_residue(ctx: &DispatchContext) -> Result<ResidueRef> {
    let [residue] = ctx.selection.as_slice() else {
        return Err(op_err(
            "INVALID_SELECTION",
            format!(
                "mutate_residue needs exactly one selected residue, got {}",
                ctx.selection.len()
            ),
        ));
    };
    // An empty design mask means the puzzle gates no design at all, not that
    // nothing may be designed.
    if !ctx.designable.is_empty() && !ctx.designable.contains(residue) {
        return Err(op_err(
            "NOT_DESIGNABLE",
            "the selected residue is not designable in this puzzle",
        ));
    }
    Ok(*residue)
}

/// The three-letter code rides on the `aa` param.
fn requested_amino_acid(params: &HashMap<String, ParamValue>) -> Result<AminoAcid> {
    let Some(ParamValue::String(code)) = params.get("aa") else {
        return Err(op_err(
            "MISSING_PARAM",
            "mutate_residue requires the `aa` param (three-letter code)",
        ));
    };
    let bytes: [u8; 3] = code.as_bytes().try_into().map_err(|_| {
        op_err(
            "INVALID_PARAM",
            format!("`aa` must be a three-letter code, got {code:?}"),
        )
    })?;
    AminoAcid::from_code(bytes)
        .ok_or_else(|| op_err("INVALID_PARAM", format!("unknown amino-acid code {code:?}")))
}

impl Plugin for DesignPlugin {
    fn init(
        &self,
        assembly_bytes: &[u8],
        _assets: &[proto::PuzzleAsset],
        _params: &HashMap<String, ParamValue>,
    ) -> Result<(u64, Vec<u8>)> {
        let assembly = Assembly::from_bytes(assembly_bytes)
            .map_err(|e| op_err("INVALID_ASSEMBLY", e.to_string()))?;
        let session = self.next_session.fetch_add(1, Ordering::Relaxed) + 1;
        self.sessions()?.insert(session, assembly);
        // No post-init normalization, so the host keeps its input assembly.
        Ok((session, Vec::new()))
    }

    fn register(&self) -> Result<proto::PluginRegistration> {
        Ok(proto::PluginRegistration {
            id: "design".to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            operations: vec![proto::PluginOp {
                id: OP_MUTATE.to_owned(),
                display_name: "Mutate".to_owned(),
                description: "Change the selected residue's amino-acid identity".to_owned(),
                kind: proto::OpKind::Invoke as i32,
                // The picker in `plugin.toml` binds `aa` per option; this spec
                // is what types and validates it.
                params: vec![proto::ParamSpec {
                    name: "aa".to_owned(),
                    display_name: "Amino acid".to_owned(),
                    description: "Three-letter code of the target amino acid".to_owned(),
                    r#type: proto::ParamType::Enum as i32,
                    default: None,
                    constraints: Some(proto::ParamConstraints {
                        constraint: Some(proto::param_constraints::Constraint::EnumValues(
                            proto::EnumValues {
                                values: AminoAcid::ALL.iter().copied().map(code_string).collect(),
                            },
                        )),
                    }),
                }],
                // Mutation rewrites an existing protein in place: it creates no
                // entity and imposes no focus-type restriction.
                compatible_focus_types: vec![],
                creates_entities: false,
                requires_focus: false,
                ui: None,
            }],
            queries: vec![],
        })
    }

    fn update_assembly(
        &self,
        session: u64,
        payload: AssemblyPayload<'_>,
        _from_gen: u64,
        _to_gen: u64,
    ) -> Result<()> {
        let mut sessions = self.sessions()?;
        let assembly = sessions
            .get_mut(&session)
            .ok_or_else(|| op_err("UNKNOWN_SESSION", format!("no session {session}")))?;
        match payload {
            AssemblyPayload::Full(bytes) => {
                *assembly = Assembly::from_bytes(bytes)
                    .map_err(|e| op_err("INVALID_ASSEMBLY", e.to_string()))?;
            }
            AssemblyPayload::Delta(bytes) => {
                let edits = molex::ops::wire::delta::deserialize_edits(bytes)
                    .map_err(|e| op_err("INVALID_DELTA", e.to_string()))?;
                assembly
                    .apply_edits(&edits)
                    .map_err(|e| op_err("APPLY_DELTA_FAILED", e.to_string()))?;
            }
        }
        Ok(())
    }

    fn drop_session(&self, session: u64) -> Result<()> {
        self.sessions()?.remove(&session);
        Ok(())
    }

    fn invoke(
        &self,
        session: u64,
        op: &str,
        ctx: &DispatchContext,
        params: &HashMap<String, ParamValue>,
    ) -> Result<Vec<u8>> {
        if op != OP_MUTATE {
            return Err(PluginError::Unsupported);
        }
        let target = single_selected_residue(ctx)?;
        let aa = requested_amino_acid(params)?;

        let mut sessions = self.sessions()?;
        let assembly = sessions
            .get_mut(&session)
            .ok_or_else(|| op_err("UNKNOWN_SESSION", format!("no session {session}")))?;

        let protein = assembly
            .entity(target.entity_id)
            .and_then(MoleculeEntity::as_protein)
            .ok_or_else(|| {
                op_err(
                    "NOT_A_PROTEIN",
                    format!("entity {:?} is not a protein", target.entity_id),
                )
            })?;


        let mutated = protein
            .mutate_residue(target.residue_index as usize, aa)
            .map_err(|e| op_err("MUTATE_FAILED", e.to_string()))?;

        // `mutate_residue` preserves the entity id.
        let mut entities = assembly.entities().to_vec();
        let slot = entities
            .iter()
            .position(|e| e.id() == target.entity_id)
            .ok_or_else(|| op_err("UNKNOWN_ENTITY", "selected entity vanished from the assembly"))?;
        entities[slot] = Arc::new(MoleculeEntity::Protein(mutated));

        let updated = Assembly::from_arcs(entities);
        let bytes = updated
            .to_bytes()
            .map_err(|e| op_err("SERIALIZE_FAILED", e.to_string()))?;
        *assembly = updated;
        Ok(bytes)
    }
}

fn new_plugin(_config_json: &str) -> Result<Box<dyn Plugin>> {
    Ok(Box::new(DesignPlugin::default()))
}

export_plugin!(new_plugin);
