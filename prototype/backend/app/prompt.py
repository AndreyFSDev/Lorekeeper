from . import config


def build_prompt(
    user_message: str,
    memory_context: list[str] = None,
    vision_context: str = None,
    chat_history: list[dict] = None,
    character_id: str = "default",
) -> list[dict]:
    """Многослойный prompt builder:
    1. System prompt — базовая роль
    2. Character prompt — из памяти (character.txt и т.д.)
    3. Memory context — найденные воспоминания
    4. Vision context — описание картинки
    5. Chat history — последние сообщения
    6. Current user message
    """
    messages = []

    # 1. System prompt
    system_parts = ["You are a helpful local AI assistant."]
    if character_id != "default":
        system_parts.append(f"Character ID: {character_id}")

    # 2 + 3. Memory (character + recollections)
    if memory_context:
        memory_block = "\n---\n".join(memory_context)
        system_parts.append(f"\nRelevant memory context:\n{memory_block}")

    # 4. Vision context
    if vision_context:
        system_parts.append(f"\nImage description: {vision_context}")

    messages.append({"role": "system", "content": "\n".join(system_parts)})

    # 5. Chat history (last N messages to fit the context)
    if chat_history:
        # Cap the history so it does not exceed the context
        max_history = min(len(chat_history), 20)
        for msg in chat_history[-max_history:]:
            messages.append(msg)

    # 6. Current message
    messages.append({"role": "user", "content": user_message})

    return messages
