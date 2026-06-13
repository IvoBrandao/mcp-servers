#!/usr/bin/env python3
"""
MCP Shell Server — bash-based, LLM-friendly sandboxed shell.
"""

import os
import sys
import asyncio
import argparse
import base64
import logging
from pathlib import Path
from typing import Optional, Dict, Set

from mcp.server.fastmcp import FastMCP, Context

# ----------------------------------------------------------------------
# Global config (set in main())
# ----------------------------------------------------------------------
ALLOWED_DIR: Path = Path.cwd()
DENIED_COMMANDS: Set[str] = {"rm", "rmdir", "dd", "mkfs", "shutdown", "reboot", "sudo", "su"}
COMMAND_TIMEOUT: int = 60
MAX_OUTPUT_SIZE: int = 200_000
VERBOSE: bool = False

# Session cwd state: session_id -> absolute path inside ALLOWED_DIR
_sessions: Dict[str, Path] = {}

logger = logging.getLogger("mcp-sh")


# ----------------------------------------------------------------------
# Helpers
# ----------------------------------------------------------------------
def get_session_cwd(session_id: Optional[str]) -> Path:
    if session_id and session_id in _sessions:
        return _sessions[session_id]
    return ALLOWED_DIR


def build_env(cwd: Path) -> Dict[str, str]:
    """Pass through the full environment but pin HOME and PWD to the sandbox."""
    env = dict(os.environ)
    env["PWD"] = str(cwd)
    env["HOME"] = str(ALLOWED_DIR)
    return env


def first_word(cmd: str) -> str:
    """Return the first word of a command string."""
    stripped = cmd.lstrip()
    parts = stripped.split()
    return parts[0] if parts else ""


def is_denied(cmd: str) -> Optional[str]:
    """Return the denied command name if the command starts with one, else None."""
    word = first_word(cmd)
    if word in DENIED_COMMANDS:
        return word
    return None


# ----------------------------------------------------------------------
# Core executor
# ----------------------------------------------------------------------
async def run_bash(
    command: str,
    cwd: Path,
    session_id: Optional[str],
    stream: bool,
    ctx: Optional[Context],
) -> str:
    """Run `command` via bash in `cwd`, return combined stdout+stderr output."""

    denied = is_denied(command)
    if denied:
        return f"❌ Command '{denied}' is denied."

    # Run the command in a bash script, prefixed with cd to the session cwd.
    # After the command finishes, print the new cwd so we can track cd changes.
    sentinel = "__MCP_CWD__:"
    # pwd -P resolves symlinks (matches ALLOWED_DIR which is always .resolve()'d)
    wrapped = f"cd {_shell_quote(str(cwd))} || exit 1\n{command}\nprintf '\\n{sentinel}%s\\n' \"$(pwd -P)\""

    proc = await asyncio.create_subprocess_shell(
        wrapped,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.STDOUT,
        env=build_env(cwd),
        executable="/bin/bash",
    )

    output_parts = []
    total = 0
    new_cwd_line = None

    try:
        async def read_output():
            nonlocal total, new_cwd_line
            assert proc.stdout
            while True:
                line = await proc.stdout.readline()
                if not line:
                    break
                decoded = line.decode("utf-8", errors="replace")
                if decoded.startswith(sentinel):
                    new_cwd_line = decoded[len(sentinel):].strip()
                    continue
                total += len(decoded)
                if total <= MAX_OUTPUT_SIZE:
                    output_parts.append(decoded)
                    if stream and ctx:
                        try:
                            ctx.info(decoded.rstrip())
                        except Exception:
                            pass

        await asyncio.wait_for(read_output(), timeout=COMMAND_TIMEOUT)
        await asyncio.wait_for(proc.wait(), timeout=5)
    except asyncio.TimeoutError:
        try:
            proc.kill()
        except Exception:
            pass
        await proc.wait()
        output_parts.append(f"\n⏱️ Timed out after {COMMAND_TIMEOUT}s")

    # Update session cwd if it changed and stays inside sandbox
    if session_id and new_cwd_line:
        new_cwd = Path(new_cwd_line)  # pwd -P already returns physical path
        if new_cwd.as_posix().startswith(ALLOWED_DIR.as_posix()):
            _sessions[session_id] = new_cwd
        else:
            # cd escaped sandbox — reset to root
            _sessions[session_id] = ALLOWED_DIR

    output = "".join(output_parts)
    if total > MAX_OUTPUT_SIZE:
        output += f"\n... (truncated — {total} chars total)"

    return output.rstrip() or "(no output)"


def _shell_quote(s: str) -> str:
    """Single-quote a string for safe embedding in a shell command."""
    return "'" + s.replace("'", "'\\''") + "'"


# ----------------------------------------------------------------------
# MCP server
# ----------------------------------------------------------------------
mcp = FastMCP("mcp-sh")


@mcp.tool()
async def shell_exec(
    command: str,
    session_id: Optional[str] = None,
    stream: bool = False,
    ctx: Context = None,
) -> str:
    """
    Execute any bash command inside the sandbox directory.

    Supports the full bash feature set: pipes, redirects, heredocs, loops,
    multi-line scripts, command substitution, wildcards, etc.

    Use session_id to persist the working directory (cd) between calls.
    """
    command = command.strip()
    if not command:
        return "❌ Empty command."

    cwd = get_session_cwd(session_id)
    if session_id and session_id not in _sessions:
        _sessions[session_id] = cwd

    try:
        return await run_bash(command, cwd, session_id, stream, ctx)
    except Exception as e:
        logger.exception("shell_exec error")
        return f"❌ Error: {e}"


@mcp.tool()
async def write_file(path: str, content: str, session_id: Optional[str] = None) -> str:
    """
    Write text content to a file inside the sandbox.

    Preferred alternative to heredocs for creating files with multi-line content.
    Creates parent directories automatically.
    """
    cwd = get_session_cwd(session_id)
    p = Path(path)
    if not p.is_absolute():
        p = cwd / p
    p = p.resolve()

    if not p.as_posix().startswith(ALLOWED_DIR.as_posix()):
        return f"❌ Path '{path}' is outside the sandbox."

    try:
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(content, encoding="utf-8")
        return f"✅ Written {len(content)} chars to {p.relative_to(ALLOWED_DIR)}"
    except Exception as e:
        return f"❌ {e}"


@mcp.tool()
async def read_file(path: str, session_id: Optional[str] = None) -> str:
    """Read a text file from inside the sandbox."""
    cwd = get_session_cwd(session_id)
    p = Path(path)
    if not p.is_absolute():
        p = cwd / p
    p = p.resolve()

    if not p.as_posix().startswith(ALLOWED_DIR.as_posix()):
        return f"❌ Path '{path}' is outside the sandbox."
    if not p.exists():
        return f"❌ File not found: {path}"
    try:
        return p.read_text(encoding="utf-8", errors="replace")
    except Exception as e:
        return f"❌ {e}"


@mcp.tool()
async def upload_file(path: str, content_base64: str, session_id: Optional[str] = None) -> str:
    """Upload a base64-encoded binary file into the sandbox."""
    cwd = get_session_cwd(session_id)
    p = Path(path)
    if not p.is_absolute():
        p = cwd / p
    p = p.resolve()

    if not p.as_posix().startswith(ALLOWED_DIR.as_posix()):
        return f"❌ Path '{path}' is outside the sandbox."

    try:
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_bytes(base64.b64decode(content_base64))
        return f"✅ Uploaded {p.stat().st_size} bytes to {p.relative_to(ALLOWED_DIR)}"
    except Exception as e:
        return f"❌ {e}"


@mcp.tool()
async def download_file(path: str, session_id: Optional[str] = None) -> str:
    """Download a file from the sandbox as base64 (max 10 MB)."""
    cwd = get_session_cwd(session_id)
    p = Path(path)
    if not p.is_absolute():
        p = cwd / p
    p = p.resolve()

    if not p.as_posix().startswith(ALLOWED_DIR.as_posix()):
        return f"❌ Path '{path}' is outside the sandbox."
    if not p.is_file():
        return f"❌ Not a file: {path}"

    data = p.read_bytes()
    if len(data) > 10 * 1024 * 1024:
        return "❌ File too large (>10 MB)"
    return base64.b64encode(data).decode("ascii")


@mcp.tool()
async def shell_info() -> str:
    """Show sandbox configuration and active sessions."""
    sessions_info = "\n".join(
        f"  {sid}: {cwd.relative_to(ALLOWED_DIR) or '/'}"
        for sid, cwd in _sessions.items()
    ) or "  (none)"
    denied = ", ".join(sorted(DENIED_COMMANDS)) or "(none)"
    return (
        f"📁 Sandbox root: {ALLOWED_DIR}\n"
        f"⏱️  Timeout: {COMMAND_TIMEOUT}s\n"
        f"📏 Max output: {MAX_OUTPUT_SIZE} chars\n"
        f"🚫 Denied commands: {denied}\n"
        f"💾 Active sessions:\n{sessions_info}\n"
        f"\n🐚 Shell: /bin/bash (full feature set)\n"
        f"📝 Tip: use write_file for multi-line file creation instead of heredocs."
    )


# ----------------------------------------------------------------------
# Entry point
# ----------------------------------------------------------------------
def main():
    global ALLOWED_DIR, DENIED_COMMANDS, COMMAND_TIMEOUT, MAX_OUTPUT_SIZE, VERBOSE

    parser = argparse.ArgumentParser(description="MCP Shell Server — bash-based sandbox")
    parser.add_argument("--root", default=str(Path.cwd()), help="Sandbox root directory")
    parser.add_argument("--timeout", type=int, default=60, help="Command timeout in seconds")
    parser.add_argument("--max-output", type=int, default=200_000, help="Max output chars")
    parser.add_argument(
        "--deny-commands", default="rm,rmdir,dd,mkfs,shutdown,reboot,sudo,su",
        help="Comma-separated list of denied command names"
    )
    parser.add_argument("--allow-all", action="store_true", help="Disable the deny list (unrestricted)")
    parser.add_argument("--verbose", action="store_true", help="Enable verbose logging to stderr")
    # Legacy flags (kept for config compatibility, ignored)
    parser.add_argument("--allow-chaining", action="store_true", help="(legacy, always enabled)")
    parser.add_argument("--allow-redirect", action="store_true", help="(legacy, always enabled)")

    args = parser.parse_args()

    ALLOWED_DIR = Path(args.root).resolve()
    ALLOWED_DIR.mkdir(parents=True, exist_ok=True)

    DENIED_COMMANDS = (
        set()
        if args.allow_all
        else {c.strip() for c in args.deny_commands.split(",") if c.strip()}
    )

    COMMAND_TIMEOUT = args.timeout
    MAX_OUTPUT_SIZE = args.max_output
    VERBOSE = args.verbose

    if VERBOSE:
        logging.basicConfig(level=logging.DEBUG, stream=sys.stderr,
                            format="%(asctime)s [%(levelname)s] %(message)s")
        logger.info(f"Sandbox root: {ALLOWED_DIR}")
        logger.info(f"Denied: {DENIED_COMMANDS}")

    mcp.run()


if __name__ == "__main__":
    main()
