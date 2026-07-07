//! Structured parsers for the two foldit-owned puzzle-setup inputs.
//!
//! These travel alongside a puzzle's structure: catalytic constraints
//! (`.cnstr`) and the design mask (which residues a designer may mutate).
//! They are parsed at load time into the strongly-typed forms below;
//! enforcement (constraint scoring, lock placement) lives downstream and
//! consumes these types.

use std::ops::RangeInclusive;

/// A single atom reference inside a constraint: an atom name plus the
/// residue it belongs to (residue number + chain). The ligand is referred
/// to by its own chain (e.g. `1X` = resnum 1, chain X).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtomRef {
    pub atom_name: String,
    pub res_num: i32,
    pub chain: char,
}

/// Geometric relation a constraint pins. The token count after the kind
/// determines how many [`AtomRef`]s follow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConstraintKind {
    /// Distance between 2 atoms.
    AtomPair,
    /// Angle across 3 atoms.
    Angle,
    /// Dihedral across 4 atoms.
    Dihedral,
}

impl ConstraintKind {
    /// Number of atom references this kind expects.
    const fn atom_count(self) -> usize {
        match self {
            Self::AtomPair => 2,
            Self::Angle => 3,
            Self::Dihedral => 4,
        }
    }
}

/// The penalty function and its parameters for a constraint.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ConstraintFunc {
    /// Flat-bottomed harmonic: zero penalty within `tol` of `x0`,
    /// harmonic outside, with standard deviation `sd`.
    FlatHarmonic { x0: f64, sd: f64, tol: f64 },
    /// Circular (periodic) harmonic about `x0` with standard deviation
    /// `sd`, for angular/dihedral terms.
    CircularHarmonic { x0: f64, sd: f64 },
}

/// One parsed catalytic constraint line.
#[derive(Debug, Clone, PartialEq)]
pub struct Constraint {
    pub kind: ConstraintKind,
    pub atoms: Vec<AtomRef>,
    pub func: ConstraintFunc,
}

/// Parse a `.cnstr` file body into a list of [`Constraint`]s.
///
/// Each non-blank, non-comment line is:
/// `<Kind> <atom1> <res1> [<atom2> <res2> ...] <Func> <func params...>`,
/// where each atom is two whitespace tokens (atom name + residue ref like
/// `353A`). Lines whose first non-space character is `#` are comments.
///
/// # Errors
///
/// Returns an `Err` with a human-readable message on any malformed line:
/// unknown kind, too few tokens, an unparseable residue ref or number, or
/// an unknown penalty function.
pub fn parse_constraints(text: &str) -> Result<Vec<Constraint>, String> {
    let mut out = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let tokens: Vec<&str> = line.split_whitespace().collect();
        out.push(
            parse_constraint_line(&tokens)
                .map_err(|e| format!("constraint line {}: {e}", i + 1))?,
        );
    }
    Ok(out)
}

fn parse_constraint_line(tokens: &[&str]) -> Result<Constraint, String> {
    let kind = match tokens.first() {
        Some(&"AtomPair") => ConstraintKind::AtomPair,
        Some(&"Angle") => ConstraintKind::Angle,
        Some(&"Dihedral") => ConstraintKind::Dihedral,
        Some(other) => return Err(format!("unknown constraint kind '{other}'")),
        None => return Err("empty line".to_owned()),
    };

    let n_atoms = kind.atom_count();
    // 1 kind token + 2 tokens per atom + 1 func token = minimum length.
    let func_idx = 1 + n_atoms * 2;
    if tokens.len() <= func_idx {
        return Err(format!(
            "expected at least {} tokens for a {kind:?} constraint, found {}",
            func_idx + 1,
            tokens.len()
        ));
    }

    let mut atoms = Vec::with_capacity(n_atoms);
    for a in 0..n_atoms {
        let name = tokens[1 + a * 2];
        let res_ref = tokens[2 + a * 2];
        let (res_num, chain) = parse_res_ref(res_ref)?;
        atoms.push(AtomRef {
            atom_name: name.to_owned(),
            res_num,
            chain,
        });
    }

    let func = parse_func(&tokens[func_idx..])?;
    Ok(Constraint { kind, atoms, func })
}

/// Parse a residue ref like `353A` into (resnum, chain): a leading integer
/// followed by a single trailing alphabetic chain character.
fn parse_res_ref(s: &str) -> Result<(i32, char), String> {
    let chain = s
        .chars()
        .next_back()
        .filter(char::is_ascii_alphabetic)
        .ok_or_else(|| format!("residue ref '{s}' has no trailing chain char"))?;
    let num_part = &s[..s.len() - chain.len_utf8()];
    let res_num = num_part
        .parse::<i32>()
        .map_err(|_| format!("residue ref '{s}' has an unparseable residue number"))?;
    Ok((res_num, chain))
}

fn parse_func(tokens: &[&str]) -> Result<ConstraintFunc, String> {
    let name = tokens.first().ok_or("missing penalty function")?;
    let params = &tokens[1..];
    let parse_f = |idx: usize, label: &str| -> Result<f64, String> {
        params
            .get(idx)
            .ok_or_else(|| format!("{name}: missing {label}"))?
            .parse::<f64>()
            .map_err(|_| format!("{name}: unparseable {label}"))
    };
    match *name {
        "FLAT_HARMONIC" => Ok(ConstraintFunc::FlatHarmonic {
            x0: parse_f(0, "x0")?,
            sd: parse_f(1, "sd")?,
            tol: parse_f(2, "tol")?,
        }),
        "CIRCULARHARMONIC" => Ok(ConstraintFunc::CircularHarmonic {
            x0: parse_f(0, "x0")?,
            sd: parse_f(1, "sd")?,
        }),
        other => Err(format!("unknown penalty function '{other}'")),
    }
}

/// The set of residues a designer is permitted to mutate.
///
/// Represented as a list of inclusive residue-number ranges; residues
/// outside every range are locked (e.g. the catalytic core).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesignMask {
    pub ranges: Vec<RangeInclusive<u32>>,
}

impl DesignMask {
    /// Whether residue `res` falls in any designable range.
    #[must_use]
    pub fn is_designable(&self, res: u32) -> bool {
        self.ranges.iter().any(|r| r.contains(&res))
    }
}

/// Parse a design-mask string like `6-163||165-294||296-352||354-440||`.
///
/// Segments are split on `||`; empty segments (e.g. from a trailing `||`)
/// are ignored. Each segment is an inclusive `start-end` range.
///
/// # Errors
///
/// Returns an `Err` on a malformed segment: not a single `start-end` pair
/// or an unparseable bound.
pub fn parse_design_mask(s: &str) -> Result<DesignMask, String> {
    let mut ranges = Vec::new();
    for seg in s.split("||") {
        let seg = seg.trim();
        if seg.is_empty() {
            continue;
        }
        let (a, b) = seg
            .split_once('-')
            .ok_or_else(|| format!("design-mask segment '{seg}' is not 'start-end'"))?;
        let start = a
            .trim()
            .parse::<u32>()
            .map_err(|_| format!("design-mask segment '{seg}' has an unparseable start"))?;
        let end = b
            .trim()
            .parse::<u32>()
            .map_err(|_| format!("design-mask segment '{seg}' has an unparseable end"))?;
        ranges.push(start..=end);
    }
    Ok(DesignMask { ranges })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sample_constraint_lines() {
        let text = "\
AtomPair  OE2 353A  C5    1X                      FLAT_HARMONIC     2.00  0.002 0.25
Angle     OE2 353A  C5    1X  O2    1X            CIRCULARHARMONIC  3.141       0.087
Dihedral  CG  353A  CD  353A  OE2 353A  C5   1X   CIRCULARHARMONIC  3.141       0.175
";
        let cs = parse_constraints(text).unwrap();
        assert_eq!(cs.len(), 3);

        // AtomPair: 2 atoms, FLAT_HARMONIC.
        assert_eq!(cs[0].kind, ConstraintKind::AtomPair);
        assert_eq!(cs[0].atoms.len(), 2);
        assert_eq!(
            cs[0].atoms[0],
            AtomRef {
                atom_name: "OE2".to_owned(),
                res_num: 353,
                chain: 'A'
            }
        );
        assert_eq!(
            cs[0].atoms[1],
            AtomRef {
                atom_name: "C5".to_owned(),
                res_num: 1,
                chain: 'X'
            }
        );
        assert_eq!(
            cs[0].func,
            ConstraintFunc::FlatHarmonic {
                x0: 2.00,
                sd: 0.002,
                tol: 0.25
            }
        );

        // Angle: 3 atoms, CIRCULARHARMONIC.
        assert_eq!(cs[1].kind, ConstraintKind::Angle);
        assert_eq!(cs[1].atoms.len(), 3);
        assert_eq!(
            cs[1].atoms[2],
            AtomRef {
                atom_name: "O2".to_owned(),
                res_num: 1,
                chain: 'X'
            }
        );
        // The angular x0 in the data is `3.141` (a 3-dp truncation of pi);
        // derive the expected value rather than writing the bare literal,
        // which the clippy approx-constant lint flags.
        let near_pi = (std::f64::consts::PI * 1000.0).trunc() / 1000.0;
        let ConstraintFunc::CircularHarmonic { x0, sd } = cs[1].func else {
            panic!("expected CircularHarmonic, got {:?}", cs[1].func);
        };
        assert!((x0 - near_pi).abs() < 1e-9);
        assert!((sd - 0.087).abs() < 1e-9);

        // Dihedral: 4 atoms.
        assert_eq!(cs[2].kind, ConstraintKind::Dihedral);
        assert_eq!(cs[2].atoms.len(), 4);
        assert_eq!(
            cs[2].atoms[0],
            AtomRef {
                atom_name: "CG".to_owned(),
                res_num: 353,
                chain: 'A'
            }
        );
        let ConstraintFunc::CircularHarmonic { x0, sd } = cs[2].func else {
            panic!("expected CircularHarmonic, got {:?}", cs[2].func);
        };
        assert!((x0 - near_pi).abs() < 1e-9);
        assert!((sd - 0.175).abs() < 1e-9);
    }

    #[test]
    fn comments_and_blanks_yield_empty() {
        let text = "\
# a comment
   # an indented comment

\t
";
        assert!(parse_constraints(text).unwrap().is_empty());
    }

    #[test]
    fn malformed_constraint_errors() {
        // Unknown kind.
        assert!(parse_constraints("Bogus OE2 353A C5 1X FLAT_HARMONIC 2.0 0.1 0.2").is_err());
        // Too few tokens for an AtomPair (missing func).
        assert!(parse_constraints("AtomPair OE2 353A C5 1X").is_err());
        // Unparseable residue ref (no chain char).
        assert!(parse_constraints("AtomPair OE2 353 C5 1X FLAT_HARMONIC 2.0 0.1 0.2").is_err());
        // Unknown penalty function.
        assert!(parse_constraints("AtomPair OE2 353A C5 1X NOPE 2.0").is_err());
    }

    #[test]
    fn parse_bglb_design_mask() {
        let mask = parse_design_mask("6-163||165-294||296-352||354-440||").unwrap();
        assert_eq!(mask.ranges.len(), 4);
        assert!(mask.is_designable(100));
        assert!(mask.is_designable(200));
        assert!(!mask.is_designable(164)); // locked catalytic gap
        assert!(!mask.is_designable(353)); // locked
    }

    #[test]
    fn malformed_design_mask_errors() {
        assert!(parse_design_mask("6-163||not-a-range-x").is_err());
        assert!(parse_design_mask("6_163").is_err());
    }
}
