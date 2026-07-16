use std::{
    collections::HashMap,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::Arc,
    time::SystemTime,
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use clap::Parser;
use flate2::{Compression, read::GzDecoder, write::GzEncoder};
use regex::Regex;
use rmcp::{ServiceExt, handler::server::wrapper::Parameters, schemars, tool, tool_router, transport::stdio};
use schemars::JsonSchema;
use serde::Deserialize;
use md5::Digest as Md5Digest;
use sha2::Sha256;
use tokio::sync::RwLock;
use walkdir::WalkDir;
use zip::{ZipWriter, write::SimpleFileOptions};

// ── CLI ──────────────────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(name = "mcp-fs", about = "Sandboxed file system MCP server")]
struct Cli {
    /// Root directory for the sandbox (all operations are restricted to this path)
    #[arg(long, default_value = ".")]
    root: PathBuf,

    /// Maximum file size in bytes for reads/writes (default: 100 MB)
    #[arg(long, default_value_t = 100 * 1024 * 1024)]
    max_file_size: u64,
}

// ── Parameter structs ─────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
struct ReadFileParams {
    #[schemars(description = "Path to the file to read (relative to sandbox root or virtual cwd)")]
    path: String,
    #[schemars(description = "Byte offset to start reading from (optional)")]
    offset: Option<u64>,
    #[schemars(description = "Maximum number of bytes to read (optional)")]
    limit: Option<u64>,
    #[schemars(description = "Return file content as base64-encoded string")]
    base64_output: Option<bool>,
    #[schemars(description = "Text encoding hint, e.g. utf-8 (currently informational)")]
    #[allow(dead_code)]
    encoding: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct WriteFileParams {
    #[schemars(description = "Path to write the file")]
    path: String,
    #[schemars(description = "Content to write")]
    content: String,
    #[schemars(description = "Append to file instead of overwriting")]
    append: Option<bool>,
    #[schemars(description = "Treat content as base64-encoded bytes")]
    base64_input: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CdParams {
    #[schemars(description = "Directory path to change to")]
    path: String,
    #[schemars(description = "Session identifier for tracking virtual cwd")]
    session_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ListDirectoryParams {
    #[schemars(description = "Directory path to list (defaults to sandbox root or virtual cwd)")]
    path: Option<String>,
    #[schemars(description = "Show detailed info: size, permissions, timestamps")]
    detailed: Option<bool>,
    #[schemars(description = "Session identifier for resolving relative paths")]
    session_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CreateDirectoryParams {
    #[schemars(description = "Path of directory to create (including any intermediate directories)")]
    path: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CopyItemParams {
    #[schemars(description = "Source path")]
    source: String,
    #[schemars(description = "Destination path")]
    destination: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct MoveItemParams {
    #[schemars(description = "Source path")]
    source: String,
    #[schemars(description = "Destination path")]
    destination: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct DeleteItemParams {
    #[schemars(description = "Path to delete")]
    path: String,
    #[schemars(description = "Recursively delete directories")]
    recursive: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GetFileInfoParams {
    #[schemars(description = "Path to inspect")]
    path: String,
    #[schemars(description = "Compute MD5 and SHA-256 hashes (slower for large files)")]
    include_hash: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SearchFilesParams {
    #[schemars(description = "Glob pattern to search for, e.g. **/*.rs")]
    pattern: String,
    #[schemars(description = "Session identifier for resolving relative base path")]
    session_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GrepFilesParams {
    #[schemars(description = "Regular expression pattern to search for")]
    pattern: String,
    #[schemars(description = "Directory to search in (defaults to sandbox root)")]
    path: Option<String>,
    #[schemars(description = "Session identifier for resolving relative paths")]
    session_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SetPermissionsParams {
    #[schemars(description = "Path to change permissions for")]
    path: String,
    #[schemars(description = "Unix permission mode as octal number, e.g. 0o644 = 420")]
    mode: u32,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct FileExistsParams {
    #[schemars(description = "Path to check")]
    path: String,
    #[schemars(description = "Session identifier for resolving relative paths")]
    session_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CompressParams {
    #[schemars(description = "Source file or directory to compress")]
    source: String,
    #[schemars(description = "Destination archive path")]
    destination: String,
    #[schemars(description = "Archive format: zip or tar.gz")]
    format: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct DecompressParams {
    #[schemars(description = "Archive file to extract")]
    source: String,
    #[schemars(description = "Destination directory (defaults to parent of archive)")]
    destination: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct BatchOperationsParams {
    #[schemars(
        description = r#"JSON array of operations: [{"op": "copy|move|delete|mkdir|chmod", "params": {...}}]"#
    )]
    operations_json: String,
}

// ── Server ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct FsServer {
    sandbox_root: PathBuf,
    max_file_size: u64,
    sessions: Arc<RwLock<HashMap<String, PathBuf>>>,
}

// ── Path helpers ──────────────────────────────────────────────────────────────

impl FsServer {
    fn new(sandbox_root: PathBuf, max_file_size: u64) -> anyhow::Result<Self> {
        let sandbox_root = sandbox_root.canonicalize()?;
        Ok(Self {
            sandbox_root,
            max_file_size,
            sessions: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    /// Resolve a user-supplied path against a base (session cwd or sandbox root),
    /// ensuring the result stays inside the sandbox.
    fn safe_path(&self, user_path: &str, base: &Path) -> Result<PathBuf, String> {
        let raw = if user_path.starts_with('/') {
            // Absolute path: treat it as relative to sandbox root by stripping the leading /
            self.sandbox_root.join(user_path.trim_start_matches('/'))
        } else {
            base.join(user_path)
        };

        // Normalize without requiring the full path to exist —
        // we canonicalize the parent and append the filename.
        let resolved = if raw.exists() {
            raw.canonicalize()
                .map_err(|e| format!("Cannot resolve path: {e}"))?
        } else {
            let parent = raw.parent().unwrap_or(&raw);
            let file_name = raw.file_name();
            if parent.exists() {
                let canon = parent
                    .canonicalize()
                    .map_err(|e| format!("Cannot resolve parent path: {e}"))?;
                match file_name {
                    Some(n) => canon.join(n),
                    None => canon,
                }
            } else {
                // Parent doesn't exist either — just do a lexical clean
                let components: Vec<_> = raw.components().collect();
                let mut clean = PathBuf::new();
                for c in &components {
                    use std::path::Component;
                    match c {
                        Component::ParentDir => {
                            clean.pop();
                        }
                        Component::CurDir => {}
                        _ => clean.push(c),
                    }
                }
                // Must be absolute
                if !clean.starts_with(&self.sandbox_root) {
                    return Err(format!(
                        "Path escapes sandbox: {}",
                        clean.display()
                    ));
                }
                clean
            }
        };

        if !resolved.starts_with(&self.sandbox_root) {
            return Err(format!(
                "Access denied: path {} is outside sandbox {}",
                resolved.display(),
                self.sandbox_root.display()
            ));
        }
        Ok(resolved)
    }

    async fn resolve(&self, user_path: &str, session_id: Option<&str>) -> Result<PathBuf, String> {
        let base = if let Some(sid) = session_id {
            let sessions = self.sessions.read().await;
            sessions
                .get(sid)
                .cloned()
                .unwrap_or_else(|| self.sandbox_root.clone())
        } else {
            self.sandbox_root.clone()
        };
        self.safe_path(user_path, &base)
    }
}

// ── Permission formatting ─────────────────────────────────────────────────────

fn format_mode(mode: u32) -> String {
    let types = [
        (0o040000, 'd'),
        (0o120000, 'l'),
        (0o010000, 'p'),
        (0o060000, 'b'),
        (0o020000, 'c'),
    ];
    let type_char = types
        .iter()
        .find(|(mask, _)| mode & 0o170000 == *mask)
        .map(|(_, c)| *c)
        .unwrap_or('-');

    let bits = [
        ('r', 0o400),
        ('-', 0),
        ('w', 0o200),
        ('-', 0),
        ('x', 0o100),
        ('-', 0),
        ('r', 0o040),
        ('-', 0),
        ('w', 0o020),
        ('-', 0),
        ('x', 0o010),
        ('-', 0),
        ('r', 0o004),
        ('-', 0),
        ('w', 0o002),
        ('-', 0),
        ('x', 0o001),
        ('-', 0),
    ];

    let mut s = String::with_capacity(10);
    s.push(type_char);
    for chunk in bits.chunks(2) {
        let (set_char, mask) = chunk[0];
        let (unset_char, _) = chunk[1];
        if mode & mask != 0 {
            s.push(set_char);
        } else {
            s.push(unset_char);
        }
    }
    s
}

fn system_time_to_rfc3339(t: SystemTime) -> String {
    let dur = t
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs();
    // Simple ISO-8601 UTC representation
    let dt = chrono_from_secs(secs);
    dt
}

fn chrono_from_secs(secs: u64) -> String {
    // Manual ISO-8601 UTC without chrono dependency
    let secs = secs as i64;
    let mut s = secs;
    let mut days = s / 86400;
    s %= 86400;
    if s < 0 {
        s += 86400;
        days -= 1;
    }
    let hh = s / 3600;
    let mm = (s % 3600) / 60;
    let ss = s % 60;
    // Convert days since epoch (1970-01-01) to year/month/day
    let (year, month, day) = days_to_ymd(days);
    format!("{year:04}-{month:02}-{day:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

fn days_to_ymd(days: i64) -> (i64, u32, u32) {
    // Gregorian calendar calculation
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m as u32, d as u32)
}

// ── Recursive copy helper ─────────────────────────────────────────────────────

fn copy_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    if src.is_dir() {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            let ft = entry.file_type()?;
            let dst_child = dst.join(entry.file_name());
            if ft.is_dir() {
                copy_recursive(&entry.path(), &dst_child)?;
            } else {
                std::fs::copy(entry.path(), dst_child)?;
            }
        }
    } else {
        if let Some(p) = dst.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::copy(src, dst)?;
    }
    Ok(())
}

// ── Zip helpers ───────────────────────────────────────────────────────────────

fn compress_zip(src: &Path, dst: &Path) -> std::io::Result<()> {
    let file = std::fs::File::create(dst)?;
    let mut zip = ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    if src.is_dir() {
        let prefix = src.parent().unwrap_or(src);
        for entry in WalkDir::new(src).into_iter().filter_map(|e| e.ok()) {
            let path = entry.path();
            let rel = path.strip_prefix(prefix).unwrap_or(path);
            let name = rel.to_string_lossy();
            if path.is_dir() {
                zip.add_directory(format!("{name}/"), options)?;
            } else {
                zip.start_file(name.as_ref(), options)?;
                let mut f = std::fs::File::open(path)?;
                std::io::copy(&mut f, &mut zip)?;
            }
        }
    } else {
        let name = src.file_name().unwrap_or_default().to_string_lossy();
        zip.start_file(name.as_ref(), options)?;
        let mut f = std::fs::File::open(src)?;
        std::io::copy(&mut f, &mut zip)?;
    }
    zip.finish()?;
    Ok(())
}

fn compress_tar_gz(src: &Path, dst: &Path) -> std::io::Result<()> {
    let file = std::fs::File::create(dst)?;
    let enc = GzEncoder::new(file, Compression::default());
    let mut tar = tar::Builder::new(enc);
    if src.is_dir() {
        let name = src.file_name().unwrap_or_default().to_string_lossy();
        tar.append_dir_all(name.as_ref(), src)?;
    } else {
        let name = src.file_name().unwrap_or_default().to_string_lossy();
        tar.append_path_with_name(src, name.as_ref())?;
    }
    tar.into_inner()?.finish()?;
    Ok(())
}

fn decompress_zip(src: &Path, dst: &Path) -> std::io::Result<()> {
    let file = std::fs::File::open(src)?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    archive
        .extract(dst)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    Ok(())
}

fn decompress_tar_gz(src: &Path, dst: &Path) -> std::io::Result<()> {
    let file = std::fs::File::open(src)?;
    let dec = GzDecoder::new(file);
    let mut archive = tar::Archive::new(dec);
    archive.unpack(dst)?;
    Ok(())
}

// ── Is-binary heuristic ───────────────────────────────────────────────────────

fn is_binary(buf: &[u8]) -> bool {
    buf.iter().take(8192).any(|&b| b == 0)
}

// ── Tool implementations ──────────────────────────────────────────────────────

#[tool_router(server_handler)]
impl FsServer {
    #[tool(description = "Read a file. Returns text content, or base64 if binary or base64_output=true. Supports byte-range reads via offset/limit.")]
    async fn read_file(
        &self,
        Parameters(ReadFileParams {
            path,
            offset,
            limit,
            base64_output,
            encoding: _,
        }): Parameters<ReadFileParams>,
    ) -> String {
        let resolved = match self.resolve(&path, None).await {
            Ok(p) => p,
            Err(e) => return format!("Error: {e}"),
        };

        if !resolved.exists() {
            return format!("Error: file not found: {}", resolved.display());
        }
        if resolved.is_dir() {
            return "Error: path is a directory, use list_directory instead".to_string();
        }

        let meta = match std::fs::metadata(&resolved) {
            Ok(m) => m,
            Err(e) => return format!("Error: cannot stat file: {e}"),
        };

        let file_size = meta.len();
        if file_size > self.max_file_size {
            return format!(
                "Error: file size {file_size} exceeds max_file_size {}",
                self.max_file_size
            );
        }

        let mut data = match std::fs::read(&resolved) {
            Ok(d) => d,
            Err(e) => return format!("Error: cannot read file: {e}"),
        };

        // Apply byte-range
        let start = offset.unwrap_or(0) as usize;
        if start >= data.len() {
            data = vec![];
        } else {
            data = data[start..].to_vec();
        }
        if let Some(l) = limit {
            data.truncate(l as usize);
        }

        let force_b64 = base64_output.unwrap_or(false);
        if force_b64 || is_binary(&data) {
            format!("base64:{}", BASE64.encode(&data))
        } else {
            match String::from_utf8(data) {
                Ok(s) => s,
                Err(e) => format!("base64:{}", BASE64.encode(e.into_bytes())),
            }
        }
    }

    #[tool(description = "Write content to a file. Supports append mode and base64-encoded binary input.")]
    async fn write_file(
        &self,
        Parameters(WriteFileParams {
            path,
            content,
            append,
            base64_input,
        }): Parameters<WriteFileParams>,
    ) -> String {
        let resolved = match self.resolve(&path, None).await {
            Ok(p) => p,
            Err(e) => return format!("Error: {e}"),
        };

        if let Some(parent) = resolved.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return format!("Error: cannot create parent dirs: {e}");
            }
        }

        let bytes: Vec<u8> = if base64_input.unwrap_or(false) {
            match BASE64.decode(content.trim()) {
                Ok(b) => b,
                Err(e) => return format!("Error: base64 decode failed: {e}"),
            }
        } else {
            content.into_bytes()
        };

        if bytes.len() as u64 > self.max_file_size {
            return format!(
                "Error: content size {} exceeds max_file_size {}",
                bytes.len(),
                self.max_file_size
            );
        }

        let result = if append.unwrap_or(false) {
            use std::io::Write;
            let mut f = match std::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(&resolved)
            {
                Ok(f) => f,
                Err(e) => return format!("Error: cannot open file for append: {e}"),
            };
            f.write_all(&bytes)
        } else {
            std::fs::write(&resolved, &bytes)
        };

        match result {
            Ok(_) => format!("Written {} bytes to {}", bytes.len(), resolved.display()),
            Err(e) => format!("Error: write failed: {e}"),
        }
    }

    #[tool(description = "Change the virtual working directory for a session. Subsequent relative paths in that session resolve against this directory.")]
    async fn cd(&self, Parameters(CdParams { path, session_id }): Parameters<CdParams>) -> String {
        let resolved = match self.resolve(&path, Some(&session_id)).await {
            Ok(p) => p,
            Err(e) => return format!("Error: {e}"),
        };

        if !resolved.exists() {
            return format!("Error: directory not found: {}", resolved.display());
        }
        if !resolved.is_dir() {
            return format!("Error: not a directory: {}", resolved.display());
        }

        let mut sessions = self.sessions.write().await;
        sessions.insert(session_id.clone(), resolved.clone());
        format!("Changed to {}", resolved.display())
    }

    #[tool(description = "List directory contents. With detailed=true shows size, permissions, and timestamps.")]
    async fn list_directory(
        &self,
        Parameters(ListDirectoryParams {
            path,
            detailed,
            session_id,
        }): Parameters<ListDirectoryParams>,
    ) -> String {
        let base_path = match path {
            Some(ref p) => {
                match self.resolve(p, session_id.as_deref()).await {
                    Ok(p) => p,
                    Err(e) => return format!("Error: {e}"),
                }
            }
            None => {
                if let Some(sid) = session_id.as_deref() {
                    let sessions = self.sessions.read().await;
                    sessions
                        .get(sid)
                        .cloned()
                        .unwrap_or_else(|| self.sandbox_root.clone())
                } else {
                    self.sandbox_root.clone()
                }
            }
        };

        if !base_path.exists() {
            return format!("Error: directory not found: {}", base_path.display());
        }
        if !base_path.is_dir() {
            return format!("Error: not a directory: {}", base_path.display());
        }

        let mut entries = match std::fs::read_dir(&base_path) {
            Ok(e) => e
                .filter_map(|r| r.ok())
                .collect::<Vec<_>>(),
            Err(e) => return format!("Error: cannot read directory: {e}"),
        };
        entries.sort_by_key(|e| e.file_name());

        let detail = detailed.unwrap_or(false);
        let mut lines = vec![format!("{}:", base_path.display())];

        for entry in entries {
            let name = entry.file_name().to_string_lossy().to_string();
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(_) => {
                    lines.push(format!("  ? {name}"));
                    continue;
                }
            };

            let icon = if meta.is_dir() { "📁" } else { "📄" };

            if detail {
                let mode = meta.permissions().mode();
                let perm_str = format_mode(mode);
                let size = meta.len();
                let mtime = meta
                    .modified()
                    .map(system_time_to_rfc3339)
                    .unwrap_or_else(|_| "unknown".to_string());
                let _entry_type = if meta.is_dir() {
                    "dir"
                } else if meta.is_symlink() {
                    "link"
                } else {
                    "file"
                };
                lines.push(format!(
                    "  {icon} {perm_str}  {size:>10}  {mtime}  {name}"
                ));
            } else {
                let suffix = if meta.is_dir() { "/" } else { "" };
                lines.push(format!("  {icon} {name}{suffix}"));
            }
        }

        lines.join("\n")
    }

    #[tool(description = "Create a directory (and all parent directories).")]
    async fn create_directory(
        &self,
        Parameters(CreateDirectoryParams { path }): Parameters<CreateDirectoryParams>,
    ) -> String {
        let resolved = match self.resolve(&path, None).await {
            Ok(p) => p,
            Err(e) => return format!("Error: {e}"),
        };

        match std::fs::create_dir_all(&resolved) {
            Ok(_) => format!("Created directory {}", resolved.display()),
            Err(e) => format!("Error: {e}"),
        }
    }

    #[tool(description = "Copy a file or directory to a new location within the sandbox.")]
    async fn copy_item(
        &self,
        Parameters(CopyItemParams { source, destination }): Parameters<CopyItemParams>,
    ) -> String {
        let src = match self.resolve(&source, None).await {
            Ok(p) => p,
            Err(e) => return format!("Error: {e}"),
        };
        let dst = match self.resolve(&destination, None).await {
            Ok(p) => p,
            Err(e) => return format!("Error: {e}"),
        };

        if !src.exists() {
            return format!("Error: source not found: {}", src.display());
        }

        match copy_recursive(&src, &dst) {
            Ok(_) => format!("Copied {} -> {}", src.display(), dst.display()),
            Err(e) => format!("Error: copy failed: {e}"),
        }
    }

    #[tool(description = "Move or rename a file or directory within the sandbox.")]
    async fn move_item(
        &self,
        Parameters(MoveItemParams { source, destination }): Parameters<MoveItemParams>,
    ) -> String {
        let src = match self.resolve(&source, None).await {
            Ok(p) => p,
            Err(e) => return format!("Error: {e}"),
        };
        let dst = match self.resolve(&destination, None).await {
            Ok(p) => p,
            Err(e) => return format!("Error: {e}"),
        };

        if !src.exists() {
            return format!("Error: source not found: {}", src.display());
        }

        // Try rename first (same filesystem), fall back to copy+delete
        let result = std::fs::rename(&src, &dst).or_else(|_| {
            copy_recursive(&src, &dst)?;
            if src.is_dir() {
                std::fs::remove_dir_all(&src)
            } else {
                std::fs::remove_file(&src)
            }
        });

        match result {
            Ok(_) => format!("Moved {} -> {}", src.display(), dst.display()),
            Err(e) => format!("Error: move failed: {e}"),
        }
    }

    #[tool(description = "Delete a file or directory. Use recursive=true to delete directories and their contents.")]
    async fn delete_item(
        &self,
        Parameters(DeleteItemParams { path, recursive }): Parameters<DeleteItemParams>,
    ) -> String {
        let resolved = match self.resolve(&path, None).await {
            Ok(p) => p,
            Err(e) => return format!("Error: {e}"),
        };

        if !resolved.exists() {
            return format!("Error: path not found: {}", resolved.display());
        }

        let result = if resolved.is_dir() {
            if recursive.unwrap_or(false) {
                std::fs::remove_dir_all(&resolved)
            } else {
                std::fs::remove_dir(&resolved)
            }
        } else {
            std::fs::remove_file(&resolved)
        };

        match result {
            Ok(_) => format!("Deleted {}", resolved.display()),
            Err(e) => format!("Error: delete failed: {e}"),
        }
    }

    #[tool(description = "Get detailed metadata about a file or directory, including permissions, timestamps, and optionally MD5/SHA-256 hashes.")]
    async fn get_file_info(
        &self,
        Parameters(GetFileInfoParams { path, include_hash }): Parameters<GetFileInfoParams>,
    ) -> String {
        let resolved = match self.resolve(&path, None).await {
            Ok(p) => p,
            Err(e) => return format!("Error: {e}"),
        };

        let meta = match std::fs::symlink_metadata(&resolved) {
            Ok(m) => m,
            Err(e) => return format!("Error: cannot stat: {e}"),
        };

        let mode = meta.permissions().mode();
        let perm_str = format_mode(mode);
        let is_symlink = meta.file_type().is_symlink();
        let entry_type = if is_symlink {
            "symlink"
        } else if meta.is_dir() {
            "directory"
        } else {
            "file"
        };

        let created = meta
            .created()
            .map(system_time_to_rfc3339)
            .unwrap_or_else(|_| "unavailable".to_string());
        let modified = meta
            .modified()
            .map(system_time_to_rfc3339)
            .unwrap_or_else(|_| "unavailable".to_string());
        let accessed = meta
            .accessed()
            .map(system_time_to_rfc3339)
            .unwrap_or_else(|_| "unavailable".to_string());

        let mut info = serde_json::json!({
            "name": resolved.file_name().unwrap_or_default().to_string_lossy(),
            "type": entry_type,
            "size": meta.len(),
            "permissions": perm_str,
            "mode_octal": format!("{:o}", mode & 0o777),
            "created": created,
            "modified": modified,
            "accessed": accessed,
            "symlink": is_symlink,
            "absolute_path": resolved.to_string_lossy(),
        });

        if include_hash.unwrap_or(false) && meta.is_file() {
            match std::fs::read(&resolved) {
                Ok(data) => {
                    let md5_hash = format!("{:x}", <md5::Md5 as Md5Digest>::digest(&data));
                    let sha256_hash = format!("{:x}", Sha256::digest(&data));
                    info["md5"] = serde_json::Value::String(md5_hash);
                    info["sha256"] = serde_json::Value::String(sha256_hash);
                }
                Err(e) => {
                    info["hash_error"] = serde_json::Value::String(e.to_string());
                }
            }
        }

        serde_json::to_string_pretty(&info).unwrap_or_else(|e| format!("Error: {e}"))
    }

    #[tool(description = "Search for files matching a glob pattern (e.g. **/*.rs) within the sandbox.")]
    async fn search_files(
        &self,
        Parameters(SearchFilesParams { pattern, session_id }): Parameters<SearchFilesParams>,
    ) -> String {
        let base = if let Some(sid) = session_id.as_deref() {
            let sessions = self.sessions.read().await;
            sessions
                .get(sid)
                .cloned()
                .unwrap_or_else(|| self.sandbox_root.clone())
        } else {
            self.sandbox_root.clone()
        };

        // Convert glob pattern to a simple matcher
        // We support: ** (any path segments), * (any filename chars), ? (single char)
        let regex_str = glob_to_regex(&pattern);
        let re = match Regex::new(&regex_str) {
            Ok(r) => r,
            Err(e) => return format!("Error: invalid pattern: {e}"),
        };

        let mut matches = Vec::new();
        for entry in WalkDir::new(&base).into_iter().filter_map(|e| e.ok()) {
            let path = entry.path();
            let rel = path
                .strip_prefix(&self.sandbox_root)
                .unwrap_or(path)
                .to_string_lossy()
                .to_string();

            if re.is_match(&rel) {
                matches.push(format!("/{rel}"));
            }
            if matches.len() >= 1000 {
                matches.push("... (truncated at 1000 results)".to_string());
                break;
            }
        }

        if matches.is_empty() {
            format!("No files found matching pattern: {pattern}")
        } else {
            matches.join("\n")
        }
    }

    #[tool(description = "Search file contents with a regex pattern. Returns file:line:match for each hit. Skips binary files. Limited to 500 matches.")]
    async fn grep_files(
        &self,
        Parameters(GrepFilesParams {
            pattern,
            path,
            session_id,
        }): Parameters<GrepFilesParams>,
    ) -> String {
        let search_root = match path {
            Some(ref p) => match self.resolve(p, session_id.as_deref()).await {
                Ok(p) => p,
                Err(e) => return format!("Error: {e}"),
            },
            None => {
                if let Some(sid) = session_id.as_deref() {
                    let sessions = self.sessions.read().await;
                    sessions
                        .get(sid)
                        .cloned()
                        .unwrap_or_else(|| self.sandbox_root.clone())
                } else {
                    self.sandbox_root.clone()
                }
            }
        };

        let re = match Regex::new(&pattern) {
            Ok(r) => r,
            Err(e) => return format!("Error: invalid regex: {e}"),
        };

        let mut results = Vec::new();
        let mut total = 0usize;

        'outer: for entry in WalkDir::new(&search_root)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
        {
            let path = entry.path();
            let data = match std::fs::read(path) {
                Ok(d) => d,
                Err(_) => continue,
            };

            if is_binary(&data) {
                continue;
            }

            let text = match std::str::from_utf8(&data) {
                Ok(t) => t,
                Err(_) => continue,
            };

            let rel = path
                .strip_prefix(&self.sandbox_root)
                .unwrap_or(path)
                .to_string_lossy()
                .to_string();

            for (lineno, line) in text.lines().enumerate() {
                if re.is_match(line) {
                    results.push(format!("/{rel}:{}:{}", lineno + 1, line.trim_end()));
                    total += 1;
                    if total >= 500 {
                        results.push("... (truncated at 500 matches)".to_string());
                        break 'outer;
                    }
                }
            }
        }

        if results.is_empty() {
            format!("No matches found for pattern: {pattern}")
        } else {
            results.join("\n")
        }
    }

    #[tool(description = "Set Unix permissions on a file or directory. Mode is specified as a decimal integer (e.g. 420 = 0o644, 493 = 0o755).")]
    async fn set_permissions(
        &self,
        Parameters(SetPermissionsParams { path, mode }): Parameters<SetPermissionsParams>,
    ) -> String {
        let resolved = match self.resolve(&path, None).await {
            Ok(p) => p,
            Err(e) => return format!("Error: {e}"),
        };

        if !resolved.exists() {
            return format!("Error: path not found: {}", resolved.display());
        }

        let perms = std::fs::Permissions::from_mode(mode);
        match std::fs::set_permissions(&resolved, perms) {
            Ok(_) => format!(
                "Set permissions on {} to {:o}",
                resolved.display(),
                mode & 0o777
            ),
            Err(e) => format!("Error: set_permissions failed: {e}"),
        }
    }

    #[tool(description = "Get disk usage statistics for the sandbox root: total, used, and free bytes.")]
    async fn disk_usage(&self) -> String {
        // Use 'df' command for portability
        let output = std::process::Command::new("df")
            .args(["-k", self.sandbox_root.to_str().unwrap_or(".")])
            .output();

        match output {
            Ok(out) if out.status.success() => {
                let text = String::from_utf8_lossy(&out.stdout);
                let lines: Vec<&str> = text.lines().collect();
                if let Some(data_line) = lines.get(1) {
                    let cols: Vec<&str> = data_line.split_whitespace().collect();
                    // df -k output: Filesystem 1K-blocks Used Available Use% Mounted
                    if cols.len() >= 4 {
                        let total_kb: u64 = cols[1].parse().unwrap_or(0);
                        let used_kb: u64 = cols[2].parse().unwrap_or(0);
                        let avail_kb: u64 = cols[3].parse().unwrap_or(0);
                        let info = serde_json::json!({
                            "sandbox_root": self.sandbox_root.to_string_lossy(),
                            "total_bytes": total_kb * 1024,
                            "used_bytes": used_kb * 1024,
                            "free_bytes": avail_kb * 1024,
                            "total_human": human_size(total_kb * 1024),
                            "used_human": human_size(used_kb * 1024),
                            "free_human": human_size(avail_kb * 1024),
                        });
                        return serde_json::to_string_pretty(&info)
                            .unwrap_or_else(|e| format!("Error serializing: {e}"));
                    }
                }
                format!("Error: unexpected df output:\n{text}")
            }
            Ok(out) => {
                let err = String::from_utf8_lossy(&out.stderr);
                format!("Error: df failed: {err}")
            }
            Err(e) => format!("Error: cannot run df: {e}"),
        }
    }

    #[tool(description = "Check whether a file or directory exists. Returns 'true' or 'false'.")]
    async fn file_exists(
        &self,
        Parameters(FileExistsParams { path, session_id }): Parameters<FileExistsParams>,
    ) -> String {
        match self.resolve(&path, session_id.as_deref()).await {
            Ok(p) => p.exists().to_string(),
            Err(_) => "false".to_string(),
        }
    }

    #[tool(description = "Compress a file or directory into a zip or tar.gz archive.")]
    async fn compress(
        &self,
        Parameters(CompressParams {
            source,
            destination,
            format,
        }): Parameters<CompressParams>,
    ) -> String {
        let src = match self.resolve(&source, None).await {
            Ok(p) => p,
            Err(e) => return format!("Error: {e}"),
        };
        let dst = match self.resolve(&destination, None).await {
            Ok(p) => p,
            Err(e) => return format!("Error: {e}"),
        };

        if !src.exists() {
            return format!("Error: source not found: {}", src.display());
        }

        let result = match format.to_lowercase().as_str() {
            "zip" => compress_zip(&src, &dst),
            "tar.gz" | "tgz" => compress_tar_gz(&src, &dst),
            other => return format!("Error: unsupported format '{other}'. Use 'zip' or 'tar.gz'"),
        };

        match result {
            Ok(_) => format!("Compressed {} -> {}", src.display(), dst.display()),
            Err(e) => format!("Error: compression failed: {e}"),
        }
    }

    #[tool(description = "Extract a zip or tar.gz archive. Destination defaults to the archive's parent directory.")]
    async fn decompress(
        &self,
        Parameters(DecompressParams { source, destination }): Parameters<DecompressParams>,
    ) -> String {
        let src = match self.resolve(&source, None).await {
            Ok(p) => p,
            Err(e) => return format!("Error: {e}"),
        };

        if !src.exists() {
            return format!("Error: archive not found: {}", src.display());
        }

        let dst = match destination {
            Some(ref d) => match self.resolve(d, None).await {
                Ok(p) => p,
                Err(e) => return format!("Error: {e}"),
            },
            None => src
                .parent()
                .unwrap_or(&self.sandbox_root)
                .to_path_buf(),
        };

        if let Err(e) = std::fs::create_dir_all(&dst) {
            return format!("Error: cannot create destination: {e}");
        }

        let name = src.to_string_lossy().to_lowercase();
        let result = if name.ends_with(".zip") {
            decompress_zip(&src, &dst)
        } else if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
            decompress_tar_gz(&src, &dst)
        } else {
            return format!(
                "Error: cannot determine archive type from filename {}. Supported: .zip, .tar.gz, .tgz",
                src.file_name().unwrap_or_default().to_string_lossy()
            );
        };

        match result {
            Ok(_) => format!("Extracted {} -> {}", src.display(), dst.display()),
            Err(e) => format!("Error: extraction failed: {e}"),
        }
    }

    #[tool(description = r#"Execute multiple file system operations atomically. operations_json is a JSON array of {"op": "copy|move|delete|mkdir|chmod", "params": {...}} objects."#)]
    async fn batch_operations(
        &self,
        Parameters(BatchOperationsParams { operations_json }): Parameters<BatchOperationsParams>,
    ) -> String {
        let ops: Vec<serde_json::Value> = match serde_json::from_str(&operations_json) {
            Ok(v) => v,
            Err(e) => return format!("Error: invalid JSON: {e}"),
        };

        let mut results = Vec::new();

        for (i, op) in ops.iter().enumerate() {
            let op_name = op.get("op").and_then(|v| v.as_str()).unwrap_or("unknown");
            let params = op.get("params").cloned().unwrap_or(serde_json::Value::Object(Default::default()));

            let result = match op_name {
                "copy" => {
                    let source = params
                        .get("source")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let destination = params
                        .get("destination")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if source.is_empty() || destination.is_empty() {
                        "Error: copy requires source and destination".to_string()
                    } else {
                        let src = match self.resolve(source, None).await {
                            Ok(p) => p,
                            Err(e) => { results.push(format!("[{i}] copy: Error: {e}")); continue; }
                        };
                        let dst = match self.resolve(destination, None).await {
                            Ok(p) => p,
                            Err(e) => { results.push(format!("[{i}] copy: Error: {e}")); continue; }
                        };
                        match copy_recursive(&src, &dst) {
                            Ok(_) => format!("Copied {} -> {}", src.display(), dst.display()),
                            Err(e) => format!("Error: {e}"),
                        }
                    }
                }
                "move" => {
                    let source = params.get("source").and_then(|v| v.as_str()).unwrap_or("");
                    let destination = params.get("destination").and_then(|v| v.as_str()).unwrap_or("");
                    if source.is_empty() || destination.is_empty() {
                        "Error: move requires source and destination".to_string()
                    } else {
                        let src = match self.resolve(source, None).await {
                            Ok(p) => p,
                            Err(e) => { results.push(format!("[{i}] move: Error: {e}")); continue; }
                        };
                        let dst = match self.resolve(destination, None).await {
                            Ok(p) => p,
                            Err(e) => { results.push(format!("[{i}] move: Error: {e}")); continue; }
                        };
                        std::fs::rename(&src, &dst)
                            .or_else(|_| {
                                copy_recursive(&src, &dst)?;
                                if src.is_dir() { std::fs::remove_dir_all(&src) } else { std::fs::remove_file(&src) }
                            })
                            .map(|_| format!("Moved {} -> {}", src.display(), dst.display()))
                            .unwrap_or_else(|e| format!("Error: {e}"))
                    }
                }
                "delete" => {
                    let path = params.get("path").and_then(|v| v.as_str()).unwrap_or("");
                    let recursive = params.get("recursive").and_then(|v| v.as_bool()).unwrap_or(false);
                    if path.is_empty() {
                        "Error: delete requires path".to_string()
                    } else {
                        let resolved = match self.resolve(path, None).await {
                            Ok(p) => p,
                            Err(e) => { results.push(format!("[{i}] delete: Error: {e}")); continue; }
                        };
                        let r = if resolved.is_dir() && recursive {
                            std::fs::remove_dir_all(&resolved)
                        } else if resolved.is_dir() {
                            std::fs::remove_dir(&resolved)
                        } else {
                            std::fs::remove_file(&resolved)
                        };
                        r.map(|_| format!("Deleted {}", resolved.display()))
                         .unwrap_or_else(|e| format!("Error: {e}"))
                    }
                }
                "mkdir" => {
                    let path = params.get("path").and_then(|v| v.as_str()).unwrap_or("");
                    if path.is_empty() {
                        "Error: mkdir requires path".to_string()
                    } else {
                        let resolved = match self.resolve(path, None).await {
                            Ok(p) => p,
                            Err(e) => { results.push(format!("[{i}] mkdir: Error: {e}")); continue; }
                        };
                        std::fs::create_dir_all(&resolved)
                            .map(|_| format!("Created {}", resolved.display()))
                            .unwrap_or_else(|e| format!("Error: {e}"))
                    }
                }
                "chmod" => {
                    let path = params.get("path").and_then(|v| v.as_str()).unwrap_or("");
                    let mode = params.get("mode").and_then(|v| v.as_u64()).unwrap_or(0o644) as u32;
                    if path.is_empty() {
                        "Error: chmod requires path".to_string()
                    } else {
                        let resolved = match self.resolve(path, None).await {
                            Ok(p) => p,
                            Err(e) => { results.push(format!("[{i}] chmod: Error: {e}")); continue; }
                        };
                        std::fs::set_permissions(&resolved, std::fs::Permissions::from_mode(mode))
                            .map(|_| format!("chmod {:o} {}", mode & 0o777, resolved.display()))
                            .unwrap_or_else(|e| format!("Error: {e}"))
                    }
                }
                other => format!("Error: unknown operation '{other}'"),
            };

            results.push(format!("[{i}] {op_name}: {result}"));
        }

        results.join("\n")
    }
}

// ── Glob → regex helper ───────────────────────────────────────────────────────

fn glob_to_regex(pattern: &str) -> String {
    let mut re = String::from("(?i)^");
    let chars: Vec<char> = pattern.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' if i + 1 < chars.len() && chars[i + 1] == '*' => {
                re.push_str(".*");
                i += 2;
                // skip optional separator
                if i < chars.len() && chars[i] == '/' {
                    i += 1;
                }
            }
            '*' => {
                re.push_str("[^/]*");
                i += 1;
            }
            '?' => {
                re.push_str("[^/]");
                i += 1;
            }
            c => {
                re.push_str(&regex::escape(&c.to_string()));
                i += 1;
            }
        }
    }
    re.push('$');
    re
}

// ── Human-readable size ───────────────────────────────────────────────────────

fn human_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    const TB: u64 = GB * 1024;
    if bytes >= TB {
        format!("{:.1} TiB", bytes as f64 / TB as f64)
    } else if bytes >= GB {
        format!("{:.1} GiB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MiB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KiB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let cli = Cli::parse();

    tracing::info!(
        root = %cli.root.display(),
        max_file_size = cli.max_file_size,
        "Starting mcp-fs server"
    );

    let server = FsServer::new(cli.root, cli.max_file_size)?;
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
