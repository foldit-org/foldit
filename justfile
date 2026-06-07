# Use PowerShell on Windows (bash resolves to WSL on some machines)
set windows-shell := ["powershell.exe", "-NoLogo", "-Command"]

# Surface the full technical-debt picture (does NOT gate the build).
# Mirrors viso's `check-all` but every workspace lint is `warn`, so these
# recipes report debt and run to completion; they don't fail the build.
check-all: clippy deny machete file-lengths

# Clippy over every root-workspace member (gates: any warning is an error)
clippy:
    cargo clippy --workspace --all-targets -- -D warnings

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
