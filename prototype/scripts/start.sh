#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(dirname "$SCRIPT_DIR")"
BACKEND_DIR="$PROJECT_ROOT/backend"

# Загрузить .env
if [ -f "$PROJECT_ROOT/.env" ]; then
    set -a; source "$PROJECT_ROOT/.env"; set +a
fi

# Проверить Ollama
if ! curl -sf http://localhost:11434/api/tags >/dev/null 2>&1; then
    echo "[!] Ollama not running. Start Ollama first."
    exit 1
fi
echo "[OK] Ollama running"

# Создать папку data если нет
mkdir -p "$PROJECT_ROOT/data"

# Запустить backend
PORT="${BACKEND_PORT:-8000}"
echo "Starting LocalChatbot backend on port $PORT..."
cd "$BACKEND_DIR"
python -m uvicorn app.main:app --host 0.0.0.0 --port "$PORT" --reload
