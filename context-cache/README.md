
---

## 📘 README – Enhanced MCP KV‑Cache Server

Create `README_KV_CACHE.md`:

```markdown
# Enhanced MCP Key‑Value Cache Server

A persistent, sandboxed key‑value store for MCP‑compatible LLMs, designed for **context paging** – the model acts as its own memory scheduler, moving inactive context to the cache and retrieving it later.

## Features

- **Persistent JSON store** – each key is a file in a subfolder.
- **TTL support** – optional per‑key time‑to‑live in seconds; keys auto‑expire and are deleted.
- **TTL inspection** – check remaining lifetime without triggering expiry (`kv_inspect_ttl`).
- **Index management** – maintain an “active pages” list to track paged‑out context.
- **Summarisation helper** – `kv_summarize` retrieves a value and returns a summarisation prompt for the model.
- **Semantic search** – optional similarity search using `sentence‑transformers` (install separately).
- **Full CRUD** – set, get, delete, list, clear.
- **Sandboxed** – all storage inside a configurable `--root` directory.

## Requirements

- Python 3.10+
- `mcp`
- (Optional) `sentence‑transformers` for semantic search

Install:

```bash
pip install mcp
# for semantic search:
pip install sentence-transformers
Usage
bash
python kv_cache_enhanced.py --root /path/to/storage
If no --root is given, the current directory is used.
Data is stored inside a kv_store_enhanced subfolder (configurable in the source).

MCP Client Configuration
json
{
  "mcpServers": {
    "kv-cache": {
      "command": "/absolute/path/to/venv/bin/python",
      "args": [
        "/absolute/path/to/kv_cache_enhanced.py",
        "--root",
        "/safe/storage/folder"
      ]
    }
  }
}
Tools
Tool name Description
kv_set Store a value under a key, optionally with TTL (seconds)
kv_get Retrieve a value; if TTL elapsed, key is deleted and “EXPIRED” returned
kv_delete Delete a key
kv_list List all non‑expired keys
kv_clear Delete all keys
kv_inspect_ttl Check remaining TTL of a key without deleting it
kv_index_add Add a page ID to the active pages index
kv_index_remove Remove a page ID from the index
kv_index_list List all active page IDs
kv_summarize Return a summarisation prompt for a stored value
kv_search Semantic similarity search (requires sentence‑transformers)
How to Use for Context Paging (RTOS Style)
Set up the model’s system prompt to act as a memory scheduler.
Example:

text
When the conversation is long, page out important context:
- Use kv_summarize on the oldest stored page.
- Store the summary with kv_set (no TTL for permanent data).
- Add the summary key to the active index with kv_index_add.
- When the user asks about earlier details, use kv_index_list to find the relevant page, then kv_get to retrieve it.
Omit TTL for permanent context pages. Use TTL only for ephemeral data (e.g., scratch notes).

The model can call kv_inspect_ttl before retrieving to avoid accidental expiration.

Use kv_search to find semantically related keys if you forget the exact key name.

Security
All keys are sanitised to alphanumeric + _ - . only.

Storage is inside the --root directory; no path traversal possible.

License
MIT
