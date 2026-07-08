//! One-shot transcription of the legacy rosetta `.ir_puzzle.filters` files
//! into `[[puzzle.filter]]` TOML blocks in the curated level assets.
//!
//! The legacy format is not standard JSON: a `version: <N>` preamble line,
//! then one or more brace objects whose members are `"key" : "value"` pairs
//! with NO commas between them and all values double-quoted. A file may carry
//! several objects back to back (one filter each). We parse each object into
//! ordered key/value string pairs, build a [`FilterSpec`] from each (the
//! single source of the block schema), then serialize it into one
//! `[[puzzle.filter]]` block. Each value renders as the TOML integer/float/
//! string it parses as.
//!
//! Filter kinds in `NATIVE_TYPES` are scored in-process by foldit and so carry
//! no `plugin`; every other kind is forwarded to the rosetta plugin
//! (`plugin = "rosetta"`). Every source key/value other than `type` is retained
//! in `params` (nothing is dropped), so a param the bridge or the native
//! evaluator depends on (e.g. `bonus`) survives the round-trip.
//!
//! Writing is surgical: `toml_edit` preserves the rest of each `puzzle.toml`
//! (tables, comments, ordering) and the pass is idempotent because it clears
//! any existing `filter` array before regenerating it. It also drops the legacy
//! `[[puzzle.objective]]` array-of-tables (and its attached comment) that the
//! `filter` array replaced, so no dead config survives the transcription.
//!
//! The same pass also ports the legacy `"can_design"` design mask out of each
//! `<id>.ir_puzzle.puzzle_setup`. That value is in rosetta POSE numbering (one
//! continuous 1..N count over every residue in the pose, chains concatenated in
//! structure order); the target `[[puzzle.design_mask]]` block is PER-CHAIN PDB
//! numbering. The new structure PDBs renumber per chain inconsistently, so a
//! pose index is mapped through the structure's residue ORDER (the i-th distinct
//! residue in `structure.pdb` is pose index i), never by arithmetic on PDB
//! numbers. The mapped residues are grouped by chain and re-compressed into
//! inclusive `start-end||` ranges, one `[[puzzle.design_mask]]` block per chain.

use anyhow::{bail, Context, Result};
use foldit_core::puzzle::{levels_root, FilterSpec};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;
use toml_edit::{value, ArrayOfTables, DocumentMut, Item, Table};

const FILTERS_DIR: &str =
    "plugins/rosetta/deps/rosetta-interactive/resources/levels";

/// Directory holding the legacy `<id>.ir_puzzle.puzzle_setup` source files,
/// which carry the pose-numbered `"can_design"` design masks. Same tree as the
/// filters sources.
const PUZZLE_SETUP_DIR: &str = FILTERS_DIR;

/// Filter kinds foldit scores natively (in-process), as opposed to forwarding
/// to the rosetta plugin. A native kind carries no `plugin` line; foldit's own
/// evaluator reads its params. Everything else is forwarded with
/// `plugin = "rosetta"`.
const NATIVE_TYPES: [&str; 1] = ["ExposedCount"];

pub fn transcribe_filters() -> Result<()> {
    let levels_root = levels_root().map_err(|e| anyhow::anyhow!(e))?;
    let filters_root = Path::new(FILTERS_DIR);

    if !levels_root.is_dir() {
        bail!("curated levels dir not found at {}", levels_root.display());
    }
    if !filters_root.is_dir() {
        bail!("rosetta filters dir not found at {FILTERS_DIR}");
    }

    // All curated level ids (directory names under the levels root).
    let mut curated: BTreeSet<String> = BTreeSet::new();
    for entry in std::fs::read_dir(&levels_root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            if let Some(name) = entry.file_name().to_str() {
                curated.insert(name.to_owned());
            }
        }
    }

    // All ids that have a `.ir_puzzle.filters` source file.
    let mut filter_ids: BTreeSet<String> = BTreeSet::new();
    // All ids that have a `.ir_puzzle.puzzle_setup` carrying a `"can_design"`.
    let mut mask_ids: BTreeSet<String> = BTreeSet::new();
    for entry in std::fs::read_dir(filters_root)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if let Some(id) = name.strip_suffix(".ir_puzzle.filters") {
            filter_ids.insert(id.to_owned());
        } else if let Some(id) = name.strip_suffix(".ir_puzzle.puzzle_setup") {
            let raw = std::fs::read_to_string(entry.path())
                .with_context(|| format!("reading {}", entry.path().display()))?;
            if extract_can_design(&raw).is_some() {
                mask_ids.insert(id.to_owned());
            }
        }
    }

    // Sources with no matching curated level: report, do not skip silently.
    let unmatched: BTreeSet<&String> = filter_ids
        .difference(&curated)
        .chain(mask_ids.difference(&curated))
        .collect();

    let mut levels_transcribed = 0usize;
    let mut blocks_written = 0usize;
    let mut mask_blocks_written = 0usize;

    // Drive every curated level that has either source, so a level with only a
    // design mask (and no `.filters`) is still transcribed.
    let mut touched: BTreeSet<&String> = BTreeSet::new();
    touched.extend(curated.intersection(&filter_ids));
    touched.extend(curated.intersection(&mask_ids));

    for id in touched {
        let puzzle_path = levels_root.join(id).join("puzzle.toml");
        if !puzzle_path.is_file() {
            // Curated dir exists but no puzzle.toml: not a level we own.
            println!("  skip {id}: no puzzle.toml in the curated level dir");
            continue;
        }

        let specs = if filter_ids.contains(id) {
            let filters_path = filters_root.join(format!("{id}.ir_puzzle.filters"));
            let raw = std::fs::read_to_string(&filters_path)
                .with_context(|| format!("reading {}", filters_path.display()))?;
            parse_filters(&raw).with_context(|| format!("parsing {}", filters_path.display()))?
        } else {
            Vec::new()
        };

        let masks = if mask_ids.contains(id) {
            let setup_path =
                Path::new(PUZZLE_SETUP_DIR).join(format!("{id}.ir_puzzle.puzzle_setup"));
            let structure_path = levels_root.join(id).join("structure.pdb");
            build_design_masks(id, &setup_path, &structure_path)
                .with_context(|| format!("mapping design mask for {id}"))?
        } else {
            None
        };

        let n = write_puzzle_setup(&puzzle_path, &specs, masks.as_deref())
            .with_context(|| format!("writing {}", puzzle_path.display()))?;

        levels_transcribed += 1;
        blocks_written += n;

        let mut report = format!("  {id}: {n} filter block(s)");
        if let Some(masks) = &masks {
            mask_blocks_written += masks.len();
            let chains: Vec<String> = masks
                .iter()
                .map(|m| format!("{}({} range(s))", m.chain, m.ranges().len()))
                .collect();
            let _ = write!(report, ", design mask -> {}", chains.join(", "));
        }
        println!("{report}");
    }

    print_transcription_summary(
        levels_transcribed,
        blocks_written,
        mask_blocks_written,
        &unmatched,
    );

    Ok(())
}

/// Print the closing transcription tally and any legacy sources that matched no
/// curated level.
fn print_transcription_summary(
    levels_transcribed: usize,
    blocks_written: usize,
    mask_blocks_written: usize,
    unmatched: &BTreeSet<&String>,
) {
    println!(
        "transcribe-filters: {levels_transcribed} level(s) transcribed, \
         {blocks_written} filter block(s), {mask_blocks_written} design-mask \
         block(s) total."
    );
    if unmatched.is_empty() {
        println!("  all legacy sources matched a curated level.");
    } else {
        println!(
            "  {} legacy source(s) had no curated level (not transcribed): {}",
            unmatched.len(),
            unmatched
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
}

/// Parse the full `.ir_puzzle.filters` body into the [`FilterSpec`]s it
/// contains. Strips the `version:` preamble, then reads each `{ ... }` object
/// as a flat sequence of double-quoted-string tokens separated by structural
/// `:` punctuation.
fn parse_filters(raw: &str) -> Result<Vec<FilterSpec>> {
    let mut filters = Vec::new();

    let tokens = tokenize(raw)?;
    let mut i = 0;
    while i < tokens.len() {
        match &tokens[i] {
            Token::Open => {
                let (filter, next) = parse_object(&tokens, i + 1)?;
                filters.push(filter);
                i = next;
            }
            // A `version:` preamble survives as a bare `version` ident token
            // followed by a `:`; bare idents and stray colons outside an
            // object are skipped.
            Token::Str(_) | Token::Colon | Token::Close | Token::Ident => i += 1,
        }
    }

    if filters.is_empty() {
        bail!("no filter objects found");
    }
    Ok(filters)
}

/// Parse one brace object, starting just after the `{`, into a [`FilterSpec`].
/// Returns the spec and the index just past the matching `}`. The `type` member
/// becomes `kind`; `plugin` is decided by [`NATIVE_TYPES`]; every other source
/// member is retained in `params` (rendered as its natural `toml::Value`).
fn parse_object(tokens: &[Token], start: usize) -> Result<(FilterSpec, usize)> {
    // Collect the object's `"key" : "value"` members in source order.
    let mut members: Vec<(String, String)> = Vec::new();
    let mut i = start;
    loop {
        match tokens.get(i) {
            Some(Token::Close) => {
                i += 1;
                break;
            }
            Some(Token::Str(key)) => {
                // Expect `: "value"`.
                let Some(Token::Colon) = tokens.get(i + 1) else {
                    bail!("expected ':' after key \"{key}\"");
                };
                let Some(Token::Str(val)) = tokens.get(i + 2) else {
                    bail!("expected quoted value after key \"{key}\"");
                };
                members.push((key.clone(), val.clone()));
                i += 3;
            }
            Some(other) => bail!("unexpected token in object: {other:?}"),
            None => bail!("unterminated filter object (missing '}}')"),
        }
    }

    let kind = members
        .iter()
        .find(|(k, _)| k == "type")
        .map(|(_, v)| v.clone())
        .context("filter object has no \"type\" member")?;

    // Every member except `type` is a param: nothing is dropped, so a param the
    // bridge or the native evaluator depends on (e.g. `bonus`) is retained.
    let params: BTreeMap<String, toml::Value> = members
        .into_iter()
        .filter(|(k, _)| k != "type")
        .map(|(k, v)| (k, render_value(&v)))
        .collect();

    let plugin = if NATIVE_TYPES.contains(&kind.as_str()) {
        None
    } else {
        Some("rosetta".to_owned())
    };

    Ok((
        FilterSpec {
            kind,
            plugin,
            params,
        },
        i,
    ))
}

#[derive(Debug)]
enum Token {
    Open,
    Close,
    Colon,
    Str(String),
    /// A bare (unquoted) word, e.g. the `version` / `1` of the preamble. The
    /// word itself is irrelevant once tokenized; only its presence matters, so
    /// it carries no payload.
    Ident,
}

/// Tolerant tokenizer for the legacy format: emits structural `{ } :`, every
/// double-quoted string, and bare words (the `version` preamble ident). Commas
/// and all whitespace are ignored, so the comma-less member separation is a
/// non-issue. Numbers in the `version: 1` preamble surface as `Ident`s and are
/// skipped by `parse_filters`.
fn tokenize(raw: &str) -> Result<Vec<Token>> {
    let mut tokens = Vec::new();
    let mut chars = raw.chars().peekable();
    while let Some(&c) = chars.peek() {
        match c {
            '{' => {
                tokens.push(Token::Open);
                chars.next();
            }
            '}' => {
                tokens.push(Token::Close);
                chars.next();
            }
            ':' => {
                tokens.push(Token::Colon);
                chars.next();
            }
            '"' => {
                chars.next(); // opening quote
                let mut s = String::new();
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => {
                            // Preserve escaped chars literally (values are
                            // simple strings; no JSON escape semantics needed).
                            if let Some(n) = chars.next() {
                                s.push(n);
                            }
                        }
                        Some(ch) => s.push(ch),
                        None => bail!("unterminated quoted string"),
                    }
                }
                tokens.push(Token::Str(s));
            }
            c if c.is_whitespace() || c == ',' => {
                chars.next();
            }
            _ => {
                // Bare word (e.g. `version`, the `1` after it). Consume it to a
                // structural char or whitespace; the word itself is discarded.
                while let Some(&ch) = chars.peek() {
                    if ch.is_whitespace() || matches!(ch, '{' | '}' | ':' | ',' | '"') {
                        break;
                    }
                    chars.next();
                }
                tokens.push(Token::Ident);
            }
        }
    }
    Ok(tokens)
}

/// Regenerate the `[[puzzle.filter]]` array (and, when `masks` is `Some`, the
/// `[[puzzle.design_mask]]` array) in `puzzle.toml` from the transcribed
/// sources, preserving the rest of the document. Clears any prior array first,
/// so re-running is idempotent, and drops the legacy `[[puzzle.objective]]`
/// array (and its comment) the filter array replaced. Returns the filter blocks
/// written.
///
/// `masks == None` means the level carries no design mask in this pass and any
/// existing `design_mask` array is left untouched; `Some(&[])` would clear it.
/// Only the eight levels with a legacy `"can_design"` pass `Some`.
fn write_puzzle_setup(
    puzzle_path: &Path,
    specs: &[FilterSpec],
    masks: Option<&[ChainMask]>,
) -> Result<usize> {
    let text = std::fs::read_to_string(puzzle_path)?;
    let mut doc: DocumentMut = text.parse().context("parsing puzzle.toml")?;

    let puzzle = doc
        .get_mut("puzzle")
        .and_then(Item::as_table_mut)
        .context("puzzle.toml has no [puzzle] table")?;

    // Drop any prior filter array so a re-run replaces (not appends to) it.
    puzzle.remove("filter");

    // Drop the legacy `[[puzzle.objective]]` array-of-tables that
    // `[[puzzle.filter]]` superseded. Its describing comment lives in the first
    // table's decor prefix, so removing the key removes the comment with it (no
    // orphaned comment is left behind).
    puzzle.remove("objective");

    let mut array = ArrayOfTables::new();
    for spec in specs {
        array.push(spec_to_table(spec));
    }
    let count = array.len();
    puzzle.insert("filter", Item::ArrayOfTables(array));

    // Replace the design-mask array only when this level carries one this pass,
    // so a re-run is a no-op (the same masks regenerate identically) and a
    // filter-only level is not stripped of a hand-authored mask.
    if let Some(masks) = masks {
        puzzle.remove("design_mask");
        let mut mask_array = ArrayOfTables::new();
        for mask in masks {
            mask_array.push(mask_to_table(mask));
        }
        puzzle.insert("design_mask", Item::ArrayOfTables(mask_array));
    }

    std::fs::write(puzzle_path, doc.to_string())?;
    Ok(count)
}

/// Lay a [`ChainMask`] out as a `toml_edit` table: `chain` then `can_design`,
/// matching the existing curated example's key order.
fn mask_to_table(mask: &ChainMask) -> Table {
    let mut tbl = Table::new();
    tbl.insert("chain", value(mask.chain.clone()));
    tbl.insert("can_design", value(mask.can_design()));
    tbl
}

/// Lay a [`FilterSpec`] out as a `toml_edit` table in deterministic key order:
/// `type` first, then `plugin` (only when present), then each `params` entry in
/// the spec's own (sorted) order. `FilterSpec` is the single source of the block
/// schema; this projects its fields without re-encoding the shape by hand.
fn spec_to_table(spec: &FilterSpec) -> Table {
    let mut tbl = Table::new();
    tbl.insert("type", value(spec.kind.clone()));
    if let Some(plugin) = &spec.plugin {
        tbl.insert("plugin", value(plugin.clone()));
    }
    for (k, v) in &spec.params {
        tbl.insert(k, value(toml_value_to_edit(v)));
    }
    tbl
}

/// Map a `toml::Value` (a `FilterSpec.params` entry) to the equivalent
/// `toml_edit::Value`. Only the scalar variants the source format can produce
/// (integer / float / string / bool) are expected; anything else falls back to
/// its TOML string form.
fn toml_value_to_edit(v: &toml::Value) -> toml_edit::Value {
    match v {
        toml::Value::Integer(n) => (*n).into(),
        toml::Value::Float(f) => (*f).into(),
        toml::Value::Boolean(b) => (*b).into(),
        toml::Value::String(s) => s.clone().into(),
        other => other.to_string().into(),
    }
}

/// Render a source string value as the most specific `toml::Value` it parses
/// as: integer, else float, else a string.
fn render_value(s: &str) -> toml::Value {
    s.parse::<i64>().map_or_else(
        |_| {
            s.parse::<f64>()
                .map_or_else(|_| toml::Value::String(s.to_owned()), toml::Value::Float)
        },
        toml::Value::Integer,
    )
}

// ─────────────────────────────────────────────────────────────────────────────
// Design mask: pose numbering -> per-chain PDB numbering
// ─────────────────────────────────────────────────────────────────────────────

/// One chain's designable residues, in PDB numbering, as the sorted list of
/// distinct residue numbers it covers. Compresses to inclusive `start-end||`
/// ranges on the way out.
struct ChainMask {
    chain: String,
    /// Sorted, de-duplicated PDB residue numbers the chain may design.
    residues: Vec<u32>,
}

impl ChainMask {
    /// Compress the sorted residue list into inclusive `(start, end)` ranges:
    /// the single source of the chain's range decomposition, consumed both by
    /// the `can_design` string formatter and by the summary range count.
    fn ranges(&self) -> Vec<(u32, u32)> {
        let mut ranges = Vec::new();
        let mut iter = self.residues.iter().copied();
        let Some(mut start) = iter.next() else {
            return ranges;
        };
        let mut prev = start;
        for r in iter {
            if r != prev + 1 {
                ranges.push((start, prev));
                start = r;
            }
            prev = r;
        }
        ranges.push((start, prev));
        ranges
    }

    /// Render the compressed ranges as inclusive `start-end` segments joined by
    /// `||` with a trailing `||`, matching the existing curated format (a single
    /// residue renders as `N-N`).
    fn can_design(&self) -> String {
        let mut out = String::new();
        for (start, end) in self.ranges() {
            let _ = write!(out, "{start}-{end}||");
        }
        out
    }
}

/// Pull the `"can_design"` value (the bare range string) out of a legacy
/// `.ir_puzzle.puzzle_setup` body, or `None` if it carries none. The format is
/// the same loose `"key" : "value"` shape the filters tokenizer handles, but the
/// design mask is the only member we need, so a targeted scan is enough.
fn extract_can_design(raw: &str) -> Option<String> {
    let tokens = tokenize(raw).ok()?;
    for window in tokens.windows(3) {
        if let [Token::Str(key), Token::Colon, Token::Str(val)] = window {
            if key == "can_design" {
                return Some(val.clone());
            }
        }
    }
    None
}

/// Build the per-chain design masks for one level by mapping its pose-numbered
/// `"can_design"` through the structure's residue order. Returns `None` only if
/// the setup file carries no `"can_design"` (the caller filters those out, so in
/// practice this is always `Some`).
///
/// # Errors
///
/// Errors if the setup or structure file cannot be read, the `can_design` value
/// is malformed, or a pose index exceeds the structure's residue count (the
/// pose-order assumption broke); the caller logs that and emits no mask.
fn build_design_masks(
    id: &str,
    setup_path: &Path,
    structure_path: &Path,
) -> Result<Option<Vec<ChainMask>>> {
    let setup_raw = std::fs::read_to_string(setup_path)
        .with_context(|| format!("reading {}", setup_path.display()))?;
    let Some(can_design) = extract_can_design(&setup_raw) else {
        return Ok(None);
    };

    let pdb_raw = std::fs::read_to_string(structure_path)
        .with_context(|| format!("reading {}", structure_path.display()))?;
    // Ordered list of distinct residues in file order; the i-th (1-based) is
    // pose index i.
    let residues = pose_order_residues(&pdb_raw);
    if residues.is_empty() {
        bail!("{id}: structure.pdb has no ATOM records to order");
    }

    let pose_indices = expand_pose_indices(&can_design)
        .with_context(|| format!("{id}: parsing can_design '{can_design}'"))?;

    // The pose-order assumption: every pose index must address a real residue.
    // If any exceeds the residue count, the structure does not match the legacy
    // pose, so emit nothing and let the caller log the skip.
    if let Some(&max) = pose_indices.iter().max() {
        if (max as usize) > residues.len() {
            bail!(
                "{id}: pose index {max} exceeds the {} residue(s) in structure.pdb \
                 (pose-order assumption broke); no mask emitted",
                residues.len()
            );
        }
    }

    // Map each pose index to its (chain, pdb_resseq) and group by chain.
    let mut by_chain: BTreeMap<String, BTreeSet<u32>> = BTreeMap::new();
    for pose in pose_indices {
        let (chain, resseq) = &residues[(pose - 1) as usize];
        by_chain.entry(chain.clone()).or_default().insert(*resseq);
    }

    let masks = by_chain
        .into_iter()
        .map(|(chain, set)| ChainMask {
            chain,
            residues: set.into_iter().collect(),
        })
        .collect();
    Ok(Some(masks))
}

/// Parse `structure.pdb` into the ordered list of distinct residues in file
/// order: each entry is `(chain, pdb_resseq)` and the i-th (1-based) is pose
/// index i. Only ATOM records are considered; the residue key is
/// `(chain = col 22, resseq = cols 23-26)`, and consecutive ATOM lines of the
/// same residue collapse to one entry (file order, not sorted).
///
/// This scans raw ATOM records in file order on purpose, to reproduce Rosetta's
/// pose numbering exactly. molex's reader is deliberately not reused: its
/// polymer-only, completion-applied, author-numbered walk would change the
/// index -> residue mapping this pass depends on.
fn pose_order_residues(pdb: &str) -> Vec<(String, u32)> {
    let mut residues: Vec<(String, u32)> = Vec::new();
    for line in pdb.lines() {
        if !line.starts_with("ATOM") {
            continue;
        }
        // PDB fixed columns (1-based): chain id is column 22, residue sequence
        // number is columns 23-26. Index into the byte slice accordingly.
        let bytes = line.as_bytes();
        if bytes.len() < 26 {
            continue;
        }
        let chain = (bytes[21] as char).to_string();
        let resseq: u32 = match line[22..26].trim().parse() {
            Ok(n) => n,
            Err(_) => continue,
        };
        let key = (chain, resseq);
        // Distinct residues only; a residue spans many consecutive ATOM lines.
        if residues.last() != Some(&key) {
            residues.push(key);
        }
    }
    residues
}

/// Expand a pose-numbered `can_design` value into the sorted, de-duplicated set
/// of pose indices it covers. Each `||`-separated segment is either `A-B`
/// (inclusive range) or a bare `N` (single residue).
///
/// The `A-B` range grammar is shared with `foldit_core::puzzle_setup::
/// parse_design_mask`, so segments are parsed through it rather than
/// re-implemented here. The canonical parser requires `start-end`, so each bare
/// single `N` (which the legacy input also permits) is first normalized to
/// `N-N`.
///
/// # Errors
///
/// Errors if a segment is neither a bare integer nor an `A-B` range with
/// parseable bounds.
fn expand_pose_indices(can_design: &str) -> Result<Vec<u32>> {
    // Normalize bare singles `N` to `N-N` so every segment is the `start-end`
    // shape the canonical parser expects.
    let normalized: String = can_design
        .split("||")
        .map(|seg| {
            let trimmed = seg.trim();
            if trimmed.is_empty() || trimmed.contains('-') {
                trimmed.to_owned()
            } else {
                format!("{trimmed}-{trimmed}")
            }
        })
        .collect::<Vec<_>>()
        .join("||");

    let mask = foldit_core::puzzle_setup::parse_design_mask(&normalized)
        .map_err(|e| anyhow::anyhow!(e))
        .with_context(|| format!("parsing can_design '{can_design}'"))?;

    let mut indices: BTreeSet<u32> = BTreeSet::new();
    for range in mask.ranges {
        for i in range {
            indices.insert(i);
        }
    }
    Ok(indices.into_iter().collect())
}
