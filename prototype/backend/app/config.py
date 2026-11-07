import os

# Models
CHAT_MODEL = os.getenv("CHAT_MODEL", "qwen3-8b-abliterated-v2")
CHAT_MODEL_FALLBACK = os.getenv("CHAT_MODEL_FALLBACK", "dolphin3-llama3.1-8b")
VISION_MODEL = os.getenv("VISION_MODEL", "qwen2.5vl:7b")
EMBED_MODEL = os.getenv("EMBED_MODEL", "nomic-embed-text")

# Context and generation
CONTEXT_SIZE = int(os.getenv("CONTEXT_SIZE", "4096"))
TEMPERATURE = float(os.getenv("TEMPERATURE", "0.8"))
TOP_P = float(os.getenv("TOP_P", "0.9"))
TOP_K = int(os.getenv("TOP_K", "40"))
REPEAT_PENALTY = float(os.getenv("REPEAT_PENALTY", "1.1"))
MAX_TOKENS = int(os.getenv("MAX_TOKENS", "600"))

# Memory
AUTO_MEMORY = os.getenv("AUTO_MEMORY", "true").lower() == "true"
MEMORY_MAX_CHUNKS = int(os.getenv("MEMORY_MAX_CHUNKS", "5"))
MEMORY_CHUNK_SIZE = int(os.getenv("MEMORY_CHUNK_SIZE", "500"))
MEMORY_CHUNK_OVERLAP = int(os.getenv("MEMORY_CHUNK_OVERLAP", "50"))

# Qdrant
QDRANT_URL = os.getenv("QDRANT_URL", "http://qdrant:6333")
QDRANT_COLLECTION = os.getenv("QDRANT_COLLECTION", "chatbot_memory")

# Ollama
OLLAMA_BASE_URL = os.getenv("OLLAMA_BASE_URL", "http://localhost:11434")

# Character
CHARACTER_ID = os.getenv("CHARACTER_ID", "default")
USER_ID = os.getenv("USER_ID", "default_user")

# Path to memory files
MEMORY_DIR = os.getenv("MEMORY_DIR", "/app/memory")
