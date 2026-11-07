import os
import re
from pathlib import Path

from qdrant_client import QdrantClient
from qdrant_client.models import Distance, VectorParams, PointStruct

from . import config
from .embeddings import get_embedding


class MemoryManager:
    def __init__(self):
        self.client = QdrantClient(url=config.QDRANT_URL, timeout=10.0)

    async def init_collection(self):
        """Создать коллекцию, если не существует."""
        collections = [c.name for c in self.client.get_collections().collections]
        if config.QDRANT_COLLECTION not in collections:
            self.client.create_collection(
                collection_name=config.QDRANT_COLLECTION,
                vectors_config=VectorParams(size=768, distance=Distance.COSINE),
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
        """Добавить текст в память: чанкинг → эмбеддинг → Qdrant."""
        chunks = self._chunk_text(text)
        points = []
        for i, chunk in enumerate(chunks):
            embedding = await get_embedding(chunk)
            payload = {
                "user_id": user_id,
                "character_id": character_id,
                "text": chunk,
                "chunk_index": i,
                **(metadata or {}),
            }
            # Unique ID from the content hash
            point_id = abs(hash(f"{user_id}:{character_id}:{chunk}")) % (2**63)
            points.append(PointStruct(id=point_id, vector=embedding, payload=payload))

        if points:
            self.client.upsert(collection_name=config.QDRANT_COLLECTION, points=points)

    async def search(
        self, query: str, user_id: str, character_id: str, limit: int = None
    ) -> list[str]:
        """Найти релевантные воспоминания."""
        limit = limit or config.MEMORY_MAX_CHUNKS
        query_embedding = await get_embedding(query)

        results = self.client.search(
            collection_name=config.QDRANT_COLLECTION,
            query_vector=query_embedding,
            query_filter={
                "must": [
                    {"key": "user_id", "match": {"value": user_id}},
                    {"key": "character_id", "match": {"value": character_id}},
                ]
            },
            limit=limit,
        )

        return [hit.payload.get("text", "") for hit in results]

    async def add_auto_memory(
        self, user_message: str, bot_reply: str, user_id: str, character_id: str
    ):
        """Сохранить важные факты из диалога (если AUTO_MEMORY=true)."""
        # Extract a short summary of the dialogue for memory
        memory_text = f"Пользователь: {user_message}\nОтвет: {bot_reply}"
        # Store only if the message is long enough (short ones carry no information)
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
        """Перечитать character-файлы (character.txt, relationship.txt и т.д.)."""
        memory_dir = Path(config.MEMORY_DIR)
        character_files = [
            "character.txt",
            "relationship.txt",
            "preferences.txt",
            "world.txt",
            "rules.txt",
        ]

        total_chunks = 0
        for filename in character_files:
            filepath = memory_dir / filename
            if filepath.exists():
                text = filepath.read_text(encoding="utf-8")
                if text.strip():
                    # Remove the old chunks of this file
                    self.client.delete(
                        collection_name=config.QDRANT_COLLECTION,
                        points_selector={
                            "filter": {
                                "must": [
                                    {"key": "character_id", "match": {"value": character_id}},
                                    {"key": "filename", "match": {"value": filename}},
                                ]
                            }
                        },
                    )
                    # Load again
                    await self.add_text(
                        text,
                        config.USER_ID,
                        character_id,
                        {"source": "character_file", "filename": filename},
                    )
                    total_chunks += len(self._chunk_text(text))

        return total_chunks
