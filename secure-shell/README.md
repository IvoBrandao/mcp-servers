# Secure MCP Shell Server

A sandboxed command‑execution server for MCP‑compatible LLMs.
It lets the model run a **restricted set of shell commands** while enforcing strict file‑system containment and blocking destructive operations.

## Features

- **Command allowlist / denylist** – only explicit commands can run; dangerous ones (`rm`, `dd`, etc.) are blocked by default.
- **Path sandboxing** – every file argument is rewritten to stay inside a configurable root directory; absolute paths that escape are rejected.
- **No shell operators** – pipes, redirects, and command chaining are forbidden, preventing injection.
- **Timeout & output limits** – commands are killed after 30 s, and output is capped at 100 k characters.
- **Transparent policy** – the `shell_list_allowed` tool shows which commands are currently permitted.

## Requirements

- Python 3.10+
- `mcp`

```bash
pip install mcp
```

### Usage

```bash
python shell_server.py --root /sandbox/dir --allow-commands "ls,cat" --deny-commands "rm"
```

### Argument Description

- `--root` The directory used as the sandbox. All file operations are confined here.
- `--allow-commands` Comma‑separated extra commands to allow (in addition to the built‑in safe list).
- `--deny-commands` Comma‑separated commands to block, even if they appear in the allowlist.

If no `--root` is given, the current working directory is used.

### MCP Client Configuration

```json
{
  "mcpServers": {
    "secure-shell": {
      "command": "/absolute/path/to/venv/bin/python",
      "args": [
        "/absolute/path/to/shell_server.py",
        "--root",
        "/safe/sandbox",
        "--allow-commands",
        "ls,cat,grep,find,mkdir,touch"
      ]
    }
  }
}
```

## How It Works

The model sends a command string via the shell_exec tool.
The server splits the command with shlex and verifies:
No shell metacharacters (|, ;, &, etc.) are present.
The command name is in the allowed list and not in the denied list.
Any file paths are resolved inside the sandbox root; absolute paths outside are rejected.

The command runs with subprocess.run(..., cwd=sandbox_root), so all relative paths operate inside the sandbox.

Output (stdout + stderr) is returned, truncated if too long.

### Built‑in Safe Commands

By default, the server allows:

```sh
ls, cat, head, tail, wc, grep, find, sort, uniq, echo, date, whoami, pwd, mkdir, touch, cp, mv
```

Commands like rm, rmdir, dd, shutdown, sudo, etc. are always denied unless you explicitly remove them from the deny list (not recommended).

### Adding More Commands

Use `--allow-commands` "nano,vim,git" to extend the list.
Be careful – any command you allow will run inside the sandbox root but can still affect the host system if it has side effects (e.g., network access). Always run the server in a controlled environment.

### Security Considerations

The server does not use a container or chroot – it relies on path rewriting and a strict allow/deny policy.

If the LLM is compromised, an attacker could still run any allowed command on files within the sandbox. Make the sandbox root a throwaway directory.

Never run the server as root or with elevated privileges.

Combine with OS‑level restrictions (e.g., firejail, Docker) for stronger isolation.

## License

MIT
