# MCP Python Sandbox Server

Securely execute arbitrary Python code through MCP. Uses AST scanning, resource limits, subprocess isolation, and optional Docker.

## Features

- **Import restrictions** – only allow safe standard library modules.
- **Block dangerous builtins** – `open`, `exec`, `eval`, `compile`.
- **Resource limits** – CPU time, memory, output size.
- **Timeout** – kills execution after N seconds.
- **Docker support** – full container isolation (network none, read-only).
- **Returns stdout/stderr + exit code**.

## Installation

```bash
git clone https://github.com/yourname/mcp-py-sandbox
cd mcp-py-sandbox
uv sync
```

For Docker support:

```bash
uv sync --extra docker
```

## Usage

Add to `claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "py-sandbox": {
      "command": "uv",
      "args": ["--directory", "/path/to/mcp-py-sandbox", "run", "server.py"]
    }
  }
}
```

## Tool: `execute_python`

| Parameter | Type | Description |
|-----------|------|-------------|
| `code` (required) | string | Python code to execute |
| `timeout` | integer | Seconds (default 5) |
| `memory_mb` | integer | Memory limit in MB (default 256) |
| `use_docker` | boolean | Use Docker isolation (requires docker) |

## Example

```json
{
  "code": "for i in range(5): print(i**2)",
  "timeout": 2
}
```

Output:

```
📤 STDOUT:
0
1
4
9
16
```

## Security

- AST analysis blocks any import of dangerous modules.
- Runs in a subprocess with `setrlimit` for memory.
- Builtins like `open`, `exec`, `eval` are removed.
- Docker mode disables network and writes.

## License

MIT

```

---

## 🔌 Add to `claude_desktop_config.json`

```json
{
  "mcpServers": {
    "py-sandbox": {
      "command": "uv",
      "args": [
        "--directory",
        "/Users/ivo/Developer/mcp-servers/mcp-py-sandbox",
        "run",
        "server.py"
      ]
    }
  }
}
```
