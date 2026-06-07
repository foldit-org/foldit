#!/usr/bin/env python3
"""Fail if any workspace-member Rust source file exceeds the line gate.

Walks only the six root-workspace members' `src` trees. The excluded
submodule directories (crates/viso, crates/molex, and the bulk of
crates/foldit-runner) are NOT members and are deliberately skipped; only
crates/foldit-runner/python-host (a real member that happens to live inside
the submodule dir) is included.
"""
import os
import sys

MAX_LINES = 800
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
bad = [
    (f, sum(1 for _ in open(f, encoding="utf-8", errors="replace")))
    for f in files
]
bad = [(f, n) for f, n in bad if n > MAX_LINES]
for f, n in sorted(bad, key=lambda x: -x[1]):
    print(f"ERROR: {f} has {n} lines (max {MAX_LINES})")
sys.exit(1 if bad else 0)
