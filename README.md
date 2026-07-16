# MCP Servers

A collection of local MCP (Model Context Protocol) servers for AI agents. All servers run locally — no cloud APIs required (except optional integrations).

## Servers

### Core

| Server | Description | Key tools |
|--------|-------------|-----------|
| [mcp-sh](mcp-sh/) | Sandboxed bash shell | `shell_exec`, `write_file`, `read_file`, `upload_file`, `download_file` |
| [mcp-fs](mcp-fs/) | File system operations | `read_file`, `write_file`, `list_directory`, `grep_files`, `search_files`, `compress`, `get_file_info` |
| [mcp-cc](mcp-cc/) | Key-value cache with TTL | `kv_set`, `kv_get`, `kv_list`, `kv_batch_set`, `kv_export`, `kv_import`, `kv_watch_*` |
| [mcp-ws](mcp-ws/) | Web search + page fetching | `web_search`, `image_search`, `fetch_page` |
| [mcp-py](mcp-py/) | Sandboxed Python executor | `execute_python`, `list_allowed_modules` |
| [mcp-git](mcp-git/) | Git repository operations | `git_status`, `git_diff`, `git_log`, `git_commit`, `git_branch_*`, `git_push`, `git_pull`, `git_blame` |
| [mcp-http](mcp-http/) | HTTP client | `http_get`, `http_post`, `http_put`, `http_patch`, `http_delete`, `http_download` |
| [mcp-db](mcp-db/) | SQLite database operations | `db_query`, `db_execute`, `db_schema`, `db_tables`, `db_import_csv`, `db_transaction` |

### AI Reasoning & Workflow

| Server | Description | Key tools |
|--------|-------------|-----------|
| [mcp-think](mcp-think/) | Structured reasoning engine | `think_add_step`, `think_add_assumption`, `think_check_contradiction`, `think_save_checkpoint` |
| [mcp-flow](mcp-flow/) | Workflow orchestrator | `flow_create_session`, `flow_add_step`, `flow_get_next_action`, `flow_update_step_status`, `flow_compact_context` |

### UI & Prototyping

| Server | Description | Key tools |
|--------|-------------|-----------|
| [mcp-html-studio](mcp-html-studio/) | HTML/CSS/JS project studio | `create_project`, `edit_file`, `open_project`, `export_project` |

## Quick Start

```bash
git clone https://github.com/your-org/mcp-servers
cd mcp-servers
cargo build --release
```

Binaries land in `target/release/`. Run any server:

```bash
./target/release/mcp-sh --root /your/sandbox
```

## Configuration

The config format is the same for Claude Desktop, LM Studio, and any MCP-compatible client.

- **Claude Desktop** — `~/Library/Application Support/Claude/claude_desktop_config.json`
- **LM Studio** — Settings → MCP → Edit config file

```json
{
  "mcpServers": {
    "mcp-sh": {
      "command": "/path/to/mcp-servers/target/release/mcp-sh",
      "args": ["--root", "/your/sandbox", "--timeout", "120"]
    },
    "mcp-fs": {
      "command": "/path/to/mcp-servers/target/release/mcp-fs",
      "args": ["--root", "/your/sandbox"]
    },
    "mcp-cc": {
      "command": "/path/to/mcp-servers/target/release/mcp-cc",
      "args": ["--root", "/path/to/kv-data"]
    },
    "mcp-ws": {
      "command": "/path/to/mcp-servers/target/release/mcp-ws",
      "args": []
    },
    "mcp-think": {
      "command": "/path/to/mcp-servers/target/release/mcp-think",
      "args": []
    },
    "mcp-flow": {
      "command": "/path/to/mcp-servers/target/release/mcp-flow",
      "args": []
    },
    "mcp-py": {
      "command": "/path/to/mcp-servers/target/release/mcp-py",
      "args": []
    },
    "html-studio": {
      "command": "/path/to/mcp-servers/target/release/mcp-html-studio",
      "args": ["--root", "/your/html-projects"]
    },
    "mcp-git": {
      "command": "/path/to/mcp-servers/target/release/mcp-git",
      "args": ["--root", "/your/projects"]
    },
    "mcp-http": {
      "command": "/path/to/mcp-servers/target/release/mcp-http",
      "args": []
    },
    "mcp-db": {
      "command": "/path/to/mcp-servers/target/release/mcp-db",
      "args": ["--root", "/your/databases"]
    }
  }
}
```

## Server options

### mcp-sh

| Flag | Default | Description |
|------|---------|-------------|
| `--root` | `$PWD` | Sandbox root — all commands run inside this directory |
| `--timeout` | `60` | Command timeout in seconds |
| `--max-output` | `200000` | Max output characters before truncation |
| `--deny-commands` | `rm,rmdir,dd,...` | Comma-separated commands to block |
| `--allow-all` | off | Disable the deny list (allow all commands) |
| `--isolation` | `auto` | OS write-confinement: `auto`, `write` (required), `off` |

### mcp-fs

| Flag | Default | Description |
|------|---------|-------------|
| `--root` | `$PWD` | Sandbox root |
| `--max-file-size` | `104857600` | Max file size in bytes (100 MB) |
| `--allowed-extensions` | _(all)_ | Comma-separated allowed extensions, e.g. `.py,.rs,.md` |

### mcp-cc

| Flag | Default | Description |
|------|---------|-------------|
| `--root` | `$PWD` | Directory for KV store files |
| `--max-keys` | `10000` | Max keys per session |
| `--ttl-cleanup` | `60` | TTL cleanup interval in seconds |

### mcp-git

| Flag | Default | Description |
|------|---------|-------------|
| `--root` | `$PWD` | Restrict git operations to this directory tree |

### mcp-db

| Flag | Default | Description |
|------|---------|-------------|
| `--root` | `$PWD` | Restrict database file access to this directory tree |

## License

MIT
