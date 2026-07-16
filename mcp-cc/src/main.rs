use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use base64::Engine;
use clap::Parser;
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use rmcp::transport::stdio;
use rmcp::{ServiceExt, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use rusqlite::{Connection, params};
use serde::Deserialize;
use tokio::time::{Duration, sleep};
use tracing::{debug, info, warn};

// ---------------------------------------------------------------------------
// Size limits
// ---------------------------------------------------------------------------

const MAX_KEY_BYTES: usize = 4 * 1024;        // 4 KB
const MAX_VALUE_BYTES: usize = 1024 * 1024;   // 1 MB

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser, Debug)]
#[clap(name = "mcp-cc", about = "Key-value cache MCP server backed by SQLite")]
struct Cli {
    /// Data directory for the SQLite database file
    #[clap(long, default_value = ".")]
    root: PathBuf,

    /// How often (in seconds) the background task purges expired rows
    #[clap(long, default_value = "60")]
    ttl_cleanup_interval: u64,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

fn resolve_session(session_id: Option<String>) -> String {
    session_id.unwrap_or_else(|| "default".to_string())
}

// ---------------------------------------------------------------------------
// Parameter structs
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct KvSetParams {
    #[schemars(description = "Key to store")]
    key: String,
    #[schemars(description = "Value to store (JSON or plain string)")]
    value: String,
    #[schemars(description = "Optional session namespace (defaults to 'default')")]
    session_id: Option<String>,
    #[schemars(description = "Optional TTL in seconds; omit or null for no expiry")]
    ttl: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct KvGetParams {
    #[schemars(description = "Key to retrieve")]
    key: String,
    #[schemars(description = "Optional session namespace")]
    session_id: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct KvDeleteParams {
    #[schemars(description = "Key to delete")]
    key: String,
    #[schemars(description = "Optional session namespace")]
    session_id: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct KvListParams {
    #[schemars(description = "Optional session namespace")]
    session_id: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct KvClearParams {
    #[schemars(description = "Optional session namespace")]
    session_id: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct KvInspectTtlParams {
    #[schemars(description = "Key to inspect")]
    key: String,
    #[schemars(description = "Optional session namespace")]
    session_id: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct KvBatchSetParams {
    #[schemars(
        description = r#"JSON array of objects: [{"key":"k","value":"v","ttl":60,"session_id":"..."}]"#
    )]
    items_json: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct KvBatchGetParams {
    #[schemars(description = r#"JSON array of key strings: ["key1","key2"]"#)]
    keys_json: String,
    #[schemars(description = "Optional session namespace")]
    session_id: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct KvExportParams {
    #[schemars(description = "Optional session namespace")]
    session_id: Option<String>,
    #[schemars(description = "Compress the JSON payload with gzip before base64-encoding")]
    compress: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct KvImportParams {
    #[schemars(
        description = "Base64-encoded export data (plain JSON or gzip-compressed; auto-detected)"
    )]
    data_base64: String,
    #[schemars(description = "Optional session namespace to import into")]
    session_id: Option<String>,
    #[schemars(description = "Overwrite existing keys; when false, existing keys are skipped")]
    overwrite: bool,
}

// ---------------------------------------------------------------------------
// Server struct
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct KvServer {
    db: Arc<Mutex<Connection>>,
    _data_dir: PathBuf,
}

impl KvServer {
    fn new(data_dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&data_dir)?;
        let db_path = data_dir.join("kv.db");
        info!("Opening SQLite database at {db_path:?}");

        let conn = Connection::open(&db_path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL;")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS kv (
                key        TEXT NOT NULL,
                session_id TEXT NOT NULL DEFAULT 'default',
                value      TEXT NOT NULL,
                created_at REAL NOT NULL,
                ttl        REAL,
                PRIMARY KEY (key, session_id)
            );
            CREATE INDEX IF NOT EXISTS idx_session ON kv(session_id);
            CREATE INDEX IF NOT EXISTS idx_expiry  ON kv(session_id, created_at, ttl);",
        )?;

        Ok(KvServer {
            db: Arc::new(Mutex::new(conn)),
            _data_dir: data_dir,
        })
    }

    fn cleanup_expired(&self) {
        let now = now_secs();
        match self.db.lock() {
            Ok(conn) => {
                match conn.execute(
                    "DELETE FROM kv WHERE ttl IS NOT NULL AND (?1 - created_at) > ttl",
                    params![now],
                ) {
                    Ok(n) => debug!("TTL cleanup removed {n} expired rows"),
                    Err(e) => warn!("TTL cleanup error: {e}"),
                }
            }
            Err(e) => warn!("Could not lock db for TTL cleanup: {e}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Tool implementations
// ---------------------------------------------------------------------------

#[tool_router(server_handler)]
impl KvServer {
    // ------------------------------------------------------------------
    // kv_set
    // ------------------------------------------------------------------
    #[tool(description = "Store a key-value pair with optional TTL (seconds). \
        Overwrites any existing entry for that key in the session.")]
    fn kv_set(&self, Parameters(p): Parameters<KvSetParams>) -> String {
        if p.key.len() > MAX_KEY_BYTES {
            return format!("Error: key exceeds maximum size of {} bytes", MAX_KEY_BYTES);
        }
        if p.value.len() > MAX_VALUE_BYTES {
            return format!("Error: value exceeds maximum size of {} MB", MAX_VALUE_BYTES / 1024 / 1024);
        }
        let sid = resolve_session(p.session_id);
        let now = now_secs();
        let ttl_f: Option<f64> = p.ttl.map(|t| t as f64);

        match self.db.lock() {
            Ok(conn) => {
                match conn.execute(
                    "INSERT INTO kv (key, session_id, value, created_at, ttl)
                     VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(key, session_id) DO UPDATE SET
                         value      = excluded.value,
                         created_at = excluded.created_at,
                         ttl        = excluded.ttl",
                    params![p.key, sid, p.value, now, ttl_f],
                ) {
                    Ok(_) => match p.ttl {
                        Some(t) => format!("OK: stored '{}' in session '{}' (TTL {}s)", p.key, sid, t),
                        None => format!("OK: stored '{}' in session '{}' (no expiry)", p.key, sid),
                    },
                    Err(e) => format!("Error: {e}"),
                }
            }
            Err(e) => format!("Error: db lock poisoned: {e}"),
        }
    }

    // ------------------------------------------------------------------
    // kv_get
    // ------------------------------------------------------------------
    #[tool(description = "Retrieve a value by key. Returns the value string, \
        'Key not found', or 'Key expired'.")]
    fn kv_get(&self, Parameters(p): Parameters<KvGetParams>) -> String {
        let sid = resolve_session(p.session_id);
        let now = now_secs();

        match self.db.lock() {
            Ok(conn) => {
                match conn.query_row(
                    "SELECT value, created_at, ttl FROM kv WHERE key=?1 AND session_id=?2",
                    params![p.key, sid],
                    |row| {
                        let value: String = row.get(0)?;
                        let created_at: f64 = row.get(1)?;
                        let ttl: Option<f64> = row.get(2)?;
                        Ok((value, created_at, ttl))
                    },
                ) {
                    Ok((_value, created_at, Some(ttl))) if (now - created_at) > ttl => {
                        "Key expired".to_string()
                    }
                    Ok((value, _, _)) => value,
                    Err(rusqlite::Error::QueryReturnedNoRows) => "Key not found".to_string(),
                    Err(e) => format!("Error: {e}"),
                }
            }
            Err(e) => format!("Error: db lock poisoned: {e}"),
        }
    }

    // ------------------------------------------------------------------
    // kv_delete
    // ------------------------------------------------------------------
    #[tool(description = "Delete a key. Returns confirmation or 'Key not found'.")]
    fn kv_delete(&self, Parameters(p): Parameters<KvDeleteParams>) -> String {
        let sid = resolve_session(p.session_id);

        match self.db.lock() {
            Ok(conn) => {
                match conn.execute(
                    "DELETE FROM kv WHERE key=?1 AND session_id=?2",
                    params![p.key, sid],
                ) {
                    Ok(0) => format!("Key not found: '{}' in session '{}'", p.key, sid),
                    Ok(_) => format!("OK: deleted '{}' from session '{}'", p.key, sid),
                    Err(e) => format!("Error: {e}"),
                }
            }
            Err(e) => format!("Error: db lock poisoned: {e}"),
        }
    }

    // ------------------------------------------------------------------
    // kv_list
    // ------------------------------------------------------------------
    #[tool(description = "List all non-expired keys in a session, one per line.")]
    fn kv_list(&self, Parameters(p): Parameters<KvListParams>) -> String {
        let sid = resolve_session(p.session_id);
        let now = now_secs();

        match self.db.lock() {
            Ok(conn) => {
                let mut stmt = match conn.prepare(
                    "SELECT key, created_at, ttl FROM kv WHERE session_id=?1 ORDER BY key",
                ) {
                    Ok(s) => s,
                    Err(e) => return format!("Error: {e}"),
                };

                let rows = stmt.query_map(params![sid], |row| {
                    let key: String = row.get(0)?;
                    let created_at: f64 = row.get(1)?;
                    let ttl: Option<f64> = row.get(2)?;
                    Ok((key, created_at, ttl))
                });

                match rows {
                    Ok(iter) => {
                        let keys: Vec<String> = iter
                            .filter_map(|r| r.ok())
                            .filter(|(_, created_at, ttl)| {
                                ttl.map(|t| (now - created_at) <= t).unwrap_or(true)
                            })
                            .map(|(k, _, _)| k)
                            .collect();

                        if keys.is_empty() {
                            format!("(no keys in session '{sid}')")
                        } else {
                            keys.join("\n")
                        }
                    }
                    Err(e) => format!("Error: {e}"),
                }
            }
            Err(e) => format!("Error: db lock poisoned: {e}"),
        }
    }

    // ------------------------------------------------------------------
    // kv_clear
    // ------------------------------------------------------------------
    #[tool(description = "Delete all keys in a session. Returns how many were deleted.")]
    fn kv_clear(&self, Parameters(p): Parameters<KvClearParams>) -> String {
        let sid = resolve_session(p.session_id);

        match self.db.lock() {
            Ok(conn) => match conn.execute("DELETE FROM kv WHERE session_id=?1", params![sid]) {
                Ok(n) => format!("OK: deleted {n} keys from session '{sid}'"),
                Err(e) => format!("Error: {e}"),
            },
            Err(e) => format!("Error: db lock poisoned: {e}"),
        }
    }

    // ------------------------------------------------------------------
    // kv_inspect_ttl
    // ------------------------------------------------------------------
    #[tool(description = "Inspect TTL metadata for a key: total TTL, elapsed, remaining seconds, \
        and whether it is expired or permanent.")]
    fn kv_inspect_ttl(&self, Parameters(p): Parameters<KvInspectTtlParams>) -> String {
        let sid = resolve_session(p.session_id);
        let now = now_secs();

        match self.db.lock() {
            Ok(conn) => {
                match conn.query_row(
                    "SELECT created_at, ttl FROM kv WHERE key=?1 AND session_id=?2",
                    params![p.key, sid],
                    |row| {
                        let created_at: f64 = row.get(0)?;
                        let ttl: Option<f64> = row.get(1)?;
                        Ok((created_at, ttl))
                    },
                ) {
                    Ok((_, None)) => {
                        format!("Key '{}' in session '{}': permanent (no TTL)", p.key, sid)
                    }
                    Ok((created_at, Some(ttl))) => {
                        let elapsed = now - created_at;
                        let remaining = ttl - elapsed;
                        if remaining <= 0.0 {
                            format!(
                                "Key '{}' in session '{}': expired\n\
                                 total_ttl:  {:.0}s\n\
                                 elapsed:    {:.1}s\n\
                                 remaining:  0s (expired {:.1}s ago)",
                                p.key,
                                sid,
                                ttl,
                                elapsed,
                                -remaining
                            )
                        } else {
                            format!(
                                "Key '{}' in session '{}': active\n\
                                 total_ttl:  {:.0}s\n\
                                 elapsed:    {:.1}s\n\
                                 remaining:  {:.1}s",
                                p.key, sid, ttl, elapsed, remaining
                            )
                        }
                    }
                    Err(rusqlite::Error::QueryReturnedNoRows) => {
                        format!("Key not found: '{}' in session '{}'", p.key, sid)
                    }
                    Err(e) => format!("Error: {e}"),
                }
            }
            Err(e) => format!("Error: db lock poisoned: {e}"),
        }
    }

    // ------------------------------------------------------------------
    // kv_batch_set
    // ------------------------------------------------------------------
    #[tool(description = r#"Batch-store multiple key-value pairs in one call. \
        items_json must be a JSON array of objects with fields: \
        key (string, required), value (string, required), ttl (integer seconds, optional), \
        session_id (string, optional). \
        Returns a JSON array of per-item results."#)]
    fn kv_batch_set(&self, Parameters(p): Parameters<KvBatchSetParams>) -> String {
        #[derive(Deserialize)]
        struct Item {
            key: String,
            value: String,
            ttl: Option<i64>,
            session_id: Option<String>,
        }

        const MAX_BATCH_JSON: usize = 10 * 1024 * 1024; // 10 MB
        if p.items_json.len() > MAX_BATCH_JSON {
            return format!("Error: items_json exceeds maximum size of 10 MB");
        }

        let items: Vec<Item> = match serde_json::from_str(&p.items_json) {
            Ok(v) => v,
            Err(e) => return format!("Error: invalid JSON: {e}"),
        };

        let now = now_secs();
        let mut results: Vec<serde_json::Value> = Vec::with_capacity(items.len());

        match self.db.lock() {
            Ok(conn) => {
                for item in items {
                    if item.key.len() > MAX_KEY_BYTES {
                        results.push(serde_json::json!({
                            "key": item.key,
                            "status": "error",
                            "error": format!("key exceeds maximum size of {} bytes", MAX_KEY_BYTES)
                        }));
                        continue;
                    }
                    if item.value.len() > MAX_VALUE_BYTES {
                        results.push(serde_json::json!({
                            "key": item.key,
                            "status": "error",
                            "error": format!("value exceeds maximum size of {} MB", MAX_VALUE_BYTES / 1024 / 1024)
                        }));
                        continue;
                    }
                    let sid = resolve_session(item.session_id);
                    let ttl_f: Option<f64> = item.ttl.map(|t| t as f64);

                    let res = conn.execute(
                        "INSERT INTO kv (key, session_id, value, created_at, ttl)
                         VALUES (?1, ?2, ?3, ?4, ?5)
                         ON CONFLICT(key, session_id) DO UPDATE SET
                             value      = excluded.value,
                             created_at = excluded.created_at,
                             ttl        = excluded.ttl",
                        params![item.key, sid, item.value, now, ttl_f],
                    );

                    results.push(match res {
                        Ok(_) => serde_json::json!({"key": item.key, "status": "ok"}),
                        Err(e) => serde_json::json!({
                            "key": item.key,
                            "status": "error",
                            "error": e.to_string()
                        }),
                    });
                }
                serde_json::to_string_pretty(&results).unwrap_or_else(|e| format!("Error: {e}"))
            }
            Err(e) => format!("Error: db lock poisoned: {e}"),
        }
    }

    // ------------------------------------------------------------------
    // kv_batch_get
    // ------------------------------------------------------------------
    #[tool(description = r#"Batch-retrieve multiple values in one call. \
        keys_json must be a JSON array of strings. \
        Returns a JSON array of {key, value, expired, not_found} objects."#)]
    fn kv_batch_get(&self, Parameters(p): Parameters<KvBatchGetParams>) -> String {
        let keys: Vec<String> = match serde_json::from_str(&p.keys_json) {
            Ok(v) => v,
            Err(e) => return format!("Error: invalid JSON: {e}"),
        };

        let sid = resolve_session(p.session_id);
        let now = now_secs();
        let mut results: Vec<serde_json::Value> = Vec::with_capacity(keys.len());

        match self.db.lock() {
            Ok(conn) => {
                for key in keys {
                    let row = conn.query_row(
                        "SELECT value, created_at, ttl FROM kv WHERE key=?1 AND session_id=?2",
                        params![key, sid],
                        |row| {
                            let value: String = row.get(0)?;
                            let created_at: f64 = row.get(1)?;
                            let ttl: Option<f64> = row.get(2)?;
                            Ok((value, created_at, ttl))
                        },
                    );

                    let entry = match row {
                        Ok((value, created_at, ttl)) => {
                            let expired =
                                ttl.map(|t| (now - created_at) > t).unwrap_or(false);
                            if expired {
                                serde_json::json!({
                                    "key": key,
                                    "value": null,
                                    "expired": true
                                })
                            } else {
                                serde_json::json!({
                                    "key": key,
                                    "value": value,
                                    "expired": false
                                })
                            }
                        }
                        Err(rusqlite::Error::QueryReturnedNoRows) => {
                            serde_json::json!({
                                "key": key,
                                "value": null,
                                "expired": false,
                                "not_found": true
                            })
                        }
                        Err(e) => serde_json::json!({
                            "key": key,
                            "error": e.to_string()
                        }),
                    };
                    results.push(entry);
                }
                serde_json::to_string_pretty(&results).unwrap_or_else(|e| format!("Error: {e}"))
            }
            Err(e) => format!("Error: db lock poisoned: {e}"),
        }
    }

    // ------------------------------------------------------------------
    // kv_export
    // ------------------------------------------------------------------
    #[tool(description = "Export all non-expired key-values in a session as a base64 string. \
        Set compress=true to gzip before encoding. The result can be imported with kv_import.")]
    fn kv_export(&self, Parameters(p): Parameters<KvExportParams>) -> String {
        let sid = resolve_session(p.session_id);
        let now = now_secs();

        match self.db.lock() {
            Ok(conn) => {
                let mut stmt = match conn.prepare(
                    "SELECT key, value, created_at, ttl \
                     FROM kv WHERE session_id=?1 ORDER BY key",
                ) {
                    Ok(s) => s,
                    Err(e) => return format!("Error: {e}"),
                };

                let rows = stmt.query_map(params![sid], |row| {
                    let key: String = row.get(0)?;
                    let value: String = row.get(1)?;
                    let created_at: f64 = row.get(2)?;
                    let ttl: Option<f64> = row.get(3)?;
                    Ok((key, value, created_at, ttl))
                });

                let items: Vec<serde_json::Value> = match rows {
                    Ok(iter) => iter
                        .filter_map(|r| r.ok())
                        .filter(|(_, _, created_at, ttl)| {
                            ttl.map(|t| (now - created_at) <= t).unwrap_or(true)
                        })
                        .map(|(key, value, created_at, ttl)| {
                            serde_json::json!({
                                "key": key,
                                "value": value,
                                "created_at": created_at,
                                "ttl": ttl
                            })
                        })
                        .collect(),
                    Err(e) => return format!("Error: {e}"),
                };

                let json = match serde_json::to_string(&items) {
                    Ok(j) => j,
                    Err(e) => return format!("Error: serialization failed: {e}"),
                };

                if p.compress {
                    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
                    if let Err(e) = encoder.write_all(json.as_bytes()) {
                        return format!("Error: compression failed: {e}");
                    }
                    match encoder.finish() {
                        Ok(compressed) => {
                            base64::engine::general_purpose::STANDARD.encode(&compressed)
                        }
                        Err(e) => format!("Error: compression finalize failed: {e}"),
                    }
                } else {
                    base64::engine::general_purpose::STANDARD.encode(json.as_bytes())
                }
            }
            Err(e) => format!("Error: db lock poisoned: {e}"),
        }
    }

    // ------------------------------------------------------------------
    // kv_import
    // ------------------------------------------------------------------
    #[tool(description = "Import key-values from data produced by kv_export. \
        Automatically detects gzip compression. \
        Set overwrite=true to replace existing keys; false skips them. \
        Returns the count of imported keys.")]
    fn kv_import(&self, Parameters(p): Parameters<KvImportParams>) -> String {
        let sid = resolve_session(p.session_id);

        let raw = match base64::engine::general_purpose::STANDARD.decode(&p.data_base64) {
            Ok(b) => b,
            Err(e) => return format!("Error: invalid base64: {e}"),
        };

        // Auto-detect gzip (magic bytes 0x1f 0x8b)
        let json: String = if raw.starts_with(&[0x1f, 0x8b]) {
            const MAX_DECOMPRESSED: u64 = 50 * 1024 * 1024; // 50 MB
            let mut decoder = GzDecoder::new(raw.as_slice());
            let mut buf = String::new();
            match std::io::Read::read_to_string(&mut (&mut decoder).take(MAX_DECOMPRESSED), &mut buf) {
                Ok(_) => {
                    // Check if there might be more data (we hit the limit)
                    let mut probe = [0u8; 1];
                    if std::io::Read::read(&mut decoder, &mut probe).unwrap_or(0) > 0 {
                        return format!("Error: decompressed data exceeds {} MB limit", MAX_DECOMPRESSED / 1024 / 1024);
                    }
                    buf
                }
                Err(e) => return format!("Error: decompression failed: {e}"),
            }
        } else {
            match String::from_utf8(raw) {
                Ok(s) => s,
                Err(e) => return format!("Error: invalid UTF-8: {e}"),
            }
        };

        #[derive(Deserialize)]
        struct ExportItem {
            key: String,
            value: String,
            created_at: Option<f64>,
            ttl: Option<f64>,
        }

        let items: Vec<ExportItem> = match serde_json::from_str(&json) {
            Ok(v) => v,
            Err(e) => return format!("Error: invalid export JSON: {e}"),
        };

        let now = now_secs();
        let mut imported = 0usize;
        let mut skipped = 0usize;
        let mut errors = 0usize;

        match self.db.lock() {
            Ok(conn) => {
                for item in items {
                    let created_at = item.created_at.unwrap_or(now);

                    let result = if p.overwrite {
                        conn.execute(
                            "INSERT INTO kv (key, session_id, value, created_at, ttl)
                             VALUES (?1, ?2, ?3, ?4, ?5)
                             ON CONFLICT(key, session_id) DO UPDATE SET
                                 value      = excluded.value,
                                 created_at = excluded.created_at,
                                 ttl        = excluded.ttl",
                            params![item.key, sid, item.value, created_at, item.ttl],
                        )
                    } else {
                        conn.execute(
                            "INSERT OR IGNORE INTO kv (key, session_id, value, created_at, ttl)
                             VALUES (?1, ?2, ?3, ?4, ?5)",
                            params![item.key, sid, item.value, created_at, item.ttl],
                        )
                    };

                    match result {
                        Ok(0) => skipped += 1,
                        Ok(_) => imported += 1,
                        Err(_) => errors += 1,
                    }
                }

                let mut msg = format!("OK: imported {imported} keys into session '{sid}'");
                if skipped > 0 {
                    msg.push_str(&format!(", {skipped} skipped (already exist)"));
                }
                if errors > 0 {
                    msg.push_str(&format!(", {errors} errors"));
                }
                msg
            }
            Err(e) => format!("Error: db lock poisoned: {e}"),
        }
    }
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let cli = Cli::parse();
    info!(
        root = ?cli.root,
        ttl_cleanup_interval = cli.ttl_cleanup_interval,
        "Starting mcp-cc"
    );

    let server = KvServer::new(cli.root)?;

    // Background task: periodically purge expired rows
    {
        let cleanup = server.clone();
        let interval = cli.ttl_cleanup_interval;
        tokio::spawn(async move {
            loop {
                sleep(Duration::from_secs(interval)).await;
                cleanup.cleanup_expired();
            }
        });
    }

    let service = server.serve(stdio()).await?;
    service.waiting().await?;

    Ok(())
}
