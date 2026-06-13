#!/usr/bin/env python3
"""
MCP File System Server (v3) — FastMCP edition
- Async I/O with aiofiles, sessions with virtual cwd
- Streaming reads, file hashing, batch operations
- Compression (zip/tar), token counting, strict path sandboxing
"""

import os
import stat
import shutil
import base64
import argparse
import asyncio
import hashlib
import json
import re
import time
import zipfile
import tarfile
from pathlib import Path
from typing import Any, Dict, List, Literal, Optional, Tuple, Set
from datetime import datetime

import aiofiles
import aiofiles.os

from mcp.server.fastmcp import FastMCP, Context

try:
    import tiktoken
    TIKTOKEN_AVAILABLE = True
except ImportError:
    TIKTOKEN_AVAILABLE = False

# ----------------------------------------------------------------------
# Configuration
# ----------------------------------------------------------------------
ALLOWED_DIR: Path = Path.cwd().resolve()
MAX_FILE_SIZE: int = 100 * 1024 * 1024  # 100 MB
ALLOWED_EXTENSIONS: Optional[Set[str]] = None  # None = all
SESSION_TIMEOUT_SECONDS: int = 3600

_sessions: Dict[str, Dict[str, Any]] = {}


# ----------------------------------------------------------------------
# Helpers
# ----------------------------------------------------------------------
def safe_path(relative: str, cwd: Optional[Path] = None) -> Path:
    base = cwd if cwd is not None else ALLOWED_DIR
    try:
        candidate = (base / relative).resolve()
        real = candidate.resolve()
        if not real.as_posix().startswith(ALLOWED_DIR.as_posix()):
            raise ValueError("Access denied: path escapes the allowed directory.")
        if not candidate.as_posix().startswith(ALLOWED_DIR.as_posix()):
            raise ValueError("Access denied: symlink points outside allowed directory.")
        return candidate
    except (OSError, ValueError) as e:
        raise ValueError(f"Invalid path '{relative}': {e}")


def check_file_size(path: Path) -> None:
    if path.is_file() and path.stat().st_size > MAX_FILE_SIZE:
        raise ValueError(f"File too large ({path.stat().st_size} > {MAX_FILE_SIZE} bytes)")


def check_extension(path: Path) -> None:
    if ALLOWED_EXTENSIONS is not None:
        ext = path.suffix.lower()
        if ext not in ALLOWED_EXTENSIONS:
            raise ValueError(f"File extension '{ext}' not allowed. Allowed: {ALLOWED_EXTENSIONS}")


def get_session(session_id: Optional[str]) -> Path:
    if session_id is None:
        return ALLOWED_DIR
    now = time.time()
    if session_id not in _sessions:
        _sessions[session_id] = {"cwd": ALLOWED_DIR, "last_access": now}
    else:
        _sessions[session_id]["last_access"] = now
    return _sessions[session_id]["cwd"]


def update_session_cwd(session_id: str, new_cwd: Path) -> None:
    _sessions[session_id]["cwd"] = new_cwd


async def compute_hash(path: Path, algo: str = "sha256") -> str:
    hash_func = hashlib.md5() if algo == "md5" else hashlib.sha256()
    async with aiofiles.open(path, "rb") as f:
        while chunk := await f.read(8192):
            hash_func.update(chunk)
    return hash_func.hexdigest()


def count_tokens(text: str, model: str = "gpt-4") -> int:
    if TIKTOKEN_AVAILABLE:
        encoding = tiktoken.encoding_for_model(model)
        return len(encoding.encode(text))
    return len(text) // 4


async def _walk_files(path: Path):
    for entry in path.iterdir():
        if entry.is_dir():
            async for sub in _walk_files(entry):
                yield sub
        else:
            yield entry


# ----------------------------------------------------------------------
# MCP server
# ----------------------------------------------------------------------
mcp = FastMCP("mcp-fs")


@mcp.tool()
async def read_file(
    path: str,
    session_id: Optional[str] = None,
    offset: Optional[int] = None,
    limit: Optional[int] = None,
    encoding: str = "utf-8",
    base64_output: bool = False,
    stream: bool = False,
    ctx: Context = None,
) -> str:
    """Read a file (text or binary). Supports offset/limit, base64 output, and streaming."""
    cwd = get_session(session_id)
    try:
        p = safe_path(path, cwd)
        if not await aiofiles.os.path.isfile(p):
            raise FileNotFoundError(f"Not a file: {path}")
        check_file_size(p)
        check_extension(p)
        async with aiofiles.open(p, "rb") as f:
            if offset:
                await f.seek(offset)
            data = await f.read(limit) if limit is not None else await f.read()
        if base64_output:
            text = base64.b64encode(data).decode("ascii")
        else:
            try:
                text = data.decode(encoding)
            except UnicodeDecodeError:
                text = "Binary file (base64): " + base64.b64encode(data).decode("ascii")
        if stream and ctx:
            chunk_size = 4096
            for i in range(0, len(text), chunk_size):
                ctx.info(text[i: i + chunk_size])
            return "[Streaming complete]"
        return text
    except Exception as e:
        return f"❌ {e}"


@mcp.tool()
async def write_file(
    path: str,
    content: str,
    session_id: Optional[str] = None,
    append: bool = False,
    base64_input: bool = False,
    encoding: str = "utf-8",
) -> str:
    """Write content to a file (text or base64). Supports append mode."""
    cwd = get_session(session_id)
    try:
        p = safe_path(path, cwd)
        if len(content) > MAX_FILE_SIZE * 2:
            raise ValueError("Content too large")
        p.parent.mkdir(parents=True, exist_ok=True)
        check_extension(p)
        if base64_input:
            data = base64.b64decode(content)
            mode = "ab" if append else "wb"
            async with aiofiles.open(p, mode) as f:
                await f.write(data)
        else:
            mode = "a" if append else "w"
            async with aiofiles.open(p, mode, encoding=encoding) as f:
                await f.write(content)
        return f"✅ Wrote {len(content)} chars to {path}"
    except Exception as e:
        return f"❌ {e}"


@mcp.tool()
async def cd(path: str = ".", session_id: Optional[str] = None) -> str:
    """Change virtual working directory for a session (requires session_id)."""
    if not session_id:
        return "❌ `cd` requires a session_id (persistent state)."
    cwd = get_session(session_id)
    try:
        new_path = safe_path(path, cwd)
        if not new_path.is_dir():
            raise NotADirectoryError(f"Not a directory: {path}")
        update_session_cwd(session_id, new_path)
        rel = new_path.relative_to(ALLOWED_DIR)
        return f"✅ Changed directory to {rel}"
    except Exception as e:
        return f"❌ {e}"


@mcp.tool()
async def list_directory(
    path: str = ".",
    session_id: Optional[str] = None,
    detailed: bool = True,
) -> str:
    """List directory contents with optional size, permissions, and timestamps."""
    cwd = get_session(session_id)
    try:
        p = safe_path(path, cwd)
        if not await aiofiles.os.path.isdir(p):
            raise NotADirectoryError(f"Not a directory: {path}")
        items = sorted(p.iterdir(), key=lambda x: x.name)
        lines = []
        for item in items:
            try:
                st = item.stat()
                if detailed:
                    kind = "📁" if item.is_dir() else "📄"
                    mtime = datetime.fromtimestamp(st.st_mtime).strftime("%Y-%m-%d %H:%M:%S")
                    perms = stat.filemode(st.st_mode)
                    lines.append(f"{kind} {item.name}  {st.st_size:>10} B  {perms}  {mtime}")
                else:
                    kind = "DIR" if item.is_dir() else "FILE"
                    lines.append(f"{kind}  {item.name}")
            except OSError:
                lines.append(f"? {item.name}")
        output = f"Contents of {p.relative_to(ALLOWED_DIR)}:\n" + "\n".join(lines)
        return output
    except Exception as e:
        return f"❌ {e}"


@mcp.tool()
async def create_directory(path: str, session_id: Optional[str] = None) -> str:
    """Create a new directory (and any missing parents)."""
    cwd = get_session(session_id)
    try:
        p = safe_path(path, cwd)
        p.mkdir(parents=True, exist_ok=True)
        return f"📁 Created {p.relative_to(ALLOWED_DIR)}"
    except Exception as e:
        return f"❌ {e}"


@mcp.tool()
async def copy_item(source: str, destination: str, session_id: Optional[str] = None) -> str:
    """Copy a file or directory recursively."""
    cwd = get_session(session_id)
    try:
        src = safe_path(source, cwd)
        dst = safe_path(destination, cwd)
        if not src.exists():
            raise FileNotFoundError(source)
        dst.parent.mkdir(parents=True, exist_ok=True)
        if src.is_dir():
            shutil.copytree(src, dst, dirs_exist_ok=True)
        else:
            shutil.copy2(src, dst)
        return f"📋 Copied {source} → {destination}"
    except Exception as e:
        return f"❌ {e}"


@mcp.tool()
async def move_item(source: str, destination: str, session_id: Optional[str] = None) -> str:
    """Move or rename a file or directory."""
    cwd = get_session(session_id)
    try:
        src = safe_path(source, cwd)
        dst = safe_path(destination, cwd)
        if not src.exists():
            raise FileNotFoundError(source)
        dst.parent.mkdir(parents=True, exist_ok=True)
        shutil.move(str(src), str(dst))
        return f"🚚 Moved {source} → {destination}"
    except Exception as e:
        return f"❌ {e}"


@mcp.tool()
async def delete_item(path: str, session_id: Optional[str] = None, recursive: bool = False) -> str:
    """Delete a file or directory. Use recursive=true to delete non-empty directories."""
    cwd = get_session(session_id)
    try:
        p = safe_path(path, cwd)
        if not p.exists():
            raise FileNotFoundError(path)
        if p.is_dir():
            if recursive:
                shutil.rmtree(p)
                return f"🗑️  Recursively deleted directory {path}"
            else:
                p.rmdir()
                return f"🗑️  Deleted empty directory {path}"
        else:
            p.unlink()
            return f"🗑️  Deleted file {path}"
    except Exception as e:
        return f"❌ {e}"


@mcp.tool()
async def get_file_info(path: str, session_id: Optional[str] = None, include_hash: bool = False) -> str:
    """Get detailed metadata for a path, including permissions, timestamps, and optional MD5/SHA256."""
    cwd = get_session(session_id)
    try:
        p = safe_path(path, cwd)
        if not p.exists():
            raise FileNotFoundError(path)
        st = p.stat()
        info: Dict[str, Any] = {
            "name": p.name,
            "type": "directory" if p.is_dir() else "file",
            "size": st.st_size,
            "permissions": stat.filemode(st.st_mode),
            "owner_uid": st.st_uid,
            "group_gid": st.st_gid,
            "created": st.st_ctime,
            "modified": st.st_mtime,
            "accessed": st.st_atime,
            "symlink": p.is_symlink(),
            "absolute_path": str(p),
            "relative_path": str(p.relative_to(ALLOWED_DIR)),
        }
        if include_hash and p.is_file():
            info["md5"] = await compute_hash(p, "md5")
            info["sha256"] = await compute_hash(p, "sha256")
        return json.dumps(info, indent=2)
    except Exception as e:
        return f"❌ {e}"


@mcp.tool()
async def search_files(
    pattern: str,
    session_id: Optional[str] = None,
    relative_to_cwd: bool = True,
) -> str:
    """Glob search for files (e.g. '**/*.py'). Results are relative to the sandbox root."""
    cwd = get_session(session_id)
    try:
        base = cwd if relative_to_cwd else ALLOWED_DIR
        if base != ALLOWED_DIR:
            rel_base = base.relative_to(ALLOWED_DIR)
            full_pattern = str(rel_base / pattern)
        else:
            full_pattern = pattern
        results = [str(p.relative_to(ALLOWED_DIR)) for p in ALLOWED_DIR.glob(full_pattern)]
        if not results:
            return "No matches"
        return "\n".join(sorted(results))
    except Exception as e:
        return f"❌ {e}"


@mcp.tool()
async def grep_files(pattern: str, path: str = ".", session_id: Optional[str] = None) -> str:
    """Search file contents with a regex pattern. Returns file:line: match lines."""
    cwd = get_session(session_id)
    try:
        base = safe_path(path, cwd)
        compiled = re.compile(pattern)
        matches = []
        async for f in _walk_files(base):
            if f.is_file():
                try:
                    async with aiofiles.open(f, "r", encoding="utf-8") as fh:
                        lineno = 0
                        async for line in fh:
                            lineno += 1
                            if compiled.search(line):
                                matches.append(f"{f.relative_to(ALLOWED_DIR)}:{lineno}: {line.strip()[:200]}")
                except (UnicodeDecodeError, PermissionError):
                    continue
        if not matches:
            return "No matches"
        return "\n".join(matches[:500])
    except Exception as e:
        return f"❌ {e}"


@mcp.tool()
async def set_permissions(path: str, mode: int, session_id: Optional[str] = None) -> str:
    """Change Unix permissions. mode is an integer (e.g. 0o644 = 420)."""
    cwd = get_session(session_id)
    try:
        p = safe_path(path, cwd)
        os.chmod(p, mode)
        return f"🔐 Permissions set to {oct(mode)} on {path}"
    except Exception as e:
        return f"❌ {e}"


@mcp.tool()
async def create_symlink(source: str, link_name: str, session_id: Optional[str] = None) -> str:
    """Create a symbolic link inside the sandbox."""
    cwd = get_session(session_id)
    try:
        src = safe_path(source, cwd)
        lnk = safe_path(link_name, cwd)
        lnk.parent.mkdir(parents=True, exist_ok=True)
        os.symlink(src, lnk)
        return f"🔗 Symlink created: {link_name} → {source}"
    except Exception as e:
        return f"❌ {e}"


@mcp.tool()
async def create_hardlink(source: str, link_name: str, session_id: Optional[str] = None) -> str:
    """Create a hard link inside the sandbox."""
    cwd = get_session(session_id)
    try:
        src = safe_path(source, cwd)
        lnk = safe_path(link_name, cwd)
        lnk.parent.mkdir(parents=True, exist_ok=True)
        os.link(src, lnk)
        return f"🔗 Hardlink created: {link_name} → {source}"
    except Exception as e:
        return f"❌ {e}"


@mcp.tool()
async def disk_usage() -> str:
    """Show total, used, and free disk space for the sandbox root."""
    try:
        usage = shutil.disk_usage(ALLOWED_DIR)
        return (
            f"Disk usage for {ALLOWED_DIR}\n"
            f"Total: {usage.total / (1024**3):.2f} GB\n"
            f"Used:  {usage.used / (1024**3):.2f} GB\n"
            f"Free:  {usage.free / (1024**3):.2f} GB"
        )
    except Exception as e:
        return f"❌ {e}"


@mcp.tool()
async def file_exists(path: str, session_id: Optional[str] = None) -> str:
    """Check whether a path exists inside the sandbox."""
    cwd = get_session(session_id)
    try:
        p = safe_path(path, cwd)
        return str(p.exists())
    except Exception:
        return "False"


@mcp.tool()
async def batch_operations(
    operations: List[Dict[str, Any]],
    session_id: Optional[str] = None,
) -> str:
    """Execute multiple file operations in one call.

    Each operation: {"op": "copy|move|delete|mkdir|chmod", "params": {...}}
    """
    cwd = get_session(session_id)
    results = []
    for idx, op in enumerate(operations):
        op_type = op.get("op")
        params = op.get("params", {})
        try:
            if op_type == "copy":
                src = safe_path(params["source"], cwd)
                dst = safe_path(params["destination"], cwd)
                if src.is_dir():
                    shutil.copytree(src, dst, dirs_exist_ok=True)
                else:
                    shutil.copy2(src, dst)
                results.append(f"✅ {idx + 1}: Copied {params['source']} → {params['destination']}")
            elif op_type == "move":
                src = safe_path(params["source"], cwd)
                dst = safe_path(params["destination"], cwd)
                shutil.move(str(src), str(dst))
                results.append(f"✅ {idx + 1}: Moved {params['source']} → {params['destination']}")
            elif op_type == "delete":
                p = safe_path(params["path"], cwd)
                recursive = params.get("recursive", False)
                if p.is_dir() and recursive:
                    shutil.rmtree(p)
                elif p.is_dir():
                    p.rmdir()
                else:
                    p.unlink()
                results.append(f"✅ {idx + 1}: Deleted {params['path']}")
            elif op_type == "mkdir":
                p = safe_path(params["path"], cwd)
                p.mkdir(parents=True, exist_ok=True)
                results.append(f"✅ {idx + 1}: Created directory {params['path']}")
            elif op_type == "chmod":
                p = safe_path(params["path"], cwd)
                os.chmod(p, params["mode"])
                results.append(f"✅ {idx + 1}: Changed permissions on {params['path']}")
            else:
                results.append(f"⚠️ {idx + 1}: Unknown operation '{op_type}'")
        except Exception as e:
            results.append(f"❌ {idx + 1}: {e}")
    return "\n".join(results)


@mcp.tool()
async def compress(
    source: str,
    destination: str,
    format: Literal["zip", "tar"] = "zip",
    session_id: Optional[str] = None,
) -> str:
    """Create a zip or tar.gz archive from a file or directory."""
    cwd = get_session(session_id)
    try:
        src = safe_path(source, cwd)
        dst = safe_path(destination, cwd)
        dst.parent.mkdir(parents=True, exist_ok=True)
        if format == "zip":
            with zipfile.ZipFile(dst, "w", zipfile.ZIP_DEFLATED) as zf:
                if src.is_dir():
                    for root, _, files in os.walk(src):
                        for file in files:
                            full = Path(root) / file
                            zf.write(full, full.relative_to(src.parent))
                else:
                    zf.write(src, src.name)
        elif format == "tar":
            with tarfile.open(dst, "w:gz") as tf:
                tf.add(src, arcname=src.name)
        else:
            raise ValueError("Format must be 'zip' or 'tar'")
        return f"✅ Compressed {source} to {destination} ({format})"
    except Exception as e:
        return f"❌ {e}"


@mcp.tool()
async def decompress(source: str, destination: str = ".", session_id: Optional[str] = None) -> str:
    """Extract a zip or tar archive to a destination directory."""
    cwd = get_session(session_id)
    try:
        src = safe_path(source, cwd)
        dst = safe_path(destination, cwd)
        dst.mkdir(parents=True, exist_ok=True)
        if src.suffix == ".zip" or zipfile.is_zipfile(src):
            with zipfile.ZipFile(src, "r") as zf:
                zf.extractall(dst)
        elif src.suffix in (".tar", ".gz", ".bz2", ".xz"):
            with tarfile.open(src, "r:*") as tf:
                tf.extractall(dst)
        else:
            raise ValueError("Unknown archive format")
        return f"✅ Extracted {source} to {destination}"
    except Exception as e:
        return f"❌ {e}"


@mcp.tool()
async def context_usage(
    path: Optional[str] = None,
    text: Optional[str] = None,
    model: str = "gpt-4",
    session_id: Optional[str] = None,
) -> str:
    """Estimate token count of a file or text string (uses tiktoken if installed)."""
    cwd = get_session(session_id)
    try:
        if path:
            p = safe_path(path, cwd)
            if not p.is_file():
                raise ValueError("Path is not a file")
            async with aiofiles.open(p, "r", encoding="utf-8") as f:
                content = await f.read()
        elif text:
            content = text
        else:
            raise ValueError("Either 'path' or 'text' must be provided")
        token_count = count_tokens(content, model)
        return f"Estimated tokens: {token_count} (using {model})"
    except Exception as e:
        return f"❌ {e}"


# ----------------------------------------------------------------------
# Main
# ----------------------------------------------------------------------
def main():
    global ALLOWED_DIR, MAX_FILE_SIZE, ALLOWED_EXTENSIONS, SESSION_TIMEOUT_SECONDS

    parser = argparse.ArgumentParser(description="MCP File System Server")
    parser.add_argument("--root", type=str, default=str(Path.cwd()), help="Allowed root directory")
    parser.add_argument("--max-file-size", type=int, default=100 * 1024 * 1024, help="Max file size in bytes")
    parser.add_argument("--allowed-extensions", type=str, default="", help="Comma-separated allowed extensions")
    parser.add_argument("--session-timeout", type=int, default=3600, help="Session idle timeout (seconds)")
    args = parser.parse_args()

    ALLOWED_DIR = Path(args.root).resolve()
    ALLOWED_DIR.mkdir(parents=True, exist_ok=True)

    MAX_FILE_SIZE = args.max_file_size
    if args.allowed_extensions:
        ALLOWED_EXTENSIONS = set(ext.strip().lower() for ext in args.allowed_extensions.split(",") if ext.strip())
    SESSION_TIMEOUT_SECONDS = args.session_timeout

    mcp.run()


if __name__ == "__main__":
    main()
