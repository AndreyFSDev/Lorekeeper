import httpx
from . import config


async def describe_image(image_b64: str) -> str:
    """Описать изображение через Qwen2.5-VL. Вызывается последовательно —
    на 8 GB VRAM нельзя держать vision и chat модели одновременно."""
    async with httpx.AsyncClient(timeout=120.0) as client:
        resp = await client.post(
            f"{config.OLLAMA_BASE_URL}/api/generate",
            json={
                "model": config.VISION_MODEL,
                "prompt": "Describe this image in detail. What do you see?",
                "images": [image_b64],
                "stream": False,
            },
        )
        resp.raise_for_status()
        return resp.json()["response"]
