//! Per-residue segment-info panel state.
//!
//! Owns the open target (with its cached identity + secondary structure)
//! and the tail-tip debounce so the host pushes a screen-anchor change only
//! when the projected tip actually moves.

use viso::VisoEngine;

use crate::session::Session;

/// The open segment-info target plus the identity and secondary structure
/// cached at the moment it was set.
///
/// Identity and SS are computed once (a single `recompute_ss()` over the
/// head assembly) when the target opens and held here for its lifetime;
/// the GUI projection rebuilds only the energies and the screen anchor on
/// each score tick, so a streaming score never re-runs DSSP.
pub struct SegmentTarget {
    pub entity: molex::EntityId,
    pub residue: usize,
    pub residue_number: i32,
    pub chain: String,
    pub aa_three: String,
    pub aa_one: String,
    pub ss_label: String,
}

/// Last segment-panel tail tip projected to the screen, value-compared
/// each frame so an unchanged tip pushes nothing.
///
/// The `Unset` arm is distinct from `Hidden`: at rest (no panel ever
/// opened) the tip is `Unset`, and the off-screen path only emits a hide
/// when a `Visible` tip preceded it. Without that distinction every idle
/// frame would push a redundant hide.
#[derive(Clone, Copy, PartialEq)]
enum TailTip {
    /// No tip has been projected yet (no panel opened this session).
    Unset,
    /// The panel is open but its residue is off-screen / behind the camera.
    Hidden,
    /// The residue's CA projects to this screen position (pixels, top-left).
    Visible(f32, f32),
}

/// A tail-tip change the host should push to the webview this frame.
///
/// Returned by [`crate::App::take_tail_update`] only when the tip changed
/// since the last push; an unchanged tip yields `None` and the host pushes
/// nothing.
pub enum TailUpdate {
    /// Move the tail tip to this screen position (pixels, origin top-left).
    Position(f32, f32),
    /// Hide the tail (the residue went off-screen, or the panel closed).
    Hide,
}

/// Human-readable secondary-structure label for the segment panel.
fn ss_label(ss: Option<molex::SSType>) -> String {
    match ss {
        Some(molex::SSType::Helix) => "Helix",
        Some(molex::SSType::Sheet) => "Sheet",
        Some(molex::SSType::Coil) | None => "Loop",
    }
    .to_owned()
}

/// The per-residue segment-info panel: the open target with its cached
/// identity, plus the tail-tip debounce cursor and the pending host push.
pub(in crate::app) struct SegmentPanel {
    /// Open segment-info target with its cached identity + SS, or `None`
    /// when the panel is closed.
    open: Option<SegmentTarget>,
    /// Last tail tip pushed to the host, value-compared each frame so an
    /// unchanged tip pushes nothing.
    last_tail_tip: TailTip,
    /// Pending tail-tip change for the host to pull this frame, set only when
    /// the projected tip differed from `last_tail_tip`.
    pending_tail: Option<TailUpdate>,
}

impl SegmentPanel {
    pub(in crate::app) const fn new() -> Self {
        Self {
            open: None,
            last_tail_tip: TailTip::Unset,
            pending_tail: None,
        }
    }

    /// The open target, or `None` when the panel is closed.
    pub(in crate::app) const fn target(&self) -> Option<&SegmentTarget> {
        self.open.as_ref()
    }

    /// Open the panel on `(eid, residue)`, computing the residue identity
    /// (number, chain, amino acid) and its secondary structure once via a
    /// single `recompute_ss()` over the head assembly and caching them on the
    /// target. Returns `false` (a no-op) when the entity or residue does not
    /// resolve, so the caller can skip marking the section dirty.
    pub(in crate::app) fn open(
        &mut self,
        store: &Session,
        eid: molex::EntityId,
        residue: usize,
    ) -> bool {
        let Some(entity) = store.entity(eid) else {
            return false;
        };
        let Some(residues) = entity.residues() else {
            return false;
        };
        let Some(res) = residues.get(residue) else {
            return false;
        };
        let residue_number = res.seq_id();
        let chain = entity
            .pdb_chain_id()
            .map_or_else(String::new, str::to_owned);
        let aa = molex::chemistry::AminoAcid::from_code(res.name);
        let aa_three = String::from_utf8_lossy(&res.name).trim().to_owned();
        let aa_one = aa.map_or_else(String::new, |a| (a.one_letter() as char).to_string());

        let mut assembly = store.head_assembly();
        assembly.recompute_ss();
        let ss_label = ss_label(assembly.ss_types(eid).get(residue).copied());

        self.open = Some(SegmentTarget {
            entity: eid,
            residue,
            residue_number,
            chain,
            aa_three,
            aa_one,
            ss_label,
        });
        true
    }

    /// Close the panel.
    pub(in crate::app) fn close(&mut self) {
        self.open = None;
    }

    /// Project the open target's CA to the screen and stage a tail-tip change
    /// when it differs from the last pushed tip. A closed panel or an
    /// off-screen residue resolves to `Hidden`; a hide is staged only when a
    /// `Visible` tip preceded it, so an idle frame with no panel pushes
    /// nothing.
    pub(in crate::app) fn update_tail_tip(&mut self, store: &Session, engine: Option<&VisoEngine>) {
        let current = match (self.open.as_ref(), engine) {
            (Some(target), Some(engine)) => store
                .entity(target.entity)
                .and_then(|entity| crate::gui_projector::ca_world_position(entity, target.residue))
                .and_then(|world| engine.world_to_screen(world))
                .map_or(TailTip::Hidden, |v| TailTip::Visible(v.x, v.y)),
            _ => TailTip::Hidden,
        };

        if current == self.last_tail_tip {
            return;
        }

        match current {
            TailTip::Visible(x, y) => self.pending_tail = Some(TailUpdate::Position(x, y)),
            // Only emit a hide when something visible preceded it; the
            // initial `Unset` -> `Hidden` transition records state silently.
            TailTip::Hidden => {
                if matches!(self.last_tail_tip, TailTip::Visible(..)) {
                    self.pending_tail = Some(TailUpdate::Hide);
                }
            }
            TailTip::Unset => {}
        }
        self.last_tail_tip = current;
    }

    /// Take the pending tail-tip change for the host to push, or `None` when
    /// the tip did not move since the last push. `Some` is returned at most
    /// once per change: `update_tail_tip` sets it only on a value change and
    /// this clears it.
    pub(in crate::app) const fn take_update(&mut self) -> Option<TailUpdate> {
        self.pending_tail.take()
    }
}
