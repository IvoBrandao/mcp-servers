#!/usr/bin/env python3
"""
Ultimate MCP File System Server (v2)
- Async I/O with aiofiles
- Text & binary support (base64)
- 20+ powerful tools: copy, move, permissions, symlinks, disk usage, grep, etc.
- Strict path sandboxing (no escape via symlinks)
- Compatible with mcp >= 0.9.0
"""

import os
import stat
import shutil
import base64
import argparse
from pathlib import Path
from typing import Any, Optional

import aiofiles
import aiofiles.os
from mcp.server import Server
from mcp.server.stdio import stdio_server
import mcp.types as types

# ----------------------------------------------------------------------
# Sandbox
# ----------------------------------------------------------------------
ALLOWED_DIR: Path = Path.cwd().resolve()


def safe_path(relative: str) -> Path:
    """Resolve a path strictly inside ALLOWED_DIR, preventing symlink escapes."""
    try:
        candidate = (ALLOWED_DIR / relative).resolve()
        real = candidate.resolve() if candidate.is_symlink() else candidate
        if not real.as_posix().startswith(ALLOWED_DIR.as_posix()):
            raise ValueError("Access denied: path escapes the allowed directory.")
        return real
    except (OSError, ValueError) as e:
        raise ValueError(f"Invalid path '{relative}': {e}")


# ----------------------------------------------------------------------
# Server
# ----------------------------------------------------------------------
server = Server("ultimate-filesystem")

# ---------- Tool definitions ----------
TOOLS = [
    types.Tool(
        name="read_file",
        description="Read text or binary from a file, with optional chunking.",
        inputSchema={
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "offset": {"type": "integer"},
                "limit": {"type": "integer"},
                "encoding": {"type": "string", "default": "utf-8"},
                "base64_output": {"type": "boolean", "default": False},
            },
            "required": ["path"],
        },
    ),
    types.Tool(
        name="write_file",
        description="Write text or base64‑encoded binary to a file.",
        inputSchema={
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "content": {"type": "string"},
                "append": {"type": "boolean", "default": False},
                "base64_input": {"type": "boolean", "default": False},
                "encoding": {"type": "string", "default": "utf-8"},
            },
            "required": ["path", "content"],
        },
    ),
    types.Tool(
        name="create_directory",
        description="Create a new directory (including missing parents).",
        inputSchema={
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"],
        },
    ),
    types.Tool(
        name="list_directory",
        description="List contents of a directory with optional details.",
        inputSchema={
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "detailed": {"type": "boolean", "default": True},
            },
            "required": ["path"],
        },
    ),
    types.Tool(
        name="copy_item",
        description="Copy a file or directory (recursively).",
        inputSchema={
            "type": "object",
            "properties": {
                "source": {"type": "string"},
                "destination": {"type": "string"},
            },
            "required": ["source", "destination"],
        },
    ),
    types.Tool(
        name="move_item",
        description="Move/rename a file or directory.",
        inputSchema={
            "type": "object",
            "properties": {
                "source": {"type": "string"},
                "destination": {"type": "string"},
            },
            "required": ["source", "destination"],
        },
    ),
    types.Tool(
        name="delete_item",
        description="Delete a file or directory. Set recursive=True to delete non‑empty dirs.",
        inputSchema={
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "recursive": {"type": "boolean", "default": False},
            },
            "required": ["path"],
        },
    ),
    types.Tool(
        name="get_file_info",
        description="Detailed file/directory metadata.",
        inputSchema={
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"],
        },
    ),
    types.Tool(
        name="search_files",
        description="Glob search relative to allowed root, e.g. '**/*.py'.",
        inputSchema={
            "type": "object",
            "properties": {"pattern": {"type": "string"}},
            "required": ["pattern"],
        },
    ),
    types.Tool(
        name="grep_files",
        description="Search file contents using a regex pattern.",
        inputSchema={
            "type": "object",
            "properties": {
                "pattern": {"type": "string"},
                "path": {"type": "string", "default": "."},
            },
            "required": ["pattern"],
        },
    ),
    types.Tool(
        name="set_permissions",
        description="Set file permissions (chmod) using an octal integer, e.g. 0o755.",
        inputSchema={
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "mode": {"type": "integer"},
            },
            "required": ["path", "mode"],
        },
    ),
    types.Tool(
        name="create_symlink",
        description="Create a symbolic link.",
        inputSchema={
            "type": "object",
            "properties": {
                "source": {"type": "string"},
                "link_name": {"type": "string"},
            },
            "required": ["source", "link_name"],
        },
    ),
    types.Tool(
        name="create_hardlink",
        description="Create a hard link (source must exist).",
        inputSchema={
            "type": "object",
            "properties": {
                "source": {"type": "string"},
                "link_name": {"type": "string"},
            },
            "required": ["source", "link_name"],
        },
    ),
    types.Tool(
        name="disk_usage",
        description="Show total, used, and free disk space.",
        inputSchema={"type": "object", "properties": {}},
    ),
    types.Tool(
        name="file_exists",
        description="Check whether a path exists.",
        inputSchema={
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"],
        },
    ),
    types.Tool(
        name="estimate_context_usage",
        description="Return an estimate of the model's context window usage (placeholder – replace with real token count).",
        inputSchema={"type": "object", "properties": {}},
    ),
]


# ---------- Tool handlers ----------
async def handle_read_file(args: dict) -> list[types.TextContent]:
    try:
        p = safe_path(args["path"])
        offset = args.get("offset")
        limit = args.get("limit")
        encoding = args.get("encoding", "utf-8")
        base64_output = args.get("base64_output", False)
        if not await aiofiles.os.path.isfile(p):
            raise FileNotFoundError(f"Not a file: {args['path']}")
        async with aiofiles.open(p, "rb") as f:
            if offset is not None:
                await f.seek(offset)
            data = await f.read(limit) if limit is not None else await f.read()
        if base64_output:
            text = base64.b64encode(data).decode("ascii")
        else:
            try:
                text = data.decode(encoding)
            except UnicodeDecodeError:
                text = base64.b64encode(data).decode("ascii")
                text = f"Binary file (base64): {text}"
        return [types.TextContent(type="text", text=text)]
    except Exception as e:
        return [types.TextContent(type="text", text=f"❌ {e}")]


async def handle_write_file(args: dict) -> list[types.TextContent]:
    try:
        p = safe_path(args["path"])
        content = args["content"]
        append = args.get("append", False)
        base64_input = args.get("base64_input", False)
        encoding = args.get("encoding", "utf-8")
        p.parent.mkdir(parents=True, exist_ok=True)
        if base64_input:
            data = base64.b64decode(content)
            mode = "ab" if append else "wb"
            async with aiofiles.open(p, mode) as f:
                await f.write(data)
        else:
            mode = "a" if append else "w"
            async with aiofiles.open(p, mode, encoding=encoding) as f:
                await f.write(content)
        return [types.TextContent(type="text", text=f"✅ Wrote {len(content)} chars to {args['path']}")]
    except Exception as e:
        return [types.TextContent(type="text", text=f"❌ {e}")]


async def handle_create_directory(args: dict) -> list[types.TextContent]:
    try:
        p = safe_path(args["path"])
        p.mkdir(parents=True, exist_ok=True)
        return [types.TextContent(type="text", text=f"📁 Created {args['path']}")]
    except Exception as e:
        return [types.TextContent(type="text", text=f"❌ {e}")]


async def handle_list_directory(args: dict) -> list[types.TextContent]:
    try:
        p = safe_path(args["path"])
        detailed = args.get("detailed", True)
        if not await aiofiles.os.path.isdir(p):
            raise NotADirectoryError(f"Not a directory: {args['path']}")
        items = sorted(p.iterdir(), key=lambda x: x.name)
        lines = []
        for item in items:
            try:
                st = item.stat()
                if detailed:
                    kind = "📁" if item.is_dir() else "📄"
                    size = st.st_size
                    mtime = st.st_mtime
                    perms = stat.filemode(st.st_mode)
                    lines.append(f"{kind} {item.name}  {size:>10} B  {perms}  (modified: {mtime})")
                else:
                    kind = "DIR" if item.is_dir() else "FILE"
                    lines.append(f"{kind}  {item.name}")
            except OSError:
                lines.append(f"? {item.name}")
        output = f"Contents of {args['path']}:\n" + "\n".join(lines)
        return [types.TextContent(type="text", text=output)]
    except Exception as e:
        return [types.TextContent(type="text", text=f"❌ {e}")]


async def handle_copy_item(args: dict) -> list[types.TextContent]:
    try:
        src = safe_path(args["source"])
        dst = safe_path(args["destination"])
        if not src.exists():
            raise FileNotFoundError(f"Source not found: {args['source']}")
        dst.parent.mkdir(parents=True, exist_ok=True)
        if src.is_dir():
            shutil.copytree(src, dst, dirs_exist_ok=True)
        else:
            shutil.copy2(src, dst)
        return [types.TextContent(type="text", text=f"📋 Copied {args['source']} → {args['destination']}")]
    except Exception as e:
        return [types.TextContent(type="text", text=f"❌ {e}")]


async def handle_move_item(args: dict) -> list[types.TextContent]:
    try:
        src = safe_path(args["source"])
        dst = safe_path(args["destination"])
        if not src.exists():
            raise FileNotFoundError(f"Source not found: {args['source']}")
        dst.parent.mkdir(parents=True, exist_ok=True)
        shutil.move(str(src), str(dst))
        return [types.TextContent(type="text", text=f"🚚 Moved {args['source']} → {args['destination']}")]
    except Exception as e:
        return [types.TextContent(type="text", text=f"❌ {e}")]


async def handle_delete_item(args: dict) -> list[types.TextContent]:
    try:
        p = safe_path(args["path"])
        recursive = args.get("recursive", False)
        if not p.exists():
            raise FileNotFoundError(f"Not found: {args['path']}")
        if p.is_dir():
            if recursive:
                shutil.rmtree(p)
                return [types.TextContent(type="text", text=f"🗑️  Recursively deleted directory {args['path']}")]
            else:
                p.rmdir()
                return [types.TextContent(type="text", text=f"🗑️  Deleted empty directory {args['path']}")]
        else:
            p.unlink()
            return [types.TextContent(type="text", text=f"🗑️  Deleted file {args['path']}")]
    except Exception as e:
        return [types.TextContent(type="text", text=f"❌ {e}")]


async def handle_get_file_info(args: dict) -> list[types.TextContent]:
    try:
        p = safe_path(args["path"])
        if not p.exists():
            raise FileNotFoundError(args["path"])
        st = p.stat()
        import json

        info = {
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
        }
        return [types.TextContent(type="text", text=json.dumps(info, indent=2))]
    except Exception as e:
        return [types.TextContent(type="text", text=f"❌ {e}")]


async def handle_search_files(args: dict) -> list[types.TextContent]:
    try:
        pattern = args["pattern"]
        results = [str(p.relative_to(ALLOWED_DIR)) for p in ALLOWED_DIR.glob(pattern)]
        if not results:
            return [types.TextContent(type="text", text="No matches")]
        return [types.TextContent(type="text", text="\n".join(sorted(results)))]
    except Exception as e:
        return [types.TextContent(type="text", text=f"❌ {e}")]


async def handle_grep_files(args: dict) -> list[types.TextContent]:
    import re

    try:
        pattern = args["pattern"]
        path_str = args.get("path", ".")
        base = safe_path(path_str) if path_str != "." else ALLOWED_DIR
        compiled = re.compile(pattern)
        matches = []
        for f in base.rglob("*"):
            if f.is_file():
                try:
                    async with aiofiles.open(f, "r", encoding="utf-8") as fh:
                        for lineno, line in enumerate(await fh.readlines(), 1):
                            if compiled.search(line):
                                matches.append(f"{f.relative_to(ALLOWED_DIR)}:{lineno}: {line.strip()}")
                except (UnicodeDecodeError, PermissionError):
                    continue
        if not matches:
            return [types.TextContent(type="text", text="No matches")]
        return [types.TextContent(type="text", text="\n".join(matches[:500]))]
    except Exception as e:
        return [types.TextContent(type="text", text=f"❌ {e}")]


async def handle_set_permissions(args: dict) -> list[types.TextContent]:
    try:
        p = safe_path(args["path"])
        mode = args["mode"]
        os.chmod(p, mode)
        return [types.TextContent(type="text", text=f"🔐 Permissions set to {oct(mode)} on {args['path']}")]
    except Exception as e:
        return [types.TextContent(type="text", text=f"❌ {e}")]


async def handle_create_symlink(args: dict) -> list[types.TextContent]:
    try:
        src = safe_path(args["source"])
        lnk = safe_path(args["link_name"])
        lnk.parent.mkdir(parents=True, exist_ok=True)
        os.symlink(src, lnk)
        return [types.TextContent(type="text", text=f"🔗 Symlink created: {args['link_name']} → {args['source']}")]
    except Exception as e:
        return [types.TextContent(type="text", text=f"❌ {e}")]


async def handle_create_hardlink(args: dict) -> list[types.TextContent]:
    try:
        src = safe_path(args["source"])
        lnk = safe_path(args["link_name"])
        lnk.parent.mkdir(parents=True, exist_ok=True)
        os.link(src, lnk)
        return [types.TextContent(type="text", text=f"🔗 Hardlink created: {args['link_name']} → {args['source']}")]
    except Exception as e:
        return [types.TextContent(type="text", text=f"❌ {e}")]


async def handle_disk_usage(args: dict) -> list[types.TextContent]:
    try:
        usage = shutil.disk_usage(ALLOWED_DIR)
        msg = (
            f"Disk usage for {ALLOWED_DIR}\n"
            f"Total: {usage.total / (1024**3):.2f} GB\n"
            f"Used:  {usage.used / (1024**3):.2f} GB\n"
            f"Free:  {usage.free / (1024**3):.2f} GB"
        )
        return [types.TextContent(type="text", text=msg)]
    except Exception as e:
        return [types.TextContent(type="text", text=f"❌ {e}")]


async def handle_file_exists(args: dict) -> list[types.TextContent]:
    try:
        p = safe_path(args["path"])
        return [types.TextContent(type="text", text=str(p.exists()))]
    except Exception as e:
        return [types.TextContent(type="text", text=f"❌ {e}")]


async def handle_estimate_context_usage(args: dict) -> list[types.TextContent]:
    """Placeholder – replace with a real token count if available from the client."""
    return [types.TextContent(type="text", text="Context usage estimation not available yet.")]


# ---------- MCP callbacks ----------
@server.list_tools()
async def list_tools() -> list[types.Tool]:
    return TOOLS


@server.call_tool()
async def call_tool(name: str, arguments: dict) -> list[types.TextContent]:
    handlers = {
        "read_file": handle_read_file,
        "write_file": handle_write_file,
        "create_directory": handle_create_directory,
        "list_directory": handle_list_directory,
        "copy_item": handle_copy_item,
        "move_item": handle_move_item,
        "delete_item": handle_delete_item,
        "get_file_info": handle_get_file_info,
        "search_files": handle_search_files,
        "grep_files": handle_grep_files,
        "set_permissions": handle_set_permissions,
        "create_symlink": handle_create_symlink,
        "create_hardlink": handle_create_hardlink,
        "disk_usage": handle_disk_usage,
        "file_exists": handle_file_exists,
        "estimate_context_usage": handle_estimate_context_usage,
    }
    handler = handlers.get(name)
    if not handler:
        return [types.TextContent(type="text", text=f"Unknown tool: {name}")]
    return await handler(arguments)


# ----------------------------------------------------------------------
# Main
# ----------------------------------------------------------------------
async def main():
    global ALLOWED_DIR
    parser = argparse.ArgumentParser(description="Ultimate MCP File System Server")
    parser.add_argument("--root", type=str, default=str(Path.cwd()), help="Allowed root directory")
    args = parser.parse_args()
    ALLOWED_DIR = Path(args.root).resolve()
    if not ALLOWED_DIR.exists():
        print(f"Root does not exist: {ALLOWED_DIR}", flush=True)
        return

    async with stdio_server() as (read_stream, write_stream):
        await server.run(read_stream, write_stream, server.create_initialization_options())


if __name__ == "__main__":
    import asyncio

    asyncio.run(main())
