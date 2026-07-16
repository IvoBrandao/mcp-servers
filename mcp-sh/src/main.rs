use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use clap::Parser;
use regex::Regex;
use rmcp::{handler::server::wrapper::Parameters, schemars, tool, tool_router};
use rmcp::{ServiceExt, transport::stdio};
use serde::Deserialize;
use tokio::sync::RwLock;

// ---------------------------------------------------------------------------
// CLI args
// ---------------------------------------------------------------------------

#[derive(Parser, Debug)]
#[command(name = "mcp-sh", about = "Sandboxed bash shell MCP server")]
struct Args {
    /// Sandbox root directory (default: current working directory)
    #[arg(long, default_value = ".")]
    root: String,

    /// Command timeout in seconds
    #[arg(long, default_value_t = 60)]
    timeout: u64,

    /// Maximum output size in characters
    #[arg(long, default_value_t = 200_000)]
    max_output: usize,

    /// Comma-separated list of denied command names
    #[arg(long, default_value = "rm,rmdir,dd,mkfs,shutdown,reboot,sudo,su")]
    deny_commands: String,

    /// Disable the deny list (allow all commands)
    #[arg(long, default_value_t = false)]
    allow_all: bool,
}

// ---------------------------------------------------------------------------
// Sandbox tool detection
// ---------------------------------------------------------------------------

/// Which OS-level sandbox wrapper is available.
#[derive(Debug, Clone)]
enum SandboxTool {
    /// Linux: bubblewrap (bwrap) found at the given path.
    #[allow(dead_code)]
    Bwrap(PathBuf),
    /// macOS: sandbox-exec is present at /usr/bin/sandbox-exec.
    SandboxExec,
    /// No OS sandbox available; fall back to deny-list only.
    None,
}

/// Detect which sandboxing tool is available on this platform.
fn detect_sandbox_tool() -> SandboxTool {
    #[cfg(target_os = "macos")]
    {
        if std::path::Path::new("/usr/bin/sandbox-exec").exists() {
            return SandboxTool::SandboxExec;
        }
    }

    #[cfg(target_os = "linux")]
    {
        // Try to find bwrap on PATH.
        if let Ok(output) = std::process::Command::new("which").arg("bwrap").output() {
            if output.status.success() {
                let path_str = String::from_utf8_lossy(&output.stdout);
                let trimmed = path_str.trim();
                if !trimmed.is_empty() {
                    return SandboxTool::Bwrap(PathBuf::from(trimmed));
                }
            }
        }
    }

    SandboxTool::None
}

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct SandboxState {
    root: PathBuf,
    denied: HashSet<String>,
    timeout_secs: u64,
    max_output: usize,
    sandbox_tool: SandboxTool,
    /// session_id -> current working directory inside sandbox
    sessions: Arc<RwLock<HashMap<String, PathBuf>>>,
}

impl SandboxState {
    fn new(
        root: PathBuf,
        denied: HashSet<String>,
        timeout_secs: u64,
        max_output: usize,
        sandbox_tool: SandboxTool,
    ) -> Self {
        Self {
            root,
            denied,
            timeout_secs,
            max_output,
            sandbox_tool,
            sessions: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    fn is_within_sandbox(&self, p: &Path) -> bool {
        p.starts_with(&self.root)
    }

    /// Returns the session cwd, defaulting to sandbox root.
    async fn session_cwd(&self, session_id: Option<&str>) -> PathBuf {
        if let Some(id) = session_id {
            let sessions = self.sessions.read().await;
            sessions.get(id).cloned().unwrap_or_else(|| self.root.clone())
        } else {
            self.root.clone()
        }
    }

    /// Resolve a (possibly relative) user-supplied path inside the sandbox.
    async fn resolve_path(&self, path: &str, session_id: Option<&str>) -> Result<PathBuf, String> {
        let p = Path::new(path);
        let cwd = self.session_cwd(session_id).await;
        let candidate = if p.is_absolute() {
            p.to_path_buf()
        } else {
            cwd.join(p)
        };
        // We can't call .canonicalize() on paths that don't exist yet (e.g. write_file),
        // so we do a lexical normalisation instead, then check prefix.
        let resolved = normalize_path(&candidate);
        if !self.is_within_sandbox(&resolved) {
            return Err(format!("Path '{path}' is outside the sandbox."));
        }
        Ok(resolved)
    }

    /// Returns the first denied word found in the command, or None.
    fn find_denied(&self, cmd: &str) -> Option<String> {
        if self.denied.is_empty() {
            return None;
        }
        let re = Regex::new(r"[;\n]|&&|\|\||[|&]").unwrap();
        for segment in re.split(cmd) {
            let word = first_word(segment);
            if !word.is_empty() && self.denied.contains(word) {
                return Some(word.to_string());
            }
        }
        None
    }
}

fn first_word(s: &str) -> &str {
    s.trim().split_whitespace().next().unwrap_or("")
}

/// Lexical path normalisation: resolve `.` and `..` without hitting the fs.
fn normalize_path(p: &Path) -> PathBuf {
    let mut components = Vec::new();
    for c in p.components() {
        use std::path::Component::*;
        match c {
            CurDir => {}
            ParentDir => {
                components.pop();
            }
            other => components.push(other),
        }
    }
    components.iter().collect()
}

// ---------------------------------------------------------------------------
// Tool parameter structs
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ShellExecParams {
    #[schemars(description = "The bash command to execute")]
    command: String,
    #[schemars(description = "Session ID to persist working directory between calls")]
    session_id: Option<String>,
    /// Accepted for API compatibility; streaming is not supported over stdio.
    #[schemars(description = "Whether to stream output (accepted for API compatibility; not active over stdio)")]
    #[allow(dead_code)]
    stream: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct WriteFileParams {
    #[schemars(description = "Path to the file to write (absolute or relative to session cwd)")]
    path: String,
    #[schemars(description = "Text content to write")]
    content: String,
    #[schemars(description = "Session ID for resolving relative paths")]
    session_id: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ReadFileParams {
    #[schemars(description = "Path to the file to read (absolute or relative to session cwd)")]
    path: String,
    #[schemars(description = "Session ID for resolving relative paths")]
    session_id: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct UploadFileParams {
    #[schemars(description = "Destination path inside sandbox (absolute or relative)")]
    path: String,
    #[schemars(description = "Base64-encoded file contents")]
    content_base64: String,
    #[schemars(description = "Session ID for resolving relative paths")]
    session_id: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct DownloadFileParams {
    #[schemars(description = "Path to the file to download (absolute or relative to session cwd)")]
    path: String,
    #[schemars(description = "Session ID for resolving relative paths")]
    session_id: Option<String>,
}

// ---------------------------------------------------------------------------
// MCP server struct
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct ShellServer {
    state: SandboxState,
}

#[tool_router(server_handler)]
impl ShellServer {
    #[tool(description = "Execute any bash command inside the sandbox directory. Supports full bash: pipes, redirects, loops, heredocs, etc. Use session_id to persist cd state between calls.")]
    async fn shell_exec(
        &self,
        Parameters(ShellExecParams { command, session_id, stream: _ }): Parameters<ShellExecParams>,
    ) -> String {
        let command = command.trim().to_string();
        if command.is_empty() {
            return "Error: Empty command.".to_string();
        }

        if let Some(denied) = self.state.find_denied(&command) {
            return format!("Error: Command '{denied}' is denied.");
        }

        let cwd = self.state.session_cwd(session_id.as_deref()).await;

        // Register session if first use
        if let Some(ref id) = session_id {
            let mut sessions = self.state.sessions.write().await;
            sessions.entry(id.clone()).or_insert_with(|| cwd.clone());
        }

        match run_bash(
            &command,
            &cwd,
            session_id.as_deref(),
            &self.state,
        )
        .await
        {
            Ok(output) => output,
            Err(e) => {
                tracing::error!("shell_exec error: {e}");
                format!("Error: {e}")
            }
        }
    }

    #[tool(description = "Write text content to a file inside the sandbox. Creates parent directories automatically. Preferred over heredocs for multi-line content.")]
    async fn write_file(
        &self,
        Parameters(WriteFileParams { path, content, session_id }): Parameters<WriteFileParams>,
    ) -> String {
        let resolved = match self.state.resolve_path(&path, session_id.as_deref()).await {
            Ok(p) => p,
            Err(e) => return format!("Error: {e}"),
        };

        if let Some(parent) = resolved.parent() {
            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                return format!("Error: could not create parent directories: {e}");
            }
        }

        let len = content.len();
        match tokio::fs::write(&resolved, content.as_bytes()).await {
            Ok(()) => {
                let rel = resolved.strip_prefix(&self.state.root).unwrap_or(&resolved);
                format!("Written {len} chars to {}", rel.display())
            }
            Err(e) => format!("Error: {e}"),
        }
    }

    #[tool(description = "Read a text file from inside the sandbox.")]
    async fn read_file(
        &self,
        Parameters(ReadFileParams { path, session_id }): Parameters<ReadFileParams>,
    ) -> String {
        let resolved = match self.state.resolve_path(&path, session_id.as_deref()).await {
            Ok(p) => p,
            Err(e) => return format!("Error: {e}"),
        };

        match tokio::fs::read(&resolved).await {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                format!("Error: File not found: {path}")
            }
            Err(e) => format!("Error: {e}"),
        }
    }

    #[tool(description = "Upload a base64-encoded binary file into the sandbox. Creates parent directories automatically.")]
    async fn upload_file(
        &self,
        Parameters(UploadFileParams { path, content_base64, session_id }): Parameters<UploadFileParams>,
    ) -> String {
        let resolved = match self.state.resolve_path(&path, session_id.as_deref()).await {
            Ok(p) => p,
            Err(e) => return format!("Error: {e}"),
        };

        let data = match BASE64.decode(content_base64.trim()) {
            Ok(d) => d,
            Err(e) => return format!("Error: invalid base64: {e}"),
        };

        if let Some(parent) = resolved.parent() {
            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                return format!("Error: could not create parent directories: {e}");
            }
        }

        let size = data.len();
        match tokio::fs::write(&resolved, &data).await {
            Ok(()) => {
                let rel = resolved.strip_prefix(&self.state.root).unwrap_or(&resolved);
                format!("Uploaded {size} bytes to {}", rel.display())
            }
            Err(e) => format!("Error: {e}"),
        }
    }

    #[tool(description = "Download a file from the sandbox as base64 (max 10 MB).")]
    async fn download_file(
        &self,
        Parameters(DownloadFileParams { path, session_id }): Parameters<DownloadFileParams>,
    ) -> String {
        let resolved = match self.state.resolve_path(&path, session_id.as_deref()).await {
            Ok(p) => p,
            Err(e) => return format!("Error: {e}"),
        };

        let meta = match tokio::fs::metadata(&resolved).await {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return format!("Error: Not a file: {path}");
            }
            Err(e) => return format!("Error: {e}"),
        };

        if !meta.is_file() {
            return format!("Error: Not a file: {path}");
        }

        const MAX_DOWNLOAD: u64 = 10 * 1024 * 1024;
        if meta.len() > MAX_DOWNLOAD {
            return "Error: File too large (>10 MB)".to_string();
        }

        match tokio::fs::read(&resolved).await {
            Ok(data) => BASE64.encode(&data),
            Err(e) => format!("Error: {e}"),
        }
    }

    #[tool(description = "Show sandbox configuration and active sessions.")]
    async fn shell_info(&self) -> String {
        let sessions = self.state.sessions.read().await;
        let sessions_info = if sessions.is_empty() {
            "  (none)".to_string()
        } else {
            sessions
                .iter()
                .map(|(id, cwd)| {
                    let rel = cwd
                        .strip_prefix(&self.state.root)
                        .unwrap_or(cwd);
                    format!("  {id}: {}", rel.display())
                })
                .collect::<Vec<_>>()
                .join("\n")
        };

        let denied = if self.state.denied.is_empty() {
            "(none)".to_string()
        } else {
            let mut v: Vec<&str> = self.state.denied.iter().map(String::as_str).collect();
            v.sort_unstable();
            v.join(", ")
        };

        let sandbox_info = match &self.state.sandbox_tool {
            SandboxTool::Bwrap(path) => format!("bwrap ({})", path.display()),
            SandboxTool::SandboxExec => "sandbox-exec (macOS)".to_string(),
            SandboxTool::None => {
                tracing::warn!(
                    "No OS sandbox active — shell commands run without write confinement. \
                     Install bubblewrap (bwrap) on Linux for OS-level sandboxing."
                );
                "none (WARNING: no write confinement; install bubblewrap on Linux)".to_string()
            }
        };

        format!(
            "Sandbox root: {root}\n\
             Timeout: {timeout}s\n\
             Max output: {max_output} chars\n\
             Denied commands: {denied}\n\
             OS sandbox: {sandbox_info}\n\
             Active sessions:\n{sessions_info}\n\
             \n\
             Shell: /bin/bash (full feature set)\n\
             Tip: use write_file for multi-line file creation instead of heredocs.",
            root = self.state.root.display(),
            timeout = self.state.timeout_secs,
            max_output = self.state.max_output,
        )
    }
}

// ---------------------------------------------------------------------------
// Core executor
// ---------------------------------------------------------------------------

async fn run_bash(
    command: &str,
    cwd: &Path,
    session_id: Option<&str>,
    state: &SandboxState,
) -> Result<String> {
    const SENTINEL: &str = "__MCP_CWD__:";

    // Shell-quote the cwd for safe embedding
    let cwd_quoted = shell_quote(cwd.to_string_lossy().as_ref());

    // Wrap command: cd to session cwd, run user command, print new cwd sentinel
    let wrapped = format!(
        "cd {cwd_quoted} || exit 1\n\
         {command}\n\
         printf '\\n{SENTINEL}%s\\n' \"$(pwd -P)\""
    );

    tracing::debug!("Running bash command in {}: {}", cwd.display(), command);

    let root_str = state.root.to_string_lossy();
    let cwd_str = cwd.to_string_lossy();

    let mut child = match &state.sandbox_tool {
        SandboxTool::Bwrap(bwrap_path) => {
            // bubblewrap: read-only bind of entire /, writable bind only for sandbox root and /tmp
            let mut cmd = tokio::process::Command::new(bwrap_path);
            cmd.args([
                "--ro-bind", "/", "/",
                "--dev", "/dev",
                "--proc", "/proc",
                "--bind", root_str.as_ref(), root_str.as_ref(),
                "--tmpfs", "/tmp",
                "--chdir", cwd_str.as_ref(),
                "--die-with-parent",
                "/bin/bash", "-c", &wrapped,
            ])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .env("PWD", cwd_str.as_ref())
            .env("TMPDIR", state.root.join(".tmp").to_string_lossy().as_ref());

            #[cfg(unix)]
            { cmd.process_group(0).kill_on_drop(true); }

            cmd.spawn()?
        }

        SandboxTool::SandboxExec => {
            // macOS sandbox-exec: allow everything by default, deny writes outside root and /tmp
            let profile = format!(
                "(version 1)\
                 (allow default)\
                 (deny file-write* (literal \"/\"))\
                 (allow file-write* (subpath \"{root}\"))\
                 (allow file-write* (subpath \"/tmp\"))\
                 (allow file-write* (subpath \"/private/tmp\"))\
                 (allow file-write* (literal \"/dev/null\"))",
                root = root_str,
            );

            let mut cmd = tokio::process::Command::new("/usr/bin/sandbox-exec");
            cmd.args(["-p", &profile, "/bin/bash", "-c", &wrapped])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .env("PWD", cwd_str.as_ref())
                .env("TMPDIR", state.root.join(".tmp").to_string_lossy().as_ref());

            #[cfg(unix)]
            { cmd.process_group(0).kill_on_drop(true); }

            cmd.spawn()?
        }

        SandboxTool::None => {
            tracing::warn!(
                "No OS sandbox available (install bubblewrap on Linux). \
                 Shell commands run without write confinement."
            );

            let mut cmd = tokio::process::Command::new("/bin/bash");
            cmd.arg("-c")
                .arg(&wrapped)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .env("PWD", cwd_str.as_ref())
                .env("TMPDIR", state.root.join(".tmp").to_string_lossy().as_ref());

            #[cfg(unix)]
            { cmd.process_group(0).kill_on_drop(true); }

            cmd.spawn()?
        }
    };

    // Collect stdout and stderr separately then merge
    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");

    // Read both streams concurrently
    let read_stdout = read_stream(stdout);
    let read_stderr = read_stream(stderr);

    let timeout_dur = std::time::Duration::from_secs(state.timeout_secs);

    let (stdout_bytes, stderr_bytes, exit_status) =
        match tokio::time::timeout(timeout_dur, async {
            let (out, err) = tokio::join!(read_stdout, read_stderr);
            let status = child.wait().await?;
            Ok::<_, anyhow::Error>((out, err, status))
        })
        .await
        {
            Ok(Ok(triple)) => triple,
            Ok(Err(e)) => return Err(e),
            Err(_timeout) => {
                // Kill the whole process group
                if let Some(pid) = child.id() {
                    libc_kill_pgrp(pid);
                }
                let _ = child.kill().await;
                return Ok(format!(
                    "\nTimed out after {}s",
                    state.timeout_secs
                ));
            }
        };

    // Merge stdout and stderr into combined output (stdout first, then stderr)
    // Similar to Python's STDOUT redirect: combine them in order received
    // Since we read separately, append stderr after stdout with a separator if non-empty
    let stdout_str = String::from_utf8_lossy(&stdout_bytes).into_owned();
    let stderr_str = String::from_utf8_lossy(&stderr_bytes).into_owned();

    // Parse __MCP_CWD__ sentinel from stdout to update session cwd
    let (output_str, new_cwd_opt) = extract_cwd_sentinel(&stdout_str, SENTINEL);

    // Build combined output: stdout output (without sentinel) + stderr
    let mut combined = output_str.to_string();
    if !stderr_str.is_empty() {
        if !combined.is_empty() && !combined.ends_with('\n') {
            combined.push('\n');
        }
        combined.push_str(&stderr_str);
    }

    // Append exit code hint if non-zero
    let exit_code = exit_status.code().unwrap_or(-1);
    if exit_code != 0 {
        tracing::debug!("Command exited with code {exit_code}");
    }

    // Truncate if needed
    let total = combined.len();
    let output = if total > state.max_output {
        let mut truncated = combined[..state.max_output].to_string();
        truncated.push_str(&format!(
            "\n... (truncated — {total} chars total)"
        ));
        truncated
    } else {
        combined
    };

    // Update session cwd
    if let (Some(id), Some(new_cwd_str)) = (session_id, new_cwd_opt) {
        let new_cwd = PathBuf::from(new_cwd_str.trim());
        if state.is_within_sandbox(&new_cwd) {
            tracing::debug!("Session '{id}' cwd -> {}", new_cwd.display());
            let mut sessions = state.sessions.write().await;
            sessions.insert(id.to_string(), new_cwd);
        } else {
            tracing::warn!(
                "Session '{id}': cd escaped sandbox ({}) — resetting to root",
                new_cwd.display()
            );
            let mut sessions = state.sessions.write().await;
            sessions.insert(id.to_string(), state.root.clone());
        }
    }

    let trimmed = output.trim_end().to_string();
    if trimmed.is_empty() {
        Ok("(no output)".to_string())
    } else {
        Ok(trimmed)
    }
}

/// Read all bytes from an async reader into a Vec.
async fn read_stream<R: tokio::io::AsyncRead + Unpin>(reader: R) -> Vec<u8> {
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::new();
    let mut r = reader;
    let _ = r.read_to_end(&mut buf).await;
    buf
}

/// Split output at the `__MCP_CWD__:` sentinel line.
/// Returns (output_without_sentinel, Option<new_cwd>).
fn extract_cwd_sentinel<'a>(s: &'a str, sentinel: &str) -> (&'a str, Option<&'a str>) {
    // Find the last occurrence of the sentinel line
    if let Some(pos) = s.rfind(sentinel) {
        // Back up to the preceding newline (the `\n` before the sentinel)
        let output_end = if pos > 0 && s.as_bytes()[pos - 1] == b'\n' {
            pos - 1
        } else {
            pos
        };
        let rest = &s[pos + sentinel.len()..];
        let new_cwd = rest.lines().next().unwrap_or("").trim();
        (&s[..output_end], if new_cwd.is_empty() { None } else { Some(new_cwd) })
    } else {
        (s, None)
    }
}

/// Single-quote a string for safe embedding in a bash command.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Kill the process group of a process (best-effort, no panic).
/// Uses `kill(-pgid, SIGKILL)` to terminate the entire process group.
#[cfg(unix)]
fn libc_kill_pgrp(pid: u32) {
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    // kill(-pgid, SIGKILL): kills every process in the group
    unsafe { kill(-(pid as i32), 9 /* SIGKILL */) };
}

#[cfg(not(unix))]
fn libc_kill_pgrp(_pid: u32) {}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let args = Args::parse();

    let root = std::fs::canonicalize(&args.root)
        .unwrap_or_else(|_| PathBuf::from(&args.root));

    // Create sandbox root and .tmp dir
    std::fs::create_dir_all(&root)?;
    std::fs::create_dir_all(root.join(".tmp"))?;

    let denied: HashSet<String> = if args.allow_all {
        HashSet::new()
    } else {
        args.deny_commands
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    };

    // Detect OS sandbox tool once at startup.
    let sandbox_tool = detect_sandbox_tool();
    match &sandbox_tool {
        SandboxTool::Bwrap(p) => tracing::info!("OS sandbox: bwrap at {}", p.display()),
        SandboxTool::SandboxExec => tracing::info!("OS sandbox: sandbox-exec (macOS)"),
        SandboxTool::None => tracing::warn!(
            "No OS sandbox available (install bubblewrap on Linux). \
             Shell commands run without write confinement."
        ),
    }

    tracing::info!("Sandbox root: {}", root.display());
    tracing::info!("Denied commands: {:?}", denied);
    tracing::info!("Timeout: {}s", args.timeout);
    tracing::info!("Max output: {} chars", args.max_output);

    let state = SandboxState::new(root, denied, args.timeout, args.max_output, sandbox_tool);
    let server = ShellServer { state };

    let service = server.serve(stdio()).await?;
    service.waiting().await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    // --- normalize_path ---

    #[test]
    fn normalize_removes_dot() {
        let p = PathBuf::from("/a/./b");
        assert_eq!(normalize_path(&p), PathBuf::from("/a/b"));
    }

    #[test]
    fn normalize_resolves_parent() {
        let p = PathBuf::from("/a/b/../c");
        assert_eq!(normalize_path(&p), PathBuf::from("/a/c"));
    }

    #[test]
    fn normalize_handles_double_parent() {
        let p = PathBuf::from("/a/b/c/../../d");
        assert_eq!(normalize_path(&p), PathBuf::from("/a/d"));
    }

    #[test]
    fn normalize_already_clean() {
        let p = PathBuf::from("/a/b/c");
        assert_eq!(normalize_path(&p), PathBuf::from("/a/b/c"));
    }

    // --- shell_quote ---

    #[test]
    fn shell_quote_simple() {
        assert_eq!(shell_quote("hello"), "'hello'");
    }

    #[test]
    fn shell_quote_with_spaces() {
        assert_eq!(shell_quote("hello world"), "'hello world'");
    }

    #[test]
    fn shell_quote_with_single_quote() {
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn shell_quote_path() {
        assert_eq!(shell_quote("/home/user/my dir"), "'/home/user/my dir'");
    }

    // --- first_word ---

    #[test]
    fn first_word_simple() {
        assert_eq!(first_word("ls -la"), "ls");
    }

    #[test]
    fn first_word_leading_whitespace() {
        assert_eq!(first_word("  rm -rf /"), "rm");
    }

    #[test]
    fn first_word_empty() {
        assert_eq!(first_word(""), "");
    }

    #[test]
    fn first_word_only_spaces() {
        assert_eq!(first_word("   "), "");
    }

    // --- extract_cwd_sentinel ---

    #[test]
    fn sentinel_extracts_cwd() {
        let s = "hello world\n__MCP_CWD__:/some/path\n";
        let (out, cwd) = extract_cwd_sentinel(s, "__MCP_CWD__:");
        assert_eq!(out, "hello world");
        assert_eq!(cwd, Some("/some/path"));
    }

    #[test]
    fn sentinel_no_sentinel_returns_full_string() {
        let s = "hello world\n";
        let (out, cwd) = extract_cwd_sentinel(s, "__MCP_CWD__:");
        assert_eq!(out, "hello world\n");
        assert_eq!(cwd, None);
    }

    #[test]
    fn sentinel_uses_last_occurrence() {
        let s = "output\n__MCP_CWD__:/first\nmore output\n__MCP_CWD__:/second\n";
        let (_, cwd) = extract_cwd_sentinel(s, "__MCP_CWD__:");
        assert_eq!(cwd, Some("/second"));
    }

    #[test]
    fn sentinel_empty_cwd_treated_as_none() {
        let s = "output\n__MCP_CWD__:\n";
        let (_, cwd) = extract_cwd_sentinel(s, "__MCP_CWD__:");
        assert_eq!(cwd, None);
    }

    // --- SandboxState::find_denied ---

    fn make_state(root: &str, denied: &[&str]) -> SandboxState {
        SandboxState::new(
            PathBuf::from(root),
            denied.iter().map(|s| s.to_string()).collect(),
            60,
            200_000,
            SandboxTool::None,
        )
    }

    #[test]
    fn find_denied_simple_match() {
        let s = make_state("/sandbox", &["rm", "sudo"]);
        assert_eq!(s.find_denied("rm -rf /"), Some("rm".to_string()));
    }

    #[test]
    fn find_denied_no_match() {
        let s = make_state("/sandbox", &["rm", "sudo"]);
        assert_eq!(s.find_denied("ls -la"), None);
    }

    #[test]
    fn find_denied_in_pipeline() {
        let s = make_state("/sandbox", &["sudo"]);
        assert_eq!(s.find_denied("echo hello | sudo tee /etc/hosts"), Some("sudo".to_string()));
    }

    #[test]
    fn find_denied_semicolon_separated() {
        let s = make_state("/sandbox", &["shutdown"]);
        assert_eq!(s.find_denied("echo hi; shutdown -h now"), Some("shutdown".to_string()));
    }

    #[test]
    fn find_denied_word_boundary() {
        // "removal" should not match the "rm" deny entry
        let s = make_state("/sandbox", &["rm"]);
        assert_eq!(s.find_denied("removal --help"), None);
    }

    #[test]
    fn find_denied_empty_list() {
        let s = make_state("/sandbox", &[]);
        assert_eq!(s.find_denied("rm -rf /"), None);
    }

    // --- SandboxState::is_within_sandbox ---

    #[test]
    fn within_sandbox_exact_root() {
        let s = make_state("/sandbox", &[]);
        assert!(s.is_within_sandbox(Path::new("/sandbox")));
    }

    #[test]
    fn within_sandbox_subdir() {
        let s = make_state("/sandbox", &[]);
        assert!(s.is_within_sandbox(Path::new("/sandbox/sub/dir")));
    }

    #[test]
    fn outside_sandbox() {
        let s = make_state("/sandbox", &[]);
        assert!(!s.is_within_sandbox(Path::new("/etc")));
    }

    #[test]
    fn sandbox_traversal_attempt() {
        let s = make_state("/sandbox", &[]);
        // A normalised path — raw `..` doesn't bypass starts_with
        let traversal = normalize_path(Path::new("/sandbox/../etc/passwd"));
        assert!(!s.is_within_sandbox(&traversal));
    }

    // --- SandboxState::resolve_path ---

    #[tokio::test]
    async fn resolve_path_absolute_inside_sandbox() {
        let s = make_state("/sandbox", &[]);
        let r = s.resolve_path("/sandbox/foo/bar", None).await;
        assert_eq!(r.unwrap(), PathBuf::from("/sandbox/foo/bar"));
    }

    #[tokio::test]
    async fn resolve_path_relative_to_root() {
        let s = make_state("/sandbox", &[]);
        let r = s.resolve_path("foo/bar", None).await;
        assert_eq!(r.unwrap(), PathBuf::from("/sandbox/foo/bar"));
    }

    #[tokio::test]
    async fn resolve_path_outside_sandbox_rejected() {
        let s = make_state("/sandbox", &[]);
        let r = s.resolve_path("/etc/passwd", None).await;
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("outside the sandbox"));
    }

    #[tokio::test]
    async fn resolve_path_traversal_rejected() {
        let s = make_state("/sandbox", &[]);
        let r = s.resolve_path("../../etc/passwd", None).await;
        assert!(r.is_err());
    }

    // --- Integration: run_bash ---

    fn sandbox_state(root: &Path) -> SandboxState {
        // Canonicalize so that macOS /var -> /private/var symlinks don't trip up
        // is_within_sandbox() when bash reports pwd via the canonical path.
        let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        SandboxState::new(root, HashSet::new(), 10, 200_000, SandboxTool::None)
    }

    #[tokio::test]
    async fn run_bash_simple_echo() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".tmp")).unwrap();
        let state = sandbox_state(dir.path());
        let out = run_bash("echo hello", dir.path(), None, &state).await.unwrap();
        assert_eq!(out.trim(), "hello");
    }

    #[tokio::test]
    async fn run_bash_tilde_expands_to_real_home() {
        // ~ must expand to the actual home directory, not the sandbox root.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".tmp")).unwrap();
        let state = sandbox_state(dir.path());

        let real_home = std::env::var("HOME").expect("HOME must be set");
        let out = run_bash("echo ~", dir.path(), None, &state).await.unwrap();
        assert_eq!(
            out.trim(),
            real_home,
            "~ should expand to real HOME, not sandbox root"
        );
    }

    #[tokio::test]
    async fn run_bash_tilde_path_not_doubled() {
        // Regression: ls ~/subdir must not produce double path segments.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".tmp")).unwrap();
        let state = sandbox_state(dir.path());

        let real_home = std::env::var("HOME").expect("HOME must be set");
        let out = run_bash("echo ~/nonexistent_dir", dir.path(), None, &state)
            .await
            .unwrap();
        let expected = format!("{}/nonexistent_dir", real_home);
        assert_eq!(
            out.trim(),
            expected,
            "~/subdir must expand without doubling the home path"
        );
    }

    #[tokio::test]
    async fn run_bash_pwd_reflects_cwd() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".tmp")).unwrap();
        let state = sandbox_state(dir.path());

        let out = run_bash("pwd -P", dir.path(), None, &state).await.unwrap();
        // canonicalize the tmpdir path for comparison (macOS /var -> /private/var)
        let canonical = std::fs::canonicalize(dir.path()).unwrap();
        assert_eq!(out.trim(), canonical.to_string_lossy().as_ref());
    }

    #[tokio::test]
    async fn run_bash_exit_code_non_zero_still_returns_output() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".tmp")).unwrap();
        let state = sandbox_state(dir.path());

        let out = run_bash("echo err >&2; exit 1", dir.path(), None, &state)
            .await
            .unwrap();
        assert!(out.contains("err"), "stderr should appear in output");
    }

    #[tokio::test]
    async fn run_bash_empty_output_returns_no_output_string() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".tmp")).unwrap();
        let state = sandbox_state(dir.path());

        let out = run_bash("true", dir.path(), None, &state).await.unwrap();
        assert_eq!(out, "(no output)");
    }

    #[tokio::test]
    async fn run_bash_timeout_returns_timeout_message() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".tmp")).unwrap();
        let mut state = sandbox_state(dir.path());
        state.timeout_secs = 1;

        let out = run_bash("sleep 10", dir.path(), None, &state).await.unwrap();
        assert!(out.contains("Timed out"), "should report timeout");
    }

    #[tokio::test]
    async fn run_bash_session_tracks_cd() {
        let dir = tempfile::tempdir().unwrap();
        let subdir = dir.path().join("sub");
        std::fs::create_dir_all(&subdir).unwrap();
        std::fs::create_dir_all(dir.path().join(".tmp")).unwrap();
        let state = sandbox_state(dir.path());

        // cd into sub/
        run_bash("cd sub", dir.path(), Some("s1"), &state).await.unwrap();

        // Next call should start in sub/
        let cwd = state.session_cwd(Some("s1")).await;
        let canonical_sub = std::fs::canonicalize(&subdir).unwrap();
        assert_eq!(cwd, canonical_sub, "session cwd should have updated after cd");
    }
}
