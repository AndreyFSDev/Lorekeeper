#!/usr/bin/env bash
set -euo pipefail

echo "=== LocalChatbot Setup ==="

# Проверка Docker
if ! command -v docker &>/dev/null; then
    echo "ERROR: Docker not found. Install Docker first."
    exit 1
fi

# Проверка Docker Compose
if ! docker compose version &>/dev/null; then
    echo "ERROR: Docker Compose not found."
    exit 1
fi

# Копирование .env
if [ ! -f .env ]; then
    cp .env.example .env
    echo "Created .env from .env.example — review and adjust if needed."
else
    echo ".env already exists, skipping."
fi

# Запуск сервисов
echo "Starting services..."
docker compose up -d

echo "Waiting for Ollama to be ready..."
until curl -sf http://localhost:11434/api/tags >/dev/null 2>&1; do
    sleep 2
done
echo "Ollama is ready."

# Скачивание моделей
echo "Pulling models..."
./scripts/pull-models.sh

# Индексация памяти
echo "Indexing memory files..."
./scripts/reindex-memory.sh

echo ""
echo "=== Setup complete ==="
echo "Open WebUI:  http://localhost:3000"
echo "Backend API: http://localhost:8000/docs"
