# mcp-html-studio — HTML Project Studio

Create, preview, and edit interactive HTML/CSS/JS projects directly from your AI assistant — entirely locally.

## Features

- **Create projects** from scratch with a single tool call
- **Live preview** with hot-reload via WebSocket (files watched with `watchfiles`)
- **Edit any file** — HTML, CSS, JS, or any asset
- **Export/Import** projects as zip files (base64)
- **Preview server** listens only on `127.0.0.1` (never exposed externally)
- **Path traversal protection** — files are strictly contained within the project directory

## Installation

```bash
cd mcp-html-studio
uv sync
```

## Usage

```bash
uv run server.py
```

Projects are stored in a `projects/` subdirectory relative to the server's working directory.

## Claude Desktop Configuration

```json
{
  "mcpServers": {
    "html-studio": {
      "command": "uv",
      "args": ["--directory", "/absolute/path/to/mcp-html-studio", "run", "server.py"]
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

- All project files are contained under `projects/` inside the server's working directory.
- Path traversal is explicitly blocked for every file operation.
- Preview servers only bind to `127.0.0.1`.

## License

MIT
