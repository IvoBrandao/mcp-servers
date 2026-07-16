// mcp-db — SQLite database operations MCP server
//
// All tool methods return `String`.  Errors are returned as strings that begin
// with "Error: …" so the LLM can report them gracefully without panicking.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use clap::Parser;
use rmcp::{handler::server::wrapper::Parameters, schemars, tool, tool_router};
use rmcp::{ServiceExt, transport::stdio};
use rusqlite::{params_from_iter, types::Value as SqlValue, Connection};
use serde::Deserialize;
use serde_json::{json, Value as JsonValue};
use tracing::{info, warn};

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser, Debug)]
#[command(name = "mcp-db", about = "SQLite database MCP server")]
struct Args {
    /// Restrict database files to this directory (created if needed). Omit to allow any path.
    #[arg(long)]
    root: Option<PathBuf>,
}

// ---------------------------------------------------------------------------
// Thread-safety shim
// ---------------------------------------------------------------------------
// `rusqlite::Connection` is `!Send` because it holds raw pointers.  When
// SQLite is compiled in serialized mode (SQLITE_THREADSAFE=1, the default for
// the bundled feature) the library itself serializes all API calls.  Our
// `Mutex` additionally ensures only one thread touches the connection at a
// time, making this wrapper sound.
struct SendConn(Connection);

impl std::fmt::Debug for SendConn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SendConn").finish_non_exhaustive()
    }
}
unsafe impl Send for SendConn {}
unsafe impl Sync for SendConn {}

impl std::ops::Deref for SendConn {
    type Target = Connection;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl std::ops::DerefMut for SendConn {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------
type ConnMap = Arc<RwLock<HashMap<String, Arc<Mutex<SendConn>>>>>;

#[derive(Debug, Clone)]
struct DbServer {
    connections: ConnMap,
    root: Option<PathBuf>,
}

impl DbServer {
    fn new(root: Option<PathBuf>) -> Self {
        Self {
            connections: Arc::new(RwLock::new(HashMap::new())),
            root,
        }
    }
}

// ---------------------------------------------------------------------------
// Parameter structs
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct NoParams {}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct OpenParams {
    #[schemars(description = "Path to the SQLite database file (created if it does not exist)")]
    path: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct PathParams {
    #[schemars(description = "Path to the SQLite database file")]
    path: String,
}

/// Reused by both db_query and db_execute (same shape, different semantics).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SqlParams {
    #[schemars(description = "Path to the SQLite database file (must be open via db_open)")]
    path: String,
    #[schemars(description = "SQL statement to execute")]
    sql: String,
    #[schemars(
        description = "Optional JSON array of bind parameters, e.g. [42, \"text\", null, 3.14]"
    )]
    params: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ImportCsvParams {
    #[schemars(description = "Path to the SQLite database file")]
    path: String,
    #[schemars(description = "Target table name (created as all-TEXT if it does not exist)")]
    table: String,
    #[schemars(description = "CSV data as a string")]
    csv_data: String,
    #[schemars(description = "Whether the first CSV row contains column names")]
    has_header: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct TableParams {
    #[schemars(description = "Path to the SQLite database file")]
    path: String,
    #[schemars(description = "Table name")]
    table: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct BackupParams {
    #[schemars(description = "Path to the source SQLite database file")]
    path: String,
    #[schemars(description = "Destination path for the backup (created or overwritten)")]
    backup_path: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct TransactionParams {
    #[schemars(description = "Path to the SQLite database file")]
    path: String,
    #[schemars(
        description = "JSON array of SQL statements to execute atomically, e.g. [\"INSERT INTO t VALUES (1)\", \"UPDATE t SET x=2\"]"
    )]
    statements_json: String,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Resolve `path` to an absolute, normalised key (no `..` or `.`).
/// Does NOT require the path to exist, so it works for new databases.
/// If `root` is `Some`, the resolved path must be inside the root directory.
fn resolve_db_path(path: &str, root: Option<&Path>) -> Result<String, String> {
    let p = std::path::Path::new(path);
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| format!("Error: cannot get cwd: {e}"))?
            .join(p)
    };
    let mut out = std::path::PathBuf::new();
    for c in abs.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    if let Some(r) = root {
        if !out.starts_with(r) {
            return Err(format!(
                "Error: path '{}' is outside the allowed root directory '{}'",
                out.display(),
                r.display()
            ));
        }
    }
    Ok(out.to_string_lossy().into_owned())
}

/// Look up an open connection, returning an `Arc<Mutex<SendConn>>` or an
/// error string.
fn get_conn(map: &ConnMap, path: &str) -> Result<Arc<Mutex<SendConn>>, String> {
    // Use no-root version here since root enforcement happens at open time.
    let key = resolve_db_path(path, None)?;
    map.read()
        .map_err(|_| "Error: connection map RwLock poisoned".to_string())?
        .get(&key)
        .cloned()
        .ok_or_else(|| format!("Error: database '{path}' is not open — call db_open first."))
}

/// Parse an optional JSON array of bind parameters into `Vec<SqlValue>`.
fn parse_bind_params(json_opt: Option<String>) -> Result<Vec<SqlValue>, String> {
    let s = match json_opt {
        Some(s) => s,
        None => return Ok(vec![]),
    };
    let v: JsonValue =
        serde_json::from_str(&s).map_err(|e| format!("Error: params is not valid JSON: {e}"))?;
    v.as_array()
        .ok_or_else(|| "Error: params must be a JSON array".to_string())?
        .iter()
        .map(|item| match item {
            JsonValue::Null => Ok(SqlValue::Null),
            JsonValue::Bool(b) => Ok(SqlValue::Integer(*b as i64)),
            JsonValue::Number(n) => n
                .as_i64()
                .map(SqlValue::Integer)
                .or_else(|| n.as_f64().map(SqlValue::Real))
                .ok_or_else(|| format!("Error: cannot represent {n} as an SQLite value")),
            JsonValue::String(s) => Ok(SqlValue::Text(s.clone())),
            other => Err(format!("Error: unsupported bind param type: {other}")),
        })
        .collect()
}

/// Convert a `rusqlite` value to `serde_json::Value`.
/// Text > 64 KiB and blobs > 64 KiB are truncated with a descriptive prefix.
fn sql_to_json(v: SqlValue) -> JsonValue {
    match v {
        SqlValue::Null => JsonValue::Null,
        SqlValue::Integer(i) => json!(i),
        SqlValue::Real(f) => serde_json::Number::from_f64(f)
            .map(JsonValue::Number)
            .unwrap_or(JsonValue::Null), // NaN / Inf → null
        SqlValue::Text(s) => {
            if s.len() > 65_536 {
                json!(format!(
                    "[TEXT {len}B, truncated]: {preview}…",
                    len = s.len(),
                    preview = &s[..1024]
                ))
            } else {
                json!(s)
            }
        }
        SqlValue::Blob(b) => {
            if b.len() > 65_536 {
                json!(format!(
                    "[BLOB {len}B, base64 preview]: {preview}",
                    len = b.len(),
                    preview = BASE64.encode(&b[..1024])
                ))
            } else {
                json!(format!("[BLOB {}B]: {}", b.len(), BASE64.encode(&b)))
            }
        }
    }
}

/// Encode a single CSV field, quoting when necessary.
fn csv_encode(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_owned()
    }
}

/// Parse a single CSV line into fields, handling quoted fields and `""` escaping.
fn parse_csv_row(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        if quoted {
            if ch == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next(); // consume second quote
                    field.push('"');
                } else {
                    quoted = false;
                }
            } else {
                field.push(ch);
            }
        } else if ch == '"' {
            quoted = true;
        } else if ch == ',' {
            fields.push(std::mem::take(&mut field));
        } else {
            field.push(ch);
        }
    }
    fields.push(field);
    fields
}

/// Validate a table or column name to prevent SQL injection via identifier injection.
fn validate_ident(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("Error: identifier is empty".into());
    }
    let all_ok = name
        .chars()
        .all(|c| c.is_alphanumeric() || c == '_');
    let starts_digit = name.chars().next().unwrap().is_ascii_digit();
    if !all_ok || starts_digit {
        return Err(format!(
            "Error: '{name}' is not a valid SQL identifier \
             (use letters, digits, underscores; cannot start with a digit)"
        ));
    }
    Ok(())
}

/// Sanitize a CSV column header that fails validate_ident() into a safe identifier.
fn sanitize_col_name(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '_' { c } else { '_' })
        .collect();
    if out.starts_with(|c: char| c.is_ascii_digit()) {
        out.insert(0, 'c');
    }
    if out.is_empty() {
        "col".to_string()
    } else {
        out
    }
}

// ---------------------------------------------------------------------------
// Tool implementations
// ---------------------------------------------------------------------------

#[tool_router(server_handler)]
impl DbServer {
    // ── 1. db_open ─────────────────────────────────────────────────────────

    #[tool(
        description = "Open (or create) a SQLite database file and register it for subsequent \
        operations. Enables WAL journal mode and foreign-key enforcement. Must be called before \
        any other db_* tool for that file."
    )]
    fn db_open(&self, Parameters(OpenParams { path }): Parameters<OpenParams>) -> String {
        let key = match resolve_db_path(&path, self.root.as_deref()) {
            Ok(k) => k,
            Err(e) => return e,
        };

        // Return early if already open
        {
            let map = match self.connections.read() {
                Ok(m) => m,
                Err(_) => return "Error: connection map lock poisoned".into(),
            };
            if map.contains_key(&key) {
                let size = std::fs::metadata(&key).map(|m| m.len()).unwrap_or(0);
                return json!({
                    "status": "already_open",
                    "path": key,
                    "file_size_bytes": size
                })
                .to_string();
            }
        }

        let conn = match Connection::open(&key) {
            Ok(c) => c,
            Err(e) => return format!("Error: cannot open '{key}': {e}"),
        };
        if let Err(e) = conn.execute_batch(
            "PRAGMA journal_mode = WAL; \
             PRAGMA synchronous  = NORMAL; \
             PRAGMA foreign_keys = ON; \
             PRAGMA temp_store   = MEMORY;",
        ) {
            return format!("Error: PRAGMA setup failed: {e}");
        }

        let size = std::fs::metadata(&key).map(|m| m.len()).unwrap_or(0);

        match self.connections.write() {
            Ok(mut map) => {
                map.insert(key.clone(), Arc::new(Mutex::new(SendConn(conn))));
            }
            Err(_) => return "Error: connection map lock poisoned".into(),
        }
        info!("Opened: {key}");
        json!({
            "status": "ok",
            "path": key,
            "file_size_bytes": size,
            "journal_mode": "WAL"
        })
        .to_string()
    }

    // ── 2. db_list ─────────────────────────────────────────────────────────

    #[tool(description = "List all currently open database connections with their file sizes.")]
    fn db_list(&self, Parameters(NoParams {}): Parameters<NoParams>) -> String {
        let map = match self.connections.read() {
            Ok(m) => m,
            Err(_) => return "Error: lock poisoned".into(),
        };
        let dbs: Vec<JsonValue> = map
            .keys()
            .map(|k| {
                let size = std::fs::metadata(k).map(|m| m.len()).unwrap_or(0);
                json!({ "path": k, "file_size_bytes": size })
            })
            .collect();
        json!({ "count": dbs.len(), "databases": dbs }).to_string()
    }

    // ── 3. db_close ────────────────────────────────────────────────────────

    #[tool(description = "Close a database connection. The file is not deleted.")]
    fn db_close(&self, Parameters(PathParams { path }): Parameters<PathParams>) -> String {
        let key = match resolve_db_path(&path, self.root.as_deref()) {
            Ok(k) => k,
            Err(e) => return e,
        };
        match self.connections.write() {
            Ok(mut map) => {
                if map.remove(&key).is_some() {
                    info!("Closed: {key}");
                    json!({ "status": "ok", "path": key }).to_string()
                } else {
                    format!("Error: database '{path}' is not open")
                }
            }
            Err(_) => "Error: lock poisoned".into(),
        }
    }

    // ── 4. db_query ────────────────────────────────────────────────────────

    #[tool(
        description = "Execute a SELECT query. Returns up to 1000 rows as a JSON array of \
        objects keyed by column name. Large text/blob values are truncated. Use the params field \
        for parameterised queries: e.g. sql=\"SELECT * FROM t WHERE id=?\" params=\"[42]\""
    )]
    fn db_query(
        &self,
        Parameters(SqlParams { path, sql, params }): Parameters<SqlParams>,
    ) -> String {
        let arc = match get_conn(&self.connections, &path) {
            Ok(a) => a,
            Err(e) => return e,
        };
        let bind = match parse_bind_params(params) {
            Ok(p) => p,
            Err(e) => return e,
        };
        let conn = match arc.lock() {
            Ok(c) => c,
            Err(_) => return "Error: connection mutex poisoned".into(),
        };

        let mut stmt = match conn.prepare(&sql) {
            Ok(s) => s,
            Err(e) => return format!("Error: prepare failed: {e}"),
        };
        let col_names: Vec<String> = stmt
            .column_names()
            .iter()
            .map(|s| s.to_string())
            .collect();
        let ncols = col_names.len();

        let mut rows_out: Vec<JsonValue> = Vec::new();
        let mut truncated = false;

        let mut result_rows = match stmt.query(params_from_iter(bind.iter())) {
            Ok(r) => r,
            Err(e) => return format!("Error: query failed: {e}"),
        };
        loop {
            match result_rows.next() {
                Err(e) => return format!("Error: row error: {e}"),
                Ok(None) => break,
                Ok(Some(row)) => {
                    if rows_out.len() >= 1000 {
                        truncated = true;
                        break;
                    }
                    let mut obj = serde_json::Map::with_capacity(ncols);
                    for i in 0..ncols {
                        let v: SqlValue = match row.get(i) {
                            Ok(v) => v,
                            Err(e) => return format!("Error: reading column {i}: {e}"),
                        };
                        obj.insert(col_names[i].clone(), sql_to_json(v));
                    }
                    rows_out.push(JsonValue::Object(obj));
                }
            }
        }

        let count = rows_out.len();
        json!({
            "columns":   col_names,
            "rows":      rows_out,
            "count":     count,
            "truncated": truncated
        })
        .to_string()
    }

    // ── 5. db_execute ──────────────────────────────────────────────────────

    #[tool(
        description = "Execute an INSERT, UPDATE, DELETE, CREATE TABLE, DROP, ALTER, or other \
        non-SELECT SQL statement. Returns rows_affected and last_insert_rowid."
    )]
    fn db_execute(
        &self,
        Parameters(SqlParams { path, sql, params }): Parameters<SqlParams>,
    ) -> String {
        let arc = match get_conn(&self.connections, &path) {
            Ok(a) => a,
            Err(e) => return e,
        };
        let bind = match parse_bind_params(params) {
            Ok(p) => p,
            Err(e) => return e,
        };
        let conn = match arc.lock() {
            Ok(c) => c,
            Err(_) => return "Error: connection mutex poisoned".into(),
        };
        match conn.execute(&sql, params_from_iter(bind.iter())) {
            Ok(n) => json!({
                "status":           "ok",
                "rows_affected":    n,
                "last_insert_rowid": conn.last_insert_rowid()
            })
            .to_string(),
            Err(e) => format!("Error: execute failed: {e}"),
        }
    }

    // ── 6. db_schema ───────────────────────────────────────────────────────

    #[tool(
        description = "Return the full database schema: CREATE TABLE / VIEW / INDEX / TRIGGER \
        statements for every named object."
    )]
    fn db_schema(&self, Parameters(PathParams { path }): Parameters<PathParams>) -> String {
        let arc = match get_conn(&self.connections, &path) {
            Ok(a) => a,
            Err(e) => return e,
        };
        let conn = match arc.lock() {
            Ok(c) => c,
            Err(_) => return "Error: mutex poisoned".into(),
        };

        let mut stmt = match conn.prepare(
            "SELECT type, name, sql FROM sqlite_master \
             WHERE sql IS NOT NULL ORDER BY type, name",
        ) {
            Ok(s) => s,
            Err(e) => return format!("Error: {e}"),
        };

        let mut objects: Vec<JsonValue> = Vec::new();
        let mut rows = match stmt.query([]) {
            Ok(r) => r,
            Err(e) => return format!("Error: {e}"),
        };
        loop {
            match rows.next() {
                Err(e) => return format!("Error: {e}"),
                Ok(None) => break,
                Ok(Some(row)) => {
                    let typ: String = match row.get(0) {
                        Ok(v) => v,
                        Err(e) => return format!("Error: {e}"),
                    };
                    let name: String = match row.get(1) {
                        Ok(v) => v,
                        Err(e) => return format!("Error: {e}"),
                    };
                    let sql: String = match row.get(2) {
                        Ok(v) => v,
                        Err(e) => return format!("Error: {e}"),
                    };
                    objects.push(json!({ "type": typ, "name": name, "sql": sql }));
                }
            }
        }
        json!({ "count": objects.len(), "schema": objects }).to_string()
    }

    // ── 7. db_tables ───────────────────────────────────────────────────────

    #[tool(
        description = "List all user tables in the database together with their row counts."
    )]
    fn db_tables(&self, Parameters(PathParams { path }): Parameters<PathParams>) -> String {
        let arc = match get_conn(&self.connections, &path) {
            Ok(a) => a,
            Err(e) => return e,
        };
        let conn = match arc.lock() {
            Ok(c) => c,
            Err(_) => return "Error: mutex poisoned".into(),
        };

        let mut stmt = match conn.prepare(
            "SELECT name FROM sqlite_master \
             WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        ) {
            Ok(s) => s,
            Err(e) => return format!("Error: {e}"),
        };

        // Collect names first so we can drop the borrow on stmt before running COUNT queries.
        let names: Vec<String> = {
            let mut rows = match stmt.query([]) {
                Ok(r) => r,
                Err(e) => return format!("Error: {e}"),
            };
            let mut v = Vec::new();
            loop {
                match rows.next() {
                    Err(e) => return format!("Error: {e}"),
                    Ok(None) => break,
                    Ok(Some(row)) => {
                        let n: String = match row.get(0) {
                            Ok(v) => v,
                            Err(e) => return format!("Error: {e}"),
                        };
                        v.push(n);
                    }
                }
            }
            v
        };
        drop(stmt);

        let mut tables: Vec<JsonValue> = Vec::new();
        for name in &names {
            // `name` came from sqlite_master so it is a real table name;
            // we quote it anyway to handle weird-but-valid names.
            let count: i64 = conn
                .query_row(&format!("SELECT COUNT(*) FROM \"{name}\""), [], |r| r.get(0))
                .unwrap_or_else(|e| {
                    warn!("COUNT(*) on {name} failed: {e}");
                    -1
                });
            tables.push(json!({ "name": name, "row_count": count }));
        }
        json!({ "count": tables.len(), "tables": tables }).to_string()
    }

    // ── 8. db_import_csv ───────────────────────────────────────────────────

    #[tool(
        description = "Import CSV data into a SQLite table. The table is created with TEXT \
        columns if it does not already exist. Runs in a single transaction. Returns rows imported."
    )]
    fn db_import_csv(
        &self,
        Parameters(ImportCsvParams {
            path,
            table,
            csv_data,
            has_header,
        }): Parameters<ImportCsvParams>,
    ) -> String {
        if let Err(e) = validate_ident(&table) {
            return e;
        }
        let arc = match get_conn(&self.connections, &path) {
            Ok(a) => a,
            Err(e) => return e,
        };
        let mut conn = match arc.lock() {
            Ok(c) => c,
            Err(_) => return "Error: mutex poisoned".into(),
        };

        let all_lines: Vec<&str> = csv_data
            .lines()
            .filter(|l| !l.trim().is_empty())
            .collect();
        if all_lines.is_empty() {
            return "Error: CSV data is empty".into();
        }

        // Determine headers and the slice of data rows.
        let (headers, data_rows): (Vec<String>, &[&str]) = if has_header {
            (parse_csv_row(all_lines[0]), &all_lines[1..])
        } else {
            let ncols = parse_csv_row(all_lines[0]).len();
            let hdrs = (1..=ncols).map(|i| format!("col{i}")).collect();
            (hdrs, all_lines.as_slice())
        };

        // Enforce row limit.
        const MAX_CSV_ROWS: usize = 100_000;
        if data_rows.len() > MAX_CSV_ROWS {
            return format!(
                "Error: CSV has {} data rows, maximum is {}",
                data_rows.len(),
                MAX_CSV_ROWS
            );
        }

        // Sanitize column headers to prevent SQL injection.
        let safe_headers: Vec<String> = headers
            .iter()
            .map(|h| {
                if validate_ident(h).is_ok() {
                    h.clone()
                } else {
                    sanitize_col_name(h)
                }
            })
            .collect();

        // CREATE TABLE IF NOT EXISTS with TEXT columns.
        let col_defs: String = safe_headers
            .iter()
            .map(|h| format!("\"{h}\" TEXT"))
            .collect::<Vec<_>>()
            .join(", ");
        let create_sql = format!("CREATE TABLE IF NOT EXISTS \"{table}\" ({col_defs})");
        if let Err(e) = conn.execute_batch(&create_sql) {
            return format!("Error: cannot create table '{table}': {e}");
        }

        let ph: String = safe_headers
            .iter()
            .map(|_| "?")
            .collect::<Vec<_>>()
            .join(", ");
        let insert_sql = format!("INSERT INTO \"{table}\" VALUES ({ph})");

        let mut imported: i64 = 0;
        let tx = match conn.transaction() {
            Ok(t) => t,
            Err(e) => return format!("Error: cannot begin transaction: {e}"),
        };
        for (i, &line) in data_rows.iter().enumerate() {
            let vals: Vec<SqlValue> = parse_csv_row(line)
                .into_iter()
                .map(SqlValue::Text)
                .collect();
            if let Err(e) = tx.execute(&insert_sql, params_from_iter(vals.iter())) {
                return format!("Error: insert failed at data row {}: {e}", i + 1);
            }
            imported += 1;
        }
        if let Err(e) = tx.commit() {
            return format!("Error: commit failed: {e}");
        }

        json!({
            "status":       "ok",
            "rows_imported": imported,
            "table":        table,
            "columns":      safe_headers
        })
        .to_string()
    }

    // ── 9. db_export_csv ───────────────────────────────────────────────────

    #[tool(description = "Export all rows from a table as CSV text (with a header row).")]
    fn db_export_csv(
        &self,
        Parameters(TableParams { path, table }): Parameters<TableParams>,
    ) -> String {
        if let Err(e) = validate_ident(&table) {
            return e;
        }
        let arc = match get_conn(&self.connections, &path) {
            Ok(a) => a,
            Err(e) => return e,
        };
        let conn = match arc.lock() {
            Ok(c) => c,
            Err(_) => return "Error: mutex poisoned".into(),
        };

        let sql = format!("SELECT * FROM \"{table}\"");
        let mut stmt = match conn.prepare(&sql) {
            Ok(s) => s,
            Err(e) => return format!("Error: {e}"),
        };
        let col_names: Vec<String> = stmt
            .column_names()
            .iter()
            .map(|s| s.to_string())
            .collect();
        let ncols = col_names.len();

        let mut out = String::new();
        // Header
        out.push_str(
            &col_names
                .iter()
                .map(|c| csv_encode(c))
                .collect::<Vec<_>>()
                .join(","),
        );
        out.push('\n');

        let mut rows = match stmt.query([]) {
            Ok(r) => r,
            Err(e) => return format!("Error: {e}"),
        };
        loop {
            match rows.next() {
                Err(e) => return format!("Error: {e}"),
                Ok(None) => break,
                Ok(Some(row)) => {
                    let fields: Vec<String> = (0..ncols)
                        .map(|i| {
                            let v: SqlValue = row.get(i).unwrap_or(SqlValue::Null);
                            match v {
                                SqlValue::Null => String::new(),
                                SqlValue::Integer(n) => n.to_string(),
                                SqlValue::Real(f) => f.to_string(),
                                SqlValue::Text(s) => csv_encode(&s),
                                SqlValue::Blob(b) => csv_encode(&BASE64.encode(&b)),
                            }
                        })
                        .collect();
                    out.push_str(&fields.join(","));
                    out.push('\n');
                }
            }
        }
        out
    }

    // ── 10. db_backup ──────────────────────────────────────────────────────

    #[tool(
        description = "Create a file-level backup of the database. A WAL checkpoint is run \
        first to ensure the backup includes all committed data. The destination file is created \
        or overwritten."
    )]
    fn db_backup(
        &self,
        Parameters(BackupParams { path, backup_path }): Parameters<BackupParams>,
    ) -> String {
        let src = match resolve_db_path(&path, self.root.as_deref()) {
            Ok(k) => k,
            Err(e) => return e,
        };
        let dst = match resolve_db_path(&backup_path, self.root.as_deref()) {
            Ok(k) => k,
            Err(e) => return e,
        };
        if src == dst {
            return "Error: source and destination are the same path".into();
        }

        // Checkpoint WAL before copying so the main DB file is up-to-date.
        {
            let map = match self.connections.read() {
                Ok(m) => m,
                Err(_) => return "Error: lock poisoned".into(),
            };
            if let Some(arc) = map.get(&src) {
                if let Ok(conn) = arc.lock() {
                    let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
                }
            }
        }

        match std::fs::copy(&src, &dst) {
            Ok(bytes) => json!({
                "status":      "ok",
                "source":      src,
                "destination": dst,
                "bytes_copied": bytes
            })
            .to_string(),
            Err(e) => format!("Error: backup failed: {e}"),
        }
    }

    // ── 11. db_vacuum ──────────────────────────────────────────────────────

    #[tool(
        description = "Run VACUUM to rebuild the database file, reclaiming unused space and \
        defragmenting pages. May take time on large databases."
    )]
    fn db_vacuum(&self, Parameters(PathParams { path }): Parameters<PathParams>) -> String {
        let arc = match get_conn(&self.connections, &path) {
            Ok(a) => a,
            Err(e) => return e,
        };
        let conn = match arc.lock() {
            Ok(c) => c,
            Err(_) => return "Error: mutex poisoned".into(),
        };
        match conn.execute_batch("VACUUM;") {
            Ok(_) => {
                let key = match resolve_db_path(&path, self.root.as_deref()) {
                    Ok(k) => k,
                    Err(e) => return e,
                };
                let size = std::fs::metadata(&key).map(|m| m.len()).unwrap_or(0);
                json!({ "status": "ok", "path": key, "file_size_bytes": size }).to_string()
            }
            Err(e) => format!("Error: VACUUM failed: {e}"),
        }
    }

    // ── 12. db_transaction ─────────────────────────────────────────────────

    #[tool(
        description = "Execute multiple SQL statements atomically. If any statement fails, all \
        changes are rolled back. statements_json must be a JSON array of SQL strings, e.g. \
        [\"INSERT INTO t VALUES (1)\", \"UPDATE t SET x=2 WHERE id=1\"]"
    )]
    fn db_transaction(
        &self,
        Parameters(TransactionParams {
            path,
            statements_json,
        }): Parameters<TransactionParams>,
    ) -> String {
        let stmts: Vec<String> = match serde_json::from_str(&statements_json) {
            Ok(v) => v,
            Err(e) => {
                return format!(
                    "Error: statements_json must be a JSON array of strings: {e}"
                )
            }
        };
        if stmts.is_empty() {
            return json!({ "status": "ok", "statements_executed": 0 }).to_string();
        }

        let arc = match get_conn(&self.connections, &path) {
            Ok(a) => a,
            Err(e) => return e,
        };
        let mut conn = match arc.lock() {
            Ok(c) => c,
            Err(_) => return "Error: mutex poisoned".into(),
        };

        let tx = match conn.transaction() {
            Ok(t) => t,
            Err(e) => return format!("Error: cannot begin transaction: {e}"),
        };
        for (i, stmt) in stmts.iter().enumerate() {
            if let Err(e) = tx.execute_batch(stmt) {
                // Dropping `tx` without committing triggers automatic rollback.
                return format!(
                    "Error: statement {} failed: {e} — all changes rolled back.",
                    i + 1
                );
            }
        }
        match tx.commit() {
            Ok(_) => json!({
                "status":              "ok",
                "statements_executed": stmts.len()
            })
            .to_string(),
            Err(e) => format!("Error: commit failed: {e}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let args = Args::parse();

    // Canonicalize root directory and create it if necessary.
    let root = if let Some(r) = args.root {
        std::fs::create_dir_all(&r)?;
        Some(r.canonicalize()?)
    } else {
        None
    };

    let service = DbServer::new(root).serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
