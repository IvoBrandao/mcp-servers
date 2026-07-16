use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use rmcp::{handler::server::wrapper::Parameters, schemars, tool, tool_router};
use rmcp::{ServiceExt, transport::stdio};
use serde::Deserialize;

// ---------------------------------------------------------------------------
// CLI args
// ---------------------------------------------------------------------------

#[derive(Parser, Debug)]
#[command(name = "mcp-git", about = "Git repository operations MCP server")]
struct Args {
    /// Restrict git operations to repositories within this directory. Omit to allow any path.
    #[arg(long)]
    root: Option<PathBuf>,

    /// Allow git push operations (disabled by default to prevent accidental remote writes)
    #[arg(long, default_value_t = false)]
    allow_push: bool,
}

// ---------------------------------------------------------------------------
// Parameter structs
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct StatusParams {
    #[schemars(description = "Path to the git repository (defaults to current working directory)")]
    repo_path: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct DiffParams {
    #[schemars(description = "Path to the git repository (defaults to current working directory)")]
    repo_path: Option<String>,
    #[schemars(description = "Show staged (index) diff instead of working tree diff")]
    staged: bool,
    #[schemars(description = "Limit diff to this specific file path")]
    file: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct LogParams {
    #[schemars(description = "Path to the git repository (defaults to current working directory)")]
    repo_path: Option<String>,
    #[schemars(description = "Maximum number of commits to show (default: 20)")]
    limit: Option<i64>,
    #[schemars(description = "Show one line per commit")]
    oneline: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct CommitParams {
    #[schemars(description = "Path to the git repository (defaults to current working directory)")]
    repo_path: Option<String>,
    #[schemars(description = "Commit message")]
    message: String,
    #[schemars(description = "Stage all tracked modified/deleted files before committing (git add -A)")]
    add_all: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct BranchListParams {
    #[schemars(description = "Path to the git repository (defaults to current working directory)")]
    repo_path: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct BranchCreateParams {
    #[schemars(description = "Path to the git repository (defaults to current working directory)")]
    repo_path: Option<String>,
    #[schemars(description = "Name of the new branch")]
    name: String,
    #[schemars(description = "Switch to the new branch after creating it")]
    checkout: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct CheckoutParams {
    #[schemars(description = "Path to the git repository (defaults to current working directory)")]
    repo_path: Option<String>,
    #[schemars(description = "Branch name or commit ref to check out")]
    branch_or_commit: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct StashParams {
    #[schemars(description = "Path to the git repository (defaults to current working directory)")]
    repo_path: Option<String>,
    #[schemars(description = "Optional stash message")]
    message: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct StashPopParams {
    #[schemars(description = "Path to the git repository (defaults to current working directory)")]
    repo_path: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct PullParams {
    #[schemars(description = "Path to the git repository (defaults to current working directory)")]
    repo_path: Option<String>,
    #[schemars(description = "Remote name (default: origin)")]
    remote: Option<String>,
    #[schemars(description = "Branch name to pull (default: current branch)")]
    branch: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct PushParams {
    #[schemars(description = "Path to the git repository (defaults to current working directory)")]
    repo_path: Option<String>,
    #[schemars(description = "Remote name (default: origin)")]
    remote: Option<String>,
    #[schemars(description = "Branch name to push (default: current branch)")]
    branch: Option<String>,
    #[schemars(description = "Force push (use with caution — rewrites remote history)")]
    force: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ShowParams {
    #[schemars(description = "Path to the git repository (defaults to current working directory)")]
    repo_path: Option<String>,
    #[schemars(description = "Commit ref to show (default: HEAD)")]
    ref_: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct BlameParams {
    #[schemars(description = "Path to the git repository (defaults to current working directory)")]
    repo_path: Option<String>,
    #[schemars(description = "File path to blame (relative to repo root)")]
    file: String,
}

// ---------------------------------------------------------------------------
// Server struct
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct GitServer {
    root: Option<PathBuf>,
    allow_push: bool,
}

// ---------------------------------------------------------------------------
// Helper: run a git command
// ---------------------------------------------------------------------------

async fn run_git(repo_path: &Path, args: &[&str]) -> String {
    tracing::debug!("git {} in {}", args.join(" "), repo_path.display());

    let result = tokio::time::timeout(
        Duration::from_secs(30),
        tokio::process::Command::new("git")
            .current_dir(repo_path)
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("LANG", "en_US.UTF-8")
            .env("LC_ALL", "en_US.UTF-8")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output(),
    )
    .await;

    match result {
        Err(_) => "Error: git command timed out after 30s".to_string(),
        Ok(Err(e)) => format!("Error: failed to spawn git: {e}"),
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);

            if output.status.success() {
                let out = stdout.trim_end().to_string();
                if out.is_empty() {
                    let err = stderr.trim_end().to_string();
                    if err.is_empty() {
                        "(no output)".to_string()
                    } else {
                        err
                    }
                } else {
                    out
                }
            } else {
                // Return stderr (or stdout if stderr empty) as error info
                let err = stderr.trim_end().to_string();
                let out = stdout.trim_end().to_string();
                if err.is_empty() && out.is_empty() {
                    format!(
                        "Error: git exited with status {}",
                        output.status.code().unwrap_or(-1)
                    )
                } else if err.is_empty() {
                    out
                } else if out.is_empty() {
                    err
                } else {
                    format!("{out}\n{err}")
                }
            }
        }
    }
}

/// Resolve the repository path: use provided path or fall back to cwd.
/// If root is provided, enforce that the resolved path is within it.
fn resolve_repo(repo_path: Option<String>, root: Option<&Path>) -> Result<PathBuf, String> {
    let path = match repo_path {
        Some(p) => PathBuf::from(&p),
        None => std::env::current_dir().map_err(|e| format!("Cannot get cwd: {e}"))?,
    };
    if !path.exists() {
        return Err(format!("Path does not exist: {}", path.display()));
    }
    let canonical = std::fs::canonicalize(&path)
        .map_err(|e| format!("Cannot resolve path: {e}"))?;
    if let Some(root) = root {
        let canonical_root = std::fs::canonicalize(root)
            .map_err(|e| format!("Cannot resolve root: {e}"))?;
        if !canonical.starts_with(&canonical_root) {
            return Err(format!(
                "Repository '{}' is outside the allowed root '{}'. Use --root to adjust.",
                canonical.display(), canonical_root.display()
            ));
        }
    }
    Ok(canonical)
}

// ---------------------------------------------------------------------------
// Tool implementations
// ---------------------------------------------------------------------------

#[tool_router(server_handler)]
impl GitServer {
    #[tool(description = "Show the working tree status of a git repository. Returns branch name and a list of changed files using short format.")]
    async fn git_status(
        &self,
        Parameters(StatusParams { repo_path }): Parameters<StatusParams>,
    ) -> String {
        let path = match resolve_repo(repo_path, self.root.as_deref()) {
            Ok(p) => p,
            Err(e) => return format!("Error: {e}"),
        };

        // Get branch info and status in one pass
        let branch = run_git(&path, &["rev-parse", "--abbrev-ref", "HEAD"]).await;
        let status = run_git(&path, &["status", "--short"]).await;

        let branch_line = if branch.starts_with("Error") {
            branch
        } else {
            format!("On branch {branch}")
        };

        if status == "(no output)" || status.is_empty() {
            format!("{branch_line}\nNothing to commit, working tree clean")
        } else {
            format!("{branch_line}\n{status}")
        }
    }

    #[tool(description = "Show changes between working tree and index, or between index and last commit. Returns unified diff output.")]
    async fn git_diff(
        &self,
        Parameters(DiffParams { repo_path, staged, file }): Parameters<DiffParams>,
    ) -> String {
        let path = match resolve_repo(repo_path, self.root.as_deref()) {
            Ok(p) => p,
            Err(e) => return format!("Error: {e}"),
        };

        let mut args: Vec<&str> = vec!["diff"];
        if staged {
            args.push("--staged");
        }

        // We need to handle the optional file separately due to lifetime issues.
        let result = if let Some(ref f) = file {
            let mut a = args.clone();
            a.push("--");
            a.push(f.as_str());
            run_git(&path, &a).await
        } else {
            run_git(&path, &args).await
        };

        result
    }

    #[tool(description = "Show commit history of a git repository. Returns formatted commit log with hash, author, date and message.")]
    async fn git_log(
        &self,
        Parameters(LogParams { repo_path, limit, oneline }): Parameters<LogParams>,
    ) -> String {
        let path = match resolve_repo(repo_path, self.root.as_deref()) {
            Ok(p) => p,
            Err(e) => return format!("Error: {e}"),
        };

        let n = limit.unwrap_or(20).max(1).min(1000);
        let n_str = n.to_string();

        let format = if oneline {
            "--oneline"
        } else {
            "--format=%C(auto)%h %C(bold blue)%an%C(reset) %C(dim)%ar%C(reset)%n  %s%n"
        };

        run_git(&path, &["log", format, "-n", &n_str]).await
    }

    #[tool(description = "Create a git commit. Optionally stages all tracked changes first with git add -A.")]
    async fn git_commit(
        &self,
        Parameters(CommitParams { repo_path, message, add_all }): Parameters<CommitParams>,
    ) -> String {
        let path = match resolve_repo(repo_path, self.root.as_deref()) {
            Ok(p) => p,
            Err(e) => return format!("Error: {e}"),
        };

        if message.trim().is_empty() {
            return "Error: commit message cannot be empty".to_string();
        }

        if add_all {
            let add_result = run_git(&path, &["add", "-A"]).await;
            if add_result.starts_with("Error") {
                return format!("Error during git add -A: {add_result}");
            }
            tracing::debug!("git add -A: {add_result}");
        }

        run_git(&path, &["commit", "-m", &message]).await
    }

    #[tool(description = "List all local and remote branches in a git repository.")]
    async fn git_branch_list(
        &self,
        Parameters(BranchListParams { repo_path }): Parameters<BranchListParams>,
    ) -> String {
        let path = match resolve_repo(repo_path, self.root.as_deref()) {
            Ok(p) => p,
            Err(e) => return format!("Error: {e}"),
        };

        run_git(&path, &["branch", "-a"]).await
    }

    #[tool(description = "Create a new git branch. Optionally checks out the branch immediately after creation.")]
    async fn git_branch_create(
        &self,
        Parameters(BranchCreateParams { repo_path, name, checkout }): Parameters<BranchCreateParams>,
    ) -> String {
        let path = match resolve_repo(repo_path, self.root.as_deref()) {
            Ok(p) => p,
            Err(e) => return format!("Error: {e}"),
        };

        if name.trim().is_empty() {
            return "Error: branch name cannot be empty".to_string();
        }

        if checkout {
            run_git(&path, &["checkout", "-b", &name]).await
        } else {
            run_git(&path, &["branch", &name]).await
        }
    }

    #[tool(description = "Switch to a different branch or commit in a git repository.")]
    async fn git_checkout(
        &self,
        Parameters(CheckoutParams { repo_path, branch_or_commit }): Parameters<CheckoutParams>,
    ) -> String {
        let path = match resolve_repo(repo_path, self.root.as_deref()) {
            Ok(p) => p,
            Err(e) => return format!("Error: {e}"),
        };

        if branch_or_commit.trim().is_empty() {
            return "Error: branch or commit ref cannot be empty".to_string();
        }

        run_git(&path, &["checkout", &branch_or_commit]).await
    }

    #[tool(description = "Stash the current working tree changes. Optionally include a descriptive message for the stash.")]
    async fn git_stash(
        &self,
        Parameters(StashParams { repo_path, message }): Parameters<StashParams>,
    ) -> String {
        let path = match resolve_repo(repo_path, self.root.as_deref()) {
            Ok(p) => p,
            Err(e) => return format!("Error: {e}"),
        };

        if let Some(ref msg) = message {
            run_git(&path, &["stash", "push", "-m", msg.as_str()]).await
        } else {
            run_git(&path, &["stash"]).await
        }
    }

    #[tool(description = "Apply and remove the most recent stash entry from the stash list.")]
    async fn git_stash_pop(
        &self,
        Parameters(StashPopParams { repo_path }): Parameters<StashPopParams>,
    ) -> String {
        let path = match resolve_repo(repo_path, self.root.as_deref()) {
            Ok(p) => p,
            Err(e) => return format!("Error: {e}"),
        };

        run_git(&path, &["stash", "pop"]).await
    }

    #[tool(description = "Fetch from and integrate with another repository or branch (git pull).")]
    async fn git_pull(
        &self,
        Parameters(PullParams { repo_path, remote, branch }): Parameters<PullParams>,
    ) -> String {
        let path = match resolve_repo(repo_path, self.root.as_deref()) {
            Ok(p) => p,
            Err(e) => return format!("Error: {e}"),
        };

        let mut args: Vec<&str> = vec!["pull"];

        let remote_str = remote.as_deref().unwrap_or("origin");
        let has_remote = remote.is_some() || branch.is_some();

        if has_remote {
            args.push(remote_str);
            if let Some(ref b) = branch {
                args.push(b.as_str());
            }
        }

        run_git(&path, &args).await
    }

    #[tool(description = "Push local commits to a remote repository. Set force=true to force push (this rewrites remote history and is dangerous).")]
    async fn git_push(
        &self,
        Parameters(PushParams { repo_path, remote, branch, force }): Parameters<PushParams>,
    ) -> String {
        if !self.allow_push {
            return "Error: push is disabled. Start the server with --allow-push to enable git push.".to_string();
        }

        let path = match resolve_repo(repo_path, self.root.as_deref()) {
            Ok(p) => p,
            Err(e) => return format!("Error: {e}"),
        };

        let mut args: Vec<&str> = vec!["push"];

        if force {
            args.push("--force");
        }

        let remote_str = remote.as_deref().unwrap_or("origin");
        let has_remote = remote.is_some() || branch.is_some() || force;

        if has_remote {
            args.push(remote_str);
            if let Some(ref b) = branch {
                args.push(b.as_str());
            }
        }

        let result = run_git(&path, &args).await;

        if force {
            format!("WARNING: force push — {result}")
        } else {
            result
        }
    }

    #[tool(description = "Show details of a specific commit including metadata and diff. Defaults to HEAD if no ref is provided.")]
    async fn git_show(
        &self,
        Parameters(ShowParams { repo_path, ref_ }): Parameters<ShowParams>,
    ) -> String {
        let path = match resolve_repo(repo_path, self.root.as_deref()) {
            Ok(p) => p,
            Err(e) => return format!("Error: {e}"),
        };

        let commit_ref = ref_.as_deref().unwrap_or("HEAD");
        run_git(&path, &["show", commit_ref]).await
    }

    #[tool(description = "Show what revision and author last modified each line of a file (git blame).")]
    async fn git_blame(
        &self,
        Parameters(BlameParams { repo_path, file }): Parameters<BlameParams>,
    ) -> String {
        let path = match resolve_repo(repo_path, self.root.as_deref()) {
            Ok(p) => p,
            Err(e) => return format!("Error: {e}"),
        };

        if file.trim().is_empty() {
            return "Error: file path cannot be empty".to_string();
        }

        run_git(&path, &["blame", &file]).await
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

    tracing::info!("Starting mcp-git server");

    let args = Args::parse();

    let service = GitServer {
        root: args.root,
        allow_push: args.allow_push,
    }
    .serve(stdio())
    .await?;
    service.waiting().await?;

    Ok(())
}
