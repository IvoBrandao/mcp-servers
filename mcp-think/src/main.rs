use rmcp::{handler::server::wrapper::Parameters, schemars, tool, tool_router};
use rmcp::{ServiceExt, transport::stdio};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

// ── helpers ───────────────────────────────────────────────────────────────────

fn data_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".mcp-think")
}

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

fn generate_id(prefix: &str) -> String {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros();
    format!("{prefix}_{ts}")
}

fn init_schema(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
        CREATE TABLE IF NOT EXISTS sessions (
            session_id     TEXT PRIMARY KEY,
            goal           TEXT,
            created        REAL NOT NULL,
            last_active    REAL NOT NULL,
            context_tokens INTEGER NOT NULL DEFAULT 0,
            context_limit  INTEGER NOT NULL DEFAULT 100000
        );
        CREATE TABLE IF NOT EXISTS reasoning_steps (
            step_id          TEXT PRIMARY KEY,
            session_id       TEXT NOT NULL,
            parent_step_id   TEXT,
            step_type        TEXT NOT NULL,
            content          TEXT NOT NULL,
            confidence       REAL,
            evaluation_score REAL,
            evaluation_note  TEXT,
            created          REAL NOT NULL
        );
        CREATE TABLE IF NOT EXISTS assumptions (
            assumption_id TEXT PRIMARY KEY,
            session_id    TEXT NOT NULL,
            content       TEXT NOT NULL,
            confidence    REAL,
            evidence      TEXT,
            created       REAL NOT NULL
        );
        CREATE TABLE IF NOT EXISTS contradictions (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id  TEXT NOT NULL,
            statement_a TEXT NOT NULL,
            statement_b TEXT NOT NULL,
            explanation TEXT,
            resolved    INTEGER NOT NULL DEFAULT 0,
            created     REAL NOT NULL
        );
        CREATE TABLE IF NOT EXISTS checkpoints (
            name       TEXT NOT NULL,
            session_id TEXT NOT NULL,
            snapshot   TEXT NOT NULL,
            created    REAL NOT NULL,
            PRIMARY KEY(name, session_id)
        );",
    )?;
    Ok(())
}

fn ensure_session(conn: &Connection, session_id: &str) {
    let now = now_secs();
    let _ = conn.execute(
        "INSERT OR IGNORE INTO sessions (session_id, created, last_active) VALUES (?1, ?2, ?2)",
        rusqlite::params![session_id, now],
    );
    let _ = conn.execute(
        "UPDATE sessions SET last_active=?1 WHERE session_id=?2",
        rusqlite::params![now, session_id],
    );
}

// ── contradiction detection ───────────────────────────────────────────────────

const NEGATIONS: &[&str] = &[
    "not ",
    "no ",
    "cannot ",
    "never ",
    "isn't ",
    "aren't ",
    "won't ",
    "can't ",
    "doesn't ",
    "don't ",
    "neither ",
    "nor ",
];

const STOPWORDS: &[&str] = &[
    "that", "this", "with", "from", "have", "will", "been", "were", "they", "them", "their",
    "there", "here", "when", "what", "which", "where", "then", "than", "also", "just", "some",
    "into", "does", "about", "would", "could", "should", "might", "must",
];

fn keywords(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 3 && !STOPWORDS.contains(w))
        .map(|w| w.to_string())
        .collect()
}

/// Naive keyword-based contradiction detection.
/// Returns Some(explanation) if a contradiction is detected between stmt_a and stmt_b.
fn detect_contradiction(stmt_a: &str, stmt_b: &str) -> Option<String> {
    let a = stmt_a.to_lowercase();
    let b = stmt_b.to_lowercase();

    // Check if B negates something that A affirms
    for kw in keywords(stmt_a) {
        for neg in NEGATIONS {
            let neg_kw = format!("{neg}{kw}");
            if b.contains(&neg_kw) && !a.contains(&neg_kw) {
                return Some(format!(
                    "Statement A affirms '{kw}' while statement B negates it ('{neg_kw}')"
                ));
            }
        }
    }

    // Check if A negates something that B affirms
    for kw in keywords(stmt_b) {
        for neg in NEGATIONS {
            let neg_kw = format!("{neg}{kw}");
            if a.contains(&neg_kw) && !b.contains(&neg_kw) {
                return Some(format!(
                    "Statement A negates '{kw}' ('{neg_kw}') while statement B affirms it"
                ));
            }
        }
    }

    None
}

// ── built-in reasoning patterns ───────────────────────────────────────────────

fn builtin_patterns() -> serde_json::Value {
    serde_json::json!([
        {
            "id": "cot",
            "name": "Chain of Thought",
            "description": "Step-by-step reasoning that makes the thought process explicit",
            "steps": [
                {"step_type": "thought",     "content": "Let me think through this step by step."},
                {"step_type": "thought",     "content": "First, I need to understand the problem: [describe problem here]"},
                {"step_type": "thought",     "content": "Key factors to consider: [list factors]"},
                {"step_type": "thought",     "content": "Reasoning through each factor: [detailed reasoning]"},
                {"step_type": "observation", "content": "Conclusion: [final answer based on reasoning]"}
            ]
        },
        {
            "id": "tot",
            "name": "Tree of Thought",
            "description": "Explore multiple reasoning branches and select the best path",
            "steps": [
                {"step_type": "thought",     "content": "Branch A: [first approach and its implications]"},
                {"step_type": "thought",     "content": "Branch B: [second approach and its implications]"},
                {"step_type": "thought",     "content": "Branch C: [third approach and its implications]"},
                {"step_type": "observation", "content": "Branch evaluation — A: [score/pros/cons], B: [score/pros/cons], C: [score/pros/cons]"},
                {"step_type": "thought",     "content": "Selected path: [chosen branch] because [reason]"}
            ]
        },
        {
            "id": "react",
            "name": "ReAct",
            "description": "Interleave Thought -> Action -> Observation cycles",
            "steps": [
                {"step_type": "thought",     "content": "Goal: [define what needs to be determined or accomplished]"},
                {"step_type": "action",      "content": "Action 1: [first action to take]"},
                {"step_type": "observation", "content": "Observation 1: [result / what was learned from action 1]"},
                {"step_type": "thought",     "content": "Revised plan based on observation 1: [updated understanding]"},
                {"step_type": "action",      "content": "Action 2: [next action based on new understanding]"},
                {"step_type": "observation", "content": "Observation 2: [result / what was learned from action 2]"},
                {"step_type": "thought",     "content": "Final answer: [conclusion derived from all observations]"}
            ]
        },
        {
            "id": "self_consistency",
            "name": "Self-Consistency",
            "description": "Generate multiple independent reasoning paths and vote on the answer",
            "steps": [
                {"step_type": "thought",     "content": "Path 1 reasoning: [first independent chain of thought]"},
                {"step_type": "observation", "content": "Path 1 answer: [answer reached via path 1]"},
                {"step_type": "thought",     "content": "Path 2 reasoning: [second independent chain of thought]"},
                {"step_type": "observation", "content": "Path 2 answer: [answer reached via path 2]"},
                {"step_type": "thought",     "content": "Path 3 reasoning: [third independent chain of thought]"},
                {"step_type": "observation", "content": "Path 3 answer: [answer reached via path 3]"},
                {"step_type": "observation", "content": "Majority vote: [most common answer across paths] — selected as final answer"}
            ]
        }
    ])
}

// ── checkpoint snapshot ───────────────────────────────────────────────────────

#[derive(Serialize, Deserialize)]
struct CheckpointSnapshot {
    steps: Vec<serde_json::Value>,
    assumptions: Vec<serde_json::Value>,
    contradictions: Vec<serde_json::Value>,
}

fn load_steps(conn: &Connection, session_id: &str) -> Vec<serde_json::Value> {
    let mut stmt = match conn.prepare(
        "SELECT step_id, parent_step_id, step_type, content, confidence,
                evaluation_score, evaluation_note, created
         FROM reasoning_steps WHERE session_id=?1 ORDER BY created ASC",
    ) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("load_steps prepare failed: {e}");
            return vec![];
        }
    };
    stmt.query_map(rusqlite::params![session_id], |row| {
        Ok(serde_json::json!({
            "step_id":          row.get::<_, Option<String>>(0)?,
            "parent_step_id":   row.get::<_, Option<String>>(1)?,
            "step_type":        row.get::<_, Option<String>>(2)?,
            "content":          row.get::<_, Option<String>>(3)?,
            "confidence":       row.get::<_, Option<f64>>(4)?,
            "evaluation_score": row.get::<_, Option<f64>>(5)?,
            "evaluation_note":  row.get::<_, Option<String>>(6)?,
            "created":          row.get::<_, Option<f64>>(7)?,
        }))
    })
    .map(|rows| rows.filter_map(|r| r.ok()).collect())
    .unwrap_or_default()
}

fn load_assumptions(conn: &Connection, session_id: &str) -> Vec<serde_json::Value> {
    let mut stmt = match conn.prepare(
        "SELECT assumption_id, content, confidence, evidence, created
         FROM assumptions WHERE session_id=?1 ORDER BY created ASC",
    ) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("load_assumptions prepare failed: {e}");
            return vec![];
        }
    };
    stmt.query_map(rusqlite::params![session_id], |row| {
        Ok(serde_json::json!({
            "assumption_id": row.get::<_, Option<String>>(0)?,
            "content":       row.get::<_, Option<String>>(1)?,
            "confidence":    row.get::<_, Option<f64>>(2)?,
            "evidence":      row.get::<_, Option<String>>(3)?,
            "created":       row.get::<_, Option<f64>>(4)?,
        }))
    })
    .map(|rows| rows.filter_map(|r| r.ok()).collect())
    .unwrap_or_default()
}

fn load_contradictions(conn: &Connection, session_id: &str) -> Vec<serde_json::Value> {
    let mut stmt = match conn.prepare(
        "SELECT id, statement_a, statement_b, explanation, resolved, created
         FROM contradictions WHERE session_id=?1 ORDER BY created ASC",
    ) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("load_contradictions prepare failed: {e}");
            return vec![];
        }
    };
    stmt.query_map(rusqlite::params![session_id], |row| {
        Ok(serde_json::json!({
            "id":          row.get::<_, Option<i64>>(0)?,
            "statement_a": row.get::<_, Option<String>>(1)?,
            "statement_b": row.get::<_, Option<String>>(2)?,
            "explanation": row.get::<_, Option<String>>(3)?,
            "resolved":    row.get::<_, Option<i64>>(4)?,
            "created":     row.get::<_, Option<f64>>(5)?,
        }))
    })
    .map(|rows| rows.filter_map(|r| r.ok()).collect())
    .unwrap_or_default()
}

// ── parameter structs ─────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct AddStepParams {
    #[schemars(description = "Session identifier (auto-created if it does not exist yet)")]
    session_id: String,
    #[schemars(description = "Content of the reasoning step")]
    content: String,
    #[schemars(description = "Step type: thought | action | observation")]
    step_type: String,
    #[schemars(description = "Optional parent step ID for tree branching")]
    parent_step_id: Option<String>,
    #[schemars(description = "Confidence in this step, 0.0-1.0")]
    confidence: Option<f64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct GetChainParams {
    #[schemars(description = "Session identifier")]
    session_id: String,
    #[schemars(description = "Maximum number of steps to return (default 100)")]
    limit: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct EvaluateStepParams {
    #[schemars(description = "Session identifier")]
    session_id: String,
    #[schemars(description = "ID of the step to evaluate")]
    step_id: String,
    #[schemars(description = "Evaluation score 0-10")]
    score: f64,
    #[schemars(description = "Optional evaluation note")]
    note: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct AddAssumptionParams {
    #[schemars(description = "Session identifier")]
    session_id: String,
    #[schemars(description = "Assumption content")]
    content: String,
    #[schemars(description = "Confidence 0.0-1.0")]
    confidence: Option<f64>,
    #[schemars(description = "Supporting evidence or rationale")]
    evidence: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SessionParams {
    #[schemars(description = "Session identifier")]
    session_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct CheckContradictionParams {
    #[schemars(description = "Session identifier")]
    session_id: String,
    #[schemars(description = "Statement to check against existing steps and assumptions")]
    statement: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ResolveContradictionParams {
    #[schemars(description = "Session identifier")]
    session_id: String,
    #[schemars(description = "ID of the contradiction to mark as resolved")]
    contradiction_id: i64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ApplyPatternParams {
    #[schemars(description = "Session identifier")]
    session_id: String,
    #[schemars(description = "Pattern ID: cot | tot | react | self_consistency")]
    pattern_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct CheckpointParams {
    #[schemars(description = "Session identifier")]
    session_id: String,
    #[schemars(description = "Checkpoint name")]
    name: String,
}

// ── server ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct ThinkServer {
    db: Arc<Mutex<Connection>>,
}

impl ThinkServer {
    fn new() -> anyhow::Result<Self> {
        let dir = data_dir();
        std::fs::create_dir_all(&dir)?;
        let conn = Connection::open(dir.join("think.db"))?;
        init_schema(&conn)?;
        tracing::info!("ThinkServer initialized at {}", dir.display());
        Ok(Self {
            db: Arc::new(Mutex::new(conn)),
        })
    }
}

#[tool_router(server_handler)]
impl ThinkServer {
    #[tool(description = "Add a reasoning step (thought/action/observation) to a session. Auto-creates the session if it does not exist. Returns the new step_id.")]
    async fn think_add_step(&self, Parameters(p): Parameters<AddStepParams>) -> String {
        let db = self.db.lock().unwrap();
        ensure_session(&db, &p.session_id);
        let step_id = generate_id("step");
        let now = now_secs();
        match db.execute(
            "INSERT INTO reasoning_steps
             (step_id, session_id, parent_step_id, step_type, content, confidence, created)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                step_id,
                p.session_id,
                p.parent_step_id,
                p.step_type,
                p.content,
                p.confidence,
                now
            ],
        ) {
            Ok(_) => {
                tracing::debug!("Added step {step_id} to session {}", p.session_id);
                step_id
            }
            Err(e) => {
                tracing::error!("think_add_step: {e}");
                format!("Error: {e}")
            }
        }
    }

    #[tool(description = "Get the ordered reasoning chain for a session as a JSON array of steps.")]
    async fn think_get_chain(&self, Parameters(p): Parameters<GetChainParams>) -> String {
        let db = self.db.lock().unwrap();
        let limit = p.limit.unwrap_or(100);
        let mut stmt = match db.prepare(
            "SELECT step_id, parent_step_id, step_type, content, confidence,
                    evaluation_score, evaluation_note, created
             FROM reasoning_steps
             WHERE session_id=?1
             ORDER BY created ASC
             LIMIT ?2",
        ) {
            Ok(s) => s,
            Err(e) => return format!("Error: {e}"),
        };
        let result = match stmt.query_map(rusqlite::params![p.session_id, limit], |row| {
            Ok(serde_json::json!({
                "step_id":          row.get::<_, Option<String>>(0)?,
                "parent_step_id":   row.get::<_, Option<String>>(1)?,
                "step_type":        row.get::<_, Option<String>>(2)?,
                "content":          row.get::<_, Option<String>>(3)?,
                "confidence":       row.get::<_, Option<f64>>(4)?,
                "evaluation_score": row.get::<_, Option<f64>>(5)?,
                "evaluation_note":  row.get::<_, Option<String>>(6)?,
                "created":          row.get::<_, Option<f64>>(7)?,
            }))
        }) {
            Ok(rows) => {
                let items: Vec<serde_json::Value> = rows.filter_map(|r| r.ok()).collect();
                serde_json::to_string_pretty(&items).unwrap_or_else(|e| format!("Error: {e}"))
            }
            Err(e) => format!("Error: {e}"),
        };
        result
    }

    #[tool(description = "Self-evaluate a reasoning step with a score (0-10) and optional note.")]
    async fn think_evaluate_step(
        &self,
        Parameters(p): Parameters<EvaluateStepParams>,
    ) -> String {
        let db = self.db.lock().unwrap();
        match db.execute(
            "UPDATE reasoning_steps
             SET evaluation_score=?1, evaluation_note=?2
             WHERE step_id=?3 AND session_id=?4",
            rusqlite::params![p.score, p.note, p.step_id, p.session_id],
        ) {
            Ok(n) if n > 0 => {
                format!("Step {} evaluated: score={:.1}", p.step_id, p.score)
            }
            Ok(_) => format!(
                "Error: step {} not found in session {}",
                p.step_id, p.session_id
            ),
            Err(e) => format!("Error: {e}"),
        }
    }

    #[tool(description = "Add an assumption to a session. Returns the assumption_id.")]
    async fn think_add_assumption(
        &self,
        Parameters(p): Parameters<AddAssumptionParams>,
    ) -> String {
        let db = self.db.lock().unwrap();
        ensure_session(&db, &p.session_id);
        let assumption_id = generate_id("assumption");
        let now = now_secs();
        match db.execute(
            "INSERT INTO assumptions
             (assumption_id, session_id, content, confidence, evidence, created)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                assumption_id,
                p.session_id,
                p.content,
                p.confidence,
                p.evidence,
                now
            ],
        ) {
            Ok(_) => assumption_id,
            Err(e) => format!("Error: {e}"),
        }
    }

    #[tool(description = "List all assumptions for a session as a JSON array.")]
    async fn think_list_assumptions(
        &self,
        Parameters(p): Parameters<SessionParams>,
    ) -> String {
        let db = self.db.lock().unwrap();
        let mut stmt = match db.prepare(
            "SELECT assumption_id, content, confidence, evidence, created
             FROM assumptions WHERE session_id=?1 ORDER BY created ASC",
        ) {
            Ok(s) => s,
            Err(e) => return format!("Error: {e}"),
        };
        let result = match stmt.query_map(rusqlite::params![p.session_id], |row| {
            Ok(serde_json::json!({
                "assumption_id": row.get::<_, Option<String>>(0)?,
                "content":       row.get::<_, Option<String>>(1)?,
                "confidence":    row.get::<_, Option<f64>>(2)?,
                "evidence":      row.get::<_, Option<String>>(3)?,
                "created":       row.get::<_, Option<f64>>(4)?,
            }))
        }) {
            Ok(rows) => {
                let items: Vec<serde_json::Value> = rows.filter_map(|r| r.ok()).collect();
                serde_json::to_string_pretty(&items).unwrap_or_else(|e| format!("Error: {e}"))
            }
            Err(e) => format!("Error: {e}"),
        };
        result
    }

    #[tool(description = "Check a statement for contradictions against existing steps and assumptions in the session using keyword-based negation detection. Returns 'No contradictions found.' or a JSON array of detected contradictions (which are also persisted in the DB).")]
    async fn think_check_contradiction(
        &self,
        Parameters(p): Parameters<CheckContradictionParams>,
    ) -> String {
        let db = self.db.lock().unwrap();

        // Collect step contents
        let mut candidates: Vec<(String, String)> = Vec::new();
        if let Ok(mut stmt) = db.prepare(
            "SELECT step_id, content FROM reasoning_steps WHERE session_id=?1",
        ) {
            if let Ok(rows) = stmt.query_map(rusqlite::params![p.session_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            }) {
                candidates.extend(rows.flatten());
            }
        }
        // Collect assumption contents
        if let Ok(mut stmt) = db.prepare(
            "SELECT assumption_id, content FROM assumptions WHERE session_id=?1",
        ) {
            if let Ok(rows) = stmt.query_map(rusqlite::params![p.session_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            }) {
                candidates.extend(rows.flatten());
            }
        }

        let now = now_secs();
        let mut found: Vec<serde_json::Value> = Vec::new();

        for (id, content) in &candidates {
            if let Some(explanation) = detect_contradiction(&p.statement, content) {
                match db.execute(
                    "INSERT INTO contradictions
                     (session_id, statement_a, statement_b, explanation, created)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    rusqlite::params![p.session_id, p.statement, content, explanation, now],
                ) {
                    Ok(_) => {
                        let contradiction_id = db.last_insert_rowid();
                        found.push(serde_json::json!({
                            "contradiction_id":    contradiction_id,
                            "conflicting_id":      id,
                            "conflicting_content": content,
                            "explanation":         explanation,
                        }));
                    }
                    Err(e) => tracing::warn!("Failed to store contradiction: {e}"),
                }
            }
        }

        if found.is_empty() {
            "No contradictions found.".to_string()
        } else {
            serde_json::to_string_pretty(&found).unwrap_or_else(|e| format!("Error: {e}"))
        }
    }

    #[tool(description = "Mark a contradiction as resolved by its numeric ID.")]
    async fn think_resolve_contradiction(
        &self,
        Parameters(p): Parameters<ResolveContradictionParams>,
    ) -> String {
        let db = self.db.lock().unwrap();
        match db.execute(
            "UPDATE contradictions SET resolved=1 WHERE id=?1 AND session_id=?2",
            rusqlite::params![p.contradiction_id, p.session_id],
        ) {
            Ok(n) if n > 0 => format!("Contradiction {} resolved.", p.contradiction_id),
            Ok(_) => format!(
                "Error: contradiction {} not found in session {}.",
                p.contradiction_id, p.session_id
            ),
            Err(e) => format!("Error: {e}"),
        }
    }

    #[tool(description = "List built-in reasoning patterns: cot (Chain of Thought), tot (Tree of Thought), react (ReAct), self_consistency (Self-Consistency). Returns a JSON array.")]
    async fn think_list_patterns(&self) -> String {
        serde_json::to_string_pretty(&builtin_patterns())
            .unwrap_or_else(|e| format!("Error: {e}"))
    }

    #[tool(description = "Scaffold template reasoning steps for a named pattern (cot|tot|react|self_consistency) into a session. Returns the IDs of the created steps.")]
    async fn think_apply_pattern(&self, Parameters(p): Parameters<ApplyPatternParams>) -> String {
        let patterns = builtin_patterns();
        let pattern = match patterns
            .as_array()
            .and_then(|arr| arr.iter().find(|pat| pat["id"] == p.pattern_id))
            .cloned()
        {
            Some(pat) => pat,
            None => {
                return format!(
                    "Error: unknown pattern '{}'. Available: cot, tot, react, self_consistency",
                    p.pattern_id
                )
            }
        };

        let steps = match pattern["steps"].as_array() {
            Some(s) => s.clone(),
            None => return "Error: pattern has no steps".to_string(),
        };

        let db = self.db.lock().unwrap();
        ensure_session(&db, &p.session_id);

        let mut created_ids: Vec<String> = Vec::new();
        for step in &steps {
            let step_type = step["step_type"].as_str().unwrap_or("thought");
            let content = step["content"].as_str().unwrap_or("");
            let step_id = generate_id("step");
            let now = now_secs();
            match db.execute(
                "INSERT INTO reasoning_steps (step_id, session_id, step_type, content, created)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![step_id, p.session_id, step_type, content, now],
            ) {
                Ok(_) => created_ids.push(step_id),
                Err(e) => return format!("Error inserting template step: {e}"),
            }
        }

        format!(
            "Applied pattern '{}' to session {}: created {} template steps: {}",
            p.pattern_id,
            p.session_id,
            created_ids.len(),
            created_ids.join(", ")
        )
    }

    #[tool(description = "Save a named checkpoint of the current session state (all steps, assumptions, and contradictions) as a JSON snapshot in the database.")]
    async fn think_save_checkpoint(&self, Parameters(p): Parameters<CheckpointParams>) -> String {
        let db = self.db.lock().unwrap();
        let snapshot = CheckpointSnapshot {
            steps: load_steps(&db, &p.session_id),
            assumptions: load_assumptions(&db, &p.session_id),
            contradictions: load_contradictions(&db, &p.session_id),
        };
        let n_steps = snapshot.steps.len();
        let n_assumptions = snapshot.assumptions.len();
        let n_contradictions = snapshot.contradictions.len();

        let json = match serde_json::to_string(&snapshot) {
            Ok(j) => j,
            Err(e) => return format!("Error serializing snapshot: {e}"),
        };
        let now = now_secs();
        match db.execute(
            "INSERT OR REPLACE INTO checkpoints (name, session_id, snapshot, created)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![p.name, p.session_id, json, now],
        ) {
            Ok(_) => format!(
                "Checkpoint '{}' saved for session {} ({n_steps} steps, {n_assumptions} assumptions, {n_contradictions} contradictions).",
                p.name, p.session_id
            ),
            Err(e) => format!("Error saving checkpoint: {e}"),
        }
    }

    #[tool(description = "Restore a named checkpoint, replacing the current session's steps/assumptions/contradictions with the saved snapshot.")]
    async fn think_restore_checkpoint(
        &self,
        Parameters(p): Parameters<CheckpointParams>,
    ) -> String {
        let db = self.db.lock().unwrap();

        let json: Option<String> = db
            .query_row(
                "SELECT snapshot FROM checkpoints WHERE name=?1 AND session_id=?2",
                rusqlite::params![p.name, p.session_id],
                |row| row.get(0),
            )
            .ok();

        let json = match json {
            Some(j) => j,
            None => {
                return format!(
                    "Error: checkpoint '{}' not found for session {}.",
                    p.name, p.session_id
                )
            }
        };

        let snapshot: CheckpointSnapshot = match serde_json::from_str(&json) {
            Ok(s) => s,
            Err(e) => return format!("Error parsing checkpoint snapshot: {e}"),
        };

        // Wipe current state for this session
        for sql in [
            "DELETE FROM reasoning_steps WHERE session_id=?1",
            "DELETE FROM assumptions WHERE session_id=?1",
            "DELETE FROM contradictions WHERE session_id=?1",
        ] {
            if let Err(e) = db.execute(sql, rusqlite::params![p.session_id]) {
                return format!("Error clearing session data: {e}");
            }
        }

        // Restore steps
        for step in &snapshot.steps {
            let step_id = step["step_id"].as_str().unwrap_or("").to_string();
            let step_type = step["step_type"].as_str().unwrap_or("thought");
            let content = step["content"].as_str().unwrap_or("");
            let parent_step_id = step["parent_step_id"].as_str();
            let confidence = step["confidence"].as_f64();
            let evaluation_score = step["evaluation_score"].as_f64();
            let evaluation_note = step["evaluation_note"].as_str();
            let created = step["created"].as_f64().unwrap_or_else(now_secs);
            if let Err(e) = db.execute(
                "INSERT OR IGNORE INTO reasoning_steps
                 (step_id, session_id, parent_step_id, step_type, content,
                  confidence, evaluation_score, evaluation_note, created)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                rusqlite::params![
                    step_id,
                    p.session_id,
                    parent_step_id,
                    step_type,
                    content,
                    confidence,
                    evaluation_score,
                    evaluation_note,
                    created
                ],
            ) {
                tracing::warn!("restore_checkpoint step insert error: {e}");
            }
        }

        // Restore assumptions
        for assumption in &snapshot.assumptions {
            let assumption_id = assumption["assumption_id"].as_str().unwrap_or("").to_string();
            let content = assumption["content"].as_str().unwrap_or("");
            let confidence = assumption["confidence"].as_f64();
            let evidence = assumption["evidence"].as_str();
            let created = assumption["created"].as_f64().unwrap_or_else(now_secs);
            if let Err(e) = db.execute(
                "INSERT OR IGNORE INTO assumptions
                 (assumption_id, session_id, content, confidence, evidence, created)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    assumption_id,
                    p.session_id,
                    content,
                    confidence,
                    evidence,
                    created
                ],
            ) {
                tracing::warn!("restore_checkpoint assumption insert error: {e}");
            }
        }

        // Restore contradictions (new auto-increment IDs are assigned on re-insert)
        for contradiction in &snapshot.contradictions {
            let stmt_a = contradiction["statement_a"].as_str().unwrap_or("");
            let stmt_b = contradiction["statement_b"].as_str().unwrap_or("");
            let explanation = contradiction["explanation"].as_str();
            let resolved = contradiction["resolved"].as_i64().unwrap_or(0);
            let created = contradiction["created"].as_f64().unwrap_or_else(now_secs);
            if let Err(e) = db.execute(
                "INSERT INTO contradictions
                 (session_id, statement_a, statement_b, explanation, resolved, created)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![p.session_id, stmt_a, stmt_b, explanation, resolved, created],
            ) {
                tracing::warn!("restore_checkpoint contradiction insert error: {e}");
            }
        }

        format!(
            "Restored checkpoint '{}' for session {}: {} steps, {} assumptions, {} contradictions.",
            p.name,
            p.session_id,
            snapshot.steps.len(),
            snapshot.assumptions.len(),
            snapshot.contradictions.len()
        )
    }

    #[tool(description = "Get a summary of a session: total steps, average confidence, assumption count, total and unresolved contradiction counts.")]
    async fn think_session_summary(&self, Parameters(p): Parameters<SessionParams>) -> String {
        let db = self.db.lock().unwrap();

        let total_steps: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM reasoning_steps WHERE session_id=?1",
                rusqlite::params![p.session_id],
                |row| row.get(0),
            )
            .unwrap_or(0);

        let avg_confidence: Option<f64> = db
            .query_row(
                "SELECT AVG(confidence) FROM reasoning_steps
                 WHERE session_id=?1 AND confidence IS NOT NULL",
                rusqlite::params![p.session_id],
                |row| row.get::<_, Option<f64>>(0),
            )
            .unwrap_or(None);

        let total_assumptions: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM assumptions WHERE session_id=?1",
                rusqlite::params![p.session_id],
                |row| row.get(0),
            )
            .unwrap_or(0);

        let total_contradictions: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM contradictions WHERE session_id=?1",
                rusqlite::params![p.session_id],
                |row| row.get(0),
            )
            .unwrap_or(0);

        let unresolved_contradictions: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM contradictions WHERE session_id=?1 AND resolved=0",
                rusqlite::params![p.session_id],
                |row| row.get(0),
            )
            .unwrap_or(0);

        let summary = serde_json::json!({
            "session_id":               p.session_id,
            "total_steps":              total_steps,
            "avg_confidence":           avg_confidence,
            "total_assumptions":        total_assumptions,
            "total_contradictions":     total_contradictions,
            "unresolved_contradictions": unresolved_contradictions,
        });

        serde_json::to_string_pretty(&summary).unwrap_or_else(|e| format!("Error: {e}"))
    }
}

// ── entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let server = ThinkServer::new()?;
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
