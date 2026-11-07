#Requires -Version 5.1
$ErrorActionPreference = "Stop"

$projectRoot = Split-Path -Parent $PSScriptRoot
$backendDir = Join-Path $projectRoot "backend"
$envFile = Join-Path $projectRoot ".env"

# Load .env
if (Test-Path $envFile) {
    Get-Content $envFile | ForEach-Object {
        if ($_ -match "^([^#][^=]+)=(.+)$") {
            [Environment]::SetEnvironmentVariable($matches[1].Trim(), $matches[2].Trim(), "Process")
        }
    }
}

# Check Ollama
try {
    $null = Invoke-RestMethod http://localhost:11434/api/tags -ErrorAction Stop
    Write-Host "[OK] Ollama running" -ForegroundColor Green
} catch {
    Write-Host "[!] Ollama not running. Start Ollama first." -ForegroundColor Red
    exit 1
}

# Create the data directory if missing
$dataDir = Join-Path $projectRoot "data"
if (-not (Test-Path $dataDir)) {
    New-Item -ItemType Directory -Path $dataDir | Out-Null
    Write-Host "[OK] Created data/ directory"
}

# Start the backend
Write-Host "Starting LocalChatbot backend on port $($env:BACKEND_PORT ?? '8000')..." -ForegroundColor Cyan
Set-Location $backendDir
python -m uvicorn app.main:app --host 0.0.0.0 --port ($env:BACKEND_PORT ?? 8000) --reload
