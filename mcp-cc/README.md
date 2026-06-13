# mcp-cc — Key-Value Cache Server

Persistent, sandboxed key-value store for AI agents via the Model Context Protocol.

## Features

- **Session isolation** — each `session_id` gets its own namespace (files or SQLite)
- **TTL support** — automatic expiration with background cleanup every 60 s
- **Nested keys** — hierarchical keys like `user.profile.name`
- **Compression** — values larger than the threshold are zlib-compressed automatically
- **Streaming retrieval** — send large values incrementally via MCP log messages
- **Batch operations** — set/get multiple keys in one call
- **Export/Import** — backup and restore as JSON (optionally compressed)
- **Token counting** — estimate context usage (uses `tiktoken` if installed)
- **Semantic search** — find keys by meaning (requires `sentence-transformers`)
- **Watch for changes** — monitor key modifications by polling
- **Dual backend** — JSON files (default) or SQLite (`--use-sqlite`, faster for large stores)

## Installation

```bash
cd mcp-cc
uv sync
# Optional:
uv pip install sentence-transformers  # semantic search
uv pip install tiktoken               # accurate token counting
```

## Usage

```bash
uv run server.py --root /path/to/data --use-sqlite
```

### Command-line options

| Option | Default | Description |
|--------|---------|-------------|
| `--root PATH` | cwd | Sandbox root directory |
| `--use-sqlite` | off | Use SQLite backend instead of JSON files |
| `--max-key-len N` | 256 | Maximum key length in characters |
| `--max-value-size N` | 10485760 | Maximum value size in bytes (default 10 MB) |
| `--max-keys N` | 10000 | Maximum keys per session |
| `--ttl-cleanup N` | 60 | TTL cleanup interval in seconds |
| `--compress-threshold N` | 1024 | Compress values larger than this (bytes) |

## MCP Tools

| Tool | Description |
|------|-------------|
| `kv_set` | Store a value with optional TTL (seconds) |
| `kv_get` | Retrieve a value; set `stream: true` for large data |
| `kv_delete` | Remove a key |
| `kv_list` | List all non-expired keys |
| `kv_clear` | Delete all keys in the session |
| `kv_inspect_ttl` | Show remaining TTL without triggering expiry |
| `kv_batch_set` | Set multiple keys in one call |
| `kv_batch_get` | Get multiple keys |
| `kv_export` | Export all pairs as base64 JSON (optionally compressed) |
| `kv_import` | Import from exported data |
| `kv_token_count` | Estimate token count of a stored value |
| `kv_search` | Semantic search over values (requires `sentence-transformers`) |
| `kv_watch_start` | Start watching a session for changes (logs via MCP) |
| `kv_watch_stop` | Stop watching |

## Session Isolation

Pass an optional `session_id` to any tool. Different values create completely separate stores.
Omitting `session_id` uses the `"default"` session.

## Claude Desktop Configuration

```json
{
  "mcpServers": {
    "mcp-cc": {
      "command": "uv",
      "args": [
        "--directory", "/absolute/path/to/mcp-cc",
        "run", "server.py",
        "--root", "/path/to/kv_data",
        "--use-sqlite"
      ]
    }
  }
}
```

## Performance Tips

- Use `--use-sqlite` for stores with more than 10,000 keys.
- Lower `--compress-threshold` (e.g. 512) to reduce disk usage at the cost of CPU.
- `kv_watch_start` polls on an interval — avoid watching many sessions simultaneously.

## Security

- All data is stored under `--root`; key sanitisation prevents path traversal.
- Sessions are isolated from each other.
- No external command execution.

## License

MIT
