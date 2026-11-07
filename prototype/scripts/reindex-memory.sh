#!/usr/bin/env bash
set -euo pipefail

BACKEND_URL="${BACKEND_URL:-http://localhost:8000}"

echo "Loading memory files into vector DB..."
response=$(curl -sf -X POST "$BACKEND_URL/memory/load-files" \
    -F "user_id=${USER_ID:-default_user}" \
    -F "character_id=${CHARACTER_ID:-default}")

echo "Response: $response"
echo "Memory reindexed."
