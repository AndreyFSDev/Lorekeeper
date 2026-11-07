#Requires -Version 5.1
$ErrorActionPreference = "Stop"

$backendUrl = if ($env:BACKEND_URL) { $env:BACKEND_URL } else { "http://localhost:8000" }
$userId = if ($env:USER_ID) { $env:USER_ID } else { "default_user" }
$characterId = if ($env:CHARACTER_ID) { $env:CHARACTER_ID } else { "default" }

Write-Host "Loading memory files into vector DB..."

$form = @{
    user_id = $userId
    character_id = $characterId
}

$response = Invoke-RestMethod -Method Post -Uri "$backendUrl/memory/load-files" -Form $form
Write-Host "Response: $($response | ConvertTo-Json -Compress)"
Write-Host "Memory reindexed." -ForegroundColor Green
