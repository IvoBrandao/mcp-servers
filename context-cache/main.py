#!/usr/bin/env python3
"""
Enhanced MCP Key‑Value Cache Server
- Persistent, sandboxed JSON store
- TTL support (keys auto‑expire)
- TTL inspection without expiry
- Built‑in summarisation tool (model‑side)
- Semantic search (requires sentence‑transformers)
- Index management for paged context
- Compatible with mcp >= 0.9.0
"""

import json
import time
import argparse
from pathlib import Path
from typing import Any, Optional

from mcp.server import Server
from mcp.server.stdio import stdio_server
import mcp.types as types

# ----------------------------------------------------------------------
# Configuration
# ----------------------------------------------------------------------
ALLOWED_DIR: Path = Path.cwd()
KV_SUBDIR = "kv_store_enhanced"

HAS_EMBEDDINGS = False
try:
    from sentence_transformers import SentenceTransformer

    EMBED_MODEL = SentenceTransformer("all-MiniLM-L6-v2")
    HAS_EMBEDDINGS = True
except ImportError:
    EMBED_MODEL = None


def kv_dir() -> Path:
    d = ALLOWED_DIR / KV_SUBDIR
    d.mkdir(parents=True, exist_ok=True)
    return d


def safe_kv_path(key: str) -> Path:
    safe_key = "".join(c for c in key if c.isalnum() or c in "_-.")
    if not safe_key:
        raise ValueError("Invalid key")
    return (kv_dir() / f"{safe_key}.json").resolve()


def now_epoch() -> float:
    return time.time()


def load_entry(key: str) -> Optional[dict]:
    p = safe_kv_path(key)
    if not p.exists():
        return None
    try:
        with p.open("r", encoding="utf-8") as f:
            return json.load(f)
    except:
        return None


def save_entry(key: str, entry: dict):
    p = safe_kv_path(key)
    with p.open("w", encoding="utf-8") as f:
        json.dump(entry, f, indent=2, ensure_ascii=False)


def is_expired(entry: dict) -> bool:
    ttl = entry.get("ttl")
    if ttl is None:
        return False
    created = entry.get("created", 0)
    return (now_epoch() - created) > ttl


# ----------------------------------------------------------------------
# Server
# ----------------------------------------------------------------------
server = Server("kv-cache-enhanced")

# Tool definitions
TOOLS = [
    types.Tool(
        name="kv_set",
        description="Store a value with optional TTL (seconds).",
        inputSchema={
            "type": "object",
            "properties": {
                "key": {"type": "string"},
                "value": {},
                "ttl": {"type": "integer"},
            },
            "required": ["key", "value"],
        },
    ),
    types.Tool(
        name="kv_get",
        description="Retrieve a value. Returns EXPIRED if TTL elapsed.",
        inputSchema={
            "type": "object",
            "properties": {"key": {"type": "string"}},
            "required": ["key"],
        },
    ),
    types.Tool(
        name="kv_delete",
        description="Delete a key.",
        inputSchema={
            "type": "object",
            "properties": {"key": {"type": "string"}},
            "required": ["key"],
        },
    ),
    types.Tool(
        name="kv_list",
        description="List all non‑expired keys.",
        inputSchema={"type": "object", "properties": {}},
    ),
    types.Tool(
        name="kv_clear",
        description="Delete all keys.",
        inputSchema={"type": "object", "properties": {}},
    ),
    types.Tool(
        name="kv_inspect_ttl",
        description="Check remaining TTL of a key without triggering expiry.",
        inputSchema={
            "type": "object",
            "properties": {"key": {"type": "string"}},
            "required": ["key"],
        },
    ),
    types.Tool(
        name="kv_index_add",
        description="Add a page ID to the active index.",
        inputSchema={
            "type": "object",
            "properties": {"page_id": {"type": "string"}},
            "required": ["page_id"],
        },
    ),
    types.Tool(
        name="kv_index_remove",
        description="Remove a page ID from the active index.",
        inputSchema={
            "type": "object",
            "properties": {"page_id": {"type": "string"}},
            "required": ["page_id"],
        },
    ),
    types.Tool(
        name="kv_index_list",
        description="List all active page IDs.",
        inputSchema={"type": "object", "properties": {}},
    ),
    types.Tool(
        name="kv_summarize",
        description="Retrieve a value and return a prompt for the model to summarise it.",
        inputSchema={
            "type": "object",
            "properties": {
                "key": {"type": "string"},
                "max_length": {"type": "integer", "default": 200},
            },
            "required": ["key"],
        },
    ),
    types.Tool(
        name="kv_search",
        description="Semantic search (requires sentence-transformers).",
        inputSchema={
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "top_k": {"type": "integer", "default": 3},
            },
            "required": ["query"],
        },
    ),
]


# ---------- Handlers ----------
async def handle_kv_set(args: dict) -> list[types.TextContent]:
    try:
        entry = {"value": args["value"], "created": now_epoch(), "ttl": args.get("ttl")}
        save_entry(args["key"], entry)
        extra = f" (TTL: {args['ttl']}s)" if args.get("ttl") else ""
        return [types.TextContent(type="text", text=f"✅ Stored '{args['key']}'{extra}")]
    except Exception as e:
        return [types.TextContent(type="text", text=f"❌ {e}")]


async def handle_kv_get(args: dict) -> list[types.TextContent]:
    try:
        entry = load_entry(args["key"])
        if entry is None:
            return [types.TextContent(type="text", text="⚠️ Key not found")]
        if is_expired(entry):
            safe_kv_path(args["key"]).unlink()
            return [types.TextContent(type="text", text=f"⏰ Key '{args['key']}' expired and deleted.")]
        return [types.TextContent(type="text", text=json.dumps(entry["value"], indent=2))]
    except Exception as e:
        return [types.TextContent(type="text", text=f"❌ {e}")]


async def handle_kv_delete(args: dict) -> list[types.TextContent]:
    try:
        p = safe_kv_path(args["key"])
        if p.exists():
            p.unlink()
            return [types.TextContent(type="text", text=f"🗑️ Deleted '{args['key']}'")]
        return [types.TextContent(type="text", text="⚠️ Key not found")]
    except Exception as e:
        return [types.TextContent(type="text", text=f"❌ {e}")]


async def handle_kv_list(args: dict) -> list[types.TextContent]:
    try:
        keys = []
        for f in kv_dir().glob("*.json"):
            key = f.stem
            entry = load_entry(key)
            if entry and not is_expired(entry):
                keys.append(key)
            elif entry and is_expired(entry):
                f.unlink()
        if not keys:
            return [types.TextContent(type="text", text="(empty)")]
        return [types.TextContent(type="text", text="\n".join(sorted(keys)))]
    except Exception as e:
        return [types.TextContent(type="text", text=f"❌ {e}")]


async def handle_kv_clear(args: dict) -> list[types.TextContent]:
    try:
        for f in kv_dir().glob("*.json"):
            f.unlink()
        return [types.TextContent(type="text", text="🧹 All keys cleared.")]
    except Exception as e:
        return [types.TextContent(type="text", text=f"❌ {e}")]


async def handle_kv_inspect_ttl(args: dict) -> list[types.TextContent]:
    key = args["key"]
    entry = load_entry(key)
    if entry is None:
        return [types.TextContent(type="text", text="Key not found.")]
    ttl = entry.get("ttl")
    created = entry.get("created")
    if ttl is None:
        return [types.TextContent(type="text", text=f"Key '{key}' has no TTL (permanent).")]
    remaining = (created + ttl) - now_epoch()
    if remaining <= 0:
        return [types.TextContent(type="text", text=f"Key '{key}' has already expired.")]
    return [types.TextContent(type="text", text=f"Key '{key}' TTL: {ttl}s total, {remaining:.0f}s remaining.")]


async def handle_kv_index_add(args: dict) -> list[types.TextContent]:
    index_key = "___active_pages___"
    entry = load_entry(index_key)
    pages = entry["value"] if (entry and not is_expired(entry)) else []
    if args["page_id"] not in pages:
        pages.append(args["page_id"])
        save_entry(index_key, {"value": pages, "created": now_epoch(), "ttl": None})
    return [types.TextContent(type="text", text=f"📑 Active pages: {pages}")]


async def handle_kv_index_remove(args: dict) -> list[types.TextContent]:
    index_key = "___active_pages___"
    entry = load_entry(index_key)
    if not entry or is_expired(entry):
        return [types.TextContent(type="text", text="Index empty.")]
    pages = entry["value"]
    if args["page_id"] in pages:
        pages.remove(args["page_id"])
        save_entry(index_key, {"value": pages, "created": now_epoch(), "ttl": None})
    return [types.TextContent(type="text", text=f"📑 Active pages now: {pages}")]


async def handle_kv_index_list(args: dict) -> list[types.TextContent]:
    entry = load_entry("___active_pages___")
    if not entry or is_expired(entry):
        return [types.TextContent(type="text", text="(no active pages)")]
    pages = entry["value"]
    return [types.TextContent(type="text", text="\n".join(pages))]


async def handle_kv_summarize(args: dict) -> list[types.TextContent]:
    try:
        entry = load_entry(args["key"])
        if not entry:
            return [types.TextContent(type="text", text="⚠️ Key not found")]
        if is_expired(entry):
            safe_kv_path(args["key"]).unlink()
            return [types.TextContent(type="text", text="Key expired.")]
        content = json.dumps(entry["value"])
        max_len = args.get("max_length", 200)
        prompt = (
            f"Please summarise the following context in no more than {max_len} characters. "
            f"Focus on the key points and keep it concise. After summarising, store the result "
            f"using kv_set with a suitable key (e.g., 'summary_of_{args['key']}').\n\n"
            f"---CONTENT---\n{content}\n---END---"
        )
        return [types.TextContent(type="text", text=prompt)]
    except Exception as e:
        return [types.TextContent(type="text", text=f"❌ {e}")]


async def handle_kv_search(args: dict) -> list[types.TextContent]:
    if not HAS_EMBEDDINGS:
        return [
            types.TextContent(
                type="text", text="❌ Embeddings not available. Install sentence-transformers and restart the server."
            )
        ]
    try:
        query_embedding = EMBED_MODEL.encode(args["query"])
        results = []
        for f in kv_dir().glob("*.json"):
            key = f.stem
            entry = load_entry(key)
            if not entry or is_expired(entry):
                continue
            val_str = json.dumps(entry["value"])
            emb = EMBED_MODEL.encode(val_str)
            similarity = float(query_embedding @ emb)
            results.append((key, similarity))
        results.sort(key=lambda x: x[1], reverse=True)
        top = results[: args.get("top_k", 3)]
        if not top:
            return [types.TextContent(type="text", text="No similar keys found.")]
        output = "\n".join([f"{key} (sim: {sim:.2f})" for key, sim in top])
        return [types.TextContent(type="text", text=output)]
    except Exception as e:
        return [types.TextContent(type="text", text=f"❌ {e}")]


# ---------- MCP callbacks ----------
@server.list_tools()
async def list_tools() -> list[types.Tool]:
    return TOOLS


@server.call_tool()
async def call_tool(name: str, arguments: dict) -> list[types.TextContent]:
    handlers = {
        "kv_set": handle_kv_set,
        "kv_get": handle_kv_get,
        "kv_delete": handle_kv_delete,
        "kv_list": handle_kv_list,
        "kv_clear": handle_kv_clear,
        "kv_inspect_ttl": handle_kv_inspect_ttl,
        "kv_index_add": handle_kv_index_add,
        "kv_index_remove": handle_kv_index_remove,
        "kv_index_list": handle_kv_index_list,
        "kv_summarize": handle_kv_summarize,
        "kv_search": handle_kv_search,
    }
    handler = handlers.get(name)
    if not handler:
        return [types.TextContent(type="text", text=f"Unknown tool: {name}")]
    return await handler(arguments)


# ----------------------------------------------------------------------
# Main
# ----------------------------------------------------------------------
async def main():
    global ALLOWED_DIR
    parser = argparse.ArgumentParser(description="Enhanced MCP KV‑Cache Server")
    parser.add_argument("--root", type=str, default=str(Path.cwd()), help="Allowed root directory")
    args = parser.parse_args()
    ALLOWED_DIR = Path(args.root).resolve()
    if not ALLOWED_DIR.exists():
        print(f"Root directory does not exist: {ALLOWED_DIR}", flush=True)
        return

    async with stdio_server() as (read_stream, write_stream):
        await server.run(read_stream, write_stream, server.create_initialization_options())


if __name__ == "__main__":
    import asyncio

    asyncio.run(main())
