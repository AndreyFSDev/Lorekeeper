import base64
from contextlib import asynccontextmanager
from pathlib import Path

from fastapi import FastAPI, File, UploadFile, Form
from fastapi.middleware.cors import CORSMiddleware
from fastapi.responses import FileResponse
from fastapi.staticfiles import StaticFiles
from pydantic import BaseModel

from . import config
from .chat import generate_chat_response
from .memory_native import MemoryManager
from .vision import describe_image

memory_manager = MemoryManager()


@asynccontextmanager
async def lifespan(app: FastAPI):
    yield


app = FastAPI(title="LocalChatbot", lifespan=lifespan)

app.add_middleware(
    CORSMiddleware,
    allow_origins=["*"],
    allow_methods=["*"],
    allow_headers=["*"],
)

# Static file serving (web UI)
STATIC_DIR = Path(__file__).parent / "static"
app.mount("/static", StaticFiles(directory=str(STATIC_DIR)), name="static")


@app.get("/")
async def root():
    return FileResponse(str(STATIC_DIR / "index.html"))


# --- Request/response models ---

class ChatRequest(BaseModel):
    message: str
    user_id: str = config.USER_ID
    character_id: str = config.CHARACTER_ID


class ChatResponse(BaseModel):
    reply: str
    memory_used: list[str] = []


class MemoryAddRequest(BaseModel):
    text: str
    user_id: str = config.USER_ID
    character_id: str = config.CHARACTER_ID
    metadata: dict = {}


class MemorySearchRequest(BaseModel):
    query: str
    user_id: str = config.USER_ID
    character_id: str = config.CHARACTER_ID
    limit: int = config.MEMORY_MAX_CHUNKS


class CharacterReloadRequest(BaseModel):
    character_id: str = config.CHARACTER_ID


# --- Endpoints ---

@app.get("/health")
async def health():
    return {"status": "ok", "chat_model": config.CHAT_MODEL, "vision_model": config.VISION_MODEL}


@app.get("/models")
async def models():
    return {
        "chat": config.CHAT_MODEL,
        "chat_fallback": config.CHAT_MODEL_FALLBACK,
        "vision": config.VISION_MODEL,
        "embeddings": config.EMBED_MODEL,
        "context_size": config.CONTEXT_SIZE,
    }


@app.post("/chat", response_model=ChatResponse)
async def chat(req: ChatRequest):
    # Search relevant memory
    memories = await memory_manager.search(req.message, req.user_id, req.character_id)

    # Generate the response
    reply = await generate_chat_response(
        user_message=req.message,
        memory_context=memories,
        user_id=req.user_id,
        character_id=req.character_id,
    )

    # Auto-save to memory
    if config.AUTO_MEMORY:
        await memory_manager.add_auto_memory(req.message, reply, req.user_id, req.character_id)

    return ChatResponse(reply=reply, memory_used=memories)


@app.post("/chat/image", response_model=ChatResponse)
async def chat_with_image(
    message: str = Form(...),
    image: UploadFile = File(...),
    user_id: str = Form(default=config.USER_ID),
    character_id: str = Form(default=config.CHARACTER_ID),
):
    # Decode the image
    image_bytes = await image.read()
    image_b64 = base64.b64encode(image_bytes).decode("utf-8")

    # Vision: describe the image (sequentially, not in parallel — 8 GB VRAM)
    vision_description = await describe_image(image_b64)

    # Memory search
    memories = await memory_manager.search(message, user_id, character_id)

    # Generate the response with vision context
    reply = await generate_chat_response(
        user_message=message,
        memory_context=memories,
        vision_context=vision_description,
        user_id=user_id,
        character_id=character_id,
    )

    if config.AUTO_MEMORY:
        await memory_manager.add_auto_memory(message, reply, user_id, character_id)

    return ChatResponse(reply=reply, memory_used=memories)


@app.post("/memory/add")
async def memory_add(req: MemoryAddRequest):
    await memory_manager.add_text(req.text, req.user_id, req.character_id, req.metadata)
    return {"status": "added"}


@app.post("/memory/search")
async def memory_search(req: MemorySearchRequest):
    results = await memory_manager.search(req.query, req.user_id, req.character_id, req.limit)
    return {"results": results}


@app.post("/memory/load-files")
async def memory_load_files(
    user_id: str = Form(default=config.USER_ID),
    character_id: str = Form(default=config.CHARACTER_ID),
):
    count = await memory_manager.load_files(user_id, character_id)
    return {"loaded_chunks": count}


@app.post("/character/reload")
async def character_reload(req: CharacterReloadRequest):
    count = await memory_manager.reload_character(req.character_id)
    return {"reloaded_chunks": count}
