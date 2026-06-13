#!/usr/bin/env python3
"""HTML Project Studio – MCP Server (FastMCP edition)"""

import asyncio
import json
import os
import shutil
import tempfile
import zipfile
import mimetypes
from pathlib import Path
from typing import Any, Dict, List, Literal, Optional
from datetime import datetime
import logging

import aiohttp
from aiohttp import web
import aiofiles
import aiofiles.os
from watchfiles import awatch
from mcp.server.fastmcp import FastMCP, Context

# ------------------------------------------------------------
# Configuration
# ------------------------------------------------------------
ALLOWED_DIR: Path = Path.cwd().resolve() / "projects"
ALLOWED_DIR.mkdir(parents=True, exist_ok=True)
PREVIEW_PORT_START = 8080
PREVIEW_PORT_END = 8090

logger = logging.getLogger("html-studio")
logging.basicConfig(level=logging.INFO)

_active_servers: Dict[str, Any] = {}


# ------------------------------------------------------------
# Helpers
# ------------------------------------------------------------
def get_project_path(project_name: str) -> Path:
    safe_name = "".join(c for c in project_name if c.isalnum() or c in "_-")
    if not safe_name:
        raise ValueError("Invalid project name")
    return (ALLOWED_DIR / safe_name).resolve()


def ensure_project(project_name: str) -> Path:
    p = get_project_path(project_name)
    p.mkdir(parents=True, exist_ok=True)
    index = p / "index.html"
    if not index.exists():
        index.write_text(
            "<!DOCTYPE html>\n<html>\n<head><meta charset=\"UTF-8\"><title>New Project</title></head>\n"
            "<body><h1>Hello, world!</h1></body>\n</html>"
        )
    return p


def get_all_projects() -> List[str]:
    return [d.name for d in ALLOWED_DIR.iterdir() if d.is_dir()]


async def _find_free_port() -> int:
    for p in range(PREVIEW_PORT_START, PREVIEW_PORT_END + 1):
        try:
            reader, writer = await asyncio.open_connection("127.0.0.1", p)
            writer.close()
            await writer.wait_closed()
            # Port is in use — try next
        except (ConnectionRefusedError, OSError):
            return p
    return PREVIEW_PORT_START  # fallback


async def start_preview_server(project_name: str) -> Dict[str, Any]:
    if project_name in _active_servers:
        return {
            "port": _active_servers[project_name]["port"],
            "url": f"http://localhost:{_active_servers[project_name]['port']}",
        }

    project_path = get_project_path(project_name)
    if not project_path.exists():
        raise FileNotFoundError(f"Project '{project_name}' does not exist")

    port = await _find_free_port()
    app = web.Application()
    websockets: set = set()

    async def handle_static(request):
        rel = request.match_info.get("filename", "index.html") or "index.html"
        file_path = project_path / rel
        if not file_path.exists():
            return web.HTTPNotFound()
        if file_path.is_dir():
            file_path = file_path / "index.html"
        mime, _ = mimetypes.guess_type(str(file_path))
        mime = mime or "application/octet-stream"
        async with aiofiles.open(file_path, "rb") as f:
            content = await f.read()
        return web.Response(body=content, content_type=mime)

    async def websocket_handler(request):
        ws = web.WebSocketResponse()
        await ws.prepare(request)
        websockets.add(ws)
        try:
            async for msg in ws:
                if msg.type == aiohttp.WSMsgType.TEXT:
                    await ws.send_str("pong")
                elif msg.type == aiohttp.WSMsgType.ERROR:
                    break
        finally:
            websockets.discard(ws)
        return ws

    app.router.add_get("/ws", websocket_handler)
    app.router.add_get("/{filename:.*}", handle_static)

    async def watch_files():
        async for _ in awatch(project_path):
            for ws in list(websockets):
                try:
                    await ws.send_str("reload")
                except Exception:
                    websockets.discard(ws)

    runner = web.AppRunner(app)
    await runner.setup()
    site = web.TCPSite(runner, "127.0.0.1", port)
    await site.start()
    watcher_task = asyncio.create_task(watch_files())

    _active_servers[project_name] = {
        "runner": runner,
        "port": port,
        "watcher_task": watcher_task,
    }
    return {"port": port, "url": f"http://localhost:{port}"}


async def stop_preview_server(project_name: str):
    if project_name not in _active_servers:
        return
    data = _active_servers.pop(project_name)
    data["watcher_task"].cancel()
    await data["runner"].cleanup()


# ------------------------------------------------------------
# MCP Server
# ------------------------------------------------------------
mcp = FastMCP("html-studio")


@mcp.tool()
async def create_project(
    name: str,
    template: Literal["basic", "tailwind", "react", "vue"] = "basic",
) -> str:
    """Create a new HTML/CSS/JS project directory with a starter index.html."""
    try:
        project_path = ensure_project(name)
        return f"✅ Project '{name}' created at {project_path}"
    except Exception as e:
        return f"❌ {e}"


@mcp.tool()
async def list_projects() -> str:
    """List all existing project directories."""
    projects = get_all_projects()
    if not projects:
        return "No projects found."
    return "\n".join(projects)


@mcp.tool()
async def open_project(name: str) -> str:
    """Start a live-reload preview server for a project. Returns the local URL."""
    try:
        project_path = get_project_path(name)
        if not project_path.exists():
            return f"Project '{name}' does not exist."
        info = await start_preview_server(name)
        return f"✅ Project '{name}' preview at {info['url']}"
    except Exception as e:
        return f"❌ {e}"


@mcp.tool()
async def close_project(name: str) -> str:
    """Stop the preview server for a project."""
    try:
        await stop_preview_server(name)
        return f"Closed preview for '{name}'."
    except Exception as e:
        return f"❌ {e}"


@mcp.tool()
async def edit_file(project: str, file: str, content: str) -> str:
    """Write content to a file inside a project (creates or overwrites)."""
    try:
        proj = get_project_path(project)
        full = (proj / file).resolve()
        if not full.as_posix().startswith(proj.as_posix()):
            return "❌ Path traversal denied."
        full.parent.mkdir(parents=True, exist_ok=True)
        async with aiofiles.open(full, "w") as f:
            await f.write(content)
        return f"✅ Wrote {len(content)} chars to {file}"
    except Exception as e:
        return f"❌ {e}"


@mcp.tool()
async def read_file(project: str, file: str) -> str:
    """Read the content of a file inside a project."""
    try:
        proj = get_project_path(project)
        full = (proj / file).resolve()
        if not full.as_posix().startswith(proj.as_posix()):
            return "❌ Path traversal denied."
        if not full.exists():
            return "❌ File not found."
        async with aiofiles.open(full, "r") as f:
            return await f.read()
    except Exception as e:
        return f"❌ {e}"


@mcp.tool()
async def delete_project(name: str) -> str:
    """Permanently delete a project and stop its preview server."""
    try:
        await stop_preview_server(name)
        proj = get_project_path(name)
        if proj.exists():
            shutil.rmtree(proj)
        return f"🗑️ Deleted project '{name}'."
    except Exception as e:
        return f"❌ {e}"


@mcp.tool()
async def export_project(name: str) -> str:
    """Export a project as a base64-encoded zip file."""
    try:
        import base64
        proj = get_project_path(name)
        if not proj.exists():
            return f"Project '{name}' does not exist."
        with tempfile.NamedTemporaryFile(suffix=".zip", delete=False) as tmp:
            tmp_path = tmp.name
        with zipfile.ZipFile(tmp_path, "w", zipfile.ZIP_DEFLATED) as zf:
            for root, _, files in os.walk(proj):
                for file in files:
                    full = Path(root) / file
                    zf.write(full, full.relative_to(proj))
        async with aiofiles.open(tmp_path, "rb") as f:
            zip_data = await f.read()
        os.unlink(tmp_path)
        return base64.b64encode(zip_data).decode("ascii")
    except Exception as e:
        return f"❌ {e}"


@mcp.tool()
async def import_project(name: str, zip_base64: str) -> str:
    """Restore a project from a base64-encoded zip. Overwrites if it already exists."""
    try:
        import base64
        zip_data = base64.b64decode(zip_base64)
        with tempfile.NamedTemporaryFile(suffix=".zip", delete=False) as tmp:
            tmp.write(zip_data)
            tmp_path = tmp.name
        proj = get_project_path(name)
        if proj.exists():
            shutil.rmtree(proj)
        with zipfile.ZipFile(tmp_path, "r") as zf:
            zf.extractall(proj)
        os.unlink(tmp_path)
        return f"✅ Project '{name}' imported."
    except Exception as e:
        return f"❌ {e}"


# ------------------------------------------------------------
# Main
# ------------------------------------------------------------
if __name__ == "__main__":
    mcp.run()
