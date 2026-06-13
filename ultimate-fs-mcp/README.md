# Ultimate MCP File System Server

A full‑featured, asynchronous file‑system server for the [Model Context Protocol (MCP)](https://modelcontextprotocol.io/).
It gives any MCP‑compatible LLM safe, read/write access to a local directory – with 20+ tools.

## Features

- **Async I/O** – never blocks the event loop, thanks to `aiofiles`.
- **Text & binary** – seamless base64 encoding for binary files.
- **Chunked reads** – offset/limit support for huge files.
- **Full file management** – copy, move, delete (recursively), permissions, symlinks, hardlinks.
- **Search** – glob pattern matching (`search_files`) and regex content search (`grep_files`).
- **Disk usage** – show total/used/free space.
- **Directory listing** – with detailed permissions and sizes.
- **Strict sandboxing** – all paths are resolved inside a configurable `--root` directory; symlink escapes are blocked.
- **Extensible** – add your own tools easily using the MCP low‑level API.

## Requirements

- Python 3.10+
- `mcp` (Model Context Protocol SDK)
- `aiofiles`

Install with `pip` or `uv`:

```bash
pip install mcp aiofiles
Usage
bash
python filesystem_server_v2.py --root /path/to/allowed/directory
If no --root is given, the current working directory is used.

MCP Client Configuration
Add to your client’s configuration (e.g. Claude Desktop, LM Studio):

json
{
  "mcpServers": {
    "ultimate-filesystem": {
      "command": "/absolute/path/to/venv/bin/python",
      "args": [
        "/absolute/path/to/filesystem_server_v2.py",
        "--root",
        "/safe/folder"
      ]
    }
  }
}
Tools
Tool name Description
read_file Read text or binary (base64) with optional offset/limit
write_file Write text or base64‑encoded binary; append mode supported
create_directory Create a directory (including parents)
list_directory List contents with optional detailed info
copy_item Copy files or directories (recursively)
move_item Move/rename files or directories
delete_item Delete a file or directory (recursive for non‑empty)
get_file_info Detailed metadata (size, permissions, timestamps, owner)
search_files Glob search (e.g. **/*.py)
grep_files Search file contents using a regex
set_permissions Change file mode (chmod) via octal integer
create_symlink Create a symbolic link
create_hardlink Create a hard link
disk_usage Show total/used/free disk space
file_exists Check if a path exists
estimate_context_usage (Placeholder) return an estimate of context window usage

## Security

All paths are forced to be inside the --root directory.
Symlink destinations are resolved and checked before access.
Use a dedicated, restricted folder to avoid accidental exposure of sensitive files.

## Extending

Add a new tool by defining it in the TOOLS list and adding a handler in the call_tool dispatch. See the source code for examples.
