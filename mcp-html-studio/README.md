# mcp-html-studio — HTML Project Studio

Create, preview, and edit interactive HTML/CSS/JS projects directly from your AI assistant — entirely locally.

## Features

- **Create projects** from scratch with a single tool call
- **Model-chosen locations** — the project name may be a nested path (e.g. `demos/landing`); files are created exactly where the model asks
- **Sandboxed** — every project lives inside a configurable `--root`; nothing is ever created or read outside it
- **Live preview** with hot-reload via WebSocket (files watched with `watchfiles`)
- **Edit any file** — HTML, CSS, JS, or any asset
- **Export/Import** projects as zip files (base64, with zip-slip protection)
- **Preview server** listens only on `127.0.0.1` (never exposed externally)
- **Path traversal protection** — file ops and the preview server are strictly contained within the sandbox

## Installation

```bash
cd mcp-html-studio
uv sync
```

## Usage

```bash
uv run server.py --root /path/to/sandbox
```

All projects are created inside the `--root` sandbox (default: `./projects`). The
model picks the location *within* that root by passing a project name, which may
be a nested path such as `clients/acme/site`. Names that try to escape the root
(`../`, absolute paths) are remapped back inside it or rejected — files are never
written or read outside the sandbox.

### Command-line options

| Option | Default | Description |
|--------|---------|-------------|
| `--root PATH` | `./projects` | Sandbox root directory for all projects |

## Claude Desktop Configuration

```json
{
  "mcpServers": {
    "html-studio": {
      "command": "uv",
      "args": ["--directory", "/absolute/path/to/mcp-html-studio", "run", "server.py", "--root", "/path/to/sandbox"]
    }
  }
}
```

Then ask Claude:

> *"Create a project called `my-dashboard`, open it, and add a chart using Chart.js."*

Claude will use the tools to create the project, write the files, and give you a local URL to view the result live.

## MCP Tools

| Tool | Description |
|------|-------------|
| `create_project` | Create a new project directory with a starter `index.html` |
| `list_projects` | List all existing projects |
| `open_project` | Start preview server; returns the local URL |
| `close_project` | Stop the preview server |
| `edit_file` | Overwrite or create a file inside a project |
| `read_file` | Read file content |
| `delete_project` | Remove a project permanently |
| `export_project` | Download project as base64-encoded zip |
| `import_project` | Restore a project from a base64 zip |

## Preview Server

- Runs on `http://127.0.0.1:808x` (first available port in range 8080–8090)
- A WebSocket endpoint at `/ws` broadcasts `reload` whenever files change
- Add `?livereload` or connect your browser extension to auto-refresh on save

## Security

- All project files are contained under the `--root` sandbox. Project names and
  file paths are resolved and verified to stay inside it (component-wise, so a
  sibling like `projects-evil` is not mistaken for being inside `projects`).
- Path traversal is blocked for every file operation, for the preview server's
  static handler, and for zip imports (zip-slip protected).
- Preview servers only bind to `127.0.0.1`.

## License

MIT
