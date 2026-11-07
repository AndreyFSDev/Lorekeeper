import httpx
from . import config
from .prompt import build_prompt


async def generate_chat_response(
    user_message: str,
    memory_context: list[str] = None,
    vision_context: str = None,
    chat_history: list[dict] = None,
    user_id: str = "default",
    character_id: str = "default",
) -> str:
    """Сгенерировать ответ через Ollama. Использует основную модель,
    при ошибке — fallback."""
    messages = build_prompt(
        user_message=user_message,
        memory_context=memory_context,
        vision_context=vision_context,
        chat_history=chat_history,
        character_id=character_id,
    )

    payload = {
        "model": config.CHAT_MODEL,
        "messages": messages,
        "stream": False,
        "options": {
            "temperature": config.TEMPERATURE,
            "top_p": config.TOP_P,
            "top_k": config.TOP_K,
            "repeat_penalty": config.REPEAT_PENALTY,
            "num_predict": config.MAX_TOKENS,
            "num_ctx": config.CONTEXT_SIZE,
        },
    }

    async with httpx.AsyncClient(timeout=180.0) as client:
        try:
            resp = await client.post(
                f"{config.OLLAMA_BASE_URL}/api/chat", json=payload
            )
            resp.raise_for_status()
            return resp.json()["message"]["content"]
        except Exception:
            # Fallback to the backup model
            payload["model"] = config.CHAT_MODEL_FALLBACK
            resp = await client.post(
                f"{config.OLLAMA_BASE_URL}/api/chat", json=payload
            )
            resp.raise_for_status()
            return resp.json()["message"]["content"]
