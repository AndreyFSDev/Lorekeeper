#!/usr/bin/env bash
set -euo pipefail

# Загрузить .env если есть
if [ -f .env ]; then
    set -a; source .env; set +a
fi

OLLAMA_URL="${OLLAMA_BASE_URL:-http://localhost:11434}"
# В docker-compose Ollama слушает на 11434 хоста
OLLAMA_URL="http://localhost:11434"

CHAT_MODEL="${CHAT_MODEL:-qwen3-8b-abliterated-v2}"
VISION_MODEL="${VISION_MODEL:-qwen2.5vl:7b}"
EMBED_MODEL="${EMBED_MODEL:-nomic-embed-text}"

echo "Pulling chat model: $CHAT_MODEL"
docker exec lc-ollama ollama pull "$CHAT_MODEL"

echo "Pulling vision model: $VISION_MODEL"
docker exec lc-ollama ollama pull "$VISION_MODEL"

echo "Pulling embedding model: $EMBED_MODEL"
docker exec lc-ollama ollama pull "$EMBED_MODEL"

echo "All models pulled."
