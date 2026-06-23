#!/usr/bin/env python3
"""
MCP Shell Server — bash-based, LLM-friendly sandboxed shell.
"""

import os
import re
import sys
import signal
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

# OS-level filesystem confinement. When active, shell_exec runs under
# `sandbox-exec` so that writes are physically denied anywhere outside the
# sandbox subtree — not just discouraged by the denylist. Set in main().
ISOLATION: str = "auto"          # auto | write | off (requested)
SANDBOX_ENABLED: bool = False    # whether confinement is actually in effect
SANDBOX_PROFILE: str = ""        # generated seatbelt profile (when enabled)
SANDBOX_EXEC: str = "/usr/bin/sandbox-exec"

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


def is_within_sandbox(p: Path) -> bool:
    """True if `p` is ALLOWED_DIR itself or a path nested inside it.

    Uses path-component comparison rather than string prefix matching, so a
    sibling like `/root/box-evil` is not treated as inside `/root/box`.
    """
    return p == ALLOWED_DIR or ALLOWED_DIR in p.parents


def sandbox_tmpdir() -> Path:
    """Temp directory kept inside the sandbox so temp writes stay confined."""
    return ALLOWED_DIR / ".tmp"


def build_env(cwd: Path) -> Dict[str, str]:
    """Pass through the full environment but pin HOME, PWD and TMPDIR to the sandbox."""
    env = dict(os.environ)
    env["PWD"] = str(cwd)
    env["HOME"] = str(ALLOWED_DIR)
    # Keep temp files inside the sandbox. Under OS confinement, writes to the
    # system temp dir are denied, so tools must use a sandbox-local TMPDIR.
    tmp = str(sandbox_tmpdir())
    env["TMPDIR"] = tmp
    env["TMP"] = tmp
    env["TEMP"] = tmp
    return env


def _sb_quote(path: str) -> str:
    """Quote a path for a seatbelt profile string literal."""
    return path.replace("\\", "\\\\").replace('"', '\\"')


def build_sandbox_profile(root: Path) -> str:
    """Build a sandbox-exec (seatbelt) profile that confines writes to `root`.

    Reads are left unrestricted so binaries, libraries and tools load normally;
    every write outside the sandbox subtree (plus a few device files) is denied
    at the kernel level. This is real confinement, not command parsing.
    """
    box = _sb_quote(str(root))
    return (
        "(version 1)\n"
        "(allow default)\n"
        "(deny file-write*)\n"
        "(allow file-write*\n"
        f'  (subpath "{box}")\n'
        '  (literal "/dev/null") (literal "/dev/zero")\n'
        '  (literal "/dev/tty") (literal "/dev/stdout") (literal "/dev/stderr")\n'
        '  (subpath "/dev/fd"))\n'
    )


def detect_sandbox(isolation: str) -> bool:
    """Decide whether OS confinement is active for the requested isolation mode.

    Returns True if shell_exec should run under sandbox-exec. For mode "write"
    (required), raises RuntimeError when confinement is unavailable so the
    server fails closed rather than silently running unconfined.
    """
    if isolation == "off":
        return False
    available = sys.platform == "darwin" and os.path.exists(SANDBOX_EXEC)
    if available:
        return True
    if isolation == "write":
        raise RuntimeError(
            "OS sandbox required (--isolation write) but sandbox-exec is "
            f"unavailable on this platform ({sys.platform})."
        )
    # auto: degrade to unconfined, but make the loss of protection loud.
    logger.warning(
        "OS filesystem confinement unavailable on %s; shell_exec runs "
        "UNCONFINED (denylist only). Use --isolation off to silence.",
        sys.platform,
    )
    return False


def first_word(cmd: str) -> str:
    """Return the first word of a command string."""
    stripped = cmd.lstrip()
    parts = stripped.split()
    return parts[0] if parts else ""


# Shell separators that begin a new command: ; & | && || and newlines.
_SEGMENT_SPLIT = re.compile(r"[;\n]|&&|\|\||[|&]")


def is_denied(cmd: str) -> Optional[str]:
    """Return a denied command name if any chained segment starts with one.

    Splits on shell separators (`;`, `&&`, `||`, `|`, `&`, newlines) so that
    chained invocations like `ls; rm -rf .` are still caught. This is a
    best-effort guard, not a security boundary — substitutions such as
    `$(rm ...)` cannot be detected this way.
    """
    if not DENIED_COMMANDS:
        return None
    for segment in _SEGMENT_SPLIT.split(cmd):
        word = first_word(segment)
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

    if SANDBOX_ENABLED:
        # Run bash under sandbox-exec so writes outside the sandbox are denied
        # by the kernel, not merely discouraged by the denylist.
        argv = [SANDBOX_EXEC, "-p", SANDBOX_PROFILE, "/bin/bash", "-c", wrapped]
    else:
        argv = ["/bin/bash", "-c", wrapped]

    proc = await asyncio.create_subprocess_exec(
        *argv,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.STDOUT,
        env=build_env(cwd),
        # Put the shell in its own process group so a timeout can kill the
        # whole tree (the shell plus any children it spawned), not just bash.
        start_new_session=True,
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
                            await ctx.info(decoded.rstrip())
                        except Exception:
                            pass

        await asyncio.wait_for(read_output(), timeout=COMMAND_TIMEOUT)
        await asyncio.wait_for(proc.wait(), timeout=5)
    except asyncio.TimeoutError:
        _kill_process_tree(proc)
        try:
            await asyncio.wait_for(proc.wait(), timeout=5)
        except asyncio.TimeoutError:
            pass
        output_parts.append(f"\n⏱️ Timed out after {COMMAND_TIMEOUT}s")

    # Update session cwd if it changed and stays inside sandbox
    if session_id and new_cwd_line:
        new_cwd = Path(new_cwd_line)  # pwd -P already returns physical path
        if is_within_sandbox(new_cwd):
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


def _kill_process_tree(proc: asyncio.subprocess.Process) -> None:
    """Kill the subprocess and any children it spawned (its process group)."""
    if proc.returncode is not None:
        return
    try:
        os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
    except (ProcessLookupError, PermissionError):
        # Group already gone, or we can't signal it — fall back to the shell.
        try:
            proc.kill()
        except ProcessLookupError:
            pass


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

    if not is_within_sandbox(p):
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

    if not is_within_sandbox(p):
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

    if not is_within_sandbox(p):
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

    if not is_within_sandbox(p):
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
        f"  {sid}: {cwd.relative_to(ALLOWED_DIR)}"
        for sid, cwd in _sessions.items()
    ) or "  (none)"
    denied = ", ".join(sorted(DENIED_COMMANDS)) or "(none)"
    if SANDBOX_ENABLED:
        isolation = "🔒 OS write-confinement ACTIVE (writes denied outside sandbox)"
    else:
        isolation = "🔓 OS confinement OFF (denylist only — writes NOT confined)"
    return (
        f"📁 Sandbox root: {ALLOWED_DIR}\n"
        f"{isolation}\n"
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
    global ISOLATION, SANDBOX_ENABLED, SANDBOX_PROFILE

    parser = argparse.ArgumentParser(description="MCP Shell Server — bash-based sandbox")
    parser.add_argument("--root", default=str(Path.cwd()), help="Sandbox root directory")
    parser.add_argument("--timeout", type=int, default=60, help="Command timeout in seconds")
    parser.add_argument("--max-output", type=int, default=200_000, help="Max output chars")
    parser.add_argument(
        "--deny-commands", default="rm,rmdir,dd,mkfs,shutdown,reboot,sudo,su",
        help="Comma-separated list of denied command names"
    )
    parser.add_argument("--allow-all", action="store_true", help="Disable the deny list (unrestricted)")
    parser.add_argument(
        "--isolation", choices=["auto", "write", "off"], default="auto",
        help="OS filesystem confinement: 'write' confines writes to the sandbox "
             "(required, fails closed if unavailable), 'auto' uses it when "
             "available, 'off' disables it (default: auto)",
    )
    parser.add_argument("--verbose", action="store_true", help="Enable verbose logging to stderr")
    # Legacy flags (kept for config compatibility, ignored)
    parser.add_argument("--allow-chaining", action="store_true", help="(legacy, always enabled)")
    parser.add_argument("--allow-redirect", action="store_true", help="(legacy, always enabled)")

    args = parser.parse_args()

    # Configure logging early so confinement warnings are visible by default.
    logging.basicConfig(
        level=logging.DEBUG if args.verbose else logging.WARNING,
        stream=sys.stderr,
        format="%(asctime)s [%(levelname)s] %(message)s",
    )

    ALLOWED_DIR = Path(args.root).resolve()
    ALLOWED_DIR.mkdir(parents=True, exist_ok=True)
    sandbox_tmpdir().mkdir(parents=True, exist_ok=True)

    DENIED_COMMANDS = (
        set()
        if args.allow_all
        else {c.strip() for c in args.deny_commands.split(",") if c.strip()}
    )

    COMMAND_TIMEOUT = args.timeout
    MAX_OUTPUT_SIZE = args.max_output
    VERBOSE = args.verbose

    ISOLATION = args.isolation
    SANDBOX_ENABLED = detect_sandbox(ISOLATION)
    if SANDBOX_ENABLED:
        SANDBOX_PROFILE = build_sandbox_profile(ALLOWED_DIR)

    if VERBOSE:
        logger.info(f"Sandbox root: {ALLOWED_DIR}")
        logger.info(f"Denied: {DENIED_COMMANDS}")
        logger.info(f"OS confinement: {'on' if SANDBOX_ENABLED else 'off'} (isolation={ISOLATION})")

    mcp.run()


if __name__ == "__main__":
    main()
