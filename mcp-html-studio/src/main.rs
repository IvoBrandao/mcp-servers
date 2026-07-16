use std::{
    collections::HashMap,
    io::Write as IoWrite,
    path::{Path, PathBuf},
    sync::Arc,
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use rmcp::{
    ServiceExt, handler::server::wrapper::Parameters, schemars, tool, tool_router,
    transport::stdio,
};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::RwLock;
use walkdir::WalkDir;
use zip::{ZipWriter, write::SimpleFileOptions};

// ── HTML Templates ────────────────────────────────────────────────────────────

const TEMPLATE_BASIC: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>My Project</title>
    <style>
        body {
            font-family: system-ui, -apple-system, BlinkMacSystemFont, sans-serif;
            max-width: 800px;
            margin: 0 auto;
            padding: 2rem;
            color: #1a1a1a;
            background: #fff;
        }
        h1 { color: #333; }
    </style>
</head>
<body>
    <h1>Hello, World!</h1>
    <p>Edit this file to get started.</p>
    <script>
        console.log('Project loaded successfully.');
    </script>
</body>
</html>
"#;

const TEMPLATE_TAILWIND: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>My Tailwind Project</title>
    <script src="https://cdn.tailwindcss.com"></script>
</head>
<body class="bg-gray-50 min-h-screen flex items-center justify-center">
    <div class="bg-white rounded-2xl shadow-lg p-10 max-w-md w-full mx-4">
        <h1 class="text-3xl font-bold text-gray-900 mb-3">Hello, Tailwind!</h1>
        <p class="text-gray-500 mb-6">Edit this file to get started with Tailwind CSS.</p>
        <button
            class="px-5 py-2.5 bg-blue-600 text-white rounded-lg font-medium hover:bg-blue-700 active:scale-95 transition-all"
            onclick="this.textContent = 'Clicked!'"
        >
            Click Me
        </button>
    </div>
</body>
</html>
"#;

const TEMPLATE_REACT: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>My React Project</title>
    <script crossorigin src="https://unpkg.com/react@18/umd/react.development.js"></script>
    <script crossorigin src="https://unpkg.com/react-dom@18/umd/react-dom.development.js"></script>
    <script src="https://unpkg.com/@babel/standalone/babel.min.js"></script>
    <style>
        body {
            font-family: system-ui, -apple-system, sans-serif;
            margin: 0;
            padding: 2rem;
            background: #f8fafc;
        }
        .card {
            background: white;
            border-radius: 12px;
            padding: 2rem;
            max-width: 400px;
            box-shadow: 0 2px 8px rgba(0,0,0,0.08);
        }
        button {
            padding: 0.5rem 1.25rem;
            background: #3b82f6;
            color: white;
            border: none;
            border-radius: 8px;
            cursor: pointer;
            font-size: 1rem;
        }
        button:hover { background: #2563eb; }
    </style>
</head>
<body>
    <div id="root"></div>
    <script type="text/babel">
        function App() {
            const [count, setCount] = React.useState(0);
            return (
                <div className="card">
                    <h1>Hello, React!</h1>
                    <p>Count: <strong>{count}</strong></p>
                    <button onClick={() => setCount(c => c + 1)}>Increment</button>
                </div>
            );
        }

        const root = ReactDOM.createRoot(document.getElementById('root'));
        root.render(<App />);
    </script>
</body>
</html>
"#;

const TEMPLATE_VUE: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>My Vue Project</title>
    <script src="https://unpkg.com/vue@3/dist/vue.global.js"></script>
    <style>
        body {
            font-family: system-ui, -apple-system, sans-serif;
            margin: 0;
            padding: 2rem;
            background: #f8fafc;
        }
        .card {
            background: white;
            border-radius: 12px;
            padding: 2rem;
            max-width: 400px;
            box-shadow: 0 2px 8px rgba(0,0,0,0.08);
        }
        button {
            padding: 0.5rem 1.25rem;
            background: #42b883;
            color: white;
            border: none;
            border-radius: 8px;
            cursor: pointer;
            font-size: 1rem;
        }
        button:hover { background: #33a06f; }
        input {
            margin-top: 0.75rem;
            padding: 0.4rem 0.75rem;
            border: 1px solid #d1d5db;
            border-radius: 6px;
            font-size: 1rem;
            width: 100%;
            box-sizing: border-box;
        }
    </style>
</head>
<body>
    <div id="app">
        <div class="card">
            <h1>Hello, {{ message }}!</h1>
            <p>Count: <strong>{{ count }}</strong></p>
            <button @click="count++">Increment</button>
            <input v-model="message" placeholder="Change the greeting" />
        </div>
    </div>
    <script>
        const { createApp, ref } = Vue;
        createApp({
            setup() {
                const message = ref('Vue');
                const count = ref(0);
                return { message, count };
            }
        }).mount('#app');
    </script>
</body>
</html>
"#;

// ── Parameter structs ─────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
struct CreateProjectParams {
    #[schemars(description = "Project name or nested path, e.g. 'my-app' or 'clients/acme'")]
    name: String,
    #[schemars(description = "Starter template: basic (default), tailwind, react, vue")]
    template: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ProjectNameParams {
    #[schemars(description = "Project name or path")]
    name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct EditFileParams {
    #[schemars(description = "Project name or path")]
    project: String,
    #[schemars(description = "File path within the project, e.g. 'index.html' or 'css/styles.css'")]
    file: String,
    #[schemars(description = "Content to write to the file")]
    content: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct FileParams {
    #[schemars(description = "Project name or path")]
    project: String,
    #[schemars(description = "File path within the project")]
    file: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ImportProjectParams {
    #[schemars(description = "Name for the imported project")]
    name: String,
    #[schemars(description = "Base64-encoded ZIP archive content")]
    zip_base64: String,
}

// ── Server state ──────────────────────────────────────────────────────────────

type ActiveServers = Arc<RwLock<HashMap<String, (u16, tokio::task::JoinHandle<()>)>>>;

#[derive(Debug, Clone)]
struct HtmlStudioServer {
    sandbox_root: PathBuf,
    active_servers: ActiveServers,
}

// ── Path helpers ──────────────────────────────────────────────────────────────

impl HtmlStudioServer {
    fn new(sandbox_root: PathBuf) -> anyhow::Result<Self> {
        std::fs::create_dir_all(&sandbox_root)?;
        let sandbox_root = sandbox_root.canonicalize()?;
        tracing::info!(root = %sandbox_root.display(), "Sandbox root initialised");
        Ok(Self {
            sandbox_root,
            active_servers: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    /// Resolve a project name into an absolute path, rejecting traversal attempts.
    fn project_path(&self, name: &str) -> Result<PathBuf, String> {
        let name = name.trim_matches('/');
        if name.split('/').any(|seg| seg == "..") {
            return Err("Project name must not contain '..' components".to_string());
        }
        if name.is_empty() {
            return Err("Project name must not be empty".to_string());
        }
        let path = self.sandbox_root.join(name);
        if !path.starts_with(&self.sandbox_root) {
            return Err(format!(
                "Project path escapes sandbox: {}",
                path.display()
            ));
        }
        Ok(path)
    }

    /// Resolve a file path within a project directory, rejecting traversal.
    fn file_path(&self, project_dir: &Path, file: &str) -> Result<PathBuf, String> {
        let file = file.trim_matches('/');
        if file.split('/').any(|seg| seg == "..") {
            return Err("File path must not contain '..' components".to_string());
        }
        if file.is_empty() {
            return Err("File path must not be empty".to_string());
        }
        let path = project_dir.join(file);
        if !path.starts_with(project_dir) {
            return Err(format!(
                "File path escapes project directory: {}",
                path.display()
            ));
        }
        Ok(path)
    }
}

// ── Port finding ──────────────────────────────────────────────────────────────

async fn find_free_port(
    range: std::ops::RangeInclusive<u16>,
) -> Option<(u16, tokio::net::TcpListener)> {
    for port in range {
        if let Ok(listener) = tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
            return Some((port, listener));
        }
    }
    None
}

// ── Tool implementations ──────────────────────────────────────────────────────

#[tool_router(server_handler)]
impl HtmlStudioServer {
    #[tool(description = "Create a new HTML project with a starter template. Templates: basic (default), tailwind, react, vue. Name supports nested paths like 'clients/acme'.")]
    async fn create_project(
        &self,
        Parameters(CreateProjectParams { name, template }): Parameters<CreateProjectParams>,
    ) -> String {
        let project_dir = match self.project_path(&name) {
            Ok(p) => p,
            Err(e) => return format!("Error: {e}"),
        };

        if project_dir.exists() {
            return format!(
                "Error: project '{}' already exists at {}",
                name,
                project_dir.display()
            );
        }

        if let Err(e) = std::fs::create_dir_all(&project_dir) {
            return format!("Error: cannot create project directory: {e}");
        }

        let tpl = template.as_deref().unwrap_or("basic");
        let html = match tpl {
            "tailwind" => TEMPLATE_TAILWIND,
            "react" => TEMPLATE_REACT,
            "vue" => TEMPLATE_VUE,
            _ => TEMPLATE_BASIC,
        };

        let index_path = project_dir.join("index.html");
        if let Err(e) = std::fs::write(&index_path, html) {
            return format!("Error: cannot write index.html: {e}");
        }

        tracing::info!(project = %name, template = tpl, "Created project");
        format!(
            "Created project '{}' with template '{}' at {}",
            name,
            tpl,
            project_dir.display()
        )
    }

    #[tool(description = "List all HTML projects (directories that contain index.html) in the sandbox. Shows which ones have an active preview server.")]
    async fn list_projects(&self) -> String {
        fn collect(base: &Path, dir: &Path, out: &mut Vec<String>) {
            let Ok(rd) = std::fs::read_dir(dir) else {
                return;
            };
            let mut entries: Vec<_> = rd.filter_map(|e| e.ok()).collect();
            entries.sort_by_key(|e| e.file_name());
            for entry in entries {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if name_str.starts_with('.') {
                    continue; // skip hidden dirs
                }
                if path.join("index.html").exists() {
                    if let Ok(rel) = path.strip_prefix(base) {
                        out.push(rel.to_string_lossy().to_string());
                    }
                }
                // Recurse regardless (nested projects)
                collect(base, &path, out);
            }
        }

        let mut projects = Vec::new();
        collect(&self.sandbox_root, &self.sandbox_root, &mut projects);

        if projects.is_empty() {
            return "No projects found. Use create_project to create one.".to_string();
        }

        let active = self.active_servers.read().await;
        let lines: Vec<String> = projects
            .iter()
            .map(|p| {
                if let Some((port, _)) = active.get(p) {
                    format!("{p}  [preview: http://localhost:{port}]")
                } else {
                    p.clone()
                }
            })
            .collect();

        format!(
            "{} project(s):\n{}",
            lines.len(),
            lines.join("\n")
        )
    }

    #[tool(description = "Start an HTTP preview server for a project and return its local URL (http://localhost:PORT). The server serves static files from the project directory.")]
    async fn open_project(
        &self,
        Parameters(ProjectNameParams { name }): Parameters<ProjectNameParams>,
    ) -> String {
        let project_dir = match self.project_path(&name) {
            Ok(p) => p,
            Err(e) => return format!("Error: {e}"),
        };

        if !project_dir.exists() {
            return format!("Error: project '{}' not found", name);
        }

        // Already running?
        {
            let active = self.active_servers.read().await;
            if let Some((port, _)) = active.get(&name) {
                return format!(
                    "Project '{}' preview is already running at http://localhost:{}",
                    name, port
                );
            }
        }

        // Find a free port in the 8080-8090 range
        let (port, listener) = match find_free_port(8080..=8090).await {
            Some(v) => v,
            None => {
                return "Error: no free port available in range 8080-8090. Close some projects first.".to_string();
            }
        };

        let serve_dir = tower_http::services::ServeDir::new(&project_dir)
            .append_index_html_on_directories(true);
        let app = axum::Router::new().fallback_service(serve_dir);

        let handle = tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                tracing::error!("Preview server terminated with error: {e}");
            }
        });

        {
            let mut active = self.active_servers.write().await;
            active.insert(name.clone(), (port, handle));
        }

        tracing::info!(project = %name, port = port, "Started preview server");
        format!(
            "Project '{}' is now running at http://localhost:{}",
            name, port
        )
    }

    #[tool(description = "Stop the HTTP preview server for a project.")]
    async fn close_project(
        &self,
        Parameters(ProjectNameParams { name }): Parameters<ProjectNameParams>,
    ) -> String {
        let mut active = self.active_servers.write().await;
        match active.remove(&name) {
            Some((port, handle)) => {
                handle.abort();
                tracing::info!(project = %name, port = port, "Stopped preview server");
                format!(
                    "Stopped preview server for '{}' (was on port {})",
                    name, port
                )
            }
            None => format!(
                "No preview server is running for project '{}'",
                name
            ),
        }
    }

    #[tool(description = "Write content to a file inside a project. Creates parent directories automatically.")]
    async fn edit_file(
        &self,
        Parameters(EditFileParams {
            project,
            file,
            content,
        }): Parameters<EditFileParams>,
    ) -> String {
        let project_dir = match self.project_path(&project) {
            Ok(p) => p,
            Err(e) => return format!("Error: {e}"),
        };

        if !project_dir.exists() {
            return format!("Error: project '{}' not found", project);
        }

        let file_path = match self.file_path(&project_dir, &file) {
            Ok(p) => p,
            Err(e) => return format!("Error: {e}"),
        };

        if let Some(parent) = file_path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return format!("Error: cannot create parent directories: {e}");
            }
        }

        let byte_count = content.len();
        match std::fs::write(&file_path, &content) {
            Ok(_) => {
                tracing::info!(
                    project = %project,
                    file = %file,
                    bytes = byte_count,
                    "Wrote file"
                );
                format!("Wrote {} bytes to {}/{}", byte_count, project, file)
            }
            Err(e) => format!("Error: cannot write file: {e}"),
        }
    }

    #[tool(description = "Read the text content of a file inside a project.")]
    async fn read_file(
        &self,
        Parameters(FileParams { project, file }): Parameters<FileParams>,
    ) -> String {
        let project_dir = match self.project_path(&project) {
            Ok(p) => p,
            Err(e) => return format!("Error: {e}"),
        };

        if !project_dir.exists() {
            return format!("Error: project '{}' not found", project);
        }

        let file_path = match self.file_path(&project_dir, &file) {
            Ok(p) => p,
            Err(e) => return format!("Error: {e}"),
        };

        if !file_path.exists() {
            return format!(
                "Error: file '{}' not found in project '{}'",
                file, project
            );
        }

        match std::fs::read_to_string(&file_path) {
            Ok(content) => content,
            Err(e) => format!("Error: cannot read file: {e}"),
        }
    }

    #[tool(description = "List all files in a project, showing relative paths sorted alphabetically.")]
    async fn list_files(
        &self,
        Parameters(ProjectNameParams { name }): Parameters<ProjectNameParams>,
    ) -> String {
        let project_dir = match self.project_path(&name) {
            Ok(p) => p,
            Err(e) => return format!("Error: {e}"),
        };

        if !project_dir.exists() {
            return format!("Error: project '{}' not found", name);
        }

        let mut files: Vec<String> = WalkDir::new(&project_dir)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .filter_map(|e| {
                e.path()
                    .strip_prefix(&project_dir)
                    .ok()
                    .map(|rel| rel.to_string_lossy().to_string())
            })
            .collect();

        files.sort();

        if files.is_empty() {
            format!("Project '{}' has no files", name)
        } else {
            format!("Files in '{}':\n{}", name, files.join("\n"))
        }
    }

    #[tool(description = "Stop the preview server (if running) and permanently delete a project and all its contents.")]
    async fn delete_project(
        &self,
        Parameters(ProjectNameParams { name }): Parameters<ProjectNameParams>,
    ) -> String {
        let project_dir = match self.project_path(&name) {
            Ok(p) => p,
            Err(e) => return format!("Error: {e}"),
        };

        if !project_dir.exists() {
            return format!("Error: project '{}' not found", name);
        }

        // Stop preview server if running
        {
            let mut active = self.active_servers.write().await;
            if let Some((port, handle)) = active.remove(&name) {
                handle.abort();
                tracing::info!(
                    project = %name,
                    port = port,
                    "Stopped preview server before deleting project"
                );
            }
        }

        match std::fs::remove_dir_all(&project_dir) {
            Ok(_) => {
                tracing::info!(project = %name, "Deleted project");
                format!("Deleted project '{}'", name)
            }
            Err(e) => format!("Error: cannot delete project: {e}"),
        }
    }

    #[tool(description = "Export a project as a base64-encoded ZIP archive. Returns the base64 string, which can be decoded or passed to import_project.")]
    async fn export_project(
        &self,
        Parameters(ProjectNameParams { name }): Parameters<ProjectNameParams>,
    ) -> String {
        let project_dir = match self.project_path(&name) {
            Ok(p) => p,
            Err(e) => return format!("Error: {e}"),
        };

        if !project_dir.exists() {
            return format!("Error: project '{}' not found", name);
        }

        let mut buf: Vec<u8> = Vec::new();
        {
            let cursor = std::io::Cursor::new(&mut buf);
            let mut zip = ZipWriter::new(cursor);
            let options =
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

            for entry in WalkDir::new(&project_dir)
                .into_iter()
                .filter_map(|e| e.ok())
            {
                let path = entry.path();
                let rel = match path.strip_prefix(&project_dir) {
                    Ok(r) => r,
                    Err(_) => continue,
                };
                let rel_str = rel.to_string_lossy();

                if path.is_dir() {
                    if !rel_str.is_empty() {
                        if let Err(e) = zip.add_directory(format!("{rel_str}/"), options) {
                            return format!("Error: zip directory entry failed: {e}");
                        }
                    }
                } else {
                    if let Err(e) = zip.start_file(rel_str.as_ref(), options) {
                        return format!("Error: zip file entry failed: {e}");
                    }
                    let data = match std::fs::read(path) {
                        Ok(d) => d,
                        Err(e) => {
                            return format!(
                                "Error: cannot read {}: {e}",
                                path.display()
                            )
                        }
                    };
                    if let Err(e) = zip.write_all(&data) {
                        return format!("Error: zip write failed: {e}");
                    }
                }
            }

            if let Err(e) = zip.finish() {
                return format!("Error: zip finalisation failed: {e}");
            }
        }

        let zip_size = buf.len();
        let encoded = BASE64.encode(&buf);
        tracing::info!(project = %name, zip_bytes = zip_size, "Exported project");
        encoded
    }

    #[tool(description = "Import a project from a base64-encoded ZIP archive. Decodes and extracts the archive into the project directory.")]
    async fn import_project(
        &self,
        Parameters(ImportProjectParams { name, zip_base64 }): Parameters<ImportProjectParams>,
    ) -> String {
        let project_dir = match self.project_path(&name) {
            Ok(p) => p,
            Err(e) => return format!("Error: {e}"),
        };

        let zip_bytes = match BASE64.decode(zip_base64.trim()) {
            Ok(b) => b,
            Err(e) => return format!("Error: base64 decode failed: {e}"),
        };

        if let Err(e) = std::fs::create_dir_all(&project_dir) {
            return format!("Error: cannot create project directory: {e}");
        }

        let cursor = std::io::Cursor::new(zip_bytes);
        let mut archive = match zip::ZipArchive::new(cursor) {
            Ok(a) => a,
            Err(e) => return format!("Error: invalid ZIP archive: {e}"),
        };

        let total = archive.len();
        let mut extracted = 0usize;

        for i in 0..total {
            let mut entry = match archive.by_index(i) {
                Ok(e) => e,
                Err(e) => return format!("Error: cannot read ZIP entry {i}: {e}"),
            };

            let entry_name = entry.name().to_string();

            // Reject any traversal within the archive
            if entry_name.split('/').any(|seg| seg == "..") {
                return format!(
                    "Error: ZIP entry '{}' contains '..' — aborting for safety",
                    entry_name
                );
            }

            let out_path = project_dir.join(&entry_name);
            if !out_path.starts_with(&project_dir) {
                return format!(
                    "Error: ZIP entry '{}' would escape project directory",
                    entry_name
                );
            }

            if entry.is_dir() {
                if let Err(e) = std::fs::create_dir_all(&out_path) {
                    return format!(
                        "Error: cannot create directory {}: {e}",
                        out_path.display()
                    );
                }
            } else {
                if let Some(parent) = out_path.parent() {
                    if let Err(e) = std::fs::create_dir_all(parent) {
                        return format!("Error: cannot create parent directories: {e}");
                    }
                }
                let mut out_file = match std::fs::File::create(&out_path) {
                    Ok(f) => f,
                    Err(e) => {
                        return format!(
                            "Error: cannot create {}: {e}",
                            out_path.display()
                        )
                    }
                };
                if let Err(e) = std::io::copy(&mut entry, &mut out_file) {
                    return format!(
                        "Error: cannot write {}: {e}",
                        out_path.display()
                    );
                }
                extracted += 1;
            }
        }

        tracing::info!(project = %name, files = extracted, "Imported project");
        format!(
            "Imported project '{}' — extracted {} file(s) to {}",
            name,
            extracted,
            project_dir.display()
        )
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

    // Sandbox root: HTML_STUDIO_ROOT env var, or $HOME/html-studio-projects, or ./html-studio-projects
    let sandbox_root = std::env::var("HTML_STUDIO_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::var("HOME")
                .map(|h| PathBuf::from(h).join("html-studio-projects"))
                .unwrap_or_else(|_| PathBuf::from("html-studio-projects"))
        });

    tracing::info!(root = %sandbox_root.display(), "Starting mcp-html-studio server");

    let server = HtmlStudioServer::new(sandbox_root)?;
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
