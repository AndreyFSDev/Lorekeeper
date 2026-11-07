#Requires -Version 5.1
$ErrorActionPreference = "Stop"

# Load .env if present
if (Test-Path .env) {
    Get-Content .env | ForEach-Object {
        if ($_ -match "^([^#][^=]+)=(.+)$") {
            [Environment]::SetEnvironmentVariable($matches[1].Trim(), $matches[2].Trim(), "Process")
        }
    }
}

$chatModel = if ($env:CHAT_MODEL) { $env:CHAT_MODEL } else { "qwen3-8b-abliterated-v2" }
$visionModel = if ($env:VISION_MODEL) { $env:VISION_MODEL } else { "qwen2.5vl:7b" }
$embedModel = if ($env:EMBED_MODEL) { $env:EMBED_MODEL } else { "nomic-embed-text" }

Write-Host "Pulling chat model: $chatModel"
docker exec lc-ollama ollama pull $chatModel

Write-Host "Pulling vision model: $visionModel"
docker exec lc-ollama ollama pull $visionModel

Write-Host "Pulling embedding model: $embedModel"
docker exec lc-ollama ollama pull $embedModel

Write-Host "All models pulled." -ForegroundColor Green
