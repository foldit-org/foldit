#!/usr/bin/env python3
"""Fail if any workspace-member Rust source file exceeds the line gate.

Walks only the six root-workspace members' `src` trees. The excluded
submodule directories (crates/viso, crates/molex, and the bulk of
crates/foldit-runner) are NOT members and are deliberately skipped; only
crates/foldit-runner/python-host (a real member that happens to live inside
the submodule dir) is included.

A file can opt out of the gate by placing the sentinel comment
`// foldit:allow-long-file` within its first few lines (mirrors a clippy
`#[allow(...)]`, but for this non-clippy check). Reserve it for files
whose length is intrinsic (e.g. an exhaustive unit-test module), not as a
way to dodge a real split.
"""
import os
import sys

MAX_LINES = 800
SENTINEL = "foldit:allow-long-file"
SENTINEL_HEAD_LINES = 10
SRC_DIRS = [
    "crates/foldit-core/src",
    "crates/foldit-desktop/src",
    "crates/foldit-gui/src",
    "crates/foldit-runner/python-host/src",
    "crates/foldit-web/src",
    "xtask/src",
]

files = [
    os.path.join(r, f)
    for d in SRC_DIRS
    for r, _, fs in os.walk(d)
    for f in fs
    if f.endswith(".rs") and "target" not in r
]


def over_limit(path):
    """Line count if the file exceeds the gate and is not exempt, else None."""
    with open(path, encoding="utf-8", errors="replace") as fh:
        lines = fh.readlines()
    if any(SENTINEL in ln for ln in lines[:SENTINEL_HEAD_LINES]):
        return None
    return len(lines) if len(lines) > MAX_LINES else None


bad = [(f, n) for f in files for n in [over_limit(f)] if n is not None]
for f, n in sorted(bad, key=lambda x: -x[1]):
    print(f"ERROR: {f} has {n} lines (max {MAX_LINES})")
sys.exit(1 if bad else 0)
