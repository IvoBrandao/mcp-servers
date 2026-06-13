#!/usr/bin/env python3
"""
Enhanced MCP Key-Value Cache Server (v3)
- Async I/O, session isolation, binary support, streaming, TTL cleanup
- Nested keys, batch operations, export/import, compression
- Optional SQLite backend for better performance
- Token counting, watch for changes, configurable limits
"""

import os
import json
import time
import zlib
import base64
import asyncio
import argparse
from contextlib import asynccontextmanager
from pathlib import Path
from typing import Any, Optional, Dict, List, Tuple, Set

import aiofiles
import aiofiles.os

# MCP
from mcp.server.fastmcp import FastMCP, Context

# Optional dependencies
try:
    import sqlite3

    SQLITE_AVAILABLE = True
except ImportError:
    SQLITE_AVAILABLE = False

try:
    from sentence_transformers import SentenceTransformer

    HAS_EMBEDDINGS = True
    EMBED_MODEL = SentenceTransformer("all-MiniLM-L6-v2")
except ImportError:
    HAS_EMBEDDINGS = False
    EMBED_MODEL = None

try:
    import tiktoken

    TIKTOKEN_AVAILABLE = True
except ImportError:
    TIKTOKEN_AVAILABLE = False

# ----------------------------------------------------------------------
# Configuration
# ----------------------------------------------------------------------
ALLOWED_DIR: Path = Path.cwd().resolve()
USE_SQLITE: bool = False  # set by command line
MAX_KEY_LEN: int = 256
MAX_VALUE_SIZE: int = 10 * 1024 * 1024  # 10 MB
MAX_KEYS_PER_SESSION: int = 10000
TTL_CLEANUP_INTERVAL: int = 60  # seconds
COMPRESS_THRESHOLD: int = 1024  # bytes – compress if larger

# Sessions (each session has its own store directory)
_sessions: Dict[str, Path] = {}  # session_id -> store_path
# For SQLite mode, keep connections per session
_sqlite_conns: Dict[str, "sqlite3.Connection"] = {}  # if USE_SQLITE

# Watch changes dict: session_id -> list of change messages
_watch_changes: Dict[str, List[str]] = {}


# ----------------------------------------------------------------------
# Helpers
# ----------------------------------------------------------------------
def get_session_dir(session_id: Optional[str]) -> Path:
    """Return the store directory for a session (creates if needed)."""
    if session_id is None:
        session_id = "default"
    if session_id not in _sessions:
        base = ALLOWED_DIR / "kv_stores" / session_id
        base.mkdir(parents=True, exist_ok=True)
        _sessions[session_id] = base
    return _sessions[session_id]


def get_sqlite_db(session_id: str) -> "sqlite3.Connection":
    """Return SQLite connection for a session (creates table if needed)."""
    if session_id not in _sqlite_conns:
        db_path = get_session_dir(session_id) / "kv_store.db"
        conn = sqlite3.connect(str(db_path), check_same_thread=False)
        conn.row_factory = sqlite3.Row
        conn.execute("""
            CREATE TABLE IF NOT EXISTS kv (
                key TEXT PRIMARY KEY,
                value TEXT,
                created REAL,
                ttl REAL,
                compressed INTEGER DEFAULT 0
            )
        """)
        conn.execute("CREATE INDEX IF NOT EXISTS idx_ttl ON kv(ttl, created)")
        _sqlite_conns[session_id] = conn
    return _sqlite_conns[session_id]


def sanitize_key(key: str) -> str:
    """Ensure key is safe for filesystem (if not using SQLite)."""
    if len(key) > MAX_KEY_LEN:
        raise ValueError(f"Key too long: {len(key)} > {MAX_KEY_LEN}")
    if not key:
        raise ValueError("Empty key")
    if USE_SQLITE:
        # SQLite allows any key, but we still sanitize for safety
        return key
    # For file-based storage, replace unsafe characters
    unsafe = '/\\?%*:|"<>'
    safe = "".join(c if c not in unsafe else "_" for c in key)
    return safe


async def get_value_path(session_dir: Path, key: str) -> Path:
    """For file mode: return path to key's file (supports nested keys as subdirs)."""
    # Nested keys can use '.' or '/' as separator
    if "." in key:
        parts = key.split(".")
        *dirs, base = parts
        path = session_dir.joinpath(*dirs)
        path.mkdir(parents=True, exist_ok=True)
        return path / f"{base}.json"
    else:
        return session_dir / f"{key}.json"


def compress_if_needed(data: bytes) -> Tuple[bytes, bool]:
    """Compress data if size > COMPRESS_THRESHOLD. Returns (compressed, was_compressed)."""
    if len(data) > COMPRESS_THRESHOLD:
        return zlib.compress(data), True
    return data, False


def decompress_if_needed(data: bytes, was_compressed: bool) -> bytes:
    return zlib.decompress(data) if was_compressed else data


def now_epoch() -> float:
    return time.time()


async def load_entry_file(session_dir: Path, key: str) -> Optional[Dict]:
    path = await get_value_path(session_dir, key)
    if not await aiofiles.os.path.exists(path):
        return None
    try:
        async with aiofiles.open(path, "rb") as f:
            raw = await f.read()
        entry = json.loads(raw.decode("utf-8"))
        # Handle compressed value field
        if entry.get("compressed"):
            entry["value"] = zlib.decompress(base64.b64decode(entry["value"]))
            entry["value"] = json.loads(entry["value"].decode("utf-8"))
            del entry["compressed"]
        return entry
    except:
        return None


async def save_entry_file(session_dir: Path, key: str, entry: Dict):
    # If value is binary or large, compress and encode
    value = entry["value"]
    was_compressed = False
    if isinstance(value, (dict, list, str, int, float, bool, type(None))):
        # JSON-serializable
        value_str = json.dumps(value, ensure_ascii=False)
        value_bytes = value_str.encode("utf-8")
        if len(value_bytes) > COMPRESS_THRESHOLD:
            value_bytes, was_compressed = compress_if_needed(value_bytes)
        if was_compressed:
            entry["value"] = base64.b64encode(value_bytes).decode("ascii")
            entry["compressed"] = True
        else:
            entry["value"] = value_str
            entry.pop("compressed", None)
    else:
        # Assume bytes
        value_bytes = value if isinstance(value, bytes) else str(value).encode()
        if len(value_bytes) > COMPRESS_THRESHOLD:
            value_bytes, was_compressed = compress_if_needed(value_bytes)
        entry["value"] = base64.b64encode(value_bytes).decode("ascii")
        entry["compressed"] = True
    path = await get_value_path(session_dir, key)
    async with aiofiles.open(path, "w", encoding="utf-8") as f:
        await f.write(json.dumps(entry, indent=2))
    # Restore original value in entry (so caller sees it)
    if was_compressed:
        entry["value"] = value


async def delete_entry_file(session_dir: Path, key: str):
    path = await get_value_path(session_dir, key)
    if await aiofiles.os.path.exists(path):
        await aiofiles.os.remove(path)


async def list_keys_file(session_dir: Path) -> List[str]:
    keys = []
    # Walk recursively
    for root, _, files in os.walk(session_dir):
        for f in files:
            if f.endswith(".json"):
                rel = Path(root).relative_to(session_dir)
                key_stem = Path(f).stem
                if str(rel) != ".":
                    key = ".".join(rel.parts) + "." + key_stem
                else:
                    key = key_stem
                keys.append(key)
    return keys


def is_expired(entry: Dict) -> bool:
    ttl = entry.get("ttl")
    if ttl is None:
        return False
    created = entry.get("created", 0)
    return (now_epoch() - created) > ttl


# ----------------------------------------------------------------------
# Session cleanup background task
# ----------------------------------------------------------------------
async def cleanup_expired_entries():
    """Periodically delete expired keys from all sessions."""
    while True:
        await asyncio.sleep(TTL_CLEANUP_INTERVAL)
        for session_id, session_dir in _sessions.items():
            try:
                if USE_SQLITE:
                    conn = get_sqlite_db(session_id)
                    now = now_epoch()
                    conn.execute("DELETE FROM kv WHERE ttl IS NOT NULL AND (created + ttl) <= ?", (now,))
                    conn.commit()
                else:
                    for key in await list_keys_file(session_dir):
                        entry = await load_entry_file(session_dir, key)
                        if entry and is_expired(entry):
                            await delete_entry_file(session_dir, key)
            except Exception:
                pass


# ----------------------------------------------------------------------
# Core KV operations (abstracted over backend)
# ----------------------------------------------------------------------
async def _kv_set(session_id: Optional[str], key: str, value: Any, ttl: Optional[int] = None) -> bool:
    session_dir = get_session_dir(session_id)
    safe_key = sanitize_key(key)
    entry = {
        "value": value,
        "created": now_epoch(),
        "ttl": ttl,
    }
    if USE_SQLITE:
        conn = get_sqlite_db(session_id or "default")
        # Serialize value to JSON
        val_str = json.dumps(value, ensure_ascii=False)
        if len(val_str) > MAX_VALUE_SIZE:
            raise ValueError("Value too large")
        compressed = False
        if len(val_str) > COMPRESS_THRESHOLD:
            compressed_bytes = zlib.compress(val_str.encode())
            val_str = base64.b64encode(compressed_bytes).decode("ascii")
            compressed = True
        conn.execute(
            "INSERT OR REPLACE INTO kv (key, value, created, ttl, compressed) VALUES (?, ?, ?, ?, ?)",
            (safe_key, val_str, entry["created"], ttl, 1 if compressed else 0),
        )
        conn.commit()
        return True
    else:
        await save_entry_file(session_dir, safe_key, entry)
        return True


async def _kv_get(
    session_id: Optional[str], key: str, stream: bool = False, context: Any = None
) -> Tuple[Optional[Any], bool]:
    session_dir = get_session_dir(session_id)
    safe_key = sanitize_key(key)
    if USE_SQLITE:
        conn = get_sqlite_db(session_id or "default")
        row = conn.execute("SELECT value, created, ttl, compressed FROM kv WHERE key = ?", (safe_key,)).fetchone()
        if not row:
            return None, False
        val_str = row["value"]
        if row["compressed"]:
            val_str = zlib.decompress(base64.b64decode(val_str)).decode()
        value = json.loads(val_str)
        entry = {"value": value, "created": row["created"], "ttl": row["ttl"]}
    else:
        entry = await load_entry_file(session_dir, safe_key)
        if not entry:
            return None, False
        value = entry["value"]
    if is_expired(entry):
        await _kv_delete(session_id, key)
        return None, True  # expired
    # Streaming: send chunks via log messages
    if stream and context:
        value_str = json.dumps(value, indent=2)
        chunk_size = 4096
        for i in range(0, len(value_str), chunk_size):
            await context.info(value_str[i : i + chunk_size])
        return "[Streaming complete]", False
    return value, False


async def _kv_delete(session_id: Optional[str], key: str) -> bool:
    session_dir = get_session_dir(session_id)
    safe_key = sanitize_key(key)
    if USE_SQLITE:
        conn = get_sqlite_db(session_id or "default")
        conn.execute("DELETE FROM kv WHERE key = ?", (safe_key,))
        conn.commit()
        return True
    else:
        await delete_entry_file(session_dir, safe_key)
        return True


async def _kv_list(session_id: Optional[str]) -> List[str]:
    session_dir = get_session_dir(session_id)
    if USE_SQLITE:
        conn = get_sqlite_db(session_id or "default")
        rows = conn.execute("SELECT key FROM kv").fetchall()
        keys = [row["key"] for row in rows]
    else:
        keys = await list_keys_file(session_dir)
    # Filter expired in file mode (sqlite already filtered? We'll re-check)
    valid = []
    for k in keys:
        val, expired = await _kv_get(session_id, k)
        if not expired and val is not None:
            valid.append(k)
    return valid


async def _kv_clear(session_id: Optional[str]) -> int:
    count = 0
    for key in await _kv_list(session_id):
        if await _kv_delete(session_id, key):
            count += 1
    return count


async def _kv_inspect_ttl(session_id: Optional[str], key: str) -> Optional[Dict]:
    session_dir = get_session_dir(session_id)
    safe_key = sanitize_key(key)
    if USE_SQLITE:
        conn = get_sqlite_db(session_id or "default")
        row = conn.execute("SELECT created, ttl FROM kv WHERE key = ?", (safe_key,)).fetchone()
        if not row:
            return None
        created = row["created"]
        ttl = row["ttl"]
    else:
        entry = await load_entry_file(session_dir, safe_key)
        if not entry:
            return None
        created = entry.get("created")
        ttl = entry.get("ttl")
    if ttl is None:
        return {"key": key, "ttl": None, "remaining": None}
    remaining = (created + ttl) - now_epoch()
    return {"key": key, "ttl": ttl, "remaining": max(0, remaining), "expired": remaining <= 0}


# ----------------------------------------------------------------------
# Batch operations
# ----------------------------------------------------------------------
async def _kv_batch_set(session_id: Optional[str], items: List[Dict]) -> List[Dict]:
    """items: [{"key": "k1", "value": v, "ttl": optional}, ...]"""
    results = []
    for item in items:
        try:
            await _kv_set(session_id, item["key"], item["value"], item.get("ttl"))
            results.append({"key": item["key"], "status": "ok"})
        except Exception as e:
            results.append({"key": item["key"], "status": "error", "error": str(e)})
    return results


async def _kv_batch_get(session_id: Optional[str], keys: List[str]) -> List[Dict]:
    results = []
    for key in keys:
        val, expired = await _kv_get(session_id, key)
        results.append({"key": key, "value": val if not expired else None, "expired": expired})
    return results


# ----------------------------------------------------------------------
# Export/Import
# ----------------------------------------------------------------------
async def _kv_export(session_id: Optional[str], compress: bool = False) -> bytes:
    """Export all key-value pairs as JSON (optionally gzip compressed)."""
    all_data = {}
    for key in await _kv_list(session_id):
        val, _ = await _kv_get(session_id, key)
        all_data[key] = val
    json_str = json.dumps(all_data, indent=2)
    data = json_str.encode("utf-8")
    if compress:
        data = zlib.compress(data)
    return data


async def _kv_import(session_id: Optional[str], data: bytes, overwrite: bool = True) -> int:
    """Import from exported data. Returns number of keys imported."""
    # Check if compressed
    if data[:2] == b"\x78\x9c":  # zlib header
        data = zlib.decompress(data)
    imported = json.loads(data.decode("utf-8"))
    count = 0
    for key, value in imported.items():
        if overwrite or (await _kv_get(session_id, key))[0] is None:
            await _kv_set(session_id, key, value)
            count += 1
    return count


# ----------------------------------------------------------------------
# Token counting
# ----------------------------------------------------------------------
async def _kv_token_count(session_id: Optional[str], key: str, model: str = "gpt-4") -> Optional[int]:
    val, expired = await _kv_get(session_id, key)
    if expired or val is None:
        return None
    val_str = json.dumps(val)
    if TIKTOKEN_AVAILABLE:
        encoding = tiktoken.encoding_for_model(model)
        return len(encoding.encode(val_str))
    else:
        return len(val_str) // 4  # rough estimate


# ----------------------------------------------------------------------
# Watch for changes (polling-based)
# ----------------------------------------------------------------------
class KVVWatch:
    def __init__(self, session_id: str, interval: float = 2.0):
        self.session_id = session_id
        self.interval = interval
        self._last_state: Dict[str, float] = {}
        self._running = False
        self._task: Optional[asyncio.Task] = None

    async def _poll(self, callback):
        while self._running:
            current_state = {}
            for key in await _kv_list(self.session_id):
                entry = await _kv_inspect_ttl(self.session_id, key)
                if entry:
                    current_state[key] = entry.get("remaining", -1)
            # Detect added, removed, changed
            added = set(current_state) - set(self._last_state)
            removed = set(self._last_state) - set(current_state)
            changed = {k for k in set(current_state) & set(self._last_state) if current_state[k] != self._last_state[k]}
            if added or removed or changed:
                await callback(added, removed, changed)
            self._last_state = current_state
            await asyncio.sleep(self.interval)

    async def start(self, callback):
        self._running = True
        self._task = asyncio.create_task(self._poll(callback))

    async def stop(self):
        self._running = False
        if self._task:
            self._task.cancel()
            try:
                await self._task
            except asyncio.CancelledError:
                pass


_watches: Dict[str, KVVWatch] = {}


# ----------------------------------------------------------------------
# Semantic search
# ----------------------------------------------------------------------
async def _kv_search(session_id: Optional[str], query: str, top_k: int = 3) -> List[Tuple[str, float]]:
    if not HAS_EMBEDDINGS:
        raise RuntimeError("Embeddings not available")
    query_emb = EMBED_MODEL.encode(query)
    results = []
    for key in await _kv_list(session_id):
        val, _ = await _kv_get(session_id, key)
        val_str = json.dumps(val)
        val_emb = EMBED_MODEL.encode(val_str)
        sim = float(query_emb @ val_emb)
        results.append((key, sim))
    results.sort(key=lambda x: x[1], reverse=True)
    return results[:top_k]


# ----------------------------------------------------------------------
# FastMCP server
# ----------------------------------------------------------------------
@asynccontextmanager
async def lifespan(app):
    # Start background TTL cleanup task
    task = asyncio.create_task(cleanup_expired_entries())
    yield
    task.cancel()
    try:
        await task
    except asyncio.CancelledError:
        pass


mcp = FastMCP("mcp-cc", lifespan=lifespan)


@mcp.tool()
async def kv_set(key: str, value: Any, session_id: Optional[str] = None, ttl: Optional[int] = None) -> str:
    """Store a value with optional TTL (seconds). Supports session isolation."""
    try:
        await _kv_set(session_id, key, value, ttl)
        msg = f"✅ Stored '{key}'" + (f" (TTL: {ttl}s)" if ttl else "")
        return msg
    except Exception as e:
        return f"❌ {e}"


@mcp.tool()
async def kv_get(key: str, session_id: Optional[str] = None, stream: bool = False, ctx: Context = None) -> str:
    """Retrieve a value. Use stream=True for large values (chunks sent via log messages)."""
    try:
        value, expired = await _kv_get(session_id, key, stream, ctx)
        if expired:
            return f"⏰ Key '{key}' expired."
        if value is None:
            return "⚠️ Key not found"
        if stream:
            return "[Streaming complete]"
        if isinstance(value, (dict, list)):
            value = json.dumps(value, indent=2)
        return str(value)
    except Exception as e:
        return f"❌ {e}"


@mcp.tool()
async def kv_delete(key: str, session_id: Optional[str] = None) -> str:
    """Delete a key."""
    try:
        await _kv_delete(session_id, key)
        return f"🗑️ Deleted '{key}'"
    except Exception as e:
        return f"❌ {e}"


@mcp.tool()
async def kv_list(session_id: Optional[str] = None) -> str:
    """List all non-expired keys."""
    try:
        keys = await _kv_list(session_id)
        if not keys:
            return "(empty)"
        return "\n".join(sorted(keys))
    except Exception as e:
        return f"❌ {e}"


@mcp.tool()
async def kv_clear(session_id: Optional[str] = None) -> str:
    """Delete all keys in the session."""
    try:
        count = await _kv_clear(session_id)
        return f"🧹 Cleared {count} keys."
    except Exception as e:
        return f"❌ {e}"


@mcp.tool()
async def kv_inspect_ttl(key: str, session_id: Optional[str] = None) -> str:
    """Check remaining TTL without expiry."""
    try:
        info = await _kv_inspect_ttl(session_id, key)
        if not info:
            return "Key not found."
        if info["ttl"] is None:
            return f"Key '{key}' has no TTL (permanent)."
        if info["expired"]:
            return f"Key '{key}' has already expired."
        return f"Key '{key}' TTL: {info['ttl']}s total, {info['remaining']:.0f}s remaining."
    except Exception as e:
        return f"❌ {e}"


@mcp.tool()
async def kv_batch_set(items: List[Dict[str, Any]], session_id: Optional[str] = None) -> str:
    """Set multiple keys in one call. items: [{"key": "k", "value": v, "ttl": optional}]"""
    try:
        results = await _kv_batch_set(session_id, items)
        return json.dumps(results, indent=2)
    except Exception as e:
        return f"❌ {e}"


@mcp.tool()
async def kv_batch_get(keys: List[str], session_id: Optional[str] = None) -> str:
    """Get multiple keys."""
    try:
        results = await _kv_batch_get(session_id, keys)
        return json.dumps(results, indent=2)
    except Exception as e:
        return f"❌ {e}"


@mcp.tool()
async def kv_export(session_id: Optional[str] = None, compress: bool = False) -> str:
    """Export all key-value pairs as base64 (optionally compressed)."""
    try:
        data = await _kv_export(session_id, compress)
        return base64.b64encode(data).decode("ascii")
    except Exception as e:
        return f"❌ {e}"


@mcp.tool()
async def kv_import(data_base64: str, session_id: Optional[str] = None, overwrite: bool = True) -> str:
    """Import previously exported data."""
    try:
        data = base64.b64decode(data_base64)
        count = await _kv_import(session_id, data, overwrite)
        return f"✅ Imported {count} keys."
    except Exception as e:
        return f"❌ {e}"


@mcp.tool()
async def kv_token_count(key: str, session_id: Optional[str] = None, model: str = "gpt-4") -> str:
    """Estimate token count of a stored value."""
    try:
        tokens = await _kv_token_count(session_id, key, model)
        if tokens is None:
            return "Key not found or expired."
        return f"📊 Token count: {tokens} (using {model})"
    except Exception as e:
        return f"❌ {e}"


@mcp.tool()
async def kv_search(query: str, session_id: Optional[str] = None, top_k: int = 3) -> str:
    """Semantic search over keys (requires sentence-transformers)."""
    try:
        results = await _kv_search(session_id, query, top_k)
        if not results:
            return "No similar keys found."
        return "\n".join([f"{key} (sim: {sim:.2f})" for key, sim in results])
    except Exception as e:
        return f"❌ {e}"


@mcp.tool()
async def kv_watch_start(session_id: str, interval: float = 2.0) -> str:
    """Start watching a session for changes (polling). Use kv_watch_poll to read changes."""
    if session_id in _watches:
        return "⚠️ Already watching this session."
    watch = KVVWatch(session_id, interval)
    _watches[session_id] = watch

    async def on_change(added, removed, changed):
        msg = f"Changes in session {session_id}: added={added}, removed={removed}, changed={changed}"
        if session_id not in _watch_changes:
            _watch_changes[session_id] = []
        _watch_changes[session_id].append(msg)

    await watch.start(on_change)
    return f"✅ Watching session {session_id} (interval {interval}s)"


@mcp.tool()
async def kv_watch_stop(session_id: str) -> str:
    """Stop watching a session."""
    if session_id in _watches:
        await _watches[session_id].stop()
        del _watches[session_id]
        return f"✅ Stopped watching {session_id}"
    return "⚠️ Not watching that session."


@mcp.tool()
async def kv_watch_poll(session_id: str) -> str:
    """Poll accumulated watch changes for a session and clear them."""
    changes = _watch_changes.pop(session_id, [])
    if not changes:
        return "(no changes)"
    return "\n".join(changes)


# ----------------------------------------------------------------------
# Main
# ----------------------------------------------------------------------
def main():
    global \
        ALLOWED_DIR, \
        USE_SQLITE, \
        MAX_KEY_LEN, \
        MAX_VALUE_SIZE, \
        MAX_KEYS_PER_SESSION, \
        TTL_CLEANUP_INTERVAL, \
        COMPRESS_THRESHOLD

    parser = argparse.ArgumentParser(description="MCP Key-Value Cache Server")
    parser.add_argument("--root", type=str, default=str(Path.cwd()), help="Sandbox root directory")
    parser.add_argument("--use-sqlite", action="store_true", help="Use SQLite backend (faster for large stores)")
    parser.add_argument("--max-key-len", type=int, default=256, help="Maximum key length")
    parser.add_argument("--max-value-size", type=int, default=10 * 1024 * 1024, help="Maximum value size in bytes")
    parser.add_argument("--max-keys", type=int, default=10000, help="Maximum keys per session")
    parser.add_argument("--ttl-cleanup", type=int, default=60, help="TTL cleanup interval (seconds)")
    parser.add_argument("--compress-threshold", type=int, default=1024, help="Compress values larger than this (bytes)")
    args = parser.parse_args()

    ALLOWED_DIR = Path(args.root).resolve()
    ALLOWED_DIR.mkdir(parents=True, exist_ok=True)
    USE_SQLITE = args.use_sqlite and SQLITE_AVAILABLE
    if args.use_sqlite and not SQLITE_AVAILABLE:
        print("Warning: SQLite not available, falling back to file backend.", flush=True)
    MAX_KEY_LEN = args.max_key_len
    MAX_VALUE_SIZE = args.max_value_size
    MAX_KEYS_PER_SESSION = args.max_keys
    TTL_CLEANUP_INTERVAL = args.ttl_cleanup
    COMPRESS_THRESHOLD = args.compress_threshold

    mcp.run()


if __name__ == "__main__":
    main()
