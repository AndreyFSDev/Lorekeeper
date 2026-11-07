#Requires -Version 5.1
Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

Write-Host "=== LocalChatbot Setup ===" -ForegroundColor Cyan

# Check Docker
if (-not (Get-Command docker -ErrorAction SilentlyContinue)) {
    Write-Host "ERROR: Docker not found. Install Docker Desktop first." -ForegroundColor Red
    exit 1
}

# Copy .env
if (-not (Test-Path .env)) {
    Copy-Item .env.example .env
    Write-Host "Created .env from .env.example — review and adjust if needed."
} else {
    Write-Host ".env already exists, skipping."
}

# Start services
Write-Host "Starting services..."
docker compose up -d

Write-Host "Waiting for Ollama to be ready..."
do {
    Start-Sleep -Seconds 2
    try { $null = Invoke-RestMethod http://localhost:11434/api/tags -ErrorAction Stop; $ready = $true }
    catch { $ready = $false }
} until ($ready)
Write-Host "Ollama is ready."

# Download models
Write-Host "Pulling models..."
& "$PSScriptRoot\pull-models.ps1"

# Index memory
Write-Host "Indexing memory files..."
& "$PSScriptRoot\reindex-memory.ps1"

Write-Host ""
Write-Host "=== Setup complete ===" -ForegroundColor Green
Write-Host "Open WebUI:  http://localhost:3000"
Write-Host "Backend API: http://localhost:8000/docs"
