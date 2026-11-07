"""Менеджер памяти на FAISS — для нативного запуска без Docker/Qdrant."""
import json
import os
from pathlib import Path

import faiss
import numpy as np

from . import config
from .embeddings import get_embedding

DATA_DIR = Path(os.getenv("DATA_DIR", "./data"))
INDEX_PATH = DATA_DIR / "memory.index"
META_PATH = DATA_DIR / "memory_meta.json"


class MemoryManager:
    def __init__(self):
        self.dimension = 768  # nomic-embed-text
        self.index: faiss.IndexFlatIP | None = None
        self.metadata: list[dict] = []
        DATA_DIR.mkdir(parents=True, exist_ok=True)
        self._load()

    def _load(self):
        """Загрузить индекс и метаданные с диска."""
        if INDEX_PATH.exists() and META_PATH.exists():
            self.index = faiss.read_index(str(INDEX_PATH))
            self.metadata = json.loads(META_PATH.read_text(encoding="utf-8"))
        else:
            self.index = faiss.IndexFlatIP(self.dimension)
            self.metadata = []

    def _save(self):
        """Сохранить индекс и метаданные на диск."""
        faiss.write_index(self.index, str(INDEX_PATH))
        META_PATH.write_text(
            json.dumps(self.metadata, ensure_ascii=False, indent=2), encoding="utf-8"
        )

    def _chunk_text(self, text: str) -> list[str]:
        """Разбить текст на чанки с перекрытием."""
        chunks = []
        start = 0
        while start < len(text):
            end = start + config.MEMORY_CHUNK_SIZE
            chunk = text[start:end]
            if chunk.strip():
                chunks.append(chunk.strip())
            start += config.MEMORY_CHUNK_SIZE - config.MEMORY_CHUNK_OVERLAP
        return chunks

    async def add_text(
        self, text: str, user_id: str, character_id: str, metadata: dict = None
    ):
        """Добавить текст в память: чанкинг → эмбеддинг → FAISS."""
        chunks = self._chunk_text(text)
        vectors = []
        for chunk in chunks:
            embedding = await get_embedding(chunk)
            vectors.append(embedding)
            payload = {
                "user_id": user_id,
                "character_id": character_id,
                "text": chunk,
                **(metadata or {}),
            }
            self.metadata.append(payload)

        if vectors:
            arr = np.array(vectors, dtype=np.float32)
            # Normalize for cosine similarity via Inner Product
            faiss.normalize_L2(arr)
            self.index.add(arr)
            self._save()

    async def search(
        self, query: str, user_id: str, character_id: str, limit: int = None
    ) -> list[str]:
        """Найти релевантные воспоминания."""
        limit = limit or config.MEMORY_MAX_CHUNKS
        if self.index.ntotal == 0:
            return []

        query_embedding = await get_embedding(query)
        query_vec = np.array([query_embedding], dtype=np.float32)
        faiss.normalize_L2(query_vec)

        # Fetch more, then filter by user_id/character_id
        search_k = min(limit * 5, self.index.ntotal)
        scores, indices = self.index.search(query_vec, search_k)

        results = []
        for score, idx in zip(scores[0], indices[0]):
            if idx < 0 or idx >= len(self.metadata):
                continue
            meta = self.metadata[idx]
            if meta.get("user_id") == user_id and meta.get("character_id") == character_id:
                results.append(meta.get("text", ""))
            if len(results) >= limit:
                break

        return results

    async def add_auto_memory(
        self, user_message: str, bot_reply: str, user_id: str, character_id: str
    ):
        """Сохранить важные факты из диалога."""
        memory_text = f"Пользователь: {user_message}\nОтвет: {bot_reply}"
        if len(user_message) > 20:
            await self.add_text(
                memory_text, user_id, character_id, {"source": "dialogue"}
            )

    async def load_files(self, user_id: str, character_id: str) -> int:
        """Загрузить все .txt и .md файлы из memory/."""
        memory_dir = Path(config.MEMORY_DIR)
        if not memory_dir.exists():
            return 0

        total_chunks = 0
        for ext in ("*.txt", "*.md"):
            for filepath in memory_dir.glob(ext):
                text = filepath.read_text(encoding="utf-8")
                if text.strip():
                    await self.add_text(
                        text,
                        user_id,
                        character_id,
                        {"source": "file", "filename": filepath.name},
                    )
                    total_chunks += len(self._chunk_text(text))

        return total_chunks

    async def reload_character(self, character_id: str) -> int:
        """Перечитать character-файлы."""
        memory_dir = Path(config.MEMORY_DIR)
        character_files = [
            "character.txt",
            "relationship.txt",
            "preferences.txt",
            "world.txt",
            "rules.txt",
        ]

        # Remove old character_file entries
        keep_indices = []
        remove_indices = []
        for i, meta in enumerate(self.metadata):
            if meta.get("source") == "character_file" and meta.get("character_id") == character_id:
                remove_indices.append(i)
            else:
                keep_indices.append(i)

        if remove_indices:
            # Rebuild the index without the removed entries
            if keep_indices:
                all_vectors = self.index.reconstruct_n(0, self.index.ntotal)
                keep_vectors = np.array([all_vectors[i] for i in keep_indices], dtype=np.float32)
                self.index = faiss.IndexFlatIP(self.dimension)
                self.index.add(keep_vectors)
                self.metadata = [self.metadata[i] for i in keep_indices]
            else:
                self.index = faiss.IndexFlatIP(self.dimension)
                self.metadata = []

        # Load again
        total_chunks = 0
        for filename in character_files:
            filepath = memory_dir / filename
            if filepath.exists():
                text = filepath.read_text(encoding="utf-8")
                if text.strip():
                    await self.add_text(
                        text,
                        config.USER_ID,
                        character_id,
                        {"source": "character_file", "filename": filename},
                    )
                    total_chunks += len(self._chunk_text(text))

        return total_chunks
