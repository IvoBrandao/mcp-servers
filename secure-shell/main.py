#!/usr/bin/env python3
"""
Secure MCP Shell Server - with optional sandboxed chaining and redirect support
"""

import sys
import os
import shlex
import subprocess
import argparse
from pathlib import Path
from typing import Any, Optional, List, Tuple

try:
    from mcp.server import Server
    from mcp.server.stdio import stdio_server
    import mcp.types as types
except ImportError as e:
    print(f"Fatal import error: {e}", file=sys.stderr)
    sys.exit(1)

# ----------------------------------------------------------------------
# Configuration
# ----------------------------------------------------------------------
ALLOWED_DIR: Path = Path.cwd()
ALLOWED_COMMANDS: set = {
    "ls",
    "cat",
    "head",
    "tail",
    "wc",
    "grep",
    "find",
    "sort",
    "uniq",
    "echo",
    "date",
    "whoami",
    "pwd",
    "mkdir",
    "touch",
    "cp",
    "mv",
    "rm",
    "rmdir",
    "chmod",
    "chown",
    "python3",
    "pip3",
    "pip",
    "python",
    "uv",
}
DENIED_COMMANDS: set = {
    "rm",
    "rmdir",
    "dd",
    "mkfs",
    "shutdown",
    "reboot",
    "kill",
    "killall",
    "sudo",
    "su",
    "passwd",
    "chown",
    "chmod",
}
COMMAND_TIMEOUT = 30
MAX_OUTPUT_SIZE = 100_000
ALLOW_REDIRECT = False  # set by --allow-redirect
ALLOW_CHAINING = False  # set by --allow-chaining


# ----------------------------------------------------------------------
# Helpers
# ----------------------------------------------------------------------
def sanitize_command(cmd_str: str) -> list[str]:
    tokens = shlex.split(cmd_str)
    for token in tokens:
        # Always block dangerous pipe
        if token == "|":
            raise ValueError(f"Pipe operator is not allowed: '|'")
        # Block chaining operators unless explicitly allowed
        if not ALLOW_CHAINING and token in ("&&", ";", "||"):
            raise ValueError(f"Chaining operator not allowed: '{token}' (use --allow-chaining)")
        # Block redirection unless explicitly allowed
        if not ALLOW_REDIRECT and token in (">", ">>", "<"):
            raise ValueError(f"Redirection not allowed: '{token}' (use --allow-redirect)")
    return tokens


def resolve_and_contain_path(token: str) -> str:
    path = Path(token)
    if token.startswith("/"):
        resolved = path.resolve()
        if not resolved.as_posix().startswith(ALLOWED_DIR.as_posix()):
            raise ValueError(f"Access denied: path '{token}' escapes sandbox.")
        return str(resolved.relative_to(ALLOWED_DIR))
    elif token.startswith("~"):
        raise ValueError("Home directory expansion not allowed.")
    elif token.startswith("./") or token.startswith("../"):
        resolved = (ALLOWED_DIR / token).resolve()
        if not resolved.as_posix().startswith(ALLOWED_DIR.as_posix()):
            raise ValueError(f"Access denied: path '{token}' escapes sandbox.")
        return str(resolved.relative_to(ALLOWED_DIR))
    else:
        return token


def rewrite_command(tokens: list[str]) -> list[str]:
    rewritten = []
    for token in tokens:
        if "/" in token or token.startswith("~") or (token.startswith("-") and len(token) > 1):
            rewritten.append(resolve_and_contain_path(token))
        else:
            rewritten.append(token)
    return rewritten


def split_chained_commands(tokens: list[str]) -> List[List[str]]:
    """Split a list of tokens into sub-lists separated by '&&', ';', or '||'."""
    commands = []
    current = []
    for token in tokens:
        if token in ("&&", ";", "||"):
            if current:
                commands.append(current)
                current = []
        else:
            current.append(token)
    if current:
        commands.append(current)
    return commands


def parse_redirect(cmd_tokens: list[str]) -> Tuple[list[str], Optional[str], str]:
    """Extract redirection from the end of a command token list."""
    for i, token in enumerate(cmd_tokens):
        if token in (">", ">>"):
            if i + 1 < len(cmd_tokens):
                mode = "overwrite" if token == ">" else "append"
                file_token = cmd_tokens[i + 1]
                safe_file = resolve_and_contain_path(file_token)
                return cmd_tokens[:i], mode, safe_file
            else:
                raise ValueError("Redirect operator used without a filename.")
    return cmd_tokens, None, ""


async def run_single_command(tokens: list[str]) -> str:
    """Run a single command (no chaining or redirect). Returns stdout+stderr."""
    cmd_name = tokens[0] if tokens else ""
    if cmd_name in DENIED_COMMANDS:
        raise PermissionError(f"Command '{cmd_name}' is denied by policy.")
    if cmd_name not in ALLOWED_COMMANDS:
        raise ValueError(f"Command '{cmd_name}' is not in the allowed list.")

    safe_tokens = rewrite_command(tokens)
    proc = subprocess.run(
        safe_tokens,
        cwd=str(ALLOWED_DIR),
        capture_output=True,
        text=True,
        timeout=COMMAND_TIMEOUT,
    )
    output = proc.stdout + proc.stderr
    if len(output) > MAX_OUTPUT_SIZE:
        output = output[:MAX_OUTPUT_SIZE] + "\n... (output truncated)"
    return output, proc.returncode


async def handle_shell_exec(args: dict) -> list[types.TextContent]:
    cmd_str = args["command"]
    try:
        tokens = sanitize_command(cmd_str)
    except ValueError as e:
        return [types.TextContent(type="text", text=f"❌ Command rejected: {e}")]

    if not tokens:
        return [types.TextContent(type="text", text="❌ Empty command.")]

    # If chaining is allowed, split into separate command groups
    if ALLOW_CHAINING:
        command_groups = split_chained_commands(tokens)
    else:
        command_groups = [tokens]

    all_outputs = []
    for i, group in enumerate(command_groups):
        # Handle redirection within each group
        if ALLOW_REDIRECT:
            try:
                cmd_tokens, redirect_mode, redirect_file = parse_redirect(group)
            except ValueError as e:
                return [types.TextContent(type="text", text=f"❌ Redirect error: {e}")]
        else:
            cmd_tokens, redirect_mode, redirect_file = group, None, ""

        if not cmd_tokens:
            continue  # skip empty commands

        try:
            output, exit_code = await run_single_command(cmd_tokens)
        except PermissionError as e:
            return [types.TextContent(type="text", text=f"❌ {e}")]
        except ValueError as e:
            return [types.TextContent(type="text", text=f"❌ {e}")]
        except subprocess.TimeoutExpired:
            all_outputs.append(f"⏰ Command {i + 1} timed out")
            continue
        except Exception as e:
            return [types.TextContent(type="text", text=f"❌ Subprocess error: {e}")]

        # Handle redirect
        if redirect_mode and redirect_file:
            try:
                file_path = ALLOWED_DIR / redirect_file
                file_path.parent.mkdir(parents=True, exist_ok=True)
                mode = "w" if redirect_mode == "overwrite" else "a"
                with open(file_path, mode, encoding="utf-8") as f:
                    f.write(output)
                all_outputs.append(f"✅ Output written to {redirect_file}")
            except Exception as e:
                return [types.TextContent(type="text", text=f"❌ Failed to write output: {e}")]
        else:
            all_outputs.append(output)

        # If any command fails and we're chaining with '&&', stop (no more commands)
        if exit_code != 0 and ALLOW_CHAINING:
            # Find the separator used (approximation)
            break

    result = "\n".join(all_outputs)
    return [types.TextContent(type="text", text=result)]


async def handle_shell_list_allowed(args: dict) -> list[types.TextContent]:
    allowed = ", ".join(sorted(ALLOWED_COMMANDS))
    denied = ", ".join(sorted(DENIED_COMMANDS))
    redirect = "enabled" if ALLOW_REDIRECT else "disabled"
    chaining = "enabled" if ALLOW_CHAINING else "disabled"
    return [
        types.TextContent(
            type="text",
            text=f"✅ Allowed: {allowed}\n🚫 Denied: {denied}\n📝 Redirect: {redirect}\n🔗 Chaining: {chaining}",
        )
    ]


# ----------------------------------------------------------------------
# Server setup
# ----------------------------------------------------------------------
server = Server("secure-shell")

TOOLS = [
    types.Tool(
        name="shell_exec",
        description="Execute shell commands inside the sandbox. Use --allow-redirect for >/>> and --allow-chaining for &&/;/||.",
        inputSchema={
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The command to run, e.g. 'mkdir dir && cd dir && echo hello > file.txt'",
                },
            },
            "required": ["command"],
        },
    ),
    types.Tool(
        name="shell_list_allowed",
        description="List allowed commands, denied commands, and flag status.",
        inputSchema={"type": "object", "properties": {}},
    ),
]


@server.list_tools()
async def list_tools() -> list[types.Tool]:
    return TOOLS


@server.call_tool()
async def call_tool(name: str, arguments: dict) -> list[types.TextContent]:
    handlers = {
        "shell_exec": handle_shell_exec,
        "shell_list_allowed": handle_shell_list_allowed,
    }
    handler = handlers.get(name)
    if not handler:
        return [types.TextContent(type="text", text=f"Unknown tool: {name}")]
    return await handler(arguments)


# ----------------------------------------------------------------------
# Main
# ----------------------------------------------------------------------
async def main():
    global ALLOWED_DIR, ALLOWED_COMMANDS, DENIED_COMMANDS, ALLOW_REDIRECT, ALLOW_CHAINING

    parser = argparse.ArgumentParser(description="Secure MCP Shell Server")
    parser.add_argument("--root", type=str, default=str(Path.cwd()), help="Sandbox root directory.")
    parser.add_argument("--allow-commands", type=str, default="", help="Extra commands to allow (comma‑separated).")
    parser.add_argument("--deny-commands", type=str, default="", help="Commands to deny (comma‑separated).")
    parser.add_argument(
        "--allow-redirect", action="store_true", help="Allow > and >> operators (output to files inside sandbox)."
    )
    parser.add_argument("--allow-chaining", action="store_true", help="Allow &&, ;, || operators to chain commands.")
    args = parser.parse_args()

    ALLOWED_DIR = Path(args.root).resolve()
    if not ALLOWED_DIR.exists():
        print(f"Sandbox root does not exist: {ALLOWED_DIR}", file=sys.stderr)
        return

    if args.allow_commands:
        extra = set(c.strip() for c in args.allow_commands.split(",") if c.strip())
        ALLOWED_COMMANDS.update(extra)
    if args.deny_commands:
        extra_deny = set(c.strip() for c in args.deny_commands.split(",") if c.strip())
        DENIED_COMMANDS.update(extra_deny)
        ALLOWED_COMMANDS.difference_update(extra_deny)

    ALLOW_REDIRECT = args.allow_redirect
    ALLOW_CHAINING = args.allow_chaining

    async with stdio_server() as (read_stream, write_stream):
        await server.run(read_stream, write_stream, server.create_initialization_options())


if __name__ == "__main__":
    import asyncio

    asyncio.run(main())
