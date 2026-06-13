#!/usr/bin/env python3
"""
Secure MCP Shell Server - State-of-the-art sandboxed shell access for MCP.
"""

import sys
import os
import shlex
import asyncio
import argparse
import logging
import base64
import re
from pathlib import Path
from typing import Any, Optional, List, Tuple, Dict, Set

from mcp.server.fastmcp import FastMCP, Context

# ----------------------------------------------------------------------
# Default configuration (can be overridden by command‑line arguments)
# ----------------------------------------------------------------------
DEFAULT_ALLOWED_COMMANDS: Set[str] = {
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
    "python3",
    "pip3",
    "pip",
    "python",
    "uv",
    "file",
    "stat",
    "du",
    "df",
}

DEFAULT_DENIED_COMMANDS: Set[str] = {
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
    "chgrp",
    "crontab",
    "mount",
    "umount",
    "systemctl",
    "service",
    "docker",
    "podman",
    "nsenter",
}

# Safe environment variables – only these will be passed to subprocesses
SAFE_ENV_VARS: Set[str] = {
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "LANG",
    "LC_ALL",
    "TERM",
    "PWD",
    "SHELL",
    "EDITOR",
    "VISUAL",
}

# ----------------------------------------------------------------------
# Global state (set at runtime)
# ----------------------------------------------------------------------
ALLOWED_DIR: Path = Path.cwd()
ALLOWED_COMMANDS: Set[str] = set()
DENIED_COMMANDS: Set[str] = set()
COMMAND_TIMEOUT: int = 30
MAX_OUTPUT_SIZE: int = 100_000
ALLOW_REDIRECT: bool = True
ALLOW_CHAINING: bool = True
ALLOW_BRACE_EXPANSION: bool = True
VERBOSE: bool = False

# Session management: maps session_id -> absolute path (inside sandbox) of virtual cwd
_sessions: Dict[str, Path] = {}

# ----------------------------------------------------------------------
# Logging setup (only if VERBOSE)
# ----------------------------------------------------------------------
logger = logging.getLogger("secure_shell_server")
if VERBOSE:
    logging.basicConfig(level=logging.INFO, stream=sys.stderr, format="%(asctime)s [%(levelname)s] %(message)s")
else:
    logger.addHandler(logging.NullHandler())


# ----------------------------------------------------------------------
# Brace expansion (safe, does not execute anything)
# ----------------------------------------------------------------------
def expand_brace_pattern(token: str) -> List[str]:
    """
    Expand a brace pattern in a single token.
    Example: 'a{b,c}d' -> ['abd', 'acd']
    Supports nested braces and escaped characters.
    """
    # Find the first unescaped '{' that has a matching '}'
    stack = []
    start = -1
    i = 0
    n = len(token)
    while i < n:
        ch = token[i]
        if ch == "\\" and i + 1 < n:
            i += 2
            continue
        if ch == "{":
            if not stack:
                start = i
            stack.append(i)
        elif ch == "}":
            if stack:
                stack.pop()
                if not stack:
                    # complete brace group from start to i
                    prefix = token[:start]
                    suffix = token[i + 1 :]
                    inner = token[start + 1 : i]
                    # split inner by commas at top level (respect nested braces)
                    choices = []
                    depth = 0
                    j = 0
                    last = 0
                    inner_len = len(inner)
                    while j < inner_len:
                        c = inner[j]
                        if c == "{":
                            depth += 1
                        elif c == "}":
                            depth -= 1
                        elif c == "," and depth == 0:
                            choices.append(inner[last:j])
                            last = j + 1
                        j += 1
                    choices.append(inner[last:])
                    # recursively expand each choice
                    expanded_choices = []
                    for choice in choices:
                        sub_exp = expand_brace_pattern(choice)
                        expanded_choices.extend(sub_exp)
                    # combine prefix + each choice + suffix
                    results = []
                    for choice in expanded_choices:
                        results.append(prefix + choice + suffix)
                    return results
        i += 1
    # No braces found, return the token as a single-element list
    return [token]


def expand_braces_in_tokens(tokens: List[str]) -> List[str]:
    """
    Expand any tokens that contain brace patterns.
    Returns a new list where each original token may be replaced by multiple tokens.
    """
    if not ALLOW_BRACE_EXPANSION:
        return tokens
    new_tokens = []
    for t in tokens:
        if "{" in t and "}" in t and not (t.startswith("'") or t.startswith('"')):
            # potential brace expansion – expand it
            expanded = expand_brace_pattern(t)
            new_tokens.extend(expanded)
        else:
            new_tokens.append(t)
    return new_tokens


# ----------------------------------------------------------------------
# Security helpers
# ----------------------------------------------------------------------
def is_dangerous_pattern(command: str) -> bool:
    """Block command substitution, backticks, globbing, and other risky syntax."""
    dangerous = [
        "`",  # backticks
        "$(",  # command substitution
        "${",  # variable expansion (can be unsafe)
        "*",  # wildcard – force user to use `find` or explicit paths
        "?",  # single‑character wildcard
        "[",  # character classes in globs
        "]",  # ...
    ]
    for pat in dangerous:
        if pat in command:
            return True
    return False


def sanitize_command(cmd_str: str) -> List[str]:
    """Tokenize, expand braces, and block unsafe operators and patterns."""
    if is_dangerous_pattern(cmd_str):
        raise ValueError("Command contains dangerous pattern (wildcard, substitution, or backticks).")

    tokens = shlex.split(cmd_str)
    # Expand braces after tokenization but before further processing
    tokens = expand_braces_in_tokens(tokens)

    for i, token in enumerate(tokens):
        if token == "|":
            raise ValueError("Pipe operator '|' is not allowed (use files or chaining instead).")
        if not ALLOW_CHAINING and token in ("&&", ";", "||"):
            raise ValueError(f"Chaining operator '{token}' not allowed (use --allow-chaining).")
        if not ALLOW_REDIRECT and token in (">", ">>", "<"):
            raise ValueError(f"Redirection operator '{token}' not allowed (use --allow-redirect).")
    return tokens


def is_path_like(token: str) -> bool:
    """Determine if a token should be treated as a file/directory path."""
    if token.startswith("-"):
        return False
    if token in ALLOWED_COMMANDS:
        return False
    if "/" in token or token.startswith("~") or token in (".", ".."):
        return True
    # Heuristic for common file extensions
    if any(token.endswith(ext) for ext in [".py", ".txt", ".md", ".c", ".h", ".json", ".yml", ".log", ".csv"]):
        return True
    return False


def resolve_path(path_str: str, base_dir: Path) -> Path:
    """
    Resolve a path relative to base_dir (which must be inside ALLOWED_DIR).
    Returns an absolute Path guaranteed to be inside ALLOWED_DIR.
    """
    p = Path(path_str)
    if p.is_absolute():
        resolved = p.resolve()
    else:
        resolved = (base_dir / p).resolve()
    if not resolved.as_posix().startswith(ALLOWED_DIR.as_posix()):
        raise ValueError(f"Path '{path_str}' escapes sandbox.")
    return resolved


def resolve_and_contain_path(token: str, cwd: Path) -> str:
    """
    Resolve a path‑like token to a string relative to ALLOWED_DIR (safe for subprocess).
    Uses the given cwd (virtual session directory) for relative paths.
    """
    path = Path(token)
    if token.startswith("/"):
        resolved = path.resolve()
        if not resolved.as_posix().startswith(ALLOWED_DIR.as_posix()):
            raise ValueError(f"Access denied: absolute path '{token}' escapes sandbox.")
        return str(resolved.relative_to(ALLOWED_DIR))
    if token.startswith("~"):
        raise ValueError("Home directory expansion (~) is not allowed.")
    # Relative path – resolve against session cwd
    resolved = (cwd / token).resolve()
    if not resolved.as_posix().startswith(ALLOWED_DIR.as_posix()):
        raise ValueError(f"Access denied: relative path '{token}' escapes sandbox.")
    return str(resolved.relative_to(ALLOWED_DIR))


def rewrite_command(tokens: List[str], cwd: Path) -> List[str]:
    """Replace path‑like tokens with safe, sandboxed paths (relative to ALLOWED_DIR)."""
    rewritten = []
    for token in tokens:
        if is_path_like(token):
            try:
                safe = resolve_and_contain_path(token, cwd)
                rewritten.append(safe)
            except ValueError as e:
                raise ValueError(f"Path error for '{token}': {e}")
        else:
            rewritten.append(token)
    return rewritten


def parse_chained_commands(tokens: List[str]) -> List[Tuple[List[str], Optional[str]]]:
    """Split tokens into (command_tokens, separator) pairs."""
    commands = []
    current = []
    sep = None
    for token in tokens:
        if token in ("&&", "||", ";"):
            if current:
                commands.append((current, sep))
                current = []
            sep = token
        else:
            current.append(token)
    if current or not commands:
        commands.append((current, sep))
    return commands


def parse_redirect(tokens: List[str]) -> Tuple[List[str], Optional[str], Optional[str]]:
    """Extract the last redirection (>, >>) and its filename from a command token list."""
    for i, token in enumerate(tokens):
        if token in (">", ">>"):
            if i + 1 >= len(tokens):
                raise ValueError("Redirect operator at end of command without filename.")
            mode = "overwrite" if token == ">" else "append"
            file_token = tokens[i + 1]
            # Filename will be resolved later with the proper cwd
            return tokens[:i], mode, file_token
    return tokens, None, None


def build_env(cwd: Path) -> Dict[str, str]:
    """Build a safe environment dictionary with only allowlisted variables."""
    env = {}
    for var in SAFE_ENV_VARS:
        if var in os.environ:
            env[var] = os.environ[var]
    # Force PWD to the virtual session cwd (which is inside sandbox)
    env["PWD"] = str(cwd)
    return env


# ----------------------------------------------------------------------
# Session management
# ----------------------------------------------------------------------
def get_session_cwd(session_id: Optional[str]) -> Path:
    """Return the absolute path of the virtual working directory for a session."""
    if session_id is None:
        # No session: always root, no persistence
        return ALLOWED_DIR
    if session_id not in _sessions:
        _sessions[session_id] = ALLOWED_DIR
    return _sessions[session_id]


def update_session_cwd(session_id: str, new_cwd: Path) -> None:
    """Update the virtual cwd for a session. new_cwd must be inside ALLOWED_DIR."""
    _sessions[session_id] = new_cwd


def handle_cd_command(tokens: List[str], session_id: str, current_cwd: Path) -> Tuple[str, int, Optional[Path]]:
    """
    Process a `cd` command internally.
    Returns (output_message, exit_code, new_cwd_or_None_if_error)
    """
    if session_id is None:
        return "❌ `cd` command requires a session_id (persistent state).", 1, None
    if len(tokens) == 1:
        # cd without argument -> go to sandbox root
        new_path = ALLOWED_DIR
    else:
        target = tokens[1]
        try:
            new_path = resolve_path(target, current_cwd)
        except ValueError as e:
            return f"❌ {e}", 1, None
    return f"✅ Changed directory to {new_path.relative_to(ALLOWED_DIR)}", 0, new_path


# ----------------------------------------------------------------------
# Command execution core (with streaming)
# ----------------------------------------------------------------------
async def run_single_command(
    tokens: List[str],
    cwd: Path,
    stream_output: bool,
    context: Optional[Any] = None,
) -> Tuple[str, int]:
    """
    Execute a single command. If stream_output is True and context is provided,
    send incremental output via MCP log messages.
    """
    if not tokens:
        return "", 0

    cmd_name = tokens[0]
    if cmd_name in DENIED_COMMANDS:
        raise PermissionError(f"Command '{cmd_name}' is explicitly denied.")
    if cmd_name not in ALLOWED_COMMANDS:
        raise ValueError(f"Command '{cmd_name}' is not in the allowed list.")

    safe_tokens = rewrite_command(tokens, cwd)
    env = build_env(cwd)

    logger.info(f"Executing in {cwd}: {' '.join(safe_tokens)}")

    # Create subprocess with pipes
    proc = await asyncio.create_subprocess_exec(
        *safe_tokens,
        cwd=str(cwd),
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
        env=env,
    )

    output_lines = []
    total_len = 0

    async def read_stream(stream, log_level: str):
        nonlocal total_len
        while True:
            line = await stream.readline()
            if not line:
                break
            decoded = line.decode("utf-8", errors="replace")
            total_len += len(decoded)
            if stream_output and context is not None:
                try:
                    await context.info(decoded.rstrip("\n"))
                except:
                    pass  # ignore if context doesn't support it
            if total_len <= MAX_OUTPUT_SIZE:
                output_lines.append(decoded)

    try:
        await asyncio.wait_for(
            asyncio.gather(read_stream(proc.stdout, "info"), read_stream(proc.stderr, "error"), return_exceptions=True),
            timeout=COMMAND_TIMEOUT,
        )
        exit_code = await asyncio.wait_for(proc.wait(), timeout=5)
    except asyncio.TimeoutError:
        proc.kill()
        await proc.wait()
        raise TimeoutError(f"Command timed out after {COMMAND_TIMEOUT} seconds.")
    except Exception as e:
        raise RuntimeError(f"Subprocess error: {e}")

    full_output = "".join(output_lines)
    if len(full_output) > MAX_OUTPUT_SIZE:
        full_output = full_output[:MAX_OUTPUT_SIZE] + "\n... (output truncated)"

    return full_output, exit_code


async def execute_with_redirect(
    tokens: List[str], cwd: Path, stream_output: bool, context: Optional[Any]
) -> Tuple[str, int]:
    """Run a command, optionally redirecting its stdout to a file."""
    cmd_tokens, mode, redirect_filename = parse_redirect(tokens)
    if redirect_filename:
        # resolve the redirect filename against current cwd
        try:
            resolved_file = resolve_path(redirect_filename, cwd)
        except ValueError as e:
            raise ValueError(f"Redirect path error: {e}")
        # Run command without redirect
        output, exit_code = await run_single_command(cmd_tokens, cwd, stream_output, context)
        # Write output to file
        resolved_file.parent.mkdir(parents=True, exist_ok=True)
        write_mode = "w" if mode == "overwrite" else "a"
        with open(resolved_file, write_mode, encoding="utf-8") as f:
            f.write(output)
        result_msg = f"✅ Output written to {resolved_file.relative_to(ALLOWED_DIR)}"
        return result_msg, exit_code
    else:
        return await run_single_command(tokens, cwd, stream_output, context)


async def execute_chained_commands(
    commands: List[Tuple[List[str], Optional[str]]],
    cwd: Path,
    stream_output: bool,
    context: Optional[Any],
) -> Tuple[str, int]:
    """Execute a sequence of commands with short‑circuit evaluation."""
    outputs = []
    last_exit_code = 0
    should_run = True

    for i, (cmd_tokens, sep) in enumerate(commands):
        if not cmd_tokens:
            continue

        # Determine if this command should run based on previous exit code and separator
        if i > 0 and sep is not None:
            if sep == "&&" and last_exit_code != 0:
                should_run = False
            elif sep == "||" and last_exit_code == 0:
                should_run = False
            # ';' always runs

        if not should_run:
            outputs.append(f"Skipped command group {i + 1} (due to previous exit code {last_exit_code})")
            continue

        try:
            if ALLOW_REDIRECT:
                output, last_exit_code = await execute_with_redirect(cmd_tokens, cwd, stream_output, context)
            else:
                output, last_exit_code = await run_single_command(cmd_tokens, cwd, stream_output, context)
            if output.strip():
                outputs.append(output)
            if last_exit_code != 0 and sep != "||":
                outputs.append(f"⚠️ Command exited with code {last_exit_code}")
        except (ValueError, PermissionError, TimeoutError, RuntimeError) as e:
            outputs.append(f"❌ Error: {e}")
            last_exit_code = 1
            if i == 0 or sep in ("&&", ";"):
                break

    result = "\n".join(outputs) if outputs else "(no output)"
    return result, last_exit_code


# ----------------------------------------------------------------------
# FastMCP server
# ----------------------------------------------------------------------
mcp = FastMCP("mcp-sh")


@mcp.tool()
async def shell_exec(command: str, session_id: Optional[str] = None, stream: bool = False, ctx: Context = None) -> str:
    """Execute a shell command inside the sandbox. Use session_id to persist cd state between calls."""
    cmd_str = command.strip()
    if not cmd_str:
        return "❌ Empty command."

    try:
        tokens = sanitize_command(cmd_str)
    except ValueError as e:
        return f"❌ Command rejected: {e}"

    # Get current virtual cwd for this session
    current_cwd = get_session_cwd(session_id)

    # Handle `cd` built‑in
    if tokens and tokens[0] == "cd":
        output, exit_code, new_cwd = handle_cd_command(tokens, session_id, current_cwd)
        if new_cwd is not None and session_id is not None:
            update_session_cwd(session_id, new_cwd)
        return output

    if ALLOW_CHAINING:
        command_groups = parse_chained_commands(tokens)
    else:
        command_groups = [(tokens, None)]

    result_output, final_exit_code = await execute_chained_commands(command_groups, current_cwd, stream, ctx)
    return result_output


@mcp.tool()
async def shell_info() -> str:
    """Display current sandbox configuration and active sessions."""
    allowed = ", ".join(sorted(ALLOWED_COMMANDS))
    denied = ", ".join(sorted(DENIED_COMMANDS))
    return (
        f"✅ Allowed commands: {allowed}\n"
        f"🚫 Denied commands: {denied}\n"
        f"📁 Sandbox root: {ALLOWED_DIR}\n"
        f"📝 Redirect: {'enabled' if ALLOW_REDIRECT else 'disabled'}\n"
        f"🔗 Chaining: {'enabled' if ALLOW_CHAINING else 'disabled'}\n"
        f"⏱️ Timeout: {COMMAND_TIMEOUT}s\n"
        f"📏 Max output: {MAX_OUTPUT_SIZE} chars\n"
        f"🔒 Safe env vars: {', '.join(sorted(SAFE_ENV_VARS))}\n"
        f"💾 Active sessions: {len(_sessions)}"
    )


@mcp.tool()
async def upload_file(path: str, content_base64: str, session_id: Optional[str] = None) -> str:
    """Upload a base64-encoded file into the sandbox."""
    if not path or not content_base64:
        return "❌ Missing 'path' or 'content_base64'."

    cwd = get_session_cwd(session_id)
    try:
        full_path = resolve_path(path, cwd)
    except ValueError as e:
        return f"❌ Invalid path: {e}"

    try:
        full_path.parent.mkdir(parents=True, exist_ok=True)
        file_data = base64.b64decode(content_base64)
        with open(full_path, "wb") as f:
            f.write(file_data)
        rel_path = full_path.relative_to(ALLOWED_DIR)
        return f"✅ File uploaded to {rel_path}"
    except Exception as e:
        return f"❌ Upload failed: {e}"


@mcp.tool()
async def download_file(path: str, session_id: Optional[str] = None) -> str:
    """Download a file from the sandbox as base64 (max 10 MB)."""
    if not path:
        return "❌ Missing 'path'."

    cwd = get_session_cwd(session_id)
    try:
        full_path = resolve_path(path, cwd)
    except ValueError as e:
        return f"❌ Invalid path: {e}"

    if not full_path.is_file():
        return f"❌ Not a file: {path}"

    try:
        with open(full_path, "rb") as f:
            file_data = f.read()
        max_download = 10 * 1024 * 1024
        if len(file_data) > max_download:
            return "❌ File too large (>10 MB)"
        return base64.b64encode(file_data).decode("ascii")
    except Exception as e:
        return f"❌ Download failed: {e}"


# ----------------------------------------------------------------------
# Main entry point (updated with --allow-brace-expansion)
# ----------------------------------------------------------------------
def main():
    global ALLOWED_DIR, ALLOWED_COMMANDS, DENIED_COMMANDS
    global COMMAND_TIMEOUT, MAX_OUTPUT_SIZE, ALLOW_REDIRECT, ALLOW_CHAINING, ALLOW_BRACE_EXPANSION, VERBOSE

    parser = argparse.ArgumentParser(
        description="Secure MCP Shell Server – sandboxed shell access with sessions and file tools.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
Examples:
  # Basic server (no chaining, no redirect)
  %(prog)s

  # Enable session state (cd) and file upload/download
  %(prog)s --allow-chaining --allow-redirect

  # Custom sandbox root
  %(prog)s --root /path/to/sandbox

  # Disable brace expansion (safer but less convenient)
  %(prog)s --no-brace-expansion

  # Increase timeout and output limit
  %(prog)s --timeout 60 --max-output 500000
        """,
    )
    parser.add_argument("--root", type=str, default=str(Path.cwd()), help="Sandbox root directory.")
    parser.add_argument(
        "--allow-commands", type=str, default="", help="Additional commands to allow (comma‑separated)."
    )
    parser.add_argument("--deny-commands", type=str, default="", help="Additional commands to deny (comma‑separated).")
    parser.add_argument("--timeout", type=int, default=30, help="Command timeout in seconds.")
    parser.add_argument("--max-output", type=int, default=100_000, help="Maximum output characters per command.")
    parser.add_argument("--allow-redirect", action="store_true", help="Allow > and >> redirection operators.")
    parser.add_argument("--allow-chaining", action="store_true", help="Allow &&, ||, ; chaining operators.")
    parser.add_argument(
        "--allow-brace-expansion",
        action="store_true",
        default=True,
        help="Enable safe brace expansion (e.g., mkdir {a,b,c}) [default: True]",
    )
    parser.add_argument(
        "--no-brace-expansion", dest="allow_brace_expansion", action="store_false", help="Disable brace expansion"
    )
    parser.add_argument("--verbose", action="store_true", help="Enable verbose logging to stderr.")

    args = parser.parse_args()

    ALLOWED_DIR = Path(args.root).resolve()
    ALLOWED_DIR.mkdir(parents=True, exist_ok=True)

    ALLOWED_COMMANDS = DEFAULT_ALLOWED_COMMANDS.copy()
    DENIED_COMMANDS = DEFAULT_DENIED_COMMANDS.copy()

    if args.allow_commands:
        extra = set(c.strip() for c in args.allow_commands.split(",") if c.strip())
        ALLOWED_COMMANDS.update(extra)

    if args.deny_commands:
        extra_deny = set(c.strip() for c in args.deny_commands.split(",") if c.strip())
        DENIED_COMMANDS.update(extra_deny)
        ALLOWED_COMMANDS.difference_update(extra_deny)

    COMMAND_TIMEOUT = args.timeout
    MAX_OUTPUT_SIZE = args.max_output
    ALLOW_REDIRECT = args.allow_redirect
    ALLOW_CHAINING = args.allow_chaining
    ALLOW_BRACE_EXPANSION = args.allow_brace_expansion
    VERBOSE = args.verbose

    if VERBOSE:
        logging.basicConfig(level=logging.INFO, stream=sys.stderr, format="%(asctime)s [%(levelname)s] %(message)s")
        logger.info("Secure Shell Server starting")
        logger.info(f"Sandbox root: {ALLOWED_DIR}")
        logger.info(f"Allowed commands: {len(ALLOWED_COMMANDS)}")
        logger.info(f"Redirect: {ALLOW_REDIRECT}, Chaining: {ALLOW_CHAINING}, Brace expansion: {ALLOW_BRACE_EXPANSION}")

    mcp.run()


if __name__ == "__main__":
    main()
