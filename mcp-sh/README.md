# mcp-sh — Sandboxed MCP Shell Server

Bash shell access for the [Model Context Protocol](https://modelcontextprotocol.io),
confined to a directory root. Runs the **full bash feature set** (pipes, redirects,
heredocs, loops, command substitution, wildcards) with a denylist of dangerous
commands, per-session working directory, and file upload/download.

> **Security note:** on macOS, writes are confined to the sandbox subtree at the
> kernel level via `sandbox-exec` (see [Filesystem confinement](#filesystem-confinement)).
> Reads outside the sandbox are still possible, and the command denylist is only a
> best-effort guard. Point `--root` at a directory you are comfortable letting an
> LLM run bash in, and prefer running it as an unprivileged user.

## Features

- **Full bash** — `/bin/bash` with pipes, redirects, heredocs, loops, substitution, globs
- **Kernel-level write confinement** (macOS) — `shell_exec` runs under `sandbox-exec`; writes outside `--root` are denied by the OS, not just the denylist
- **Path sandboxing** — file tools reject absolute/relative paths that escape `--root`
- **Session state** — `cd` persists a working directory per `session_id`
- **Command denylist** — block named commands (also catches chained `ls; rm …`)
- **File upload/download** — transfer files as base64 without leaving the sandbox
- **`write_file` / `read_file`** — create and read text files (no heredocs needed)
- **Streaming output** — stream command output incrementally via MCP log messages
- **Timeout with process-group kill** — a timed-out command and its children are killed
- **Configurable timeouts and output limits**
- **Verbose logging** — optional debug output to stderr

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
| `--root PATH` | cwd | Sandbox root directory (created if missing) |
| `--timeout SECONDS` | 60 | Command timeout |
| `--max-output CHARS` | 200000 | Maximum output size per command |
| `--deny-commands LIST` | see below | Comma-separated denied command names |
| `--allow-all` | off | Disable the denylist entirely (unrestricted) |
| `--isolation MODE` | auto | OS write confinement: `auto`, `write`, or `off` (see below) |
| `--verbose` | off | Log details to stderr |

#### `--isolation` modes

| Mode | Behaviour |
|------|-----------|
| `auto` *(default)* | Use kernel write-confinement when available (macOS + `sandbox-exec`); otherwise log a warning and run unconfined. |
| `write` | Require confinement. If unavailable, the server **fails to start** rather than running unconfined. |
| `off` | Disable OS confinement entirely (denylist only). |

> `--allow-chaining` and `--allow-redirect` are accepted but ignored — chaining and
> redirection are always enabled. They remain only for config compatibility.

### Default denied commands

`rm`, `rmdir`, `dd`, `mkfs`, `shutdown`, `reboot`, `sudo`, `su`

## MCP Tools

### `shell_exec`

Execute a bash command inside the sandbox. Use `session_id` to persist `cd` state
between calls; set `stream` to receive output incrementally as MCP log messages.

```json
{
  "command": "ls -la",
  "session_id": "my-session",
  "stream": false
}
```

### `write_file`

Write text content to a file inside the sandbox (creates parent dirs).

```json
{ "path": "src/main.py", "content": "print('hi')\n" }
```

### `read_file`

Read a text file from inside the sandbox.

```json
{ "path": "src/main.py" }
```

### `upload_file`

Write a file into the sandbox from base64-encoded content.

```json
{ "path": "data/input.csv", "content_base64": "aWQ..." }
```

### `download_file`

Read a file from the sandbox and return it as base64 (max 10 MB).

```json
{ "path": "results/output.json" }
```

### `shell_info`

Display current sandbox configuration (root, timeout, output limit, denied
commands, active sessions).

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
        "--isolation", "write"
      ]
    }
  }
}
```

## Filesystem confinement

On macOS, `shell_exec` runs each command under `sandbox-exec` (seatbelt) with a
profile that **denies all writes outside the `--root` subtree** — at the kernel
level, regardless of how the path is constructed (absolute paths, `cd /etc`,
command substitution, `xargs`, symlinks, etc.). Writes to a handful of device
files (`/dev/null`, `/dev/tty`, …) are permitted so pipelines work.

- `TMPDIR`/`TMP`/`TEMP` are pointed at `<root>/.tmp`, so temp files stay inside the
  sandbox instead of leaking to the system temp dir.
- **Reads are not confined.** Programs and libraries anywhere on the system can be
  read and executed (this is what lets normal tools run); a command can still *read*
  files outside the sandbox. Confinement protects against modification/deletion and
  data destruction, not against reading.
- Confinement is controlled by `--isolation` (see above). Use `--isolation write`
  to refuse to start without it.
- On non-macOS platforms `sandbox-exec` is unavailable; `auto` logs a warning and
  runs unconfined, `write` refuses to start.

## Security notes

- **File tools** (`write_file`, `read_file`, `upload_file`, `download_file`) resolve
  paths and reject anything outside `--root`. `cd` that escapes the root resets the
  session back to the root.
- **The denylist is best-effort.** It matches the first word of each chained segment
  (`;`, `&&`, `||`, `|`, `&`, newlines), so `ls; rm -rf .` is blocked — but it cannot
  see through command substitution (`$(rm …)`), `xargs`, aliases, or absolute paths
  like `/bin/rm`. Do not rely on it as a security boundary — the OS write-confinement
  is the real protection.
- `HOME`, `PWD` and `TMPDIR` are pinned to the sandbox; the rest of the environment
  is inherited.
- Timed-out commands are killed as a process group, so background children don't leak.

## License

MIT
