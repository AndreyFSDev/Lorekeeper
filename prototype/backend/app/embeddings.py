import httpx
from . import config


async def get_embedding(text: str) -> list[float]:
    """Получить эмбеддинг текста через Ollama."""
    async with httpx.AsyncClient(timeout=60.0) as client:
        resp = await client.post(
            f"{config.OLLAMA_BASE_URL}/api/embeddings",
            json={"model": config.EMBED_MODEL, "prompt": text},
        )
        resp.raise_for_status()
        return resp.json()["embedding"]
