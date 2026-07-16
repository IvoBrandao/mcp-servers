use std::collections::HashSet;
use std::sync::Arc;

use anyhow::Context;
use regex::Regex;
use reqwest::Client;
use rmcp::{ServiceExt, handler::server::wrapper::Parameters, schemars, tool, tool_router, transport::stdio};
use serde::Deserialize;

// ---------------------------------------------------------------------------
// Parameter structs
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct WebSearchParams {
    #[schemars(description = "Search query string")]
    query: String,
    #[schemars(description = "Maximum number of results to return (default: 10)")]
    limit: Option<i64>,
    #[schemars(description = "If true, also fetch and include text content from the top 3 results")]
    fetch_content: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct FetchPageParams {
    #[schemars(description = "URL to fetch")]
    url: String,
    #[schemars(description = "If true, strip HTML tags and return plain text; otherwise return raw content")]
    extract_text: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct WebSearchQuickParams {
    #[schemars(description = "Search query string")]
    query: String,
}

// ---------------------------------------------------------------------------
// Server struct
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct WebSearchServer {
    client: Arc<Client>,
}

// Platform-appropriate user-agent: sites sometimes serve different content
// (or block requests) based on UA. Match the binary's actual host OS.
#[cfg(target_os = "macos")]
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
    AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";
#[cfg(not(target_os = "macos"))]
const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) \
    AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

impl WebSearchServer {
    fn new() -> anyhow::Result<Self> {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .user_agent(USER_AGENT)
            .redirect(reqwest::redirect::Policy::limited(10))
            .build()
            .context("Failed to build reqwest client")?;

        Ok(Self {
            client: Arc::new(client),
        })
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    /// Fetch a URL and return the response body as a String.
    async fn get_text(&self, url: &str) -> anyhow::Result<String> {
        let resp = self.client.get(url).send().await?.error_for_status()?;
        Ok(resp.text().await?)
    }

    /// Parse DuckDuckGo HTML results and return (title, url, snippet) triples.
    fn parse_ddg_results(html: &str) -> Vec<(String, String, String)> {
        let mut results = Vec::new();

        // Each result is wrapped in a <div class="result ..."> block.
        // We split on that boundary and then extract fields from each chunk.
        for chunk in html.split(r#"class="result results_links"#) {
            // Extract href from <a class="result__a" href="...">
            let url: String = extract_between(chunk, r#"class="result__a" href=""#, "\"")
                .unwrap_or_default()
                .to_string();

            if url.is_empty() {
                continue;
            }

            // The link text is the title
            let title = {
                let after_href = chunk.find(r#"class="result__a""#).map(|p| &chunk[p..]);
                after_href
                    .and_then(|s| extract_between(s, ">", "</a>"))
                    .map(|t| strip_tags(t).trim().to_string())
                    .unwrap_or_default()
            };

            // Snippet
            let snippet = extract_between(chunk, r#"class="result__snippet""#, "</a>")
                .map(|s| {
                    // Drop everything up to the first `>`
                    let inner = s.find('>').map(|i| &s[i + 1..]).unwrap_or(s);
                    strip_tags(inner).trim().to_string()
                })
                .unwrap_or_default();

            // Resolve DuckDuckGo redirect URLs
            let resolved_url = if url.starts_with("//duckduckgo.com/l/?") {
                // Extract uddg= param
                extract_between(&url, "uddg=", "&")
                    .and_then(|enc| urlencoding_decode(enc))
                    .unwrap_or_else(|| url.to_string())
            } else {
                url.to_string()
            };

            if !resolved_url.is_empty() && resolved_url.starts_with("http") {
                results.push((title, resolved_url, snippet));
            }
        }

        results
    }

    /// Perform a DuckDuckGo HTML search and return (title, url, snippet) pairs.
    async fn ddg_search(&self, query: &str) -> anyhow::Result<Vec<(String, String, String)>> {
        // Percent-encode the query
        let encoded = percent_encode(query);
        let search_url = format!("https://html.duckduckgo.com/html/?q={encoded}");

        tracing::debug!("Fetching DDG search URL: {search_url}");

        let html = self.get_text(&search_url).await?;
        let results = Self::parse_ddg_results(&html);

        tracing::debug!("Parsed {} DDG results", results.len());
        Ok(results)
    }

    /// Strip HTML tags and collapse whitespace from a string.
    fn extract_text_from_html(html: &str) -> String {
        // Remove block-level noise elements with their content
        let noise_tags = ["script", "style", "nav", "footer", "header", "noscript"];
        let mut text = html.to_string();

        for tag in &noise_tags {
            // Remove <tag ...>...</tag> (non-greedy via manual loop)
            let open = format!("<{tag}");
            let close = format!("</{tag}>");
            while let Some(start) = text.to_lowercase().find(&open) {
                let search_from = start + open.len();
                if let Some(rel_end) = text[search_from..].to_lowercase().find(&close) {
                    let end = search_from + rel_end + close.len();
                    text.replace_range(start..end, " ");
                } else {
                    // No closing tag found; just remove the open tag itself
                    if let Some(close_bracket) = text[start..].find('>') {
                        text.replace_range(start..start + close_bracket + 1, " ");
                    } else {
                        break;
                    }
                }
            }
        }

        // Strip remaining HTML tags
        let tag_re = Regex::new(r"<[^>]+>").expect("valid regex");
        let stripped = tag_re.replace_all(&text, " ");

        // Decode common HTML entities
        let decoded = stripped
            .replace("&amp;", "&")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&#39;", "'")
            .replace("&nbsp;", " ")
            .replace("&mdash;", "—")
            .replace("&ndash;", "-")
            .replace("&hellip;", "...");

        // Collapse whitespace
        let ws_re = Regex::new(r"[ \t]+").expect("valid regex");
        let single_spaced = ws_re.replace_all(&decoded, " ");

        // Collapse multiple blank lines
        let nl_re = Regex::new(r"\n{3,}").expect("valid regex");
        let clean = nl_re.replace_all(&single_spaced, "\n\n");

        clean.trim().to_string()
    }

    /// Deduplicate results by domain, keeping the first hit per domain.
    fn deduplicate_by_domain(results: Vec<(String, String, String)>) -> Vec<(String, String, String)> {
        let mut seen_domains: HashSet<String> = HashSet::new();
        results
            .into_iter()
            .filter(|(_, url, _)| {
                let domain = extract_domain(url);
                seen_domains.insert(domain)
            })
            .collect()
    }

    /// Format a list of results as a numbered string.
    fn format_results(results: &[(String, String, String)]) -> String {
        results
            .iter()
            .enumerate()
            .map(|(i, (title, url, snippet))| {
                let n = i + 1;
                let snippet_part = if snippet.is_empty() {
                    String::new()
                } else {
                    format!("\n   {snippet}")
                };
                format!("{n}. {title}\n   {url}{snippet_part}")
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

// ---------------------------------------------------------------------------
// Tool implementations
// ---------------------------------------------------------------------------

#[tool_router(server_handler)]
impl WebSearchServer {
    #[tool(description = "Search the web using DuckDuckGo. Returns numbered results with title, URL, and snippet. Deduplicates by domain. Optionally fetches full text content from the top 3 results.")]
    async fn web_search(
        &self,
        Parameters(WebSearchParams {
            query,
            limit,
            fetch_content,
        }): Parameters<WebSearchParams>,
    ) -> String {
        tracing::info!("web_search: query={query:?} limit={limit:?} fetch_content={fetch_content}");

        let raw_results = match self.ddg_search(&query).await {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("DDG search failed: {e:#}");
                return format!("Error: search failed: {e}");
            }
        };

        let deduped = Self::deduplicate_by_domain(raw_results);
        let max_results = limit.unwrap_or(10).max(1).min(50) as usize;
        let results: Vec<_> = deduped.into_iter().take(max_results).collect();

        if results.is_empty() {
            return "No results found.".to_string();
        }

        let mut output = format!(
            "Search results for: {query}\n\n{}\n",
            Self::format_results(&results)
        );

        if fetch_content {
            let top = results.iter().take(3);
            for (i, (title, url, _)) in top.enumerate() {
                output.push_str(&format!("\n\n--- Content from result {} ({}) ---\n", i + 1, title));
                match self.get_text(url).await {
                    Ok(html) => {
                        let text = Self::extract_text_from_html(&html);
                        let truncated = truncate_str(&text, 3000);
                        output.push_str(truncated);
                    }
                    Err(e) => {
                        tracing::warn!("Failed to fetch {url}: {e}");
                        output.push_str(&format!("(Failed to fetch content: {e})"));
                    }
                }
            }
        }

        output
    }

    #[tool(description = "Fetch a web page by URL. If extract_text=true, strips HTML tags and returns clean plain text. For non-HTML responses (JSON, etc.) returns content as-is. Truncated at 10,000 characters.")]
    async fn fetch_page(
        &self,
        Parameters(FetchPageParams { url, extract_text }): Parameters<FetchPageParams>,
    ) -> String {
        tracing::info!("fetch_page: url={url:?} extract_text={extract_text}");

        let resp = match self.client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("fetch_page request failed for {url}: {e:#}");
                return format!("Error: request failed: {e}");
            }
        };

        let status = resp.status();
        if !status.is_success() {
            return format!("Error: HTTP {status} for {url}");
        }

        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();

        let body = match resp.text().await {
            Ok(b) => b,
            Err(e) => return format!("Error: failed to read response body: {e}"),
        };

        let is_html = content_type.contains("html");

        let content = if extract_text && is_html {
            Self::extract_text_from_html(&body)
        } else {
            body
        };

        let result = truncate_str(&content, 10_000);

        if result.len() < content.len() {
            format!("{result}\n\n[Content truncated at 10,000 characters]")
        } else {
            result.to_string()
        }
    }

    #[tool(description = "Fast web search returning top 5 results as plain text. No content fetching. Good for quick lookups.")]
    async fn web_search_quick(
        &self,
        Parameters(WebSearchQuickParams { query }): Parameters<WebSearchQuickParams>,
    ) -> String {
        tracing::info!("web_search_quick: query={query:?}");

        let raw_results = match self.ddg_search(&query).await {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("DDG search failed: {e:#}");
                return format!("Error: search failed: {e}");
            }
        };

        let deduped = Self::deduplicate_by_domain(raw_results);
        let results: Vec<_> = deduped.into_iter().take(5).collect();

        if results.is_empty() {
            return "No results found.".to_string();
        }

        format!(
            "Top results for: {query}\n\n{}",
            Self::format_results(&results)
        )
    }
}

// ---------------------------------------------------------------------------
// String utilities
// ---------------------------------------------------------------------------

/// Find text between `start` delimiter and `end` delimiter.
fn extract_between<'a>(s: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let begin = s.find(start)? + start.len();
    let finish = s[begin..].find(end)?;
    Some(&s[begin..begin + finish])
}

/// Minimal percent-encoding for query strings.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            b' ' => out.push('+'),
            _ => {
                use std::fmt::Write;
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

/// Minimal percent-decode for DuckDuckGo redirect URLs.
fn urlencoding_decode(s: &str) -> Option<String> {
    let mut out = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
            let byte = u8::from_str_radix(hex, 16).ok()?;
            out.push(byte);
            i += 3;
        } else if bytes[i] == b'+' {
            out.push(b' ');
            i += 1;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Strip HTML tags from a string slice (does not modify a String).
fn strip_tags(s: &str) -> String {
    let tag_re = Regex::new(r"<[^>]+>").expect("valid regex");
    tag_re.replace_all(s, " ").into_owned()
}

/// Extract the hostname/domain from a URL.
fn extract_domain(url: &str) -> String {
    // Strip scheme
    let without_scheme = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);

    // Take up to first '/'
    without_scheme
        .split('/')
        .next()
        .unwrap_or(without_scheme)
        .to_lowercase()
}

/// Truncate a string at `max_chars` character boundary.
fn truncate_str(s: &str, max_chars: usize) -> &str {
    if s.chars().count() <= max_chars {
        return s;
    }
    // Find byte position at char boundary
    let mut char_count = 0;
    for (byte_idx, _) in s.char_indices() {
        if char_count == max_chars {
            return &s[..byte_idx];
        }
        char_count += 1;
    }
    s
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

    tracing::info!("Starting mcp-ws (web search & fetch) server");

    let server = WebSearchServer::new()?;
    let service = server.serve(stdio()).await?;
    service.waiting().await?;

    Ok(())
}
