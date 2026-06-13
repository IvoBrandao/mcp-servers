#!/usr/bin/env python3
"""
Flow Orchestrator MCP Server - Enhanced with Automatic Context Compaction
State-of-the-art hierarchical memory and summarisation.
"""

import json
import sqlite3
import time
import re
import hashlib
from pathlib import Path
from typing import Any, Dict, List, Optional, Tuple
from collections import Counter

from mcp.server.fastmcp import FastMCP

# ----------------------------------------------------------------------
# Configuration (can be overridden via env or arguments)
# ----------------------------------------------------------------------
DATA_DIR = Path.home() / ".mcp-flow"
DATA_DIR.mkdir(parents=True, exist_ok=True)
DB_PATH = DATA_DIR / "flow.db"

# Compaction thresholds
COMPACTION_TOKEN_RATIO = 0.80  # trigger when context_usage / limit >= this
COMPACTION_RETAIN_STEPS = 10  # keep most recent N steps after compaction
COMPACTION_RETAIN_TOKENS = 2000  # keep approx this many tokens of recent context
SUMMARY_MODEL = None  # will be set if a small LLM is available

# Optional summariser (local or API)
try:
    # Attempt to load a small transformer for abstractive summarisation
    from transformers import pipeline

    SUMMARY_MODEL = pipeline("summarization", model="sshleifer/distilbart-cnn-12-6", device=-1)
    SUMMARISER_AVAILABLE = True
except ImportError:
    SUMMARISER_AVAILABLE = False

# For extractive summarisation fallback
try:
    from sklearn.feature_extraction.text import TfidfVectorizer
    import numpy as np

    SKLEARN_AVAILABLE = True
except ImportError:
    SKLEARN_AVAILABLE = False

# Semantic search (already present)
SEMANTIC_ENABLED = False
try:
    from sentence_transformers import SentenceTransformer
    import numpy as np

    SEMANTIC_ENABLED = True
    _embedding_model = SentenceTransformer("all-MiniLM-L6-v2")
except ImportError:
    SEMANTIC_ENABLED = False


# ----------------------------------------------------------------------
# SQLite schema upgrade (add long_term_memory table)
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
                context_usage INTEGER DEFAULT 0,
                context_limit INTEGER DEFAULT 200000
            );
            CREATE TABLE IF NOT EXISTS plan_steps (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT,
                step_name TEXT,
                description TEXT,
                dependencies TEXT,
                status TEXT,
                result TEXT,
                error TEXT,
                created REAL,
                updated REAL,
                FOREIGN KEY(session_id) REFERENCES sessions(session_id)
            );
            CREATE TABLE IF NOT EXISTS memories (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT,
                key TEXT,
                value TEXT,
                ttl REAL,
                created REAL,
                embedding BLOB,
                FOREIGN KEY(session_id) REFERENCES sessions(session_id)
            );
            CREATE TABLE IF NOT EXISTS long_term_memory (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT,
                summary TEXT,
                compressed_steps TEXT,    -- JSON list of step IDs
                compressed_from REAL,
                compressed_until REAL,
                created REAL,
                embedding BLOB
            );
            CREATE TABLE IF NOT EXISTS execution_log (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT,
                step_id INTEGER,
                action TEXT,
                input TEXT,
                output TEXT,
                timestamp REAL,
                FOREIGN KEY(session_id) REFERENCES sessions(session_id)
            );
            CREATE TABLE IF NOT EXISTS checkpoints (
                name TEXT,
                session_id TEXT,
                snapshot TEXT,
                created REAL,
                PRIMARY KEY (name, session_id)
            );
        """)


init_db()


# ----------------------------------------------------------------------
# Helper functions
# ----------------------------------------------------------------------
def now_ts() -> float:
    return time.time()


def sanitize_session_id(sid: str) -> str:
    # allow alphanumeric, underscore, hyphen
    return re.sub(r"[^a-zA-Z0-9_-]", "_", sid)[:64]


def get_session(session_id: str) -> Optional[Dict]:
    with get_db() as conn:
        row = conn.execute("SELECT * FROM sessions WHERE session_id = ?", (session_id,)).fetchone()
        return dict(row) if row else None


def ensure_session(session_id: str, goal: str = "") -> None:
    with get_db() as conn:
        cur = conn.execute("SELECT session_id FROM sessions WHERE session_id = ?", (session_id,))
        if not cur.fetchone():
            conn.execute(
                "INSERT INTO sessions (session_id, goal, created, last_active, context_usage, context_limit) VALUES (?, ?, ?, ?, ?, ?)",
                (session_id, goal, now_ts(), now_ts(), 0, 200000),
            )
        else:
            conn.execute("UPDATE sessions SET last_active = ? WHERE session_id = ?", (now_ts(), session_id))


def update_context_usage(session_id: str, tokens_used: int) -> None:
    with get_db() as conn:
        conn.execute("UPDATE sessions SET context_usage = ? WHERE session_id = ?", (tokens_used, session_id))


# ----------------------------------------------------------------------
# Memory (with optional semantic search)
# ----------------------------------------------------------------------
def set_memory(session_id: str, key: str, value: str, ttl_seconds: int = 0) -> None:
    expire = now_ts() + ttl_seconds if ttl_seconds > 0 else 0
    embedding = None
    if SEMANTIC_ENABLED:
        emb = _embedding_model.encode(value).astype(np.float32).tobytes()
        embedding = emb
    with get_db() as conn:
        conn.execute(
            "INSERT OR REPLACE INTO memories (session_id, key, value, ttl, created, embedding) VALUES (?, ?, ?, ?, ?, ?)",
            (session_id, key, value, expire, now_ts(), embedding),
        )
        # delete expired
        conn.execute("DELETE FROM memories WHERE ttl > 0 AND ttl <= ?", (now_ts(),))


def get_memory(session_id: str, key: str) -> Optional[str]:
    with get_db() as conn:
        row = conn.execute(
            "SELECT value FROM memories WHERE session_id = ? AND key = ? AND (ttl = 0 OR ttl > ?)",
            (session_id, key, now_ts()),
        ).fetchone()
        return row["value"] if row else None


def query_memory(session_id: str, query: str, top_k: int = 5, use_semantic: bool = True) -> List[Dict]:
    if use_semantic and SEMANTIC_ENABLED:
        q_emb = _embedding_model.encode(query).astype(np.float32)
        with get_db() as conn:
            rows = conn.execute(
                "SELECT key, value, embedding FROM memories WHERE session_id = ? AND embedding IS NOT NULL",
                (session_id,),
            ).fetchall()
        if rows:
            scored = []
            for row in rows:
                emb = np.frombuffer(row["embedding"], dtype=np.float32)
                sim = float(np.dot(q_emb, emb) / (np.linalg.norm(q_emb) * np.linalg.norm(emb) + 1e-8))
                scored.append((sim, row["key"], row["value"]))
            scored.sort(reverse=True, key=lambda x: x[0])
            return [{"key": k, "value": v, "score": s} for s, k, v in scored[:top_k]]
    # Fallback to keyword search
    with get_db() as conn:
        rows = conn.execute(
            "SELECT key, value FROM memories WHERE session_id = ? AND (ttl = 0 OR ttl > ?)", (session_id, now_ts())
        ).fetchall()
        keyword_matches = []
        query_lower = query.lower()
        for row in rows:
            if query_lower in row["key"].lower() or query_lower in row["value"].lower():
                keyword_matches.append({"key": row["key"], "value": row["value"], "score": 1.0})
        return keyword_matches[:top_k]


def delete_memory(session_id: str, key: str) -> bool:
    with get_db() as conn:
        cur = conn.execute("DELETE FROM memories WHERE session_id = ? AND key = ?", (session_id, key))
        return cur.rowcount > 0


# ----------------------------------------------------------------------
# Plan steps
# ----------------------------------------------------------------------
def add_step(session_id: str, step_name: str, description: str, dependencies: List[str] = None) -> int:
    ensure_session(session_id)
    deps_json = json.dumps(dependencies or [])
    with get_db() as conn:
        cur = conn.execute(
            "INSERT INTO plan_steps (session_id, step_name, description, dependencies, status, created, updated) VALUES (?, ?, ?, ?, 'pending', ?, ?)",
            (session_id, step_name, description, deps_json, now_ts(), now_ts()),
        )
        return cur.lastrowid


def update_step_status(session_id: str, step_name: str, status: str, result: str = "", error: str = "") -> None:
    allowed = {"pending", "in_progress", "done", "failed", "blocked"}
    if status not in allowed:
        raise ValueError(f"Invalid status: {status}")
    with get_db() as conn:
        conn.execute(
            "UPDATE plan_steps SET status = ?, result = ?, error = ?, updated = ? WHERE session_id = ? AND step_name = ?",
            (status, result, error, now_ts(), session_id, step_name),
        )


def get_next_action(session_id: str) -> Optional[Dict]:
    """Return the next pending step that has all dependencies satisfied."""
    with get_db() as conn:
        rows = conn.execute(
            "SELECT step_name, description, dependencies, status FROM plan_steps WHERE session_id = ? AND status IN ('pending', 'failed') ORDER BY created ASC",
            (session_id,),
        ).fetchall()
    for row in rows:
        if row["status"] == "failed":
            # return a retry suggestion
            return {"action": "retry", "step_name": row["step_name"], "description": row["description"]}
        # check dependencies
        deps = json.loads(row["dependencies"])
        if not deps:
            return {"action": "execute", "step_name": row["step_name"], "description": row["description"]}
        # verify all deps are 'done'
        with get_db() as conn2:
            done = conn2.execute(
                "SELECT COUNT(*) FROM plan_steps WHERE session_id = ? AND step_name IN ({}) AND status = 'done'".format(
                    ",".join("?" * len(deps))
                ),
                (session_id, *deps),
            ).fetchone()[0]
            if done == len(deps):
                return {"action": "execute", "step_name": row["step_name"], "description": row["description"]}
    # no pending step – maybe all done
    with get_db() as conn:
        total = conn.execute("SELECT COUNT(*) FROM plan_steps WHERE session_id = ?", (session_id,)).fetchone()[0]
        done = conn.execute(
            "SELECT COUNT(*) FROM plan_steps WHERE session_id = ? AND status = 'done'", (session_id,)
        ).fetchone()[0]
        if total > 0 and done == total:
            return {"action": "complete", "message": "All steps finished."}
    return None


def list_steps(session_id: str) -> List[Dict]:
    with get_db() as conn:
        rows = conn.execute(
            "SELECT step_name, description, status, result, error FROM plan_steps WHERE session_id = ?", (session_id,)
        ).fetchall()
        return [dict(row) for row in rows]


def delete_step(session_id: str, step_name: str) -> bool:
    with get_db() as conn:
        cur = conn.execute("DELETE FROM plan_steps WHERE session_id = ? AND step_name = ?", (session_id, step_name))
        return cur.rowcount > 0


# ----------------------------------------------------------------------
# Execution log
# ----------------------------------------------------------------------
def log_execution(session_id: str, step_id: int, action: str, input_data: str, output_data: str) -> None:
    with get_db() as conn:
        conn.execute(
            "INSERT INTO execution_log (session_id, step_id, action, input, output, timestamp) VALUES (?, ?, ?, ?, ?, ?)",
            (session_id, step_id, action, input_data, output_data, now_ts()),
        )


def get_execution_history(session_id: str, limit: int = 50) -> List[Dict]:
    with get_db() as conn:
        rows = conn.execute(
            "SELECT action, input, output, timestamp FROM execution_log WHERE session_id = ? ORDER BY timestamp DESC LIMIT ?",
            (session_id, limit),
        ).fetchall()
        return [dict(row) for row in rows]


# ----------------------------------------------------------------------
# Checkpoints
# ----------------------------------------------------------------------
def save_checkpoint(session_id: str, name: str) -> bool:
    """Save full session state (plan + memories) as JSON."""
    with get_db() as conn:
        # fetch all plan steps
        steps = conn.execute(
            "SELECT step_name, description, dependencies, status, result, error FROM plan_steps WHERE session_id = ?",
            (session_id,),
        ).fetchall()
        memories = conn.execute("SELECT key, value, ttl FROM memories WHERE session_id = ?", (session_id,)).fetchall()
        snapshot = {
            "steps": [dict(s) for s in steps],
            "memories": [dict(m) for m in memories],
            "session_goal": conn.execute("SELECT goal FROM sessions WHERE session_id = ?", (session_id,)).fetchone()[0],
        }
        snapshot_json = json.dumps(snapshot)
        conn.execute(
            "INSERT OR REPLACE INTO checkpoints (name, session_id, snapshot, created) VALUES (?, ?, ?, ?)",
            (name, session_id, snapshot_json, now_ts()),
        )
        return True


def restore_checkpoint(session_id: str, name: str) -> bool:
    with get_db() as conn:
        row = conn.execute(
            "SELECT snapshot FROM checkpoints WHERE session_id = ? AND name = ?", (session_id, name)
        ).fetchone()
        if not row:
            return False
        snapshot = json.loads(row["snapshot"])
        # clear existing plan and memories for this session
        conn.execute("DELETE FROM plan_steps WHERE session_id = ?", (session_id,))
        conn.execute("DELETE FROM memories WHERE session_id = ?", (session_id,))
        # restore steps
        for step in snapshot["steps"]:
            conn.execute(
                "INSERT INTO plan_steps (session_id, step_name, description, dependencies, status, result, error, created, updated) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                (
                    session_id,
                    step["step_name"],
                    step["description"],
                    step["dependencies"],
                    step["status"],
                    step["result"],
                    step["error"],
                    now_ts(),
                    now_ts(),
                ),
            )
        # restore memories
        for mem in snapshot["memories"]:
            conn.execute(
                "INSERT INTO memories (session_id, key, value, ttl, created) VALUES (?, ?, ?, ?, ?)",
                (session_id, mem["key"], mem["value"], mem["ttl"], now_ts()),
            )
        # update goal
        conn.execute(
            "UPDATE sessions SET goal = ? WHERE session_id = ?", (snapshot.get("session_goal", ""), session_id)
        )
        return True


def list_checkpoints(session_id: str) -> List[str]:
    with get_db() as conn:
        rows = conn.execute("SELECT name FROM checkpoints WHERE session_id = ?", (session_id,)).fetchall()
        return [r["name"] for r in rows]


# ----------------------------------------------------------------------
# Context management & recovery prompts (with compaction awareness)
# ----------------------------------------------------------------------
def get_context_status(session_id: str) -> Dict:
    with get_db() as conn:
        row = conn.execute(
            "SELECT context_usage, context_limit FROM sessions WHERE session_id = ?", (session_id,)
        ).fetchone()
        if not row:
            return {"used": 0, "limit": 200000, "percent": 0}
    used = row["context_usage"]
    limit = row["context_limit"]
    percent = (used / limit) * 100 if limit > 0 else 0
    return {"used": used, "limit": limit, "percent": percent}


def get_recovery_prompt(session_id: str) -> str:
    """Generate a prompt to help the model recover from failure or near‑context limit."""
    status = get_context_status(session_id)
    steps = list_steps(session_id)
    pending = [s for s in steps if s["status"] in ("pending", "failed")]
    history = get_execution_history(session_id, limit=10)

    prompt_parts = []
    if status["percent"] > 80:
        prompt_parts.append(
            f"⚠️ Context is {status['percent']:.0f}% full ({status['used']}/{status['limit']} tokens). Automatic compaction recommended. Call `flow_compact_context` or rely on auto‑trigger in `flow_update_context`."
        )
    if pending:
        prompt_parts.append(f"📋 Pending steps: {', '.join(s['step_name'] for s in pending)}")
        next_action = get_next_action(session_id)
        if next_action and next_action["action"] == "retry":
            prompt_parts.append(
                f"🔁 Last step '{next_action['step_name']}' failed. You can either retry it (call `flow_update_step_status` with status='in_progress') or re‑plan (call `flow_add_step`)."
            )
    # Add long‑term memory reminder
    lt_mem = get_long_term_memory(session_id, top_k=1)
    if lt_mem:
        prompt_parts.append("📚 Long‑term memory available. Use `flow_query_long_term_memory` if needed.")
    if not prompt_parts:
        prompt_parts.append("✅ No immediate issues. Continue with the next step in the plan.")
    return "\n".join(prompt_parts)


# ----------------------------------------------------------------------
# NEW: Context Compaction Functions
# ----------------------------------------------------------------------
def get_old_steps(session_id: str, keep_last: int = None, keep_tokens: int = None) -> List[Dict]:
    """
    Retrieve steps that are candidates for compaction.
    If keep_last is given, returns all steps except the most recent N.
    If keep_tokens is given, estimates token count (1 token ~ 4 chars) and returns steps older than the token window.
    """
    with get_db() as conn:
        rows = conn.execute(
            "SELECT id, step_name, description, status, result, created FROM plan_steps WHERE session_id = ? ORDER BY created ASC",
            (session_id,),
        ).fetchall()
    steps = [dict(r) for r in rows]
    if keep_last is not None and keep_last > 0:
        return steps[:-keep_last] if len(steps) > keep_last else []
    if keep_tokens is not None and keep_tokens > 0:
        cumulative = 0
        cutoff_index = len(steps)
        for i, step in enumerate(reversed(steps)):
            desc_len = len(step.get("description", "")) + len(step.get("result", ""))
            cumulative += desc_len // 4  # approximate tokens
            if cumulative > keep_tokens:
                cutoff_index = len(steps) - i
                break
        return steps[:cutoff_index]
    return steps


def extractive_summarize(texts: List[str], max_summary_sentences: int = 3) -> str:
    """
    Simple extractive summarisation using TF-IDF.
    Returns a summary consisting of the top sentences.
    """
    if not texts:
        return "No content to summarise."
    full_text = "\n".join(texts)
    sentences = re.split(r"(?<=[.!?])\s+", full_text)
    if len(sentences) <= max_summary_sentences:
        return full_text
    if SKLEARN_AVAILABLE:
        vectorizer = TfidfVectorizer(stop_words="english")
        tfidf = vectorizer.fit_transform(sentences)
        scores = np.array(tfidf.sum(axis=1)).flatten()
        top_indices = scores.argsort()[-max_summary_sentences:][::-1]
        summary = " ".join([sentences[i] for i in sorted(top_indices)])
        return summary
    else:
        # Fallback: return first few sentences
        return " ".join(sentences[:max_summary_sentences])


def abstractive_summarize(text: str, max_length: int = 150) -> str:
    """Use a small transformer model for abstractive summarisation."""
    if not SUMMARISER_AVAILABLE or SUMMARY_MODEL is None:
        return extractive_summarize([text], 3)
    try:
        result = SUMMARY_MODEL(text, max_length=max_length, min_length=30, do_sample=False)
        return result[0]["summary_text"]
    except Exception:
        return extractive_summarize([text], 3)


def compress_steps(session_id: str, steps_to_compress: List[Dict]) -> str:
    """Generate a textual summary from a list of steps."""
    step_texts = []
    for step in steps_to_compress:
        text = f"Step: {step['step_name']}\nDescription: {step['description']}\nStatus: {step['status']}\n"
        if step.get("result"):
            text += f"Result: {step['result']}\n"
        step_texts.append(text)
    combined = "\n".join(step_texts)
    if SUMMARISER_AVAILABLE:
        summary = abstractive_summarize(combined, max_length=200)
    else:
        summary = extractive_summarize(step_texts, max_summary_sentences=5)
    return summary


def store_long_term_memory(
    session_id: str, summary: str, compressed_step_ids: List[int], min_ts: float, max_ts: float
) -> None:
    embedding = None
    if SEMANTIC_ENABLED:
        emb = _embedding_model.encode(summary).astype(np.float32).tobytes()
        embedding = emb
    with get_db() as conn:
        conn.execute(
            "INSERT INTO long_term_memory (session_id, summary, compressed_steps, compressed_from, compressed_until, created, embedding) VALUES (?, ?, ?, ?, ?, ?, ?)",
            (session_id, summary, json.dumps(compressed_step_ids), min_ts, max_ts, now_ts(), embedding),
        )


def delete_compressed_steps(session_id: str, step_ids: List[int]) -> None:
    if not step_ids:
        return
    placeholders = ",".join("?" * len(step_ids))
    with get_db() as conn:
        conn.execute(f"DELETE FROM plan_steps WHERE session_id = ? AND id IN ({placeholders})", (session_id, *step_ids))


def get_long_term_memory(session_id: str, query: str = "", top_k: int = 3) -> List[Dict]:
    """
    Retrieve relevant long‑term memories, optionally using semantic search.
    """
    if query and SEMANTIC_ENABLED:
        q_emb = _embedding_model.encode(query).astype(np.float32)
        with get_db() as conn:
            rows = conn.execute(
                "SELECT id, summary, created FROM long_term_memory WHERE session_id = ? AND embedding IS NOT NULL",
                (session_id,),
            ).fetchall()
        if rows:
            scored = []
            for row in rows:
                emb = np.frombuffer(row["embedding"], dtype=np.float32)
                sim = float(np.dot(q_emb, emb) / (np.linalg.norm(q_emb) * np.linalg.norm(emb) + 1e-8))
                scored.append((sim, row["summary"], row["created"]))
            scored.sort(reverse=True, key=lambda x: x[0])
            return [{"summary": s, "created": c, "score": sc} for sc, s, c in scored[:top_k]]
    # Fallback: return most recent
    with get_db() as conn:
        rows = conn.execute(
            "SELECT summary, created FROM long_term_memory WHERE session_id = ? ORDER BY created DESC LIMIT ?",
            (session_id, top_k),
        ).fetchall()
        return [dict(r) for r in rows]


def compact_context(session_id: str, keep_last: int = None, keep_tokens: int = None) -> Dict:
    """
    Perform the compaction: summarise old steps, move to long‑term memory, delete original.
    Returns a status report.
    """
    if keep_last is None:
        keep_last = COMPACTION_RETAIN_STEPS
    if keep_tokens is None:
        keep_tokens = COMPACTION_RETAIN_TOKENS
    old_steps = get_old_steps(session_id, keep_last=keep_last, keep_tokens=keep_tokens)
    if not old_steps:
        return {"compacted": False, "reason": "No steps to compact"}
    step_ids = [s["id"] for s in old_steps]
    min_ts = min(s["created"] for s in old_steps)
    max_ts = max(s["created"] for s in old_steps)
    summary = compress_steps(session_id, old_steps)
    store_long_term_memory(session_id, summary, step_ids, min_ts, max_ts)
    delete_compressed_steps(session_id, step_ids)
    return {
        "compacted": True,
        "steps_removed": len(old_steps),
        "summary": summary,
    }


# ----------------------------------------------------------------------
# FastMCP tools (original + new)
# ----------------------------------------------------------------------
mcp = FastMCP("flow-orchestrator")


@mcp.tool()
async def flow_create_session(session_id: str, goal: str = "") -> str:
    """Start a new workflow session."""
    session_id = sanitize_session_id(session_id)
    if not session_id:
        return "❌ 'session_id' is required."
    ensure_session(session_id, goal)
    return f"✅ Session '{session_id}' created."


@mcp.tool()
async def flow_add_step(session_id: str, step_name: str, description: str, dependencies: List[str] = None) -> str:
    """Add a step to the plan."""
    session_id = sanitize_session_id(session_id)
    if not session_id or not step_name:
        return "❌ Missing session_id or step_name."
    step_id = add_step(session_id, step_name, description, dependencies)
    return f"✅ Step '{step_name}' added (ID {step_id})."


@mcp.tool()
async def flow_get_next_action(session_id: str) -> str:
    """Get the next pending action (step to execute, or retry)."""
    session_id = sanitize_session_id(session_id)
    if not session_id:
        return "❌ Missing session_id."
    action = get_next_action(session_id)
    if action:
        return json.dumps(action, indent=2)
    return "No pending actions."


@mcp.tool()
async def flow_update_step_status(
    session_id: str, step_name: str, status: str, result: str = "", error: str = ""
) -> str:
    """Update status of a step (pending, in_progress, done, failed)."""
    session_id = sanitize_session_id(session_id)
    if not session_id or not step_name or not status:
        return "❌ Missing session_id, step_name, or status."
    update_step_status(session_id, step_name, status, result, error)
    log_execution(session_id, 0, "update_status", f"{step_name} -> {status}", result)
    return f"✅ Step '{step_name}' status set to {status}."


@mcp.tool()
async def flow_set_memory(session_id: str, key: str, value: str, ttl: int = 0) -> str:
    """Store a fact (key-value) with optional TTL."""
    session_id = sanitize_session_id(session_id)
    if not session_id or not key:
        return "❌ Missing session_id or key."
    set_memory(session_id, key, value, ttl)
    return f"✅ Memory '{key}' stored."


@mcp.tool()
async def flow_get_memory(session_id: str, key: str) -> str:
    """Retrieve a stored fact by key."""
    session_id = sanitize_session_id(session_id)
    if not session_id or not key:
        return "❌ Missing session_id or key."
    val = get_memory(session_id, key)
    if val is None:
        return "Key not found."
    return val


@mcp.tool()
async def flow_query_memory(session_id: str, query: str, top_k: int = 5) -> str:
    """Semantic or keyword search over stored facts."""
    session_id = sanitize_session_id(session_id)
    if not session_id or not query:
        return "❌ Missing session_id or query."
    results = query_memory(session_id, query, top_k)
    return json.dumps(results, indent=2)


@mcp.tool()
async def flow_save_checkpoint(session_id: str, name: str) -> str:
    """Save a full snapshot of the current session (plan + memory)."""
    session_id = sanitize_session_id(session_id)
    if not session_id or not name:
        return "❌ Missing session_id or checkpoint name."
    save_checkpoint(session_id, name)
    return f"✅ Checkpoint '{name}' saved."


@mcp.tool()
async def flow_restore_checkpoint(session_id: str, name: str) -> str:
    """Restore a previous snapshot."""
    session_id = sanitize_session_id(session_id)
    if not session_id or not name:
        return "❌ Missing session_id or checkpoint name."
    ok = restore_checkpoint(session_id, name)
    if ok:
        return f"✅ Restored checkpoint '{name}'."
    return f"❌ Checkpoint '{name}' not found."


@mcp.tool()
async def flow_list_steps(session_id: str) -> str:
    """List all steps with their status."""
    session_id = sanitize_session_id(session_id)
    if not session_id:
        return "❌ Missing session_id."
    steps = list_steps(session_id)
    return json.dumps(steps, indent=2)


@mcp.tool()
async def flow_delete_step(session_id: str, step_name: str) -> str:
    """Delete a step from the plan."""
    session_id = sanitize_session_id(session_id)
    if not session_id or not step_name:
        return "❌ Missing session_id or step_name."
    ok = delete_step(session_id, step_name)
    if ok:
        return f"✅ Deleted step '{step_name}'."
    return f"❌ Step '{step_name}' not found."


# ----------------------------------------------------------------------
# NEW: Compaction tools
# ----------------------------------------------------------------------
@mcp.tool()
async def flow_compact_context(
    session_id: str,
    keep_last: int = None,
    keep_tokens: int = None,
) -> str:
    """
    Compact the reasoning context: summarise older steps into long‑term memory and delete them.
    Automatically called when context usage exceeds threshold, but can be invoked manually.
    """
    session_id = sanitize_session_id(session_id)
    if not session_id:
        return "❌ Missing session_id."
    result = compact_context(session_id, keep_last, keep_tokens)
    if not result["compacted"]:
        return f"ℹ️ Compaction not performed: {result.get('reason', 'no steps to compact')}"
    # Also log compaction event
    log_execution(session_id, 0, "compact_context", f"removed {result['steps_removed']} steps", result["summary"][:200])
    return f"✅ Compaction completed. Removed {result['steps_removed']} steps.\nSummary:\n{result['summary']}"


@mcp.tool()
async def flow_query_long_term_memory(session_id: str, query: str = "", top_k: int = 3) -> str:
    """
    Retrieve archived summaries from long‑term memory (semantic search if query provided).
    """
    session_id = sanitize_session_id(session_id)
    if not session_id:
        return "❌ Missing session_id."
    results = get_long_term_memory(session_id, query, top_k)
    if not results:
        return "No long‑term memory entries found."
    return json.dumps(results, indent=2)


# ----------------------------------------------------------------------
# Override flow_update_context to automatically trigger compaction
# ----------------------------------------------------------------------
@mcp.tool()
async def flow_update_context(session_id: str, tokens_used: int = 0) -> str:
    """Report token usage; automatically triggers compaction if threshold exceeded."""
    session_id = sanitize_session_id(session_id)
    if not session_id:
        return "❌ Missing session_id."
    update_context_usage(session_id, tokens_used)
    status = get_context_status(session_id)
    # Automatic compaction if threshold reached and not recently compacted (avoid loops)
    if status["percent"] >= COMPACTION_TOKEN_RATIO * 100:
        # Check last compaction time (using execution log)
        with get_db() as conn:
            last_compact = conn.execute(
                "SELECT timestamp FROM execution_log WHERE session_id = ? AND action = 'compact_context' ORDER BY timestamp DESC LIMIT 1",
                (session_id,),
            ).fetchone()
        # Only compact if not compacted in the last 30 seconds (prevent thrashing)
        if not last_compact or (now_ts() - last_compact["timestamp"]) > 30:
            compact_result = compact_context(session_id)
            if compact_result["compacted"]:
                log_execution(
                    session_id, 0, "auto_compact", "triggered by context usage", compact_result["summary"][:200]
                )
                prompt = get_recovery_prompt(session_id)
                return f"🔧 Automatic compaction triggered (context {status['percent']:.0f}%). Removed {compact_result['steps_removed']} steps.\n\n{compact_result['summary'][:500]}\n\n{prompt}"
    prompt = get_recovery_prompt(session_id)
    return f"Context: {status['used']}/{status['limit']} tokens ({status['percent']:.1f}%)\n\n{prompt}"


# ----------------------------------------------------------------------
# Entry point
# ----------------------------------------------------------------------
if __name__ == "__main__":
    mcp.run()
