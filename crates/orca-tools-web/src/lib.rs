//! Web tools (port of `OpenOrca.Tools.Web`): `web_fetch` and `web_search` (DuckDuckGo's HTML
//! page, no API key).

use std::sync::{Arc, OnceLock};

use orca_abstractions::{Tool, ToolContext, ToolRenderHints, ToolResult, ToolRiskLevel};
use orca_tools_common::html::{escape_data_string, html_decode, unescape_data_string};
use orca_tools_common::{append_line, get_bool_lenient, get_i32_lenient, http, utf16_len};
use regex::Regex;
use serde_json::Value;

/// Every tool in this crate.
pub fn all_tools() -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(WebFetchTool), Arc::new(WebSearchTool::default())]
}

fn schema(text: &str) -> Value {
    serde_json::from_str(text).expect("tool schema is valid JSON")
}

fn regex(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("valid regex"))
}

pub struct WebFetchTool;

#[async_trait::async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn description(&self) -> &str {
        "Fetch the content of a URL and return it as text. HTML is stripped to plain text by default. Set raw=true to get the original response body. Useful for reading documentation, APIs, and web pages."
    }

    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::ReadOnly
    }

    fn render_hints(&self) -> Option<ToolRenderHints> {
        Some(ToolRenderHints {
            emoji: "\u{1f310}",
            short_description: "Fetch web page",
            summary_arg_keys: &["url"],
            aliases: &["fetch"],
        })
    }

    fn parameter_schema(&self) -> Value {
        schema(
            r#"{
        "type": "object",
        "properties": {
            "url": {
                "type": "string",
                "description": "The URL to fetch"
            },
            "max_length": {
                "type": "integer",
                "description": "Maximum characters to return. Defaults to 20000."
            },
            "raw": {
                "type": "boolean",
                "description": "Return the raw response body without HTML stripping. Defaults to false."
            }
        },
        "required": ["url"]
    }"#,
        )
    }

    async fn execute(&self, args: &Value, _ctx: &ToolContext) -> ToolResult {
        let url = args.get("url").and_then(Value::as_str).unwrap_or("");
        let max_length = get_i32_lenient(args.get("max_length"), 20000);
        let raw = get_bool_lenient(args.get("raw"), false);

        let parsed = match reqwest::Url::parse(url) {
            Ok(u) if u.scheme() == "http" || u.scheme() == "https" => u,
            _ => return ToolResult::error("Invalid URL. Must be an absolute http or https URL."),
        };

        http::throttle(url).await;
        let response = match http::client().get(parsed.clone()).send().await {
            Ok(r) => r,
            Err(e) if e.is_timeout() => {
                return ToolResult::error(format!("Request timed out fetching {url}"));
            }
            Err(e) => {
                return ToolResult::error(format!(
                    "HTTP error fetching {url}: {}",
                    request_error_message(&e, &parsed)
                ));
            }
        };

        let status = response.status();
        if !status.is_success() {
            return ToolResult::error(format!(
                "HTTP {} {}",
                status.as_u16(),
                reason_phrase(&response)
            ));
        }

        let mut body = match response.text().await {
            Ok(b) => b,
            Err(e) if e.is_timeout() => {
                return ToolResult::error(format!("Request timed out fetching {url}"));
            }
            Err(e) => {
                return ToolResult::error(format!(
                    "HTTP error fetching {url}: {}",
                    request_error_message(&e, &parsed)
                ));
            }
        };

        if !raw {
            body = strip_html(&body);
        }

        let length = utf16_len(&body);
        if length as i64 > max_length as i64 {
            if max_length < 0 {
                // body[..maxLength] throws; the generic catch reports it.
                return ToolResult::error(format!(
                    "Error fetching {url}: Length cannot be less than zero. (Parameter 'length')"
                ));
            }
            let max = max_length as usize;
            body = format!(
                "{}\n\n... ({} chars truncated)",
                utf16_prefix(&body, max),
                length - max
            );
        }

        ToolResult::success(format!(
            "URL: {url}\nLength: {} chars\n\n{body}",
            utf16_len(&body)
        ))
    }
}

/// The first `max` UTF-16 units of `s` (a surrogate pair cut in half is dropped; C# would keep
/// its lone high surrogate).
fn utf16_prefix(s: &str, max: usize) -> &str {
    let mut units = 0;
    for (i, c) in s.char_indices() {
        units += c.len_utf16();
        if units > max {
            return &s[..i];
        }
    }
    s
}

/// `HttpResponseMessage.ReasonPhrase`: the server's phrase when hyper kept it (it does only
/// for non-standard phrases), else the standard one.
fn reason_phrase(response: &reqwest::Response) -> String {
    if let Some(r) = response.extensions().get::<hyper::ext::ReasonPhrase>() {
        return String::from_utf8_lossy(r.as_bytes()).into_owned();
    }
    response
        .status()
        .canonical_reason()
        .unwrap_or_default()
        .to_string()
}

/// The text of the `HttpRequestException` .NET throws for a failed request: for socket errors
/// the OS message plus `(host:port)`, as `SocketsHttpHandler` words it; otherwise the reqwest
/// error chain.
pub fn request_error_message(err: &reqwest::Error, url: &reqwest::Url) -> String {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = source {
        if let Some(io) = e.downcast_ref::<std::io::Error>()
            && io.raw_os_error().is_some()
        {
            let text = io.to_string();
            let text = match text.rfind(" (os error ") {
                Some(i) => text[..i].to_string(),
                None => text,
            };
            let host = url.host_str().unwrap_or_default();
            let port = url.port_or_known_default().unwrap_or(0);
            return format!("{text} ({host}:{port})");
        }
        source = e.source();
    }
    let mut text = err.to_string();
    let mut source = std::error::Error::source(err);
    while let Some(e) = source {
        text.push_str(": ");
        text.push_str(&e.to_string());
        source = e.source();
    }
    text
}

/// `WebFetchTool.StripHtml`: drops `<script>`/`<style>` blocks and tags (a tag becomes a
/// space), decodes entities, collapses runs of spaces/tabs and of 3+ newlines, trims.
pub fn strip_html(html: &str) -> String {
    static TAG: OnceLock<Regex> = OnceLock::new();
    static SPACES: OnceLock<Regex> = OnceLock::new();
    static NEWLINES: OnceLock<Regex> = OnceLock::new();
    let text = remove_script_and_style(html);
    let text = regex(&TAG, "<[^>]+>").replace_all(&text, " ");
    let text = html_decode(&text);
    let text = regex(&SPACES, "[ \t]+").replace_all(&text, " ");
    let text = regex(&NEWLINES, "\n{3,}").replace_all(&text, "\n\n");
    text.trim().to_string()
}

/// `Regex.Replace(html, @"<(script|style)[^>]*>[\s\S]*?</\1>", "", IgnoreCase)`, without the
/// backreference: find an opening tag, then the first matching closing tag; an opening tag
/// with no closing tag stays and the scan goes on after its `<`.
fn remove_script_and_style(html: &str) -> String {
    static OPEN: OnceLock<Regex> = OnceLock::new();
    static CLOSE_SCRIPT: OnceLock<Regex> = OnceLock::new();
    static CLOSE_STYLE: OnceLock<Regex> = OnceLock::new();
    let open = regex(&OPEN, r"(?i)<(script|style)[^>]*>");
    let mut out = String::with_capacity(html.len());
    let mut copied = 0;
    let mut pos = 0;
    while let Some(caps) = open.captures_at(html, pos) {
        let whole = caps.get(0).unwrap();
        let close = if caps[1].eq_ignore_ascii_case("script") {
            regex(&CLOSE_SCRIPT, r"(?i)</script>")
        } else {
            regex(&CLOSE_STYLE, r"(?i)</style>")
        };
        match close.find_at(html, whole.end()) {
            Some(end) => {
                out.push_str(&html[copied..whole.start()]);
                copied = end.end();
                pos = end.end();
            }
            None => pos = whole.start() + 1,
        }
    }
    out.push_str(&html[copied..]);
    out
}

pub struct WebSearchTool {
    /// The DuckDuckGo HTML endpoint; tests point it at a local server.
    pub endpoint: String,
}

impl Default for WebSearchTool {
    fn default() -> Self {
        Self {
            endpoint: "https://html.duckduckgo.com/html/".to_string(),
        }
    }
}

#[async_trait::async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }

    fn description(&self) -> &str {
        "Search the web using DuckDuckGo and return result titles, URLs, and snippets. No API key required. Use for finding documentation, answers, package info, and current information."
    }

    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::ReadOnly
    }

    fn render_hints(&self) -> Option<ToolRenderHints> {
        Some(ToolRenderHints {
            emoji: "\u{1f50e}",
            short_description: "Search the web",
            summary_arg_keys: &["query"],
            aliases: &["search"],
        })
    }

    fn parameter_schema(&self) -> Value {
        schema(
            r#"{
        "type": "object",
        "properties": {
            "query": {
                "type": "string",
                "description": "The search query"
            },
            "max_results": {
                "type": "integer",
                "description": "Maximum number of results to return. Defaults to 5."
            }
        },
        "required": ["query"]
    }"#,
        )
    }

    async fn execute(&self, args: &Value, _ctx: &ToolContext) -> ToolResult {
        let query = args.get("query").and_then(Value::as_str).unwrap_or("");
        let max_results = get_i32_lenient(args.get("max_results"), 5);
        let url = format!("{}?q={}", self.endpoint, escape_data_string(query));

        http::throttle(&url).await;
        let parsed = match reqwest::Url::parse(&url) {
            Ok(u) => u,
            Err(e) => return ToolResult::error(format!("Search error: {e}")),
        };
        let response = match http::client().get(parsed.clone()).send().await {
            Ok(r) => r,
            Err(e) if e.is_timeout() => return ToolResult::error("Search request timed out"),
            Err(e) => {
                return ToolResult::error(format!(
                    "Search error: {}",
                    request_error_message(&e, &parsed)
                ));
            }
        };
        if !response.status().is_success() {
            return ToolResult::error(format!(
                "Search failed: HTTP {}",
                response.status().as_u16()
            ));
        }
        let html = match response.text().await {
            Ok(t) => t,
            Err(e) if e.is_timeout() => return ToolResult::error("Search request timed out"),
            Err(e) => {
                return ToolResult::error(format!(
                    "Search error: {}",
                    request_error_message(&e, &parsed)
                ));
            }
        };
        format_results(query, &html, max_results)
    }
}

/// The tool's answer for a results page (everything after the HTTP request).
pub fn format_results(query: &str, html: &str, max_results: i32) -> ToolResult {
    let results = parse_results(html, max_results);
    if results.is_empty() {
        // Distinguish "no results" from a page the patterns no longer understand.
        return if html.contains("result__a") || html.contains("result__snippet") {
            ToolResult::error(
                "Search returned results but failed to parse them. DuckDuckGo HTML structure may have changed.",
            )
        } else {
            ToolResult::success(format!("No results found for: {query}"))
        };
    }
    let mut sb = String::new();
    append_line(&mut sb, &format!("Search results for: {query}"));
    append_line(&mut sb, &format!("{} results:", results.len()));
    append_line(&mut sb, "");
    for (i, r) in results.iter().enumerate() {
        append_line(&mut sb, &format!("{}. {}", i + 1, r.title));
        append_line(&mut sb, &format!("   {}", r.url));
        if !r.snippet.trim().is_empty() {
            append_line(&mut sb, &format!("   {}", r.snippet));
        }
        append_line(&mut sb, "");
    }
    ToolResult::success(sb)
}

/// One search hit.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// `WebSearchTool.ParseResults`: the first title/link pattern that matches gives the results,
/// the first snippet pattern that matches gives the snippets (paired by index); DuckDuckGo's
/// redirect links are unwrapped (`uddg=`).
pub fn parse_results(html: &str, max_results: i32) -> Vec<SearchResult> {
    static TITLE_LINK: OnceLock<[Regex; 4]> = OnceLock::new();
    static SNIPPET: OnceLock<[Regex; 2]> = OnceLock::new();
    static UDDG: OnceLock<Regex> = OnceLock::new();
    let title_link = TITLE_LINK.get_or_init(|| {
        [
            r#"(?s)<a\s+[^>]*?class="result__a"[^>]*?href="([^"]+)"[^>]*?>(.*?)</a>"#,
            r#"(?s)<a\s+[^>]*?href="([^"]+)"[^>]*?class="result__a"[^>]*?>(.*?)</a>"#,
            r#"(?s)<a\s+[^>]*?class='result__a'[^>]*?href='([^']+)'[^>]*?>(.*?)</a>"#,
            r#"(?s)<a\s+[^>]*?href='([^']+)'[^>]*?class='result__a'[^>]*?>(.*?)</a>"#,
        ]
        .map(|p| Regex::new(p).expect("valid regex"))
    });
    let snippet = SNIPPET.get_or_init(|| {
        [
            r#"(?s)class="result__snippet"[^>]*>(.*?)</\w+>"#,
            r#"(?s)class='result__snippet'[^>]*>(.*?)</\w+>"#,
        ]
        .map(|p| Regex::new(p).expect("valid regex"))
    });
    let uddg = regex(&UDDG, "uddg=([^&]+)");

    let Some(blocks) = title_link
        .iter()
        .map(|r| r.captures_iter(html).collect::<Vec<_>>())
        .find(|m| !m.is_empty())
    else {
        return Vec::new();
    };
    let snippets: Vec<_> = snippet
        .iter()
        .map(|r| r.captures_iter(html).collect::<Vec<_>>())
        .find(|m| !m.is_empty())
        .unwrap_or_default();

    let take = blocks.len().min(max_results.max(0) as usize);
    blocks[..take]
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let raw_url = &m[1];
            let title = strip_tags(&m[2]);
            let url = match uddg.captures(raw_url) {
                Some(c) => unescape_data_string(&c[1]),
                None => raw_url.to_string(),
            };
            let snippet = snippets
                .get(i)
                .map(|s| strip_tags(&s[1]))
                .unwrap_or_default();
            SearchResult {
                title: title.trim().to_string(),
                url: url.trim().to_string(),
                snippet: snippet.trim().to_string(),
            }
        })
        .collect()
}

/// `WebSearchTool.StripTags`: tags removed, entities decoded, trimmed.
fn strip_tags(html: &str) -> String {
    static TAG: OnceLock<Regex> = OnceLock::new();
    let text = regex(&TAG, "<[^>]+>").replace_all(html, "");
    html_decode(&text).trim().to_string()
}
