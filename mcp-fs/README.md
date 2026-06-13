# mcp-fs — File System Server

Rich file system access for the Model Context Protocol, with strict path sandboxing and session support.

## Features

- **Strict path sandboxing** — symlinks and `..` that escape the root are rejected
- **Per-session virtual working directory** — `cd` support with persistent sessions
- **Streaming reads** — send large file contents in chunks via MCP log messages
- **File hashing** — MD5/SHA256 on demand
- **Batch operations** — execute multiple file ops in one call
- **Compression** — create and extract `.zip` and `.tar.gz` archives
- **Token counting** — estimate context usage (uses `tiktoken` if installed)
- **Async I/O** — fully non-blocking with `aiofiles`
- **20+ tools** — copy, move, delete, search, grep, permissions, symlinks, disk usage, and more

## Installation

```bash
cd mcp-fs
uv sync
```

## Usage

```bash
uv run server.py --root /path/to/sandbox
```

### Command-line options

| Option | Default | Description |
|--------|---------|-------------|
| `--root PATH` | cwd | Sandbox root directory |
| `--max-file-size N` | 104857600 | Maximum file size in bytes (default 100 MB) |
| `--allowed-extensions LIST` | all | Comma-separated list (e.g. `.txt,.py,.json`) |
| `--session-timeout N` | 3600 | Session idle timeout in seconds |

## MCP Tools

| Tool | Description |
|------|-------------|
| `read_file` | Read text/binary; optional offset, limit, base64, streaming |
| `write_file` | Write text or base64 content; supports append |
| `cd` | Change virtual working directory (requires `session_id`) |
| `list_directory` | List contents with optional details |
| `create_directory` | Create directory (and parents) |
| `copy_item` | Copy file or directory |
| `move_item` | Move or rename |
| `delete_item` | Delete file or directory (`recursive` optional) |
| `get_file_info` | Metadata, permissions, timestamps, optional MD5/SHA256 |
| `search_files` | Glob search (e.g. `**/*.py`) |
| `grep_files` | Regex search inside file contents |
| `set_permissions` | Change Unix permissions (octal mode integer) |
| `create_symlink` | Create symbolic link |
| `create_hardlink` | Create hard link |
| `disk_usage` | Show total/used/free space for the sandbox root |
| `file_exists` | Check whether a path exists |
| `batch_operations` | Execute multiple ops (copy, move, delete, mkdir, chmod) |
| `compress` | Create a `.zip` or `.tar.gz` archive |
| `decompress` | Extract an archive |
| `context_usage` | Estimate token count of a file or text string |

## Session Management

Pass the same `session_id` across calls to maintain a persistent virtual working directory:

```json
{ "tool": "cd",             "arguments": { "session_id": "s1", "path": "src" } }
{ "tool": "list_directory", "arguments": { "session_id": "s1" } }
```

Sessions expire after the configured timeout.

## Claude Desktop Configuration

```json
{
  "mcpServers": {
    "mcp-fs": {
      "command": "uv",
      "args": [
        "--directory", "/absolute/path/to/mcp-fs",
        "run", "server.py",
        "--root", "/path/to/sandbox",
        "--allowed-extensions", ".txt,.md,.py,.json"
      ]
    }
  }
}
```

## Security

- All paths resolved against the sandbox root; `..` and escaping symlinks are rejected.
- File size and extension limits prevent resource exhaustion.
- No command execution — file system operations only.

## License

MIT
