# mcp-sh — Secure MCP Shell Server

Sandboxed shell access for the [Model Context Protocol](https://modelcontextprotocol.io).
Executes commands safely inside a directory root with an explicit allowlist, session state, and file upload/download.

## Features

- **Command allowlist + denylist** — fine-grained control over which commands run
- **Full path sandboxing** — absolute and relative paths that escape the root are rejected
- **Session state** — `cd` persists a virtual working directory per `session_id`
- **Optional chaining** (`&&`, `||`, `;`) with short-circuit evaluation
- **Optional redirection** (`>`, `>>`) to files inside the sandbox
- **File upload/download** — transfer files as base64 without leaving the sandbox
- **Environment allowlisting** — only safe vars (`PATH`, `HOME`, …) reach subprocesses
- **Streaming output** — send command output incrementally via MCP log messages
- **Configurable timeouts and output limits**
- **Blocks dangerous patterns** — no command substitution, backticks, or glob expansion
- **Audit logging** — optional verbose output to stderr

## Installation

```bash
cd mcp-sh
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
| `--allow-commands LIST` | — | Extra commands to allow (comma-separated, e.g. `rg,jq`) |
| `--deny-commands LIST` | — | Additional commands to deny |
| `--timeout SECONDS` | 30 | Command timeout |
| `--max-output CHARS` | 100000 | Maximum output size per command |
| `--allow-redirect` | off | Enable `>` and `>>` operators |
| `--allow-chaining` | off | Enable `&&`, `\|\|`, `;` operators |
| `--verbose` | off | Log details to stderr |

### Default allowed commands

`cat`, `cp`, `date`, `df`, `du`, `echo`, `file`, `find`, `grep`, `head`, `ls`, `mkdir`, `mv`, `pip`, `pip3`, `python`, `python3`, `pwd`, `sort`, `stat`, `tail`, `touch`, `uniq`, `uv`, `wc`, `whoami`

### Default denied commands

`chmod`, `chown`, `crontab`, `dd`, `docker`, `kill`, `killall`, `mkfs`, `mount`, `nsenter`, `passwd`, `podman`, `reboot`, `rm`, `rmdir`, `service`, `shutdown`, `su`, `sudo`, `systemctl`, `umount`

## MCP Tools

### `shell_exec`

Execute a shell command inside the sandbox. Use `session_id` to persist `cd` state between calls.

```json
{
  "command": "ls -la",
  "session_id": "my-session",
  "stream": false
}
```

### `shell_info`

Display current sandbox configuration (allowed/denied commands, root, flags, active sessions).

### `upload_file`

Write a file into the sandbox from base64-encoded content.

```json
{
  "path": "data/input.csv",
  "content_base64": "aWQ..."
}
```

### `download_file`

Read a file from the sandbox and return it as base64 (max 10 MB).

```json
{
  "path": "results/output.json"
}
```

## Claude Desktop Configuration

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
    }
  }
}
```

## Security Notes

- The server never executes commands outside `--root`.
- Commands not in the allowlist are rejected before execution.
- Denied commands always override the allowlist.
- Operators `|`, `$(…)`, `` ` ``, `*`, `?`, `[` are blocked unconditionally.
- The subprocess environment is sanitised — only explicitly allowlisted variables are inherited.
- File redirections always resolve inside the sandbox root.
- `cd` only moves within the sandbox; attempts to escape are rejected.

## License

MIT
