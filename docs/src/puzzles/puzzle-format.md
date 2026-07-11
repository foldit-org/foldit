# Puzzle Format and Conversion

A puzzle is a level directory. In the current engine each lives under
`assets/levels/<id>/` and is described by a single self-contained
`puzzle.toml`, alongside the asset files it names by relative path
(`structure.pdb`, a ligand `.params`, a `.cnstr` constraints file, density
maps). The parser is `crates/foldit-core/src/puzzle_toml.rs` (the serde
schema is the source of truth) and `puzzle.toml` is read in one shot by
`load_puzzle` (`puzzle_toml.rs:221`). `puzzle_load.rs` lifts the parsed
struct into the runtime `LoadedPuzzle`.

This page is both a format reference for `puzzle.toml` and a guide for
converting the legacy Rosetta `.ir_puzzle.*` puzzles into it.

## Anatomy of a `puzzle.toml`

The whole file deserializes into `PuzzleLevel` (`puzzle_toml.rs:10`): a
required `[puzzle]` table plus optional sub-tables, then the dialogue
arrays (`sequence`, `sequence_alt`, `branch`, `on_event`, `game_event`).

### `[puzzle]` — metadata (`PuzzleMeta`, required)

| Key | Type | Meaning |
| --- | --- | --- |
| `title` | string | Display name (required). |
| `start_energy` | float | Starting Rosetta energy of the pose (required). |
| `completion_score` | float | Game-score threshold that marks the puzzle solved (required). |
| `structure` | table | The starting structure — see `[puzzle.structure]` (required). |
| `camera` | table | Initial camera — see `[puzzle.camera]` (required). |
| `view_preset` | string | Optional named view preset from `assets/view_presets`. |
| `weights` | table | Optional score-term weight patch, `term = value`. |

`PuzzleMeta` also carries `#[serde(flatten)] extra` (`puzzle_toml.rs:68`),
so **any unrecognized key under `[puzzle]` is silently absorbed and has no
runtime effect**. Real levels carry several such keys as informational or
not-yet-wired settings: `view_setup`, `scorefxn`, `min_moves`,
`guide_visible`, `can_align_guide`, `use_dof_tethers`. Do not assume a key
does something because the game accepts it without error; only the fields
in the table above (and the sub-tables below) are consumed.

### `[puzzle.structure]` — the starting pose (`StructureRef`)

```toml
[puzzle.structure]
path = "structure.pdb"   # relative to the level dir; or use `data` for inline
format = "pdb"           # "pdb", "cif", ...
ss = "LEEEEEELLLL..."    # optional per-residue secondary-structure string
```

`path` (a sibling file) or `data` (inline text) supplies the coordinates;
`format` names the parser; `ss` optionally pins the secondary structure
that the cartoon renders (`puzzle_toml.rs:97`).

### `[puzzle.camera]` — initial view (`Camera`)

```toml
[puzzle.camera]
center = [-12, -24, -10]
eye    = [27, 11, 86]
up     = [-0.75, -0.45, 0.48]
```

Three `[f64; 3]` vectors (`puzzle_toml.rs:106`). Note the type difference
from the legacy format, where each was a `"x y z"` string.

### `[[puzzle.filter]]` — win conditions and objectives (`FilterSpec`)

Each filter is one array-of-tables entry that folds a raw score bonus into
the headline game score, mirroring the Rosetta `Filter` model
(`puzzle_toml.rs:72`).

```toml
[[puzzle.filter]]
type = "ClashCount"       # the filter kind (TOML key is `type`, Rust field `kind`)
plugin = "rosetta"        # who scores it; omit for a foldit-native filter
bonus = -200              # every other key is a free parameter
max_clashes = 1
```

- `type` selects the filter; everything else becomes a flat `params` map
  and is forwarded verbatim, so no parameter is lost.
- `plugin` is the routing switch (`puzzle_toml.rs:87`). `plugin = "rosetta"`
  forwards the filter to the Rosetta bridge; **omitting `plugin`** marks a
  filter scored in-process by foldit's own evaluator.
- An unknown `type` parses but evaluates to no bonus (forward-compatible).

Filter kinds shipped in the current campaign, for reference when authoring:
`ClashCount` (`max_clashes`), `VoidSize` (`max_void`), `HBondCount`
(`min_bonds`), `ExposedCount` (`max_exposed_hydrophobics`, native — no
`plugin`), `DisulfideCountScore`, `LayerScore`, `ResidueCountScore`,
`ResidueIEScore`, `RoScriptScore`, `DSSPToGuide`, `Abego`. Each carries its
own parameters; copy a shipped level using the same `type` as a template
rather than guessing parameter names. The exhaustive kind vocabulary and
each kind's parameters live in the Rosetta `Filter` sources, not in this
repo.

### Other `[puzzle.*]` asset tables

```toml
[[puzzle.ligand]]              # LigandRef; repeatable
params = "ligand.params"       # Rosetta residue-type topology
conformers = "rotamers.pdb"    # optional conformer library

[puzzle.constraints]           # ConstraintsRef
file = "catalytic.cnstr"

[puzzle.density]               # DensityRef: a precomputed electron-density map
path = "map.ccp4"
format = "ccp4"                # optional
resolution = 2.5
grid_spacing = 0.5             # optional

[puzzle.reflns]                # ReflnsRef: structure factors (sf.cif)
path = "sf.cif"
space_group = 19               # optional override
```

`[puzzle.reflns]` (structure factors, computed into a map) supersedes a
static `[puzzle.density]` when both are present; the same reflections drive
both the R-free objective and the `elec_dens` fit (`puzzle_toml.rs:143`,
`puzzle_load.rs`).

### `[[puzzle.design_mask]]` — designable residues (`DesignMaskEntry`)

```toml
[[puzzle.design_mask]]
chain = "A"
can_design = "1-40||55-90||"   # inclusive start-end ranges, `||`-separated
```

One block per structure **chain** (`puzzle_toml.rs:157`). A chain with no
entry (for example, the ligand) is locked. The range syntax is inclusive
`start-end` groups joined by `||`, trailing `||` allowed.

### Dialogue: `sequence`, `branch`, and events

The tutorial flow is arrays of speech bubbles plus scripted events. A
`Bubble` (`puzzle_toml.rs:169`):

```toml
[[sequence]]
text = "Fold up the tertiary structure."
point_to = "level"            # what to highlight: "void", "clash", "score", "level", ...
point_to_index = 12           # optional; integer or string target
color = "science"             # "science", "game", "yellow", ...
button = "Next"               # optional custom advance-button label
alt_button = "Skip"           # optional; presents a choice
alt_next = "branch_301"       # jump to a [[branch.<name>]] chain on the alt choice
no_repeat = true              # show at most once
trigger = "..."               # advance-trigger name
image = "hint.png"
link_name = "wiki"
link_url = "https://..."
```

- `[[sequence]]` is the main flow; `[[sequence_alt]]` is a secondary chain;
  `[[branch.<name>]]` are named chains jumped to via a bubble's `alt_next`.
- `[[on_event]]` is an `EventBubble` (`puzzle_toml.rs:187`): a bubble fired
  on a named `event` (with optional `once`, `threshold`), e.g. a bubble
  shown on `"level_complete"`.
- `[[game_event]]` is a `GameEvent` (`puzzle_toml.rs:197`): a scripted
  action keyed by `type`, gated by `condition`/`parameter`, with any extra
  fields (e.g. `type = "show_voids"`, `show_voids = false`).

## The legacy format (what you convert from)

Pre-TOML puzzles are **not** one file. A level is a fan of companion files
sharing the stem `<id>.ir_puzzle`, distinguished by extension. The sources
are on disk (944 files) at:

```
plugins/rosetta/deps/rosetta-interactive/resources/levels/
```

| Legacy file | Carries | New home |
| --- | --- | --- |
| `<id>.ir_puzzle.intro` | JSON metadata: title, camera, `start_score`/`start_energy`, `completion_score`, `view_setup`, dialogue bubbles + events, and `*_sum` MD5 checksums of the companion files | `[puzzle]` + `[puzzle.camera]` + the dialogue arrays |
| `.pdb` / `.opdb` | Starting coordinates (`o` = encrypted) | `[puzzle.structure]` |
| `.params` | Rosetta ligand/residue-type topology | `[[puzzle.ligand]]` |
| `.puzzle_setup` | Per-residue lock/design model (see below) | `[[puzzle.design_mask]]` (only `can_design`; the rest is dropped) |
| `.filters` | Win conditions | `[[puzzle.filter]]` |
| `.wts` / `.wts_patch` | Full scorefunction / weight patch | `[puzzle.weights]` (patch); a full `.wts` has no first-class home |
| `.cnstr` / `.constraints` | Catalytic constraints (`## VIZTHRESH` header) | `[puzzle.constraints]` |
| `.density` / `.sf` | Electron density / structure factors | `[puzzle.density]` / `[puzzle.reflns]` |
| `.second.pdb`, `.template.pdb`, `.align`, `.sym`, `.contact_heatmap`, ... | Guide poses, threading templates, symmetry, etc. | No new equivalent (the contact-heatmap channel is already dead in the legacy client) |

The `.puzzle_setup` file itself is a versioned dictionary, not Rosetta
flags: a `version: 1` line, then a `{ ... }` block of `"key" : "value"`
pairs. Beyond `can_design` it encodes an entire lock/action/GUI-gating
layer (`backbone_locked`, `sidechain_locked`, `banned_actions`,
`actions/*`, `gui/*`, tool toggles, length bounds). **None of that layer
survives into `puzzle.toml`** — the new format expresses only the
designable-residue mask, not the full lock model.

A real `.filters` object looks like:

```
version: 1
{
 "type" : "RMSDToGuide"
 "start_res" : "1"
 "stop_res" : "90"
 "value_per_distance" : "50000.0"
 "max_distance" : "20"
}
```

(one brace object per filter, values all double-quoted, no commas).

### Two things the new format cannot represent

- **Multi-stage puzzles.** A legacy level with `.2.puzzle_setup`,
  `.2.params`, etc. is a puzzle with multiple *starting structures* the
  loader stacked in parallel. `puzzle.toml`'s `structure` is a single
  `StructureRef`, and the `sequence`/`branch` system is *dialogue*, not
  structure staging. Old multi-pose puzzles have no clean target; convert
  the primary stage and handle the rest out-of-band.
- **The lock model and action/GUI gating** from `.puzzle_setup`, and
  `narrative_scenes` from `.intro`. Dropped entirely.

## Converting a legacy puzzle

### Step 1 — run the automated transcriber

`cargo xtask transcribe-filters` (`xtask/src/filters_transcribe.rs`)
handles the two mechanical, error-prone parts across every curated level:

1. It parses each `<id>.ir_puzzle.filters` object and writes a
   `[[puzzle.filter]]` block into the matching `assets/levels/<id>/puzzle.toml`.
   `type` becomes the filter kind; every other key is retained in the
   block. Kinds in `NATIVE_TYPES` (currently just `ExposedCount`) are
   emitted with no `plugin`; all others get `plugin = "rosetta"`.
2. It ports the `can_design` mask out of `<id>.ir_puzzle.puzzle_setup` into
   `[[puzzle.design_mask]]` blocks.

The pass is idempotent (it clears any existing `filter` array first, and
drops the obsolete `[[puzzle.objective]]` array it replaced) and surgical
(`toml_edit` preserves the rest of each file — tables, comments, ordering).

> **The design-mask numbering subtlety.** Legacy `can_design` is in Rosetta
> **pose numbering**: one continuous `1..N` count over every residue in the
> pose, chains concatenated in structure order. The target
> `[[puzzle.design_mask]]` is **per-chain PDB numbering**. Because the new
> structure PDBs renumber per chain inconsistently, the transcriber maps a
> pose index through the structure's residue *order* (the i-th distinct
> residue in `structure.pdb` is pose index i) — never by arithmetic on PDB
> numbers — then groups the result by chain into `start-end||` ranges. If
> you hand-convert a design mask, follow the same order-based mapping.

### Step 2 — port the rest by hand

The transcriber does not touch metadata, structure, camera, assets, or
dialogue. Map those from the legacy `.intro` (and the asset companions)
using this table:

| Legacy source | `puzzle.toml` |
| --- | --- |
| `.intro` `title` | `[puzzle] title` |
| `.intro` `start_energy` | `[puzzle] start_energy` |
| `.intro` `completion_score` | `[puzzle] completion_score` |
| `.intro` camera `cam_center` / `cam_eye` / `cam_up` (`"x y z"` strings) | `[puzzle.camera] center` / `eye` / `up` (float arrays) |
| `.intro` `view_setup` | `[puzzle] view_setup` — informational only (absorbed into `extra`) |
| `.pdb` | `[puzzle.structure] path` + `format` (add `ss` if pinning SS) |
| `.wts_patch` term overrides | `[puzzle.weights]` map |
| `.params` | `[[puzzle.ligand]] params` |
| `.cnstr` | `[puzzle.constraints] file` |
| `.density` / `.sf` | `[puzzle.density]` / `[puzzle.reflns]` |
| `.intro` `text_bubbles[]` | `[[sequence]]` (+ `[[branch.*]]` / `[[sequence_alt]]`) |
| `.intro` `puzzle_events[]` | `[[game_event]]` / `[[on_event]]` |

Bubble and event fields map name-for-name from the legacy JSON parsed by
`level_util.cc` (in the Rosetta source): a bubble's `text_to_display` →
`text`, `target_to_point_to` (an `LBL_*` enum) → `point_to` (lowercased
tail, e.g. `LBL_VOID` → `"void"`), `color` → `color`,
`unique_button_text` → `button`, `goto`/param jumps → `alt_next` naming a
`[[branch.<name>]]`. Events map `event_condition` → `condition`,
`event_parameter` → `parameter`, `event_trigger_once` → `once`.

### Step 3 — verify

Load the level (`cargo run --release <id>`) and confirm it parses and the
scene, camera, filters, and dialogue behave. `load_puzzle` returns a
`PuzzleError::Parse` on malformed TOML, but remember that an *unrecognized*
`[puzzle]` key does **not** error — it is silently swallowed by `extra`, so
a typo'd table key fails quietly. Cross-check unfamiliar keys against the
schema in `puzzle_toml.rs`.

## Lossy and unverified points

- `start_score` (game units) vs `start_energy` (Rosetta units) collapse to
  a single `start_energy`; the legacy game-unit start value is not carried.
- A full `.wts` scorefunction file has no first-class home; only a
  `.wts_patch`-style term override maps to `[puzzle.weights]`. How the
  `scorefxn` string selects a base scorefunction is not documented here.
- The complete legacy filter-kind catalog and each kind's parameter names
  live in the Rosetta `Filter` sources, which are not in this repository.
  The transcriber preserves whatever keys a `.filters` object carries, so
  the round-trip is faithful even for kinds not listed above; to author a
  new filter by hand, copy a shipped level using the same `type`.
