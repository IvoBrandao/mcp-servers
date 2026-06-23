#!/usr/bin/env python3
"""HTML Project Studio – MCP Server (FastMCP edition)"""

import argparse
import asyncio
import json
import os
import shutil
import sys
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
# Sandbox root. All projects live somewhere inside this directory. The model
# chooses *where* (the project name may be a nested path like "clients/acme"),
# but nothing is ever created or read outside this root. Set via --root in main().
ALLOWED_DIR: Path = Path.cwd().resolve() / "projects"
PREVIEW_PORT_START = 8080
PREVIEW_PORT_END = 8090

logger = logging.getLogger("html-studio")
logging.basicConfig(level=logging.INFO)

_active_servers: Dict[str, Any] = {}


# ------------------------------------------------------------
# Helpers
# ------------------------------------------------------------
def is_within_sandbox(p: Path) -> bool:
    """True if `p` is ALLOWED_DIR itself or a path nested inside it.

    Uses path-component comparison rather than string prefix matching, so a
    sibling like `/root/projects-evil` is not treated as inside `/root/projects`.
    """
    return p == ALLOWED_DIR or ALLOWED_DIR in p.parents


def safe_join(base: Path, rel: str) -> Path:
    """Resolve `rel` under `base`, rejecting anything that escapes `base`.

    `rel` is always treated as relative (a leading `/` is stripped), so an
    absolute-looking path can't replace the base. Traversal via `..` is caught
    by the post-resolution containment check.
    """
    cleaned = (rel or "").strip().lstrip("/")
    if not cleaned:
        raise ValueError("Empty path")
    full = (base / cleaned).resolve()
    if full != base and base not in full.parents:
        raise ValueError(f"Path '{rel}' escapes '{base.name}'")
    return full


def get_project_path(project_name: str) -> Path:
    """Resolve a model-specified project location, confined to the sandbox.

    The name may be a nested path (e.g. "clients/acme/site"); it is always
    interpreted relative to the sandbox root and may never resolve to the root
    itself or anywhere outside it.
    """
    full = safe_join(ALLOWED_DIR, project_name)
    if full == ALLOWED_DIR:
        raise ValueError("Project name must not be empty or the sandbox root")
    return full


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
    """List projects (directories containing an index.html), as paths relative
    to the sandbox root so nested locations are visible."""
    projects = {
        index.parent.relative_to(ALLOWED_DIR).as_posix()
        for index in ALLOWED_DIR.rglob("index.html")
        if index.parent != ALLOWED_DIR
    }
    return sorted(projects)


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
        try:
            file_path = safe_join(project_path, rel)
        except ValueError:
            return web.HTTPForbidden()
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
    """Create a new HTML/CSS/JS project directory with a starter index.html.

    `name` chooses where the project is created and may be a nested path
    (e.g. "demos/landing-page"). The location is always confined to the
    server's sandbox root — paths that try to escape it are rejected.
    """
    try:
        project_path = ensure_project(name)
        rel = project_path.relative_to(ALLOWED_DIR).as_posix()
        return f"✅ Project '{rel}' created at {project_path}"
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
        full = safe_join(proj, file)
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
        full = safe_join(proj, file)
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
        proj.mkdir(parents=True, exist_ok=True)
        with zipfile.ZipFile(tmp_path, "r") as zf:
            # Guard against zip-slip: confine every entry inside the project dir.
            for member in zf.namelist():
                safe_join(proj, member)
            zf.extractall(proj)
        os.unlink(tmp_path)
        return f"✅ Project '{name}' imported."
    except Exception as e:
        return f"❌ {e}"


# ------------------------------------------------------------
# Main
# ------------------------------------------------------------
def main():
    global ALLOWED_DIR

    parser = argparse.ArgumentParser(description="HTML Project Studio — MCP server")
    parser.add_argument(
        "--root",
        default=str(Path.cwd() / "projects"),
        help="Sandbox root directory; all projects are created and confined here "
             "(default: ./projects)",
    )
    args = parser.parse_args()

    ALLOWED_DIR = Path(args.root).resolve()
    ALLOWED_DIR.mkdir(parents=True, exist_ok=True)
    logger.info("Sandbox root: %s", ALLOWED_DIR)

    mcp.run()


if __name__ == "__main__":
    main()
