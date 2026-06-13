# mcp-flow — Flow Orchestrator

Persistent memory, plan management, execution tracking, and recovery for long-running AI workflows.
State is stored in `~/.mcp_flow_orchestrator/flow.db` (SQLite) and survives across sessions.

## Features

- **Sessions** — isolate workflows with a `session_id`
- **Plan steps** — add, list, update, and delete steps with dependency tracking
- **Memory store** — key-value with TTL and optional semantic search (sentence-transformers)
- **Execution log** — automatic recording of step updates
- **Checkpoints** — save/restore full session state (plan + memory)
- **Context tracking** — model reports token usage; server returns recovery advice when nearing limit
- **Next action** — returns the next executable pending step, or suggests retrying failed ones

## Installation

```bash
cd mcp-flow
uv sync
# Optional: for semantic memory search
uv pip install sentence-transformers torch
```

## Claude Desktop Configuration

```json
{
  "mcpServers": {
    "mcp-flow": {
      "command": "uv",
      "args": ["--directory", "/absolute/path/to/mcp-flow", "run", "server.py"]
    }
  }
}
```

## MCP Tools

| Tool | Description |
|------|-------------|
| `flow_create_session` | Start a new workflow session |
| `flow_add_step` | Add a step with optional dependencies |
| `flow_get_next_action` | Get the next step to execute (or retry) |
| `flow_update_step_status` | Mark a step as `pending`, `in_progress`, `done`, `failed`, or `blocked` |
| `flow_list_steps` | List all steps with status |
| `flow_delete_step` | Remove a step |
| `flow_set_memory` | Store a key-value fact with optional TTL |
| `flow_get_memory` | Retrieve a stored fact by key |
| `flow_query_memory` | Semantic or keyword search over stored facts |
| `flow_save_checkpoint` | Snapshot current session state |
| `flow_restore_checkpoint` | Restore a previous snapshot |
| `flow_update_context` | Report token usage; returns status and recovery advice |

## Typical usage pattern

1. `flow_create_session` — start with a goal
2. `flow_add_step` — define the plan (multiple calls)
3. Loop: `flow_get_next_action` → execute → `flow_update_step_status`
4. `flow_set_memory` — store intermediate results
5. `flow_update_context` — call periodically; summarise if context is > 80%

## Example workflow

```
Session: research-agent
Steps:
  search_papers    → done
  extract_methods  → failed
  write_summary    → pending (depends on extract_methods)

flow_get_next_action → { "action": "retry", "step_name": "extract_methods" }
```

## Security

- Data is stored locally in `~/.mcp_flow_orchestrator/flow.db`.
- No external network calls (unless semantic search loads a model from HuggingFace on first use).

## License

MIT
