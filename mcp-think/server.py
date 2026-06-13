#!/usr/bin/env python3
"""
Ultimate MCP Thinking Server
External reasoning engine for LLMs: steps, assumptions, contradictions, self-evaluation.
"""

import json
import sqlite3
import time
import hashlib
from pathlib import Path
from typing import Dict, List, Literal, Optional

from mcp.server.fastmcp import FastMCP, Context

# ----------------------------------------------------------------------
# Configuration
# ----------------------------------------------------------------------
DATA_DIR = Path.home() / ".mcp_think"
DATA_DIR.mkdir(parents=True, exist_ok=True)
DB_PATH = DATA_DIR / "think.db"

RERANKER_AVAILABLE = False
try:
    from openai import AsyncOpenAI
    RERANKER_AVAILABLE = True
except ImportError:
    pass


# ----------------------------------------------------------------------
# SQLite helpers
# ----------------------------------------------------------------------
def get_db():
    conn = sqlite3.connect(str(DB_PATH))
    conn.row_factory = sqlite3.Row
    return conn


def init_db():
    with get_db() as conn:
        conn.executescript("""
            CREATE TABLE IF NOT EXISTS sessions (
                session_id TEXT PRIMARY KEY,
                goal TEXT,
                created REAL,
                last_active REAL,
                context_tokens INTEGER DEFAULT 0,
                context_limit INTEGER DEFAULT 200000
            );

            CREATE TABLE IF NOT EXISTS reasoning_steps (
                step_id TEXT PRIMARY KEY,
                session_id TEXT,
                parent_step_id TEXT,
                step_type TEXT,
                content TEXT,
                confidence REAL DEFAULT 0.5,
                evaluation_score REAL,
                evaluation_note TEXT,
                created REAL,
                FOREIGN KEY(session_id) REFERENCES sessions(session_id)
            );

            CREATE TABLE IF NOT EXISTS assumptions (
                assumption_id TEXT PRIMARY KEY,
                session_id TEXT,
                content TEXT,
                confidence REAL,
                evidence TEXT,
                created REAL,
                FOREIGN KEY(session_id) REFERENCES sessions(session_id)
            );

            CREATE TABLE IF NOT EXISTS contradictions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT,
                statement_a TEXT,
                statement_b TEXT,
                explanation TEXT,
                resolved BOOLEAN DEFAULT 0,
                created REAL,
                FOREIGN KEY(session_id) REFERENCES sessions(session_id)
            );

            CREATE TABLE IF NOT EXISTS reasoning_patterns (
                pattern_id TEXT PRIMARY KEY,
                name TEXT,
                description TEXT,
                template TEXT,
                created REAL
            );

            CREATE TABLE IF NOT EXISTS checkpoints (
                name TEXT,
                session_id TEXT,
                snapshot TEXT,
                created REAL,
                PRIMARY KEY (name, session_id)
            );
        """)

        default_patterns = [
            (
                "cot",
                "Chain of Thought",
                "Sequential reasoning steps.",
                '{"steps": [{"type": "thought", "content": ""}]}',
            ),
            ("tot", "Tree of Thought", "Branching reasoning.", '{"steps": [{"type": "thought", "branches": []}]}'),
            (
                "react",
                "ReAct",
                "Thought → Action → Observation loop.",
                '{"steps": [{"type": "thought"}, {"type": "action"}, {"type": "observation"}]}',
            ),
            (
                "self_consistency",
                "Self-Consistency",
                "Multiple paths then voting.",
                '{"steps": [{"type": "thought", "variants": []}]}',
            ),
        ]
        for pid, name, desc, tmpl in default_patterns:
            conn.execute(
                "INSERT OR IGNORE INTO reasoning_patterns (pattern_id, name, description, template, created) VALUES (?, ?, ?, ?, ?)",
                (pid, name, desc, tmpl, time.time()),
            )


init_db()


def now_ts() -> float:
    return time.time()


def generate_step_id() -> str:
    return f"step_{int(now_ts() * 1000)}_{hashlib.md5(str(now_ts()).encode()).hexdigest()[:6]}"


# ----------------------------------------------------------------------
# Session management
# ----------------------------------------------------------------------
def _ensure_session(session_id: str, goal: str = "") -> None:
    with get_db() as conn:
        cur = conn.execute("SELECT session_id FROM sessions WHERE session_id = ?", (session_id,))
        if not cur.fetchone():
            conn.execute(
                "INSERT INTO sessions (session_id, goal, created, last_active, context_tokens) VALUES (?, ?, ?, ?, ?)",
                (session_id, goal, now_ts(), now_ts(), 0),
            )
        else:
            conn.execute("UPDATE sessions SET last_active = ? WHERE session_id = ?", (now_ts(), session_id))


def _get_session(session_id: str) -> Optional[Dict]:
    with get_db() as conn:
        row = conn.execute("SELECT * FROM sessions WHERE session_id = ?", (session_id,)).fetchone()
        return dict(row) if row else None


def _update_context_tokens(session_id: str, tokens: int) -> None:
    with get_db() as conn:
        conn.execute("UPDATE sessions SET context_tokens = ? WHERE session_id = ?", (tokens, session_id))


# ----------------------------------------------------------------------
# Reasoning steps
# ----------------------------------------------------------------------
def _add_reasoning_step(
    session_id: str, content: str, step_type: str = "thought", parent_step_id: str = None, confidence: float = 0.5
) -> str:
    _ensure_session(session_id)
    step_id = generate_step_id()
    with get_db() as conn:
        conn.execute(
            "INSERT INTO reasoning_steps (step_id, session_id, parent_step_id, step_type, content, confidence, created) VALUES (?, ?, ?, ?, ?, ?, ?)",
            (step_id, session_id, parent_step_id, step_type, content, confidence, now_ts()),
        )
    return step_id


def _get_reasoning_chain(session_id: str, limit: int = 50) -> List[Dict]:
    with get_db() as conn:
        rows = conn.execute(
            "SELECT step_id, step_type, content, confidence, evaluation_score, created FROM reasoning_steps WHERE session_id = ? ORDER BY created ASC LIMIT ?",
            (session_id, limit),
        ).fetchall()
        return [dict(row) for row in rows]


def _update_step_evaluation(session_id: str, step_id: str, score: float, note: str = "") -> None:
    with get_db() as conn:
        conn.execute(
            "UPDATE reasoning_steps SET evaluation_score = ?, evaluation_note = ? WHERE session_id = ? AND step_id = ?",
            (score, note, session_id, step_id),
        )


# ----------------------------------------------------------------------
# Assumptions
# ----------------------------------------------------------------------
def _add_assumption(session_id: str, content: str, confidence: float = 0.7, evidence: List[str] = None) -> str:
    ass_id = f"ass_{int(now_ts() * 1000)}_{hashlib.md5(content.encode()).hexdigest()[:6]}"
    evidence_str = json.dumps(evidence or [])
    with get_db() as conn:
        conn.execute(
            "INSERT INTO assumptions (assumption_id, session_id, content, confidence, evidence, created) VALUES (?, ?, ?, ?, ?, ?)",
            (ass_id, session_id, content, confidence, evidence_str, now_ts()),
        )
    return ass_id


def _get_assumptions(session_id: str) -> List[Dict]:
    with get_db() as conn:
        rows = conn.execute(
            "SELECT assumption_id, content, confidence, evidence FROM assumptions WHERE session_id = ? ORDER BY created DESC",
            (session_id,),
        ).fetchall()
        return [dict(row) for row in rows]


# ----------------------------------------------------------------------
# Contradiction detection
# ----------------------------------------------------------------------
def _check_contradiction(session_id: str, statement: str) -> List[Dict]:
    with get_db() as conn:
        assumptions = conn.execute("SELECT content FROM assumptions WHERE session_id = ?", (session_id,)).fetchall()
        steps = conn.execute(
            "SELECT content FROM reasoning_steps WHERE session_id = ? AND step_type IN ('thought', 'observation')",
            (session_id,),
        ).fetchall()
    all_texts = [a["content"] for a in assumptions] + [s["content"] for s in steps]

    contradictions = []
    statement_lower = statement.lower()
    for text in all_texts:
        if ("not " in text.lower() and statement_lower in text.lower().replace("not ", "")) or (
            "no " in text.lower() and statement_lower in text.lower().replace("no ", "")
        ):
            contradictions.append({"existing": text, "new": statement, "explanation": "Potential negation detected."})
    return contradictions[:5]


def _add_contradiction(session_id: str, statement_a: str, statement_b: str, explanation: str) -> int:
    with get_db() as conn:
        cur = conn.execute(
            "INSERT INTO contradictions (session_id, statement_a, statement_b, explanation, created) VALUES (?, ?, ?, ?, ?)",
            (session_id, statement_a, statement_b, explanation, now_ts()),
        )
        return cur.lastrowid


def _resolve_contradiction(contradiction_id: int) -> None:
    with get_db() as conn:
        conn.execute("UPDATE contradictions SET resolved = 1 WHERE id = ?", (contradiction_id,))


# ----------------------------------------------------------------------
# Reasoning patterns
# ----------------------------------------------------------------------
def _list_patterns() -> List[Dict]:
    with get_db() as conn:
        rows = conn.execute("SELECT pattern_id, name, description FROM reasoning_patterns").fetchall()
        return [dict(row) for row in rows]


def _get_pattern_template(pattern_id: str) -> Optional[str]:
    with get_db() as conn:
        row = conn.execute("SELECT template FROM reasoning_patterns WHERE pattern_id = ?", (pattern_id,)).fetchone()
        return row["template"] if row else None


def _apply_pattern(session_id: str, pattern_id: str) -> List[str]:
    template_str = _get_pattern_template(pattern_id)
    if not template_str:
        return []
    template = json.loads(template_str)
    step_ids = []
    for step_def in template.get("steps", []):
        step_type = step_def.get("type", "thought")
        content = step_def.get("content", f"[{step_type}]")
        step_id = _add_reasoning_step(session_id, content, step_type)
        step_ids.append(step_id)
    return step_ids


# ----------------------------------------------------------------------
# Checkpoints
# ----------------------------------------------------------------------
def _save_checkpoint(session_id: str, name: str) -> bool:
    snapshot = {
        "steps": _get_reasoning_chain(session_id, limit=1000),
        "assumptions": _get_assumptions(session_id),
        "goal": (_get_session(session_id) or {}).get("goal", ""),
    }
    with get_db() as conn:
        conn.execute(
            "INSERT OR REPLACE INTO checkpoints (name, session_id, snapshot, created) VALUES (?, ?, ?, ?)",
            (name, session_id, json.dumps(snapshot), now_ts()),
        )
    return True


def _restore_checkpoint(session_id: str, name: str) -> bool:
    with get_db() as conn:
        row = conn.execute(
            "SELECT snapshot FROM checkpoints WHERE session_id = ? AND name = ?", (session_id, name)
        ).fetchone()
        if not row:
            return False
        snapshot = json.loads(row["snapshot"])
        conn.execute("DELETE FROM reasoning_steps WHERE session_id = ?", (session_id,))
        conn.execute("DELETE FROM assumptions WHERE session_id = ?", (session_id,))
        for step in snapshot["steps"]:
            conn.execute(
                "INSERT INTO reasoning_steps (step_id, session_id, parent_step_id, step_type, content, confidence, evaluation_score, evaluation_note, created) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                (
                    step["step_id"],
                    session_id,
                    step.get("parent_step_id"),
                    step["step_type"],
                    step["content"],
                    step.get("confidence", 0.5),
                    step.get("evaluation_score"),
                    step.get("evaluation_note", ""),
                    step["created"],
                ),
            )
        for ass in snapshot["assumptions"]:
            conn.execute(
                "INSERT INTO assumptions (assumption_id, session_id, content, confidence, evidence, created) VALUES (?, ?, ?, ?, ?, ?)",
                (ass["assumption_id"], session_id, ass["content"], ass["confidence"], ass["evidence"], ass["created"]),
            )
        conn.execute("UPDATE sessions SET goal = ? WHERE session_id = ?", (snapshot.get("goal", ""), session_id))
    return True


# ----------------------------------------------------------------------
# Context & recovery prompts
# ----------------------------------------------------------------------
def _get_context_status(session_id: str) -> Dict:
    with get_db() as conn:
        row = conn.execute(
            "SELECT context_tokens, context_limit FROM sessions WHERE session_id = ?", (session_id,)
        ).fetchone()
        if not row:
            return {"used": 0, "limit": 200000, "percent": 0}
    used = row["context_tokens"]
    limit = row["context_limit"]
    percent = (used / limit) * 100 if limit > 0 else 0
    return {"used": used, "limit": limit, "percent": percent}


def _get_recovery_prompt(session_id: str) -> str:
    status = _get_context_status(session_id)
    steps = _get_reasoning_chain(session_id, limit=10)
    with get_db() as conn:
        rows = conn.execute(
            "SELECT statement_a, statement_b FROM contradictions WHERE session_id = ? AND resolved = 0", (session_id,)
        ).fetchall()
        contradictions = [dict(r) for r in rows]
    parts = []
    if status["percent"] > 80:
        parts.append(f"⚠️ Context {status['percent']:.0f}% full. Summarise the reasoning so far and continue.")
    if contradictions:
        parts.append(f"🔍 Unresolved contradictions: {len(contradictions)}. Consider resolving them with new evidence.")
    if steps:
        parts.append(f"📝 Last step: {steps[-1]['content']}")
    return "\n".join(parts) if parts else "No immediate issues."


# ----------------------------------------------------------------------
# LLM reranking (optional)
# ----------------------------------------------------------------------
async def _rerank_steps(session_id: str, query: str, top_k: int = 5) -> List[Dict]:
    if not RERANKER_AVAILABLE:
        return _get_reasoning_chain(session_id, limit=top_k)
    client = AsyncOpenAI()
    steps = _get_reasoning_chain(session_id, limit=50)
    if not steps:
        return []
    prompt = f"Relevance query: {query}\n\nReasoning steps:\n"
    for i, s in enumerate(steps):
        prompt += f"{i + 1}. {s['content']}\n"
    prompt += "\nReturn only the indices of the top 5 most relevant steps, separated by commas (e.g., 2,5,1,3,4)."
    try:
        resp = await client.chat.completions.create(
            model="gpt-4o-mini", messages=[{"role": "user", "content": prompt}], temperature=0
        )
        indices = [int(x.strip()) - 1 for x in resp.choices[0].message.content.split(",") if x.strip().isdigit()]
        ranked = [steps[i] for i in indices if 0 <= i < len(steps)]
        return ranked[:top_k]
    except Exception:
        return steps[:top_k]


# ----------------------------------------------------------------------
# MCP Server
# ----------------------------------------------------------------------
mcp = FastMCP("mcp-think")


@mcp.tool()
async def think_add_step(
    session_id: str,
    content: str,
    step_type: Literal["thought", "action", "observation"] = "thought",
    parent_step_id: Optional[str] = None,
    confidence: float = 0.5,
) -> str:
    """Add a reasoning step (thought, action, or observation) to a session."""
    step_id = _add_reasoning_step(session_id, content, step_type, parent_step_id, confidence)
    return f"Step added: {step_id}"


@mcp.tool()
async def think_get_chain(session_id: str, limit: int = 50) -> str:
    """Retrieve the full reasoning chain for a session."""
    steps = _get_reasoning_chain(session_id, limit)
    return json.dumps(steps, indent=2)


@mcp.tool()
async def think_evaluate_step(session_id: str, step_id: str, score: float, note: str = "") -> str:
    """Self-evaluate a reasoning step with a score (0–10) and optional note."""
    _update_step_evaluation(session_id, step_id, score, note)
    return f"Evaluated step {step_id} with score {score}"


@mcp.tool()
async def think_add_assumption(
    session_id: str,
    content: str,
    confidence: float = 0.7,
    evidence: Optional[List[str]] = None,
) -> str:
    """Record an assumption with a confidence level and supporting evidence step IDs."""
    ass_id = _add_assumption(session_id, content, confidence, evidence or [])
    return f"Assumption added: {ass_id}"


@mcp.tool()
async def think_list_assumptions(session_id: str) -> str:
    """List all assumptions for a session."""
    assumptions = _get_assumptions(session_id)
    return json.dumps(assumptions, indent=2)


@mcp.tool()
async def think_check_contradiction(session_id: str, statement: str) -> str:
    """Check a new statement against stored facts and assumptions for contradictions."""
    contradictions = _check_contradiction(session_id, statement)
    if contradictions:
        for c in contradictions:
            _add_contradiction(session_id, c["existing"], statement, c["explanation"])
        return f"Found {len(contradictions)} potential contradictions.\n{json.dumps(contradictions, indent=2)}"
    return "No contradictions detected."


@mcp.tool()
async def think_resolve_contradiction(contradiction_id: int) -> str:
    """Mark a stored contradiction as resolved."""
    _resolve_contradiction(contradiction_id)
    return f"Resolved contradiction {contradiction_id}."


@mcp.tool()
async def think_list_patterns() -> str:
    """List available reasoning patterns (cot, tot, react, self_consistency)."""
    patterns = _list_patterns()
    return json.dumps(patterns, indent=2)


@mcp.tool()
async def think_apply_pattern(session_id: str, pattern_id: str) -> str:
    """Initialize reasoning steps from a pattern template (cot, tot, react, self_consistency)."""
    step_ids = _apply_pattern(session_id, pattern_id)
    if not step_ids:
        return f"❌ Pattern '{pattern_id}' not found."
    return f"Created {len(step_ids)} steps: {', '.join(step_ids)}"


@mcp.tool()
async def think_save_checkpoint(session_id: str, name: str) -> str:
    """Save the full reasoning state (steps + assumptions) as a named checkpoint."""
    _save_checkpoint(session_id, name)
    return f"Checkpoint '{name}' saved."


@mcp.tool()
async def think_restore_checkpoint(session_id: str, name: str) -> str:
    """Restore a previously saved checkpoint, replacing current steps and assumptions."""
    ok = _restore_checkpoint(session_id, name)
    if ok:
        return f"Restored checkpoint '{name}'."
    return f"❌ Checkpoint '{name}' not found."


@mcp.tool()
async def think_update_context(session_id: str, tokens_used: int = 0) -> str:
    """Report current token usage and receive a recovery/continuation prompt."""
    _update_context_tokens(session_id, tokens_used)
    status = _get_context_status(session_id)
    prompt = _get_recovery_prompt(session_id)
    return f"Context: {status['used']}/{status['limit']} tokens ({status['percent']:.1f}%)\n\n{prompt}"


@mcp.tool()
async def think_rerank_steps(session_id: str, query: str, top_k: int = 5) -> str:
    """Reorder reasoning steps by relevance to a query using an LLM (requires OPENAI_API_KEY)."""
    ranked = await _rerank_steps(session_id, query, top_k)
    return json.dumps(ranked, indent=2)


# ----------------------------------------------------------------------
# Main
# ----------------------------------------------------------------------
def main():
    mcp.run()


if __name__ == "__main__":
    main()
