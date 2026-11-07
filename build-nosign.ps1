Set-Location $PSScriptRoot
# Portable build: bundling is off, the result is a single src-tauri/target/release/cloufam.exe.
# The updater signature is applied separately by tools/make-release.mjs (tauri signer sign).
pnpm tauri build --no-sign 2>&1
Write-Host "EXIT: $LASTEXITCODE"
exit $LASTEXITCODE

