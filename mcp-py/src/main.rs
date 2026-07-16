use std::io::Write as _;
use std::time::Duration;

use anyhow::Result;
use regex::Regex;
use rmcp::{handler::server::wrapper::Parameters, schemars, tool, tool_router};
use rmcp::{ServiceExt, transport::stdio};
use serde::Deserialize;
use tempfile::NamedTempFile;
use tokio::process::Command;
use tokio::time::timeout;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const DEFAULT_TIMEOUT_SECS: u64 = 5;

// Safe PATH for executing python3. Includes Homebrew locations on macOS
// (arm64 uses /opt/homebrew/bin; x86_64 uses /usr/local/bin) so that
// python3 installed via Homebrew is found without inheriting the full
// user PATH (which could contain untrusted directories).
#[cfg(target_os = "macos")]
const PYTHON_PATH: &str = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin";
#[cfg(not(target_os = "macos"))]
const PYTHON_PATH: &str = "/usr/local/bin:/usr/bin:/bin:/usr/local/sbin:/usr/sbin:/sbin";
const MAX_OUTPUT_CHARS: usize = 50_000;

/// Modules that the static validator explicitly allows (everything else
/// requires that none of the blocked patterns fire).
const ALLOWED_MODULES: &[&str] = &[
    "math",
    "random",
    "json",
    "re",
    "collections",
    "itertools",
    "functools",
    "string",
    "datetime",
    "calendar",
    "typing",
    "enum",
    "decimal",
    "statistics",
    "fractions",
    "array",
    "bisect",
    "heapq",
    "copy",
    "pprint",
    "textwrap",
    "struct",
    "hashlib",
    "base64",
    "binascii",
    "html",
    "urllib.parse",
];

/// Dangerous module names that are blocked at the static-analysis step.
const BLOCKED_MODULES: &[&str] = &[
    "os",
    "sys",
    "subprocess",
    "socket",
    "signal",
    "multiprocessing",
    "threading",
    "ctypes",
    "cffi",
    "fcntl",
    "posix",
    "pty",
    "resource",
    "getpass",
    "pwd",
    "grp",
    "platform",
    "sysconfig",
    "importlib",
    "builtins",
    "code",
    "inspect",
    "ast",
];

// ---------------------------------------------------------------------------
// Static security validator
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct SecurityValidator {
    patterns: Vec<Regex>,
}

impl SecurityValidator {
    fn new() -> Self {
        // Build regex patterns for dangerous constructs.
        // These are intentionally broad — any match is a hard block.
        let raw_patterns: &[&str] = &[
            // Blocked imports via "import X" or "import X as ..."
            r"(?m)^\s*import\s+(os|sys|subprocess|socket|signal|multiprocessing|threading|ctypes|cffi|fcntl|posix|pty|resource|getpass|pwd|grp|platform|sysconfig|importlib|builtins|code|inspect|ast)(\s|$|,|;|\.)",
            // "from X import ..." style
            r"(?m)^\s*from\s+(os|sys|subprocess|socket|signal|multiprocessing|threading|ctypes|cffi|fcntl|posix|pty|resource|getpass|pwd|grp|platform|sysconfig|importlib|builtins|code|inspect|ast)(\s+import|\.|$)",
            // Dynamic import via __import__
            r"__import__\s*\(",
            // os.system / os.popen / os.exec* accessed as attribute (catches `import os as _o; _o.system(...)` partially)
            r"\bos\s*\.\s*(system|popen|exec|execl|execle|execlp|execlpe|execv|execve|execvp|execvpe|spawn|spawnl|spawnle|spawnlp|spawnlpe|spawnv|spawnve|spawnvp|spawnvpe|fork|forkpty|kill|killpg|abort|getenv|putenv|environ|listdir|walk|remove|unlink|rmdir|mkdir|makedirs|rename|replace|symlink|link|stat|lstat|chmod|chown|chroot|chdir|getcwd|urandom)\s*\(",
            // subprocess module access
            r"\bsubprocess\s*\.",
            // Dangerous builtins
            r"\b(exec|eval|compile)\s*\(",
            // open() — blocks file I/O
            r"\bopen\s*\(",
        ];

        let patterns = raw_patterns
            .iter()
            .map(|p| Regex::new(p).expect("valid regex"))
            .collect();

        Self { patterns }
    }

    /// Returns `Some(matched_text)` if a security violation is found.
    fn check(&self, code: &str) -> Option<String> {
        for re in &self.patterns {
            if let Some(m) = re.find(code) {
                return Some(m.as_str().trim().to_string());
            }
        }
        None
    }
}

// ---------------------------------------------------------------------------
// Tool parameter structs
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ExecutePythonParams {
    #[schemars(description = "Python source code to execute")]
    code: String,
    #[schemars(description = "Execution timeout in seconds (default: 5, max: 300)")]
    timeout_secs: Option<u64>,
    #[schemars(description = "Memory limit in MB. Enforced via ulimit on Linux; no-op on macOS (kernel does not honor RLIMIT_AS).")]
    memory_mb: Option<u64>,
}

// ---------------------------------------------------------------------------
// Server struct
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct PythonServer {
    validator: std::sync::Arc<SecurityValidator>,
}

impl PythonServer {
    fn new() -> Self {
        Self {
            validator: std::sync::Arc::new(SecurityValidator::new()),
        }
    }
}

// ---------------------------------------------------------------------------
// Tool implementations
// ---------------------------------------------------------------------------

#[tool_router(server_handler)]
impl PythonServer {
    #[tool(description = "Execute Python code in a sandboxed subprocess. Dangerous imports (os, sys, subprocess, socket, etc.) and builtins (exec, eval, open) are blocked by static analysis before execution. Returns stdout, stderr, and exit code.")]
    async fn execute_python(
        &self,
        Parameters(ExecutePythonParams { code, timeout_secs, memory_mb }): Parameters<ExecutePythonParams>,
    ) -> String {
        // --- Static security check ---
        if let Some(violation) = self.validator.check(&code) {
            return format!(
                "Security violation: detected dangerous pattern: `{violation}`\n\
                 Execution blocked. Allowed modules: {}",
                ALLOWED_MODULES.join(", ")
            );
        }

        // --- Clamp timeout ---
        let t = timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS).min(300).max(1);

        // --- Write code to a temp file ---
        let mut tmp = match NamedTempFile::with_suffix(".py") {
            Ok(f) => f,
            Err(e) => return format!("Error: could not create temp file: {e}"),
        };

        if let Err(e) = tmp.write_all(code.as_bytes()) {
            return format!("Error: could not write temp file: {e}");
        }

        let tmp_path = tmp.path().to_owned();
        tracing::debug!("Executing Python code from {}", tmp_path.display());

        // --- Build the command ---
        // On Linux, `ulimit -v` (RLIMIT_AS) is enforced by the kernel and
        // effectively caps virtual address space. On macOS, RLIMIT_AS is not
        // enforced, so we skip the ulimit there to avoid a silent no-op.
        let (prog, args): (&str, Vec<String>) = if let Some(mb) = memory_mb {
            // On Linux, ulimit -v (RLIMIT_AS) caps virtual address space and
            // is actually enforced. On macOS the kernel ignores RLIMIT_AS, so
            // we skip the prefix to avoid a misleading no-op.
            let ulimit_prefix = {
                #[cfg(target_os = "linux")]
                { format!("ulimit -v {}; ", mb.saturating_mul(1024)) }
                #[cfg(not(target_os = "linux"))]
                { let _ = mb; String::new() }
            };
            let ulimit_cmd = format!(
                "{ulimit_prefix}exec python3 '{path}'",
                path = tmp_path.display()
            );
            ("/bin/sh", vec!["-c".into(), ulimit_cmd])
        } else {
            ("python3", vec![tmp_path.to_string_lossy().into_owned()])
        };

        let mut cmd = Command::new(prog);
        cmd.args(&args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .stdin(std::process::Stdio::null())
            // Keep the inherited environment but strip dangerous vars and
            // use a platform-aware PATH so python3 is found on all targets
            // (including macOS arm64 Homebrew at /opt/homebrew/bin).
            .env("PATH", PYTHON_PATH)
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .env("PYTHONIOENCODING", "utf-8")
            // Strip variables that could be used to escape the sandbox
            .env_remove("PYTHONSTARTUP")
            .env_remove("PYTHONPATH")
            .env_remove("PYTHONHOME")
            .env_remove("PYTHONINSPECT")
            .env_remove("PYTHONDEBUG")
            .env_remove("LD_PRELOAD")
            .env_remove("DYLD_INSERT_LIBRARIES")
            .env_remove("DYLD_LIBRARY_PATH");

        let result = timeout(Duration::from_secs(t), async {
            match cmd.output().await {
                Ok(out) => Ok(out),
                Err(e) => Err(e),
            }
        })
        .await;

        // Temp file is dropped here (auto-deleted) after process finishes.
        drop(tmp);

        match result {
            Err(_elapsed) => {
                format!("Error: Execution timed out after {t}s")
            }
            Ok(Err(e)) => {
                if e.kind() == std::io::ErrorKind::NotFound {
                    "Error: python3 not found. Install Python 3 to use this tool.".to_string()
                } else {
                    format!("Error: failed to spawn python3: {e}")
                }
            }
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
                let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
                let exit_code = output.status.code().unwrap_or(-1);

                format_output(&stdout, &stderr, exit_code)
            }
        }
    }

    #[tool(description = "Check whether python3 is available on the system and return its version string.")]
    async fn check_python_available(&self) -> String {
        match Command::new("python3")
            .arg("--version")
            .output()
            .await
        {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                "python3 is not available on this system.".to_string()
            }
            Err(e) => format!("Error checking python3: {e}"),
            Ok(out) => {
                let ver_stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
                let ver_stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                // `python3 --version` outputs on stdout (3.x) or stderr (2.x legacy)
                let version = if !ver_stdout.is_empty() {
                    ver_stdout
                } else {
                    ver_stderr
                };
                if out.status.success() {
                    format!("python3 is available: {version}")
                } else {
                    format!("python3 returned non-zero exit: {version}")
                }
            }
        }
    }

    #[tool(description = "List the Python modules that are safe to import in the sandboxed executor. Any import not on this list may be blocked by the security validator.")]
    fn list_allowed_modules(&self) -> String {
        let modules = ALLOWED_MODULES.join(", ");
        let blocked = BLOCKED_MODULES.join(", ");
        format!(
            "Allowed modules:\n{modules}\n\n\
             Blocked modules (security violation if imported):\n{blocked}\n\n\
             Additionally, the following builtins are blocked:\n\
             exec, eval, compile, __import__, open"
        )
    }
}

// ---------------------------------------------------------------------------
// Output formatter
// ---------------------------------------------------------------------------

fn format_output(stdout: &str, stderr: &str, exit_code: i32) -> String {
    let stdout_trunc = truncate(stdout, MAX_OUTPUT_CHARS);
    let stderr_trunc = truncate(stderr, MAX_OUTPUT_CHARS);

    let mut parts: Vec<String> = Vec::new();

    if !stdout_trunc.is_empty() {
        parts.push(format!("=== STDOUT ===\n{stdout_trunc}"));
    } else {
        parts.push("=== STDOUT ===\n(empty)".to_string());
    }

    if !stderr_trunc.is_empty() {
        parts.push(format!("=== STDERR ===\n{stderr_trunc}"));
    }

    parts.push(format!("=== EXIT CODE ===\n{exit_code}"));

    parts.join("\n\n")
}

/// Truncate a string to `max_chars`, appending a notice if truncated.
fn truncate(s: &str, max_chars: usize) -> String {
    if s.len() <= max_chars {
        s.to_string()
    } else {
        let mut out = s[..max_chars].to_string();
        out.push_str(&format!("\n... (truncated — {} chars total)", s.len()));
        out
    }
}

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

    tracing::info!("mcp-py: sandboxed Python executor starting");

    let server = PythonServer::new();
    let service = server.serve(stdio()).await?;
    service.waiting().await?;

    Ok(())
}
