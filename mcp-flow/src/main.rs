use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use rmcp::{ServiceExt, handler::server::wrapper::Parameters, schemars, tool, tool_router, transport::stdio};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn now_ts() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

/// Sanitize a session_id to [a-zA-Z0-9_-], max 64 chars.
fn sanitize_id(raw: &str) -> Option<String> {
    let s: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .take(64)
        .collect();
    if s.is_empty() { None } else { Some(s) }
}

fn unix_now() -> f64 {
    now_ts()
}

// ---------------------------------------------------------------------------
// DB helpers
// ---------------------------------------------------------------------------

fn init_db(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS sessions (
            session_id   TEXT PRIMARY KEY,
            goal         TEXT,
            created      REAL NOT NULL,
            last_active  REAL NOT NULL,
            context_usage INTEGER NOT NULL DEFAULT 0,
            context_limit INTEGER NOT NULL DEFAULT 200000
        );

        CREATE TABLE IF NOT EXISTS plan_steps (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id   TEXT NOT NULL,
            step_name    TEXT NOT NULL,
            description  TEXT NOT NULL DEFAULT '',
            dependencies TEXT NOT NULL DEFAULT '',
            status       TEXT NOT NULL DEFAULT 'pending',
            result       TEXT,
            error        TEXT,
            created      REAL NOT NULL,
            updated      REAL NOT NULL,
            UNIQUE(session_id, step_name)
        );

        CREATE TABLE IF NOT EXISTS memories (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id   TEXT NOT NULL,
            key          TEXT NOT NULL,
            value        TEXT NOT NULL,
            ttl          REAL,
            created      REAL NOT NULL,
            UNIQUE(session_id, key)
        );

        CREATE TABLE IF NOT EXISTS long_term_memory (
            id              INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id      TEXT NOT NULL,
            summary         TEXT NOT NULL,
            compressed_steps TEXT NOT NULL DEFAULT '',
            from_ts         REAL NOT NULL,
            until_ts        REAL NOT NULL,
            created         REAL NOT NULL
        );

        CREATE TABLE IF NOT EXISTS checkpoints (
            name        TEXT NOT NULL,
            session_id  TEXT NOT NULL,
            snapshot    TEXT NOT NULL,
            created     REAL NOT NULL,
            PRIMARY KEY (name, session_id)
        );

        CREATE TABLE IF NOT EXISTS execution_log (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id  TEXT NOT NULL,
            action      TEXT NOT NULL,
            details     TEXT NOT NULL DEFAULT '',
            timestamp   REAL NOT NULL
        );",
    )?;
    Ok(())
}

fn log_action(conn: &Connection, session_id: &str, action: &str, details: &str) {
    let _ = conn.execute(
        "INSERT INTO execution_log (session_id, action, details, timestamp) VALUES (?1, ?2, ?3, ?4)",
        params![session_id, action, details, unix_now()],
    );
}

fn touch_session(conn: &Connection, session_id: &str) {
    let _ = conn.execute(
        "UPDATE sessions SET last_active = ?1 WHERE session_id = ?2",
        params![unix_now(), session_id],
    );
}

fn session_exists(conn: &Connection, session_id: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM sessions WHERE session_id = ?1",
        params![session_id],
        |_| Ok(()),
    )
    .is_ok()
}

// ---------------------------------------------------------------------------
// Snapshot helpers for checkpoints
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
struct StepSnapshot {
    step_name:    String,
    description:  String,
    dependencies: String,
    status:       String,
    result:       Option<String>,
    error:        Option<String>,
    created:      f64,
    updated:      f64,
}

#[derive(Serialize, Deserialize)]
struct MemorySnapshot {
    key:     String,
    value:   String,
    ttl:     Option<f64>,
    created: f64,
}

#[derive(Serialize, Deserialize)]
struct SessionSnapshot {
    goal:          Option<String>,
    context_usage: i64,
    context_limit: i64,
    steps:         Vec<StepSnapshot>,
    memories:      Vec<MemorySnapshot>,
}

fn take_snapshot(conn: &Connection, session_id: &str) -> rusqlite::Result<String> {
    let (goal, context_usage, context_limit): (Option<String>, i64, i64) = conn.query_row(
        "SELECT goal, context_usage, context_limit FROM sessions WHERE session_id = ?1",
        params![session_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;

    let mut stmt = conn.prepare(
        "SELECT step_name, description, dependencies, status, result, error, created, updated
         FROM plan_steps WHERE session_id = ?1 ORDER BY id",
    )?;
    let steps: Vec<StepSnapshot> = stmt
        .query_map(params![session_id], |row| {
            Ok(StepSnapshot {
                step_name:    row.get(0)?,
                description:  row.get(1)?,
                dependencies: row.get(2)?,
                status:       row.get(3)?,
                result:       row.get(4)?,
                error:        row.get(5)?,
                created:      row.get(6)?,
                updated:      row.get(7)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    let mut stmt2 = conn.prepare(
        "SELECT key, value, ttl, created FROM memories WHERE session_id = ?1 ORDER BY id",
    )?;
    let memories: Vec<MemorySnapshot> = stmt2
        .query_map(params![session_id], |row| {
            Ok(MemorySnapshot {
                key:     row.get(0)?,
                value:   row.get(1)?,
                ttl:     row.get(2)?,
                created: row.get(3)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    let snap = SessionSnapshot {
        goal,
        context_usage,
        context_limit,
        steps,
        memories,
    };
    Ok(serde_json::to_string(&snap).unwrap_or_default())
}

fn restore_snapshot(conn: &Connection, session_id: &str, snapshot_json: &str) -> anyhow::Result<()> {
    let snap: SessionSnapshot = serde_json::from_str(snapshot_json)
        .context("Failed to parse snapshot JSON")?;

    conn.execute(
        "UPDATE sessions SET goal = ?1, context_usage = ?2, context_limit = ?3, last_active = ?4 WHERE session_id = ?5",
        params![snap.goal, snap.context_usage, snap.context_limit, unix_now(), session_id],
    )?;

    conn.execute("DELETE FROM plan_steps WHERE session_id = ?1", params![session_id])?;
    conn.execute("DELETE FROM memories WHERE session_id = ?1", params![session_id])?;

    for step in &snap.steps {
        conn.execute(
            "INSERT INTO plan_steps (session_id, step_name, description, dependencies, status, result, error, created, updated)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                session_id, step.step_name, step.description, step.dependencies,
                step.status, step.result, step.error, step.created, step.updated
            ],
        )?;
    }

    for mem in &snap.memories {
        conn.execute(
            "INSERT OR REPLACE INTO memories (session_id, key, value, ttl, created) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![session_id, mem.key, mem.value, mem.ttl, mem.created],
        )?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Parameter structs
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct CreateSessionParams {
    #[schemars(description = "Unique identifier for this workflow session")]
    session_id: String,
    #[schemars(description = "Optional high-level goal or objective for this session")]
    goal: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct AddStepParams {
    #[schemars(description = "Session identifier")]
    session_id: String,
    #[schemars(description = "Unique name for this step within the session")]
    step_name: String,
    #[schemars(description = "Human-readable description of what this step does")]
    description: String,
    #[schemars(description = "Comma-separated list of step names that must complete before this step can run")]
    dependencies: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SessionOnlyParams {
    #[schemars(description = "Session identifier")]
    session_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct UpdateStepStatusParams {
    #[schemars(description = "Session identifier")]
    session_id: String,
    #[schemars(description = "Name of the step to update")]
    step_name: String,
    #[schemars(description = "New status: pending | in_progress | done | failed | blocked")]
    status: String,
    #[schemars(description = "Optional result output from the step")]
    result: Option<String>,
    #[schemars(description = "Optional error message if the step failed")]
    error: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct DeleteStepParams {
    #[schemars(description = "Session identifier")]
    session_id: String,
    #[schemars(description = "Name of the step to delete")]
    step_name: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SetMemoryParams {
    #[schemars(description = "Session identifier")]
    session_id: String,
    #[schemars(description = "Memory key")]
    key: String,
    #[schemars(description = "Value to store")]
    value: String,
    #[schemars(description = "Time-to-live in seconds from now (null = no expiry)")]
    ttl: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct GetMemoryParams {
    #[schemars(description = "Session identifier")]
    session_id: String,
    #[schemars(description = "Memory key to retrieve")]
    key: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct QueryMemoryParams {
    #[schemars(description = "Session identifier")]
    session_id: String,
    #[schemars(description = "Keyword or phrase to search for in memory values")]
    query: String,
    #[schemars(description = "Maximum number of results to return (default 5)")]
    top_k: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct CheckpointParams {
    #[schemars(description = "Session identifier")]
    session_id: String,
    #[schemars(description = "Name for this checkpoint")]
    name: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct UpdateContextParams {
    #[schemars(description = "Session identifier")]
    session_id: String,
    #[schemars(description = "Cumulative number of tokens used so far in this session")]
    tokens_used: i64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct CompactContextParams {
    #[schemars(description = "Session identifier")]
    session_id: String,
    #[schemars(description = "Number of most-recent steps to retain (default 10)")]
    keep_last: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct QueryLtmParams {
    #[schemars(description = "Session identifier")]
    session_id: String,
    #[schemars(description = "Optional keyword to filter long-term memory summaries")]
    query: Option<String>,
    #[schemars(description = "Maximum number of results to return (default 10)")]
    top_k: Option<i64>,
}

// ---------------------------------------------------------------------------
// Server struct
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct FlowServer {
    db: Arc<Mutex<Connection>>,
}

impl FlowServer {
    fn new(data_dir: PathBuf) -> anyhow::Result<Self> {
        std::fs::create_dir_all(&data_dir)
            .with_context(|| format!("create data dir {data_dir:?}"))?;
        let db_path = data_dir.join("flow.db");
        let conn = Connection::open(&db_path)
            .with_context(|| format!("open db {db_path:?}"))?;
        init_db(&conn).context("init db schema")?;
        tracing::info!(path = %db_path.display(), "database opened");
        Ok(Self { db: Arc::new(Mutex::new(conn)) })
    }

    fn with_db<F, R>(&self, f: F) -> Result<R, String>
    where
        F: FnOnce(&Connection) -> Result<R, String>,
    {
        let conn = self.db.lock().map_err(|e| format!("Error: db lock poisoned: {e}"))?;
        f(&conn)
    }
}

// ---------------------------------------------------------------------------
// Tool implementations
// ---------------------------------------------------------------------------

#[tool_router(server_handler)]
impl FlowServer {
    #[tool(description = "Create a new workflow session with an optional goal. Returns confirmation with session_id.")]
    fn flow_create_session(
        &self,
        Parameters(CreateSessionParams { session_id, goal }): Parameters<CreateSessionParams>,
    ) -> String {
        let sid = match sanitize_id(&session_id) {
            Some(s) => s,
            None => return "Error: session_id must contain alphanumeric, underscore, or dash characters".to_string(),
        };

        self.with_db(|conn| {
            let ts = unix_now();
            let existing: bool = session_exists(conn, &sid);
            if existing {
                return Err(format!("Error: session '{}' already exists", sid));
            }
            conn.execute(
                "INSERT INTO sessions (session_id, goal, created, last_active, context_usage, context_limit)
                 VALUES (?1, ?2, ?3, ?4, 0, 200000)",
                params![sid, goal, ts, ts],
            )
            .map_err(|e| format!("Error: {e}"))?;
            log_action(conn, &sid, "create_session", goal.as_deref().unwrap_or(""));
            Ok(format!(
                "Session '{}' created. Goal: {}",
                sid,
                goal.as_deref().unwrap_or("(none)")
            ))
        })
        .unwrap_or_else(|e| e)
    }

    #[tool(description = "Add a step to the workflow plan. Dependencies is a comma-separated list of step names that must be 'done' before this step can run.")]
    fn flow_add_step(
        &self,
        Parameters(AddStepParams { session_id, step_name, description, dependencies }): Parameters<AddStepParams>,
    ) -> String {
        let sid = match sanitize_id(&session_id) {
            Some(s) => s,
            None => return "Error: invalid session_id".to_string(),
        };

        self.with_db(|conn| {
            if !session_exists(conn, &sid) {
                return Err(format!("Error: session '{}' not found", sid));
            }
            let deps = dependencies.as_deref().unwrap_or("").trim().to_string();
            let ts = unix_now();
            conn.execute(
                "INSERT INTO plan_steps (session_id, step_name, description, dependencies, status, created, updated)
                 VALUES (?1, ?2, ?3, ?4, 'pending', ?5, ?6)",
                params![sid, step_name, description, deps, ts, ts],
            )
            .map_err(|e| format!("Error: step already exists or db error: {e}"))?;
            touch_session(conn, &sid);
            log_action(conn, &sid, "add_step", &format!("step={step_name} deps={deps}"));
            Ok(format!(
                "Step '{}' added to session '{}'. Dependencies: [{}]",
                step_name, sid, deps
            ))
        })
        .unwrap_or_else(|e| e)
    }

    #[tool(description = "Get the next actionable step whose dependencies are all 'done'. Returns JSON with the step details, or status 'complete' / 'no_pending'.")]
    fn flow_get_next_action(
        &self,
        Parameters(SessionOnlyParams { session_id }): Parameters<SessionOnlyParams>,
    ) -> String {
        let sid = match sanitize_id(&session_id) {
            Some(s) => s,
            None => return "Error: invalid session_id".to_string(),
        };

        self.with_db(|conn| {
            if !session_exists(conn, &sid) {
                return Err(format!("Error: session '{}' not found", sid));
            }

            // Check if any steps exist that are not done
            let total_pending: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM plan_steps WHERE session_id = ?1 AND status NOT IN ('done', 'failed', 'blocked')",
                    params![sid],
                    |r| r.get(0),
                )
                .unwrap_or(0);

            if total_pending == 0 {
                // Check if there's anything at all
                let total: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM plan_steps WHERE session_id = ?1",
                        params![sid],
                        |r| r.get(0),
                    )
                    .unwrap_or(0);
                let status = if total == 0 { "no_pending" } else { "complete" };
                return Ok(serde_json::json!({ "status": status }).to_string());
            }

            // Fetch all pending steps and their dependencies
            let mut stmt = conn.prepare(
                "SELECT step_name, description, dependencies FROM plan_steps
                 WHERE session_id = ?1 AND status = 'pending'
                 ORDER BY id"
            ).map_err(|e| format!("Error: {e}"))?;

            struct PendingStep {
                name: String,
                description: String,
                dependencies: String,
            }

            let pending: Vec<PendingStep> = stmt
                .query_map(params![sid], |row| {
                    Ok(PendingStep {
                        name: row.get(0)?,
                        description: row.get(1)?,
                        dependencies: row.get(2)?,
                    })
                })
                .map_err(|e| format!("Error: {e}"))?
                .filter_map(|r| r.ok())
                .collect();

            // Find first step where all deps are done
            for step in &pending {
                let deps_satisfied = if step.dependencies.trim().is_empty() {
                    true
                } else {
                    let dep_names: Vec<&str> = step.dependencies
                        .split(',')
                        .map(|s| s.trim())
                        .filter(|s| !s.is_empty())
                        .collect();

                    dep_names.iter().all(|dep| {
                        conn.query_row(
                            "SELECT status FROM plan_steps WHERE session_id = ?1 AND step_name = ?2",
                            params![sid, dep],
                            |r| r.get::<_, String>(0),
                        )
                        .map(|s| s == "done")
                        .unwrap_or(false)
                    })
                };

                if deps_satisfied {
                    touch_session(conn, &sid);
                    return Ok(serde_json::json!({
                        "status": "ready",
                        "step_name": step.name,
                        "description": step.description,
                        "dependencies": step.dependencies,
                    })
                    .to_string());
                }
            }

            // All pending steps are blocked by incomplete deps
            Ok(serde_json::json!({ "status": "blocked", "message": "All pending steps have unmet dependencies" }).to_string())
        })
        .unwrap_or_else(|e| e)
    }

    #[tool(description = "Update the status of a plan step. Status must be one of: pending | in_progress | done | failed | blocked. Optionally record a result or error message.")]
    fn flow_update_step_status(
        &self,
        Parameters(UpdateStepStatusParams { session_id, step_name, status, result, error }): Parameters<UpdateStepStatusParams>,
    ) -> String {
        let sid = match sanitize_id(&session_id) {
            Some(s) => s,
            None => return "Error: invalid session_id".to_string(),
        };

        const VALID_STATUSES: &[&str] = &["pending", "in_progress", "done", "failed", "blocked"];
        if !VALID_STATUSES.contains(&status.as_str()) {
            return format!(
                "Error: invalid status '{}'. Must be one of: {}",
                status,
                VALID_STATUSES.join(", ")
            );
        }

        self.with_db(|conn| {
            if !session_exists(conn, &sid) {
                return Err(format!("Error: session '{}' not found", sid));
            }
            let rows = conn.execute(
                "UPDATE plan_steps SET status = ?1, result = ?2, error = ?3, updated = ?4
                 WHERE session_id = ?5 AND step_name = ?6",
                params![status, result, error, unix_now(), sid, step_name],
            )
            .map_err(|e| format!("Error: {e}"))?;
            if rows == 0 {
                return Err(format!("Error: step '{}' not found in session '{}'", step_name, sid));
            }
            touch_session(conn, &sid);
            log_action(conn, &sid, "update_step", &format!("step={step_name} status={status}"));
            Ok(format!("Step '{}' status updated to '{}'", step_name, status))
        })
        .unwrap_or_else(|e| e)
    }

    #[tool(description = "List all steps in a session as a JSON array with their current status, description, and dependencies.")]
    fn flow_list_steps(
        &self,
        Parameters(SessionOnlyParams { session_id }): Parameters<SessionOnlyParams>,
    ) -> String {
        let sid = match sanitize_id(&session_id) {
            Some(s) => s,
            None => return "Error: invalid session_id".to_string(),
        };

        self.with_db(|conn| {
            if !session_exists(conn, &sid) {
                return Err(format!("Error: session '{}' not found", sid));
            }
            let mut stmt = conn.prepare(
                "SELECT step_name, description, dependencies, status, result, error, created, updated
                 FROM plan_steps WHERE session_id = ?1 ORDER BY id",
            )
            .map_err(|e| format!("Error: {e}"))?;

            let steps: Vec<serde_json::Value> = stmt
                .query_map(params![sid], |row| {
                    Ok(serde_json::json!({
                        "step_name":    row.get::<_, String>(0)?,
                        "description":  row.get::<_, String>(1)?,
                        "dependencies": row.get::<_, String>(2)?,
                        "status":       row.get::<_, String>(3)?,
                        "result":       row.get::<_, Option<String>>(4)?,
                        "error":        row.get::<_, Option<String>>(5)?,
                        "created":      row.get::<_, f64>(6)?,
                        "updated":      row.get::<_, f64>(7)?,
                    }))
                })
                .map_err(|e| format!("Error: {e}"))?
                .filter_map(|r| r.ok())
                .collect();

            touch_session(conn, &sid);
            Ok(serde_json::to_string_pretty(&steps).unwrap_or_default())
        })
        .unwrap_or_else(|e| e)
    }

    #[tool(description = "Delete a specific step from the workflow plan.")]
    fn flow_delete_step(
        &self,
        Parameters(DeleteStepParams { session_id, step_name }): Parameters<DeleteStepParams>,
    ) -> String {
        let sid = match sanitize_id(&session_id) {
            Some(s) => s,
            None => return "Error: invalid session_id".to_string(),
        };

        self.with_db(|conn| {
            if !session_exists(conn, &sid) {
                return Err(format!("Error: session '{}' not found", sid));
            }
            let rows = conn.execute(
                "DELETE FROM plan_steps WHERE session_id = ?1 AND step_name = ?2",
                params![sid, step_name],
            )
            .map_err(|e| format!("Error: {e}"))?;
            if rows == 0 {
                return Err(format!("Error: step '{}' not found in session '{}'", step_name, sid));
            }
            touch_session(conn, &sid);
            log_action(conn, &sid, "delete_step", &format!("step={step_name}"));
            Ok(format!("Step '{}' deleted from session '{}'", step_name, sid))
        })
        .unwrap_or_else(|e| e)
    }

    #[tool(description = "Store a key-value pair in session memory. Optionally set a TTL in seconds after which the memory expires.")]
    fn flow_set_memory(
        &self,
        Parameters(SetMemoryParams { session_id, key, value, ttl }): Parameters<SetMemoryParams>,
    ) -> String {
        let sid = match sanitize_id(&session_id) {
            Some(s) => s,
            None => return "Error: invalid session_id".to_string(),
        };

        self.with_db(|conn| {
            if !session_exists(conn, &sid) {
                return Err(format!("Error: session '{}' not found", sid));
            }
            let ts = unix_now();
            let ttl_abs: Option<f64> = ttl.map(|t| ts + t as f64);
            conn.execute(
                "INSERT INTO memories (session_id, key, value, ttl, created)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(session_id, key) DO UPDATE SET value = excluded.value, ttl = excluded.ttl, created = excluded.created",
                params![sid, key, value, ttl_abs, ts],
            )
            .map_err(|e| format!("Error: {e}"))?;
            touch_session(conn, &sid);
            log_action(conn, &sid, "set_memory", &format!("key={key}"));
            Ok(format!("Memory key '{}' set in session '{}'", key, sid))
        })
        .unwrap_or_else(|e| e)
    }

    #[tool(description = "Retrieve a value from session memory by key. Returns the value or a not-found message.")]
    fn flow_get_memory(
        &self,
        Parameters(GetMemoryParams { session_id, key }): Parameters<GetMemoryParams>,
    ) -> String {
        let sid = match sanitize_id(&session_id) {
            Some(s) => s,
            None => return "Error: invalid session_id".to_string(),
        };

        self.with_db(|conn| {
            if !session_exists(conn, &sid) {
                return Err(format!("Error: session '{}' not found", sid));
            }
            let ts = unix_now();
            let result: Option<(String, Option<f64>)> = conn
                .query_row(
                    "SELECT value, ttl FROM memories WHERE session_id = ?1 AND key = ?2",
                    params![sid, key],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .ok();

            match result {
                None => Ok(format!("Memory key '{}' not found in session '{}'", key, sid)),
                Some((value, ttl)) => {
                    if let Some(exp) = ttl {
                        if ts > exp {
                            // Expired — clean it up
                            let _ = conn.execute(
                                "DELETE FROM memories WHERE session_id = ?1 AND key = ?2",
                                params![sid, key],
                            );
                            return Ok(format!("Memory key '{}' has expired", key));
                        }
                    }
                    touch_session(conn, &sid);
                    Ok(serde_json::json!({ "key": key, "value": value }).to_string())
                }
            }
        })
        .unwrap_or_else(|e| e)
    }

    #[tool(description = "Search session memory using keyword matching. Returns up to top_k matching key-value pairs sorted by relevance.")]
    fn flow_query_memory(
        &self,
        Parameters(QueryMemoryParams { session_id, query, top_k }): Parameters<QueryMemoryParams>,
    ) -> String {
        let sid = match sanitize_id(&session_id) {
            Some(s) => s,
            None => return "Error: invalid session_id".to_string(),
        };

        self.with_db(|conn| {
            if !session_exists(conn, &sid) {
                return Err(format!("Error: session '{}' not found", sid));
            }
            let ts = unix_now();
            let limit = top_k.unwrap_or(5).max(1).min(100);

            // Simple keyword search: check if query term appears in key or value (case-insensitive)
            let mut stmt = conn.prepare(
                "SELECT key, value, ttl FROM memories
                 WHERE session_id = ?1 AND (ttl IS NULL OR ttl > ?2)
                 ORDER BY id DESC"
            ).map_err(|e| format!("Error: {e}"))?;

            let query_lower = query.to_lowercase();
            let keywords: Vec<&str> = query_lower.split_whitespace().collect();

            let results: Vec<serde_json::Value> = stmt
                .query_map(params![sid, ts], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<f64>>(2)?,
                    ))
                })
                .map_err(|e| format!("Error: {e}"))?
                .filter_map(|r| r.ok())
                .filter(|(k, v, _)| {
                    let haystack = format!("{} {}", k.to_lowercase(), v.to_lowercase());
                    keywords.iter().any(|kw| haystack.contains(kw))
                })
                .take(limit as usize)
                .map(|(k, v, _)| serde_json::json!({ "key": k, "value": v }))
                .collect();

            touch_session(conn, &sid);
            Ok(serde_json::to_string_pretty(&results).unwrap_or_default())
        })
        .unwrap_or_else(|e| e)
    }

    #[tool(description = "Save a named checkpoint of the current session state (steps, memories, context). Can be restored later.")]
    fn flow_save_checkpoint(
        &self,
        Parameters(CheckpointParams { session_id, name }): Parameters<CheckpointParams>,
    ) -> String {
        let sid = match sanitize_id(&session_id) {
            Some(s) => s,
            None => return "Error: invalid session_id".to_string(),
        };

        self.with_db(|conn| {
            if !session_exists(conn, &sid) {
                return Err(format!("Error: session '{}' not found", sid));
            }
            let snapshot = take_snapshot(conn, &sid).map_err(|e| format!("Error: snapshot failed: {e}"))?;
            let ts = unix_now();
            conn.execute(
                "INSERT INTO checkpoints (name, session_id, snapshot, created)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(name, session_id) DO UPDATE SET snapshot = excluded.snapshot, created = excluded.created",
                params![name, sid, snapshot, ts],
            )
            .map_err(|e| format!("Error: {e}"))?;
            log_action(conn, &sid, "save_checkpoint", &format!("name={name}"));
            Ok(format!("Checkpoint '{}' saved for session '{}'", name, sid))
        })
        .unwrap_or_else(|e| e)
    }

    #[tool(description = "Restore session state from a named checkpoint. All current steps and memories are replaced with the checkpoint state.")]
    fn flow_restore_checkpoint(
        &self,
        Parameters(CheckpointParams { session_id, name }): Parameters<CheckpointParams>,
    ) -> String {
        let sid = match sanitize_id(&session_id) {
            Some(s) => s,
            None => return "Error: invalid session_id".to_string(),
        };

        self.with_db(|conn| {
            if !session_exists(conn, &sid) {
                return Err(format!("Error: session '{}' not found", sid));
            }
            let snapshot: Option<String> = conn
                .query_row(
                    "SELECT snapshot FROM checkpoints WHERE name = ?1 AND session_id = ?2",
                    params![name, sid],
                    |r| r.get(0),
                )
                .ok();

            let snapshot = match snapshot {
                Some(s) => s,
                None => return Err(format!("Error: checkpoint '{}' not found in session '{}'", name, sid)),
            };

            restore_snapshot(conn, &sid, &snapshot)
                .map_err(|e| format!("Error: restore failed: {e}"))?;
            log_action(conn, &sid, "restore_checkpoint", &format!("name={name}"));
            Ok(format!("Checkpoint '{}' restored for session '{}'", name, sid))
        })
        .unwrap_or_else(|e| e)
    }

    #[tool(description = "Report cumulative token usage for context tracking. Returns context status and, if usage exceeds 80%, a compaction recommendation.")]
    fn flow_update_context(
        &self,
        Parameters(UpdateContextParams { session_id, tokens_used }): Parameters<UpdateContextParams>,
    ) -> String {
        let sid = match sanitize_id(&session_id) {
            Some(s) => s,
            None => return "Error: invalid session_id".to_string(),
        };

        self.with_db(|conn| {
            if !session_exists(conn, &sid) {
                return Err(format!("Error: session '{}' not found", sid));
            }
            conn.execute(
                "UPDATE sessions SET context_usage = ?1, last_active = ?2 WHERE session_id = ?3",
                params![tokens_used, unix_now(), sid],
            )
            .map_err(|e| format!("Error: {e}"))?;

            let (context_limit,): (i64,) = conn
                .query_row(
                    "SELECT context_limit FROM sessions WHERE session_id = ?1",
                    params![sid],
                    |r| Ok((r.get(0)?,)),
                )
                .map_err(|e| format!("Error: {e}"))
                .and_then(|v| Ok(v))?;

            let pct = if context_limit > 0 {
                (tokens_used as f64 / context_limit as f64 * 100.0) as u32
            } else {
                0
            };

            let mut obj = serde_json::json!({
                "session_id": sid,
                "tokens_used": tokens_used,
                "context_limit": context_limit,
                "usage_pct": pct,
            });

            if pct >= 80 {
                obj["warning"] = serde_json::json!("Context is over 80% full");
                obj["recommendation"] = serde_json::json!(
                    "Run flow_compact_context to archive old steps and free context space"
                );
            } else {
                obj["status"] = serde_json::json!("ok");
            }

            log_action(conn, &sid, "update_context", &format!("tokens={tokens_used} pct={pct}"));
            Ok(serde_json::to_string_pretty(&obj).unwrap_or_default())
        })
        .unwrap_or_else(|e| e)
    }

    #[tool(description = "Compact context by archiving old completed steps into long-term memory. Keeps the most recent `keep_last` steps (default 10) and summarizes the rest.")]
    fn flow_compact_context(
        &self,
        Parameters(CompactContextParams { session_id, keep_last }): Parameters<CompactContextParams>,
    ) -> String {
        let sid = match sanitize_id(&session_id) {
            Some(s) => s,
            None => return "Error: invalid session_id".to_string(),
        };
        let keep = keep_last.unwrap_or(10).max(0);

        self.with_db(|conn| {
            if !session_exists(conn, &sid) {
                return Err(format!("Error: session '{}' not found", sid));
            }

            // Get all steps ordered by id; take all except the last `keep`
            let mut stmt = conn.prepare(
                "SELECT id, step_name, description, status, result, created, updated
                 FROM plan_steps WHERE session_id = ?1 ORDER BY id ASC"
            ).map_err(|e| format!("Error: {e}"))?;

            struct FullStep {
                id: i64,
                name: String,
                description: String,
                status: String,
                result: Option<String>,
                created: f64,
                updated: f64,
            }

            let all_steps: Vec<FullStep> = stmt
                .query_map(params![sid], |row| {
                    Ok(FullStep {
                        id: row.get(0)?,
                        name: row.get(1)?,
                        description: row.get(2)?,
                        status: row.get(3)?,
                        result: row.get(4)?,
                        created: row.get(5)?,
                        updated: row.get(6)?,
                    })
                })
                .map_err(|e| format!("Error: {e}"))?
                .filter_map(|r| r.ok())
                .collect();

            let total = all_steps.len();
            let to_archive = if total as i64 > keep { total as i64 - keep } else { 0 };

            if to_archive == 0 {
                return Ok(format!(
                    "Nothing to compact: session '{}' has {} steps, keep_last={}",
                    sid, total, keep
                ));
            }

            let archive_slice = &all_steps[..to_archive as usize];

            // Build summary text
            let mut summary_parts: Vec<String> = Vec::new();
            let mut compressed_ids: Vec<i64> = Vec::new();
            let from_ts = archive_slice.first().map(|s| s.created).unwrap_or(0.0);
            let until_ts = archive_slice.last().map(|s| s.updated).unwrap_or(0.0);

            for step in archive_slice {
                let result_text = step.result.as_deref().unwrap_or("(no result)");
                summary_parts.push(format!(
                    "[{}] {} ({}): {}",
                    step.status, step.name, step.description, result_text
                ));
                compressed_ids.push(step.id);
            }

            let summary = format!(
                "Archived {} steps from session '{}'. Steps: {}",
                to_archive,
                sid,
                summary_parts.join(" | ")
            );
            let compressed_steps = compressed_ids
                .iter()
                .map(|id| id.to_string())
                .collect::<Vec<_>>()
                .join(",");

            // Insert into long_term_memory
            let ts = unix_now();
            conn.execute(
                "INSERT INTO long_term_memory (session_id, summary, compressed_steps, from_ts, until_ts, created)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![sid, summary, compressed_steps, from_ts, until_ts, ts],
            )
            .map_err(|e| format!("Error: insert long_term_memory: {e}"))?;

            // Delete archived steps
            for id in &compressed_ids {
                conn.execute(
                    "DELETE FROM plan_steps WHERE id = ?1",
                    params![id],
                )
                .map_err(|e| format!("Error: delete step: {e}"))?;
            }

            touch_session(conn, &sid);
            log_action(
                conn, &sid, "compact_context",
                &format!("archived={to_archive} kept={keep}"),
            );
            Ok(format!(
                "Compacted session '{}': archived {} steps, {} steps remain. Summary stored in long-term memory.",
                sid, to_archive, total as i64 - to_archive
            ))
        })
        .unwrap_or_else(|e| e)
    }

    #[tool(description = "Query archived summaries from long-term memory. Optionally filter by keyword. Returns a JSON array of summaries.")]
    fn flow_query_long_term_memory(
        &self,
        Parameters(QueryLtmParams { session_id, query, top_k }): Parameters<QueryLtmParams>,
    ) -> String {
        let sid = match sanitize_id(&session_id) {
            Some(s) => s,
            None => return "Error: invalid session_id".to_string(),
        };

        self.with_db(|conn| {
            if !session_exists(conn, &sid) {
                return Err(format!("Error: session '{}' not found", sid));
            }
            let limit = top_k.unwrap_or(10).max(1).min(200);

            let mut stmt = conn.prepare(
                "SELECT id, summary, from_ts, until_ts, created
                 FROM long_term_memory WHERE session_id = ?1 ORDER BY id DESC"
            ).map_err(|e| format!("Error: {e}"))?;

            let filter = query
                .as_deref()
                .map(|q| q.to_lowercase())
                .unwrap_or_default();

            let results: Vec<serde_json::Value> = stmt
                .query_map(params![sid], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, f64>(2)?,
                        row.get::<_, f64>(3)?,
                        row.get::<_, f64>(4)?,
                    ))
                })
                .map_err(|e| format!("Error: {e}"))?
                .filter_map(|r| r.ok())
                .filter(|(_, summary, _, _, _)| {
                    filter.is_empty() || summary.to_lowercase().contains(&filter)
                })
                .take(limit as usize)
                .map(|(id, summary, from_ts, until_ts, created)| {
                    serde_json::json!({
                        "id": id,
                        "summary": summary,
                        "from_ts": from_ts,
                        "until_ts": until_ts,
                        "created": created,
                    })
                })
                .collect();

            touch_session(conn, &sid);
            Ok(serde_json::to_string_pretty(&results).unwrap_or_default())
        })
        .unwrap_or_else(|e| e)
    }

    #[tool(description = "Get an actionable recovery prompt based on current session state: pending steps, context usage, and available long-term memory.")]
    fn flow_get_recovery_prompt(
        &self,
        Parameters(SessionOnlyParams { session_id }): Parameters<SessionOnlyParams>,
    ) -> String {
        let sid = match sanitize_id(&session_id) {
            Some(s) => s,
            None => return "Error: invalid session_id".to_string(),
        };

        self.with_db(|conn| {
            if !session_exists(conn, &sid) {
                return Err(format!("Error: session '{}' not found", sid));
            }

            let (goal, context_usage, context_limit): (Option<String>, i64, i64) = conn
                .query_row(
                    "SELECT goal, context_usage, context_limit FROM sessions WHERE session_id = ?1",
                    params![sid],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .map_err(|e| format!("Error: {e}"))?;

            let usage_pct = if context_limit > 0 {
                (context_usage as f64 / context_limit as f64 * 100.0) as u32
            } else {
                0
            };

            // Pending steps
            let mut pending_stmt = conn.prepare(
                "SELECT step_name, description, dependencies FROM plan_steps
                 WHERE session_id = ?1 AND status = 'pending' ORDER BY id"
            ).map_err(|e| format!("Error: {e}"))?;

            let pending_steps: Vec<(String, String, String)> = pending_stmt
                .query_map(params![sid], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })
                .map_err(|e| format!("Error: {e}"))?
                .filter_map(|r| r.ok())
                .collect();

            // In-progress steps
            let mut ip_stmt = conn.prepare(
                "SELECT step_name, description FROM plan_steps
                 WHERE session_id = ?1 AND status = 'in_progress' ORDER BY id"
            ).map_err(|e| format!("Error: {e}"))?;

            let in_progress: Vec<(String, String)> = ip_stmt
                .query_map(params![sid], |row| Ok((row.get(0)?, row.get(1)?)))
                .map_err(|e| format!("Error: {e}"))?
                .filter_map(|r| r.ok())
                .collect();

            // Long-term memory count
            let ltm_count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM long_term_memory WHERE session_id = ?1",
                    params![sid],
                    |r| r.get(0),
                )
                .unwrap_or(0);

            let mut lines: Vec<String> = Vec::new();
            lines.push(format!("# Recovery Prompt for Session '{}'", sid));
            lines.push(String::new());

            if let Some(g) = &goal {
                lines.push(format!("**Goal:** {}", g));
                lines.push(String::new());
            }

            lines.push(format!(
                "**Context usage:** {} / {} tokens ({}%)",
                context_usage, context_limit, usage_pct
            ));
            if usage_pct >= 80 {
                lines.push("**WARNING:** Context is nearly full. Run `flow_compact_context` immediately.".to_string());
            }
            lines.push(String::new());

            if !in_progress.is_empty() {
                lines.push("**Currently in-progress steps:**".to_string());
                for (name, desc) in &in_progress {
                    lines.push(format!("  - {} : {}", name, desc));
                }
                lines.push(String::new());
            }

            if !pending_steps.is_empty() {
                lines.push("**Pending steps (in order):**".to_string());
                for (name, desc, deps) in &pending_steps {
                    if deps.trim().is_empty() {
                        lines.push(format!("  - {} : {} [no deps — ready]", name, desc));
                    } else {
                        lines.push(format!("  - {} : {} [deps: {}]", name, desc, deps));
                    }
                }
                lines.push(String::new());
                lines.push("Run `flow_get_next_action` to get the first actionable step.".to_string());
            } else {
                lines.push("No pending steps. The plan may be complete or empty.".to_string());
            }

            if ltm_count > 0 {
                lines.push(String::new());
                lines.push(format!(
                    "**Long-term memory:** {} archived summaries available. Use `flow_query_long_term_memory` to recall context from earlier in the session.",
                    ltm_count
                ));
            }

            touch_session(conn, &sid);
            Ok(lines.join("\n"))
        })
        .unwrap_or_else(|e| e)
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let data_dir = dirs_home().join(".mcp-flow");
    let server = FlowServer::new(data_dir)?;
    tracing::info!("mcp-flow starting");
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

fn dirs_home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}
