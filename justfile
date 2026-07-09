# Use PowerShell on Windows (bash resolves to WSL on some machines)
set windows-shell := ["powershell.exe", "-NoLogo", "-Command"]

# Run all gates. Workspace lints are `warn`, but `clippy` passes `-D warnings`,
# so this DOES fail on any lint. `fmt-check` is deliberately not a dependency:
# the root tree has never been rustfmt'd (see the `fmt` recipe).
check-all: clippy deny machete file-lengths

# Clippy over every root-workspace member (gates: any warning is an error)
clippy:
    cargo clippy --workspace --all-targets -- -D warnings

# Format check (nightly). The root has no rustfmt.toml and has never been
# formatted, so this reports a large diff. That is why `check-all` does not
# depend on it.
#
# Adopting viso/molex's rustfmt.toml verbatim was tried and reverted. If you
# attempt it again, know what it costs:
#   - `max_width = 80` rewraps one-line match arms into blocks, which trips
#     `clippy::semicolon_if_nothing_returned`, and inflates 7 functions past
#     the 100-line `clippy::too_many_lines` cap. viso survives 80 columns only
#     because viso's code was written at 80.
#   - `wrap_comments = true` reflows ~152 hand-aligned comment lines (ASCII
#     tables, aligned `->` maps) into prose. Set it to false.
#   - `error_on_line_overflow = true` can never pass: ~20 long lines live in
#     `macro_rules!` bodies, which rustfmt does not format.
fmt-check:
    cargo +nightly fmt --all --check

# Format (nightly). Reformats the whole workspace -- see `fmt-check` first.
fmt:
    cargo +nightly fmt --all

# Type-check the workspace (must stay green; warn-level lints don't break it)
check:
    cargo check --workspace

# Run tests
test:
    cargo test --workspace

# Build docs and surface rustdoc warnings
doc:
    cargo doc --workspace --no-deps --document-private-items

# Dependency advisory / license / source audit
deny:
    cargo deny check

# Unused-dependency report
machete:
    cargo machete

# File-length gate (max 800 lines over the six members' src trees)
file-lengths:
    python3 scripts/check_file_lengths.py

# Count clippy warnings across the workspace
warnings:
    cargo clippy --workspace --all-targets 2>&1 | rg '^warning' | wc -l

# Clippy violations per-rule, per-crate
lint:
    #!/usr/bin/env bash
    cargo clippy --workspace --all-targets --message-format=json 2>/dev/null \
    | python3 -c "
    import sys, json
    seen = set()
    rows = []
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            msg = json.loads(line)
        except ValueError:
            continue
        if msg.get('reason') != 'compiler-message':
            continue
        m = msg['message']
        code = m.get('code')
        if not code or not code.get('code'):
            continue
        rule = code['code']
        spans = m.get('spans', [])
        primary = next((s for s in spans if s.get('is_primary')), None)
        if not primary:
            continue
        f = primary['file_name']
        # Only report on root-owned crates. Patched path deps (viso, molex)
        # get dragged in by the [patch] blocks and, being local sources, are
        # not lint-capped, so rustc lints like dead_code leak through here.
        # They own their own gates (their own workspaces); skip them.
        if f.startswith('crates/') and not f.split('/')[1].startswith('foldit-'):
            continue
        key = (f, primary.get('line_start'), rule)
        if key in seen:
            continue
        seen.add(key)
        parts = f.split('/')
        crate = parts[1] if f.startswith('crates/') and len(parts) > 1 else parts[0]
        rows.append((crate, rule))
    if not rows:
        print('No violations found.')
        sys.exit(0)
    from collections import Counter
    counts = Counter(rows)
    by_crate = {}
    for (crate, rule), n in counts.items():
        by_crate.setdefault(crate, []).append((rule, n))
    total = sum(counts.values())
    for crate in sorted(by_crate):
        rules = sorted(by_crate[crate], key=lambda x: -x[1])
        crate_total = sum(n for _, n in rules)
        print(f'\n  {crate} ({crate_total})')
        for rule, n in rules:
            print(f'    {n:3d}  {rule}')
    print(f'\n  total: {total}')
    "
