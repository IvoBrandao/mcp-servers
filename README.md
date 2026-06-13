# MCP Servers

A collection of local MCP (Model Context Protocol) servers for AI agents.

## Servers

| Server | Description |
|--------|-------------|
| [mcp-sh](mcp-sh/) | Sandboxed shell — execute commands safely inside a directory |
| [mcp-fs](mcp-fs/) | File system — read, write, search, and manage files |
| [mcp-cc](mcp-cc/) | Key-value cache — persistent store with TTL, sessions, and batch ops |
| [mcp-flow](mcp-flow/) | Flow orchestrator — plan management and memory for long-running workflows |
| [mcp-html-studio](mcp-html-studio/) | HTML Studio — create and preview local HTML/CSS/JS projects |
| [mcp-ws](mcp-ws/) | Web search — 10+ search engines, image search, and page fetching |
| [mcp-think](mcp-think/) | Thinking engine — structured reasoning, assumptions, contradictions, and self-evaluation |

## Quick Start

Each server uses [uv](https://docs.astral.sh/uv/) for dependency management. Install uv first:

```bash
curl -LsSf https://astral.sh/uv/install.sh | sh
```

Then run any server directly:

```bash
uv --directory /path/to/mcp-sh run server.py --root /your/sandbox
```

## Claude Desktop Configuration

Add any server to `~/Library/Application Support/Claude/claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "mcp-sh": {
      "command": "uv",
      "args": [
        "--directory", "/absolute/path/to/mcp-sh",
        "run", "server.py",
        "--root", "/path/to/sandbox",
        "--allow-chaining",
        "--allow-redirect"
      ]
    },
    "mcp-fs": {
      "command": "uv",
      "args": [
        "--directory", "/absolute/path/to/mcp-fs",
        "run", "server.py",
        "--root", "/path/to/data",
        "--allowed-extensions", ".txt,.md,.py,.json,.html,.css,.js"
      ]
    },
    "mcp-cc": {
      "command": "uv",
      "args": [
        "--directory", "/absolute/path/to/mcp-cc",
        "run", "server.py",
        "--root", "/path/to/kv_data",
        "--use-sqlite"
      ]
    },
    "mcp-flow": {
      "command": "uv",
      "args": [
        "--directory", "/absolute/path/to/mcp-flow",
        "run", "server.py"
      ]
    },
    "html-studio": {
      "command": "uv",
      "args": [
        "--directory", "/absolute/path/to/mcp-html-studio",
        "run", "server.py"
      ]
    },
    "mcp-ws": {
      "command": "uv",
      "args": [
        "--directory", "/absolute/path/to/mcp-ws",
        "run", "server.py"
      ]
    },
    "mcp-think": {
      "command": "uv",
      "args": [
        "--directory", "/absolute/path/to/mcp-think",
        "run", "server.py"
      ]
    }
  }
}
```

See each server's README for full options and tool documentation.

## License

MIT
