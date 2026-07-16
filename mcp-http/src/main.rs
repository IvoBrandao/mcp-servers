use std::collections::HashMap;

use base64::Engine;
use reqwest::{
    header::{HeaderMap, HeaderName, HeaderValue, USER_AGENT},
    redirect, Client,
};
use rmcp::{handler::server::wrapper::Parameters, schemars, tool, tool_router};
use rmcp::{transport::stdio, ServiceExt};
use serde::Deserialize;

// ── constants ────────────────────────────────────────────────────────────────

const USER_AGENT_VALUE: &str = "mcp-http/0.1.0";
const BODY_TRUNCATE: usize = 100_000;
const TIMEOUT_SECS: u64 = 30;
const MAX_REDIRECTS: usize = 10;

// ── parameter structs ────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct GetParams {
    #[schemars(description = "URL to request")]
    url: String,
    #[schemars(description = "Optional JSON object of request headers, e.g. {\"Authorization\": \"Bearer token\"}")]
    headers: Option<String>,
    #[schemars(description = "Whether to follow HTTP redirects (default true)")]
    follow_redirects: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct BodyParams {
    #[schemars(description = "URL to request")]
    url: String,
    #[schemars(description = "Request body (JSON string, form data, plain text, etc.)")]
    body: String,
    #[schemars(description = "Content-Type header value; defaults to application/json")]
    content_type: Option<String>,
    #[schemars(description = "Optional JSON object of additional request headers")]
    headers: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct NoBodyParams {
    #[schemars(description = "URL to request")]
    url: String,
    #[schemars(description = "Optional JSON object of request headers")]
    headers: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct DownloadParams {
    #[schemars(description = "URL of the file to download")]
    url: String,
    #[schemars(description = "Local filesystem path to write the downloaded file to")]
    output_path: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct FormPostParams {
    #[schemars(description = "URL to POST to")]
    url: String,
    #[schemars(description = "JSON object whose keys/values become URL-encoded form fields")]
    fields_json: String,
    #[schemars(description = "Optional JSON object of additional request headers")]
    headers: Option<String>,
}

// ── server struct ────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct HttpServer {
    /// Shared client that follows redirects (up to MAX_REDIRECTS).
    client_follow: Client,
    /// Shared client that does NOT follow redirects.
    client_no_follow: Client,
}

impl HttpServer {
    fn new() -> anyhow::Result<Self> {
        let client_follow = Client::builder()
            .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
            .redirect(redirect::Policy::limited(MAX_REDIRECTS))
            .use_rustls_tls()
            .user_agent(USER_AGENT_VALUE)
            .build()?;

        let client_no_follow = Client::builder()
            .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
            .redirect(redirect::Policy::none())
            .use_rustls_tls()
            .user_agent(USER_AGENT_VALUE)
            .build()?;

        Ok(Self {
            client_follow,
            client_no_follow,
        })
    }
}

// ── helper functions ─────────────────────────────────────────────────────────

/// Parse a JSON object string into a `HashMap<String, String>`.
/// Returns an error string on failure.
fn parse_headers(raw: Option<String>) -> Result<HashMap<String, String>, String> {
    match raw {
        None => Ok(HashMap::new()),
        Some(s) => {
            if s.trim().is_empty() {
                return Ok(HashMap::new());
            }
            let map: serde_json::Value =
                serde_json::from_str(&s).map_err(|e| format!("Error: invalid headers JSON: {e}"))?;
            let obj = map
                .as_object()
                .ok_or_else(|| "Error: headers must be a JSON object".to_string())?;
            let mut result = HashMap::new();
            for (k, v) in obj {
                let val = match v {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                result.insert(k.clone(), val);
            }
            Ok(result)
        }
    }
}

/// Build a `HeaderMap` from a string→string map and an optional User-Agent override.
fn build_header_map(extra: HashMap<String, String>) -> Result<HeaderMap, String> {
    let mut map = HeaderMap::new();
    // Always set a User-Agent (can be overridden by caller).
    map.insert(
        USER_AGENT,
        HeaderValue::from_static(USER_AGENT_VALUE),
    );
    for (k, v) in extra {
        let name = HeaderName::from_bytes(k.as_bytes())
            .map_err(|e| format!("Error: invalid header name '{k}': {e}"))?;
        let val = HeaderValue::from_str(&v)
            .map_err(|e| format!("Error: invalid header value for '{k}': {e}"))?;
        map.insert(name, val);
    }
    Ok(map)
}

/// Determine if a content-type indicates binary content.
fn is_binary_content_type(ct: &str) -> bool {
    let ct = ct.to_lowercase();
    // Text types we know are safe to display.
    if ct.starts_with("text/")
        || ct.contains("json")
        || ct.contains("xml")
        || ct.contains("javascript")
        || ct.contains("x-www-form-urlencoded")
        || ct.contains("yaml")
        || ct.contains("html")
        || ct.contains("csv")
    {
        return false;
    }
    // Everything else (image/*, audio/*, video/*, application/octet-stream, zip, …).
    true
}

/// Format response headers as a pretty JSON object string.
fn format_response_headers(headers: &reqwest::header::HeaderMap) -> String {
    let mut map = serde_json::Map::new();
    for (k, v) in headers {
        let val = v.to_str().unwrap_or("<binary>").to_string();
        map.insert(k.as_str().to_string(), serde_json::Value::String(val));
    }
    serde_json::to_string_pretty(&serde_json::Value::Object(map))
        .unwrap_or_else(|_| "{}".to_string())
}

/// Format a completed response into the standard output string.
async fn format_response(resp: reqwest::Response) -> String {
    let status = resp.status();
    let status_line = format!("{} {}", status.as_u16(), status.canonical_reason().unwrap_or(""));
    let ct = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let headers_json = format_response_headers(resp.headers());

    let body_text = match resp.bytes().await {
        Err(e) => return format!("Error: failed to read response body: {e}"),
        Ok(bytes) => {
            if is_binary_content_type(&ct) {
                format!(
                    "[binary {} bytes, base64]\n{}",
                    bytes.len(),
                    base64::engine::general_purpose::STANDARD.encode(&bytes)
                )
            } else {
                let text = String::from_utf8_lossy(&bytes).to_string();
                if text.len() > BODY_TRUNCATE {
                    format!(
                        "{}\n[truncated at {BODY_TRUNCATE} chars, total {} chars]",
                        &text[..BODY_TRUNCATE],
                        text.len()
                    )
                } else {
                    text
                }
            }
        }
    };

    format!(
        "Status: {status_line}\nHeaders: {headers_json}\n\n{body_text}"
    )
}

// ── tool implementations ─────────────────────────────────────────────────────

#[tool_router(server_handler)]
impl HttpServer {
    #[tool(description = "Perform an HTTP GET request. Returns status, headers as JSON, and the response body (text or base64 for binary content).")]
    async fn http_get(
        &self,
        Parameters(GetParams {
            url,
            headers,
            follow_redirects,
        }): Parameters<GetParams>,
    ) -> String {
        tracing::info!(%url, follow_redirects, "http_get");
        let extra = match parse_headers(headers) {
            Ok(h) => h,
            Err(e) => return e,
        };
        let header_map = match build_header_map(extra) {
            Ok(h) => h,
            Err(e) => return e,
        };

        let client = if follow_redirects {
            &self.client_follow
        } else {
            &self.client_no_follow
        };

        let resp = match client.get(&url).headers(header_map).send().await {
            Ok(r) => r,
            Err(e) => return format!("Error: {e}"),
        };

        format_response(resp).await
    }

    #[tool(description = "Perform an HTTP POST request with a body. content_type defaults to application/json. Returns status and response body.")]
    async fn http_post(
        &self,
        Parameters(BodyParams {
            url,
            body,
            content_type,
            headers,
        }): Parameters<BodyParams>,
    ) -> String {
        tracing::info!(%url, "http_post");
        self.send_with_body(reqwest::Method::POST, url, body, content_type, headers).await
    }

    #[tool(description = "Perform an HTTP PUT request with a body. content_type defaults to application/json. Returns status and response body.")]
    async fn http_put(
        &self,
        Parameters(BodyParams {
            url,
            body,
            content_type,
            headers,
        }): Parameters<BodyParams>,
    ) -> String {
        tracing::info!(%url, "http_put");
        self.send_with_body(reqwest::Method::PUT, url, body, content_type, headers).await
    }

    #[tool(description = "Perform an HTTP PATCH request with a body. content_type defaults to application/json. Returns status and response body.")]
    async fn http_patch(
        &self,
        Parameters(BodyParams {
            url,
            body,
            content_type,
            headers,
        }): Parameters<BodyParams>,
    ) -> String {
        tracing::info!(%url, "http_patch");
        self.send_with_body(reqwest::Method::PATCH, url, body, content_type, headers).await
    }

    #[tool(description = "Perform an HTTP DELETE request. Returns status and response body.")]
    async fn http_delete(
        &self,
        Parameters(NoBodyParams { url, headers }): Parameters<NoBodyParams>,
    ) -> String {
        tracing::info!(%url, "http_delete");
        let extra = match parse_headers(headers) {
            Ok(h) => h,
            Err(e) => return e,
        };
        let header_map = match build_header_map(extra) {
            Ok(h) => h,
            Err(e) => return e,
        };
        let resp = match self
            .client_follow
            .delete(&url)
            .headers(header_map)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => return format!("Error: {e}"),
        };
        format_response(resp).await
    }

    #[tool(description = "Perform an HTTP HEAD request. Returns only the response status and headers (no body).")]
    async fn http_head(
        &self,
        Parameters(NoBodyParams { url, headers }): Parameters<NoBodyParams>,
    ) -> String {
        tracing::info!(%url, "http_head");
        let extra = match parse_headers(headers) {
            Ok(h) => h,
            Err(e) => return e,
        };
        let header_map = match build_header_map(extra) {
            Ok(h) => h,
            Err(e) => return e,
        };
        let resp = match self
            .client_follow
            .head(&url)
            .headers(header_map)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => return format!("Error: {e}"),
        };

        let status = resp.status();
        let status_line = format!("{} {}", status.as_u16(), status.canonical_reason().unwrap_or(""));
        let headers_json = format_response_headers(resp.headers());
        format!("Status: {status_line}\nHeaders: {headers_json}")
    }

    #[tool(description = "Download a file from a URL and save it to a local path. Returns the file size and path.")]
    async fn http_download(
        &self,
        Parameters(DownloadParams { url, output_path }): Parameters<DownloadParams>,
    ) -> String {
        use tokio::io::AsyncWriteExt;

        tracing::info!(%url, %output_path, "http_download");
        let resp = match self.client_follow.get(&url).send().await {
            Ok(r) => r,
            Err(e) => return format!("Error: {e}"),
        };

        if !resp.status().is_success() {
            let status = resp.status();
            return format!(
                "Error: server returned {} {}",
                status.as_u16(),
                status.canonical_reason().unwrap_or("")
            );
        }

        // Create parent directories if needed.
        if let Some(parent) = std::path::Path::new(&output_path).parent() {
            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                return format!("Error: could not create parent directory: {e}");
            }
        }

        let mut file = match tokio::fs::File::create(&output_path).await {
            Ok(f) => f,
            Err(e) => return format!("Error: could not create file '{output_path}': {e}"),
        };

        let bytes = match resp.bytes().await {
            Ok(b) => b,
            Err(e) => return format!("Error: failed to read response body: {e}"),
        };

        let size = bytes.len();
        if let Err(e) = file.write_all(&bytes).await {
            return format!("Error: failed to write file: {e}");
        }

        format!("Downloaded {size} bytes to {output_path}")
    }

    #[tool(description = "Perform an HTTP POST with application/x-www-form-urlencoded encoding. fields_json is a JSON object of field names to values.")]
    async fn http_form_post(
        &self,
        Parameters(FormPostParams {
            url,
            fields_json,
            headers,
        }): Parameters<FormPostParams>,
    ) -> String {
        tracing::info!(%url, "http_form_post");
        let extra = match parse_headers(headers) {
            Ok(h) => h,
            Err(e) => return e,
        };
        let mut header_map = match build_header_map(extra) {
            Ok(h) => h,
            Err(e) => return e,
        };

        // Parse fields.
        let fields_val: serde_json::Value = match serde_json::from_str(&fields_json) {
            Ok(v) => v,
            Err(e) => return format!("Error: invalid fields_json: {e}"),
        };
        let obj = match fields_val.as_object() {
            Some(o) => o,
            None => return "Error: fields_json must be a JSON object".to_string(),
        };
        let mut form_fields: Vec<(String, String)> = Vec::new();
        for (k, v) in obj {
            let val = match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            form_fields.push((k.clone(), val));
        }

        // URL-encode the form body.
        let encoded: String = form_fields
            .iter()
            .map(|(k, v)| {
                format!(
                    "{}={}",
                    urlenccode(k),
                    urlenccode(v)
                )
            })
            .collect::<Vec<_>>()
            .join("&");

        header_map.insert(
            reqwest::header::CONTENT_TYPE,
            HeaderValue::from_static("application/x-www-form-urlencoded"),
        );

        let resp = match self
            .client_follow
            .post(&url)
            .headers(header_map)
            .body(encoded)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => return format!("Error: {e}"),
        };
        format_response(resp).await
    }
}

// ── private helpers (not tools) ───────────────────────────────────────────────

impl HttpServer {
    /// Shared logic for POST / PUT / PATCH.
    async fn send_with_body(
        &self,
        method: reqwest::Method,
        url: String,
        body: String,
        content_type: Option<String>,
        headers: Option<String>,
    ) -> String {
        let extra = match parse_headers(headers) {
            Ok(h) => h,
            Err(e) => return e,
        };
        let mut header_map = match build_header_map(extra) {
            Ok(h) => h,
            Err(e) => return e,
        };

        let ct = content_type
            .unwrap_or_else(|| "application/json".to_string());
        match HeaderValue::from_str(&ct) {
            Ok(v) => {
                header_map.insert(reqwest::header::CONTENT_TYPE, v);
            }
            Err(e) => return format!("Error: invalid content-type '{ct}': {e}"),
        }

        let resp = match self
            .client_follow
            .request(method, &url)
            .headers(header_map)
            .body(body)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => return format!("Error: {e}"),
        };
        format_response(resp).await
    }
}

/// Minimal percent-encoding for application/x-www-form-urlencoded values.
fn urlenccode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9'
            | b'-' | b'_' | b'.' | b'*' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ── entry point ──────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let server = HttpServer::new()?;
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
