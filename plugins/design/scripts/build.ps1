# Plugin-owned build recipe, invoked by `cargo xtask setup-plugins design`.
# Consumes the FOLDIT_* env contract xtask exports:
#   FOLDIT_PLUGIN_DIR          this plugin dir (absolute)
#   FOLDIT_LOCAL_DIR           <plugin_dir>/local (install target, gitignored)
#   FOLDIT_NATIVE_BINARY_NAME  decorated shared-library filename to install
#   FOLDIT_TARGET_TRIPLE       host target triple
#   FOLDIT_RECIPE_CLEAN        "1" to wipe the build dir first

$ErrorActionPreference = "Stop"

Set-Location $env:FOLDIT_PLUGIN_DIR

if ($env:FOLDIT_RECIPE_CLEAN -eq "1") {
    & cargo clean
    if ($LASTEXITCODE -ne 0) { exit 1 }
}

& cargo build --release
if ($LASTEXITCODE -ne 0) { exit 1 }

if (-not (Test-Path $env:FOLDIT_LOCAL_DIR)) {
    New-Item -ItemType Directory -Path $env:FOLDIT_LOCAL_DIR | Out-Null
}
Copy-Item "target\release\$env:FOLDIT_NATIVE_BINARY_NAME" `
    (Join-Path $env:FOLDIT_LOCAL_DIR $env:FOLDIT_NATIVE_BINARY_NAME)

Write-Host "installed $env:FOLDIT_NATIVE_BINARY_NAME -> $env:FOLDIT_LOCAL_DIR"
