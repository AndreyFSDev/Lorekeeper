# The updater signing key comes from the environment: TAURI_SIGNING_PRIVATE_KEY (contents)
# or CLOUFAM_SIGNING_KEY_PATH (path to a file). Tauri reads exactly the contents —
# a path in TAURI_SIGNING_PRIVATE_KEY silently produces an unsigned build.
if (-not $env:TAURI_SIGNING_PRIVATE_KEY) {
  $keyPath = $env:CLOUFAM_SIGNING_KEY_PATH
  if (-not $keyPath -or -not (Test-Path -LiteralPath $keyPath)) {
    throw "нет ключа подписи: задайте TAURI_SIGNING_PRIVATE_KEY или CLOUFAM_SIGNING_KEY_PATH — без него сборка не даст .sig и обновления не примутся"
  }
  $env:TAURI_SIGNING_PRIVATE_KEY = (Get-Content -LiteralPath $keyPath -Raw).Trim()
}
if ($null -eq $env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD) {
  $env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD = ""
}
Set-Location (Split-Path $PSScriptRoot -Parent)
pnpm tauri build
if ($LASTEXITCODE -ne 0) {
  throw "pnpm tauri build завершился с кодом $LASTEXITCODE"
}


