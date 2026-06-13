#!/usr/bin/env python3
"""
Secure Python Sandbox MCP Server
Execute arbitrary Python code with resource limits, import restrictions, and optional Docker isolation.
"""

import ast
import sys
import os
import resource
import signal
import tempfile
import subprocess
import time
import re
import json
import argparse
from pathlib import Path
from typing import Any, List, Dict, Optional, Tuple

from mcp.server import Server
from mcp.server.stdio import stdio_server
import mcp.types as types

# ----------------------------------------------------------------------
# Configuration
# ----------------------------------------------------------------------
DEFAULT_TIMEOUT = 5  # seconds
DEFAULT_MEMORY_MB = 256
DEFAULT_OUTPUT_MAX = 50000  # characters
ALLOWED_MODULES = {
    "math",
    "random",
    "json",
    "re",
    "collections",
    "itertools",
    "functools",
    "string",
    "datetime",
    "calendar",
    "typing",
    "enum",
    "decimal",
    "statistics",
    "fractions",
    "array",
    "bisect",
    "heapq",
    "copy",
    "pprint",
    "textwrap",
    "struct",
    "hashlib",
    "base64",
    "binascii",
    "html",
    "urllib.parse",
}
# We'll block imports that are not in ALLOWED_MODULES via AST check
BLOCKED_MODULES = {
    "os",
    "sys",
    "subprocess",
    "socket",
    "signal",
    "multiprocessing",
    "threading",
    "ctypes",
    "cffi",
    "fcntl",
    "posix",
    "pty",
    "resource",
    "getpass",
    "pwd",
    "grp",
    "platform",
    "sysconfig",
    "distutils",
    "setuptools",
    "pip",
    "importlib",
    "__builtins__",
    "builtins",
    "code",
    "codeop",
    "compileall",
    "py_compile",
    "runpy",
    "trace",
    "profile",
    "cProfile",
    "pdb",
    "bdb",
    "inspect",
    "ast",
    "_ast",
}
# Also deny writing to files
BLOCKED_BUILTINS = {"open", "exec", "eval", "compile", "__import__", "breakpoint", "print"}

# Optional Docker
DOCKER_ENABLED = False
try:
    import docker

    DOCKER_ENABLED = True
except ImportError:
    pass


# ----------------------------------------------------------------------
# Security: AST validation
# ----------------------------------------------------------------------
def validate_code(code: str) -> Tuple[bool, str]:
    """
    Parse AST and check for dangerous imports, builtins, and syntax.
    Returns (is_safe, error_message)
    """
    try:
        tree = ast.parse(code)
    except SyntaxError as e:
        return False, f"Syntax error: {e}"

    for node in ast.walk(tree):
        # Block import statements
        if isinstance(node, ast.Import):
            for alias in node.names:
                mod = alias.name.split(".")[0]
                if mod in BLOCKED_MODULES or mod not in ALLOWED_MODULES:
                    return False, f"Import of '{mod}' is not allowed"
        if isinstance(node, ast.ImportFrom):
            mod = node.module.split(".")[0] if node.module else ""
            if mod in BLOCKED_MODULES or (mod and mod not in ALLOWED_MODULES):
                return False, f"Import from '{mod}' is not allowed"
        # Block dangerous builtins usage
        if isinstance(node, ast.Call) and isinstance(node.func, ast.Name):
            if node.func.id in BLOCKED_BUILTINS:
                return False, f"Use of '{node.func.id}' is forbidden"
        # Block attribute access like os.system
        if isinstance(node, ast.Attribute):
            if isinstance(node.value, ast.Name) and node.value.id in BLOCKED_MODULES:
                return False, f"Access to '{node.value.id}.{node.attr}' is forbidden"
    return True, ""


# ----------------------------------------------------------------------
# Resource limits (for subprocess)
# ----------------------------------------------------------------------
def set_limits(memory_mb: int):
    """Set memory and CPU limits for the current process (Linux only)."""
    try:
        # Memory limit in bytes
        resource.setrlimit(resource.RLIMIT_AS, (memory_mb * 1024 * 1024, memory_mb * 1024 * 1024))
        # CPU time limit (will be handled by timeout in subprocess)
        # resource.setrlimit(resource.RLIMIT_CPU, (cpu_seconds, cpu_seconds))
    except Exception:
        pass


# ----------------------------------------------------------------------
# Execution in a subprocess
# ----------------------------------------------------------------------
def run_code_in_subprocess(code: str, timeout: int, memory_mb: int) -> Dict[str, Any]:
    """
    Run code in a separate Python process with resource limits.
    Returns dict with stdout, stderr, returncode, timed_out.
    """
    # Create a temporary file to hold the code
    with tempfile.NamedTemporaryFile(mode="w", suffix=".py", delete=False) as f:
        # Patch builtins: remove open, exec, etc.
        safe_code = f"""
import sys
import builtins

# Remove dangerous builtins
for _b in {BLOCKED_BUILTINS}:
    if hasattr(builtins, _b):
        delattr(builtins, _b)

# Prevent writing to files
builtins.open = None

# Only allow printing to stdout (safe)
def safe_print(*args, **kwargs):
    sys.stdout.write(' '.join(str(a) for a in args) + '\\n')
builtins.print = safe_print

# Execute user code
_user_code = {repr(code)}
exec(_user_code, {{'__name__': '__main__'}})
"""
        f.write(safe_code)
        script_path = f.name

    try:
        # Run subprocess with resource limits
        # Use `prlimit` if available on Linux to set memory limit before exec
        cmd = [sys.executable, script_path]
        proc = subprocess.Popen(
            cmd,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            stdin=subprocess.DEVNULL,
            text=True,
            preexec_fn=lambda: set_limits(memory_mb) if hasattr(os, "setrlimit") else None,
        )
        try:
            stdout, stderr = proc.communicate(timeout=timeout)
            timed_out = False
        except subprocess.TimeoutExpired:
            proc.kill()
            stdout, stderr = proc.communicate()
            timed_out = True
        returncode = proc.returncode
    finally:
        os.unlink(script_path)

    # Truncate output
    if len(stdout) > DEFAULT_OUTPUT_MAX:
        stdout = stdout[:DEFAULT_OUTPUT_MAX] + "\n... (output truncated)"
    if len(stderr) > DEFAULT_OUTPUT_MAX:
        stderr = stderr[:DEFAULT_OUTPUT_MAX] + "\n... (output truncated)"

    return {"stdout": stdout, "stderr": stderr, "returncode": returncode, "timed_out": timed_out}


# ----------------------------------------------------------------------
# Docker execution (if enabled)
# ----------------------------------------------------------------------
def run_code_in_docker(code: str, timeout: int, memory_mb: int) -> Dict[str, Any]:
    if not DOCKER_ENABLED:
        return {"error": "Docker support not installed"}
    client = docker.from_env()
    # Create a temporary file with the code
    import tempfile

    with tempfile.NamedTemporaryFile(mode="w", suffix=".py", delete=False) as f:
        # Same safe wrapper as above but without builtin deletions (Docker provides network none)
        safe_code = f"""
import sys
sys.stdout = sys.stderr = open('/dev/null', 'w')  # disable output? No, we want capture.
# Actually we keep stdout/stderr
_exec_code = {repr(code)}
exec(_exec_code)
"""
        f.write(safe_code)
        script_path = f.name
    try:
        # Mount script, run with memory and CPU limits, network none
        container = client.containers.run(
            image="python:3.10-slim",
            command=["python", "/script.py"],
            volumes={script_path: {"bind": "/script.py", "mode": "ro"}},
            mem_limit=f"{memory_mb}m",
            nano_cpus=int(0.5 * 1e9),  # 0.5 CPU
            network_disabled=True,
            read_only=True,
            remove=True,
            detach=True,
        )
        try:
            result = container.wait(timeout=timeout)
            stdout = container.logs(stdout=True, stderr=False).decode()
            stderr = container.logs(stdout=False, stderr=True).decode()
            returncode = result["StatusCode"]
            timed_out = False
        except:
            container.kill()
            container.wait()
            stdout = ""
            stderr = "Execution timed out"
            returncode = -1
            timed_out = True
    finally:
        os.unlink(script_path)
    return {
        "stdout": stdout[:DEFAULT_OUTPUT_MAX],
        "stderr": stderr[:DEFAULT_OUTPUT_MAX],
        "returncode": returncode,
        "timed_out": timed_out,
    }


# ----------------------------------------------------------------------
# MCP tool handler
# ----------------------------------------------------------------------
async def handle_execute_python(arguments: dict, context=None) -> List[types.TextContent]:
    code = arguments.get("code", "")
    if not code:
        return [types.TextContent(type="text", text="❌ Missing 'code' argument.")]

    timeout = arguments.get("timeout", DEFAULT_TIMEOUT)
    memory_mb = arguments.get("memory_mb", DEFAULT_MEMORY_MB)

    # 1. Validate code statically
    safe, err = validate_code(code)
    if not safe:
        return [types.TextContent(type="text", text=f"❌ Security violation: {err}")]

    # 2. Execute
    if DOCKER_ENABLED and arguments.get("use_docker", False):
        result = run_code_in_docker(code, timeout, memory_mb)
    else:
        result = run_code_in_subprocess(code, timeout, memory_mb)

    # 3. Format output
    output = []
    if result.get("timed_out"):
        output.append(f"⏰ Execution timed out after {timeout} seconds.")
    if result.get("stdout"):
        output.append(f"📤 STDOUT:\n{result['stdout']}")
    if result.get("stderr"):
        output.append(f"⚠️ STDERR:\n{result['stderr']}")
    if "returncode" in result and result["returncode"] != 0:
        output.append(f"💀 Exit code: {result['returncode']}")
    if not output:
        output.append("✅ Code executed successfully (no output).")
    return [types.TextContent(type="text", text="\n\n".join(output))]


# ----------------------------------------------------------------------
# MCP Server
# ----------------------------------------------------------------------
server = Server("py-sandbox")

TOOLS = [
    types.Tool(
        name="execute_python",
        description="Execute Python code in a secure sandbox with resource limits. Allowed modules: math, random, json, re, collections, itertools, functools, string, datetime, etc. Dangerous imports (os, sys, subprocess, etc.) are blocked. Output is truncated after 50k chars.",
        inputSchema={
            "type": "object",
            "properties": {
                "code": {"type": "string", "description": "Python code to execute."},
                "timeout": {"type": "integer", "description": "Max seconds (default 5)."},
                "memory_mb": {"type": "integer", "description": "Memory limit in MB (default 256)."},
                "use_docker": {
                    "type": "boolean",
                    "description": "Use Docker for extra isolation (requires docker installed).",
                },
            },
            "required": ["code"],
        },
    )
]


@server.list_tools()
async def list_tools() -> List[types.Tool]:
    return TOOLS


@server.call_tool()
async def call_tool(name: str, arguments: dict, context: Any = None) -> List[types.TextContent]:
    if name == "execute_python":
        return await handle_execute_python(arguments, context)
    else:
        return [types.TextContent(type="text", text=f"Unknown tool: {name}")]


async def main():
    async with stdio_server() as (read_stream, write_stream):
        await server.run(read_stream, write_stream, server.create_initialization_options())


if __name__ == "__main__":
    import asyncio

    asyncio.run(main())
