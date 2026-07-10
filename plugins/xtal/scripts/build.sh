#!/usr/bin/env bash
# Plugin-owned build recipe, invoked by `cargo xtask setup-plugins xtal`.
# Consumes the FOLDIT_* env contract xtask exports:
#   FOLDIT_PLUGIN_DIR          this plugin dir (absolute)
#   FOLDIT_LOCAL_DIR           <plugin_dir>/local (install target, gitignored)
#   FOLDIT_NATIVE_BINARY_NAME  decorated shared-library filename to install
#   FOLDIT_TARGET_TRIPLE       host target triple
#   FOLDIT_RECIPE_CLEAN        "1" to wipe the build dir first
#
# Windows: xtask automatically selects scripts/build.ps1 instead.
set -euo pipefail

cd "$FOLDIT_PLUGIN_DIR"

if [ "${FOLDIT_RECIPE_CLEAN:-0}" = "1" ]; then
    cargo clean
fi

cargo build --release

mkdir -p "$FOLDIT_LOCAL_DIR"
cp "target/release/$FOLDIT_NATIVE_BINARY_NAME" \
   "$FOLDIT_LOCAL_DIR/$FOLDIT_NATIVE_BINARY_NAME"

echo "installed $FOLDIT_NATIVE_BINARY_NAME -> $FOLDIT_LOCAL_DIR"
