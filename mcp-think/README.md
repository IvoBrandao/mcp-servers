# MCP Thinking Server

External reasoning engine for LLMs – improve your model's thinking with structured steps, assumptions, contradictions, and self‑evaluation.

## Features

- **Reasoning steps** – chain‑of‑thought, actions, observations.
- **Self‑evaluation** – models rate their own reasoning (0‑10).
- **Assumptions** – track explicit assumptions with confidence and evidence.
- **Contradiction detection** – automatically find conflicts.
- **Reasoning patterns** – pre‑built templates: CoT, ToT, ReAct, self‑consistency.
- **Checkpoints** – save/load full reasoning state.
- **Context tracking** – model reports token usage; server advises when to summarise.
- **LLM reranking** – optional OpenAI-based reordering of relevant steps.

## Installation

```bash
git clone https://github.com/yourname/mcp-think
cd mcp-think
uv sync
```

For optional reranking (requires OpenAI key):

```bash
uv sync --extra rerank
```

## Usage

Add to `claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "think": {
      "command": "uv",
      "args": ["--directory", "/path/to/mcp-think", "run", "server.py"]
    }
  }
}
```

## Example workflow

1. Create session: `think_add_step(session_id="math-problem", content="We need to solve x^2 = 4")`
2. Add assumption: `think_add_assumption(session_id, "x is real", confidence=0.8)`
3. Check contradiction: `think_check_contradiction(session_id, "x is complex")` → detects conflict.
4. Evaluate step: `think_evaluate_step(session_id, step_id, score=9, note="Correct path")`
5. Save checkpoint: `think_save_checkpoint(session_id, "before_solution")`
6. Update context: `think_update_context(session_id, tokens_used=5000)` → gets recovery prompt.

## Tools overview

| Tool | Description |
|------|-------------|
| `think_add_step` | Append a reasoning step. |
| `think_get_chain` | Retrieve all steps. |
| `think_evaluate_step` | Score a step (0‑10). |
| `think_add_assumption` | Record an assumption. |
| `think_list_assumptions` | List assumptions. |
| `think_check_contradiction` | Detect conflicts. |
| `think_resolve_contradiction` | Mark as resolved. |
| `think_list_patterns` | Show available patterns. |
| `think_apply_pattern` | Initialise steps from a template. |
| `think_save_checkpoint` | Snapshot state. |
| `think_restore_checkpoint` | Restore state. |
| `think_update_context` | Report token usage → get recovery advice. |
| `think_rerank_steps` | LLM‑based reordering of steps. |

## Security & privacy

- All data stored locally in `~/.mcp_think/think.db`.
- No external API calls unless you enable reranking (OpenAI).

## License

MIT

```

---

## 🔌 `mcpServers` entry for Claude Desktop

Add to your configuration:

```json
{
  "mcpServers": {
    "think": {
      "command": "uv",
      "args": [
        "--directory",
        "/Users/ivo/Developer/mcp-servers/mcp-think",
        "run",
        "server.py"
      ]
    }
  }
}
```
