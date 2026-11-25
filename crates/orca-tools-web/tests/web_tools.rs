//! Ported from `WebToolTests` (tests/OpenOrca.Tools.Tests), plus the HTML stripping and result
//! parsing. The C# tests fetch example.com and query DuckDuckGo; here a local server plays both
//! (no internet in unit tests).

use std::time::Duration;

use orca_abstractions::{Tool, ToolContext, ToolResult};
use orca_testkit::{Page, StaticServer};
use orca_tools_common::NEWLINE;
use orca_tools_web::*;
use serde_json::{Value, json};

const EXAMPLE_PAGE: &str = "<!doctype html>\n<html>\n<head>\n    <title>Example Domain</title>\n    <style type=\"text/css\">\n    body { background-color: #f0f0f2; }\n    </style>\n</head>\n\n<body>\n<div>\n    <h1>Example Domain</h1>\n    <p>This domain is for use in illustrative examples in documents. You may use this\n    domain in literature without prior coordination or asking for permission.</p>\n    <p><a href=\"https://www.iana.org/domains/example\">More information...</a></p>\n</div>\n</body>\n</html>\n";

async fn example_server() -> StaticServer {
    // All test servers are on 127.0.0.1: without this the per-host limiter spaces them out.
    orca_tools_common::http::set_min_delay(Duration::ZERO);
    StaticServer::start([
        ("/".to_string(), Page::html(EXAMPLE_PAGE)),
        (
            "/plain".to_string(),
            Page::new(200, "text/plain", "a <b> &amp; c"),
        ),
        (
            "/teapot".to_string(),
            Page::new(418, "text/plain", "short and stout"),
        ),
        (
            "/latin1".to_string(),
            Page::new(200, "text/plain; charset=iso-8859-1", b"caf\xe9".to_vec()),
        ),
    ])
    .await
}

async fn fetch(args: Value) -> ToolResult {
    WebFetchTool.execute(&args, &ToolContext::new(".")).await
}

// ── WebFetchTool ──

#[tokio::test]
async fn web_fetch_tool_rejects_invalid_url() {
    let result = fetch(json!({"url": "not-a-url"})).await;
    assert!(result.is_error);
    assert!(result.content.contains("Invalid URL"));
}

#[tokio::test]
async fn web_fetch_tool_rejects_ftp_scheme() {
    let result = fetch(json!({"url": "ftp://example.com/file.txt"})).await;
    assert!(result.is_error);
    assert!(result.content.contains("Invalid URL"));
}

#[tokio::test]
async fn web_fetch_tool_fetches_example_dot_com() {
    let server = example_server().await;
    let url = format!("{}/", server.base_url());
    let result = fetch(json!({"url": url})).await;
    assert!(!result.is_error, "{}", result.content);
    assert!(result.content.contains("Example Domain"));
    assert!(result.content.starts_with(&format!("URL: {url}\nLength: ")));
    assert!(!result.content.contains("background-color"));
    assert!(!result.content.contains("<h1>"));
}

#[tokio::test]
async fn web_fetch_tool_respects_max_length() {
    let server = example_server().await;
    let result = fetch(json!({"url": server.base_url(), "max_length": 100})).await;
    assert!(!result.is_error);
    assert!(result.content.contains("truncated"));
    let body = result.content.split_once("\n\n").unwrap().1;
    let (kept, note) = body.rsplit_once("\n\n... (").unwrap();
    assert_eq!(kept.chars().count(), 100);
    assert!(note.ends_with(" chars truncated)"));
    assert!(
        result
            .content
            .contains(&format!("Length: {} chars", body.chars().count()))
    );
}

#[tokio::test]
async fn web_fetch_tool_raw_keeps_markup() {
    let server = example_server().await;
    let url = format!("{}/plain", server.base_url());
    let result = fetch(json!({"url": url, "raw": true})).await;
    assert_eq!(
        result.content,
        format!("URL: {url}\nLength: 13 chars\n\na <b> &amp; c")
    );
    let stripped = fetch(json!({"url": url})).await;
    assert_eq!(
        stripped.content,
        format!("URL: {url}\nLength: 5 chars\n\na & c")
    );
}

#[tokio::test]
async fn web_fetch_tool_reports_http_status() {
    let server = example_server().await;
    let result = fetch(json!({"url": format!("{}/teapot", server.base_url())})).await;
    assert!(result.is_error);
    assert_eq!(result.content, "HTTP 418 I'm a teapot");
    let missing = fetch(json!({"url": format!("{}/missing", server.base_url())})).await;
    assert_eq!(missing.content, "HTTP 404 Not Found");
}

#[tokio::test]
async fn web_fetch_tool_decodes_declared_charset() {
    let server = example_server().await;
    let result = fetch(json!({"url": format!("{}/latin1", server.base_url())})).await;
    assert!(result.content.ends_with("caf\u{e9}"), "{}", result.content);
}

#[tokio::test]
async fn web_fetch_tool_negative_max_length_fails_like_csharp() {
    let server = example_server().await;
    let url = format!("{}/plain", server.base_url());
    let result = fetch(json!({"url": url, "max_length": -1})).await;
    assert!(result.is_error);
    assert_eq!(
        result.content,
        format!("Error fetching {url}: Length cannot be less than zero. (Parameter 'length')")
    );
}

#[tokio::test]
async fn web_fetch_tool_connection_refused_names_host_and_port() {
    // Bind and drop a listener to get a port nothing listens on.
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let url = format!("http://127.0.0.1:{port}/");
    let result = fetch(json!({"url": url})).await;
    assert!(result.is_error);
    assert!(
        result
            .content
            .starts_with(&format!("HTTP error fetching {url}: ")),
        "{}",
        result.content
    );
    assert!(
        result.content.ends_with(&format!("(127.0.0.1:{port})")),
        "{}",
        result.content
    );
}

// ── StripHtml ──

#[test]
fn strip_html_removes_scripts_styles_and_tags() {
    let html = "<html><head><SCRIPT type=x>var a = '<b>';</script><style>p{}</STYLE></head>\
                <body><p>Hello&nbsp;&amp;\t\t world</p>\n\n\n\n<div>next</div></body></html>";
    assert_eq!(strip_html(html), "Hello\u{a0}& world \n\n next");
}

#[test]
fn strip_html_keeps_an_unclosed_script_block() {
    assert_eq!(strip_html("a<script>b"), "a b");
    assert_eq!(strip_html("<script>x</style>y"), "x y");
    assert_eq!(strip_html("<scripts>x</scripts>y</script>z"), "z");
}

// ── WebSearchTool ──

const DDG_PAGE: &str = r#"<html><body>
<div class="result">
  <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fplatform.openai.com%2Fdocs&amp;rut=abc">OpenAI <b>API</b> documentation</a>
  <a class="result__snippet" href="x">The <b>OpenAI</b> API &amp; docs.</a>
</div>
<div class="result">
  <a rel="nofollow" class="result__a" href="https://example.com/two">Second</a>
  <a class="result__snippet" href="y">   </a>
</div>
<div class="result">
  <a rel="nofollow" class="result__a" href="https://example.com/three">Third</a>
</div>
</body></html>"#;

#[test]
fn parse_results_unwraps_redirects_and_pairs_snippets() {
    let results = parse_results(DDG_PAGE, 5);
    assert_eq!(results.len(), 3);
    assert_eq!(results[0].title, "OpenAI API documentation");
    assert_eq!(results[0].url, "https://platform.openai.com/docs");
    // The lazy snippet pattern stops at the first closing tag (`</b>`), as in C#.
    assert_eq!(results[0].snippet, "The OpenAI");
    assert_eq!(results[1].url, "https://example.com/two");
    assert_eq!(results[1].snippet, "");
    assert_eq!(results[2].snippet, "");
    assert_eq!(parse_results(DDG_PAGE, 1).len(), 1);
}

#[test]
fn parse_results_href_before_class_and_single_quotes() {
    let html = "<a href='https://a.example/' class='result__a'>A &lt;1&gt;</a>";
    let results = parse_results(html, 5);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].title, "A <1>");
    assert_eq!(results[0].url, "https://a.example/");
}

#[test]
fn format_results_layout() {
    let result = format_results("q", DDG_PAGE, 2);
    let nl = NEWLINE;
    assert_eq!(
        result.content,
        format!(
            "Search results for: q{nl}2 results:{nl}{nl}1. OpenAI API documentation{nl}   https://platform.openai.com/docs{nl}   The OpenAI{nl}{nl}2. Second{nl}   https://example.com/two{nl}{nl}"
        )
    );
}

#[test]
fn format_results_no_results_and_parse_failure() {
    assert_eq!(
        format_results("zzz", "<html></html>", 5).content,
        "No results found for: zzz"
    );
    let broken = format_results("zzz", "<span class=\"result__a\">x</span>", 5);
    assert!(broken.is_error);
    assert!(broken.content.contains("failed to parse"));
}

#[tokio::test]
async fn web_search_tool_searches_successfully() {
    orca_tools_common::http::set_min_delay(Duration::ZERO);
    let server = StaticServer::start([("/html/".to_string(), Page::html(DDG_PAGE))]).await;
    let tool = WebSearchTool {
        endpoint: format!("{}/html/", server.base_url()),
    };
    let result = tool
        .execute(
            &json!({"query": "OpenAI API documentation", "max_results": 3}),
            &ToolContext::new("."),
        )
        .await;
    assert!(!result.is_error, "{}", result.content);
    assert!(result.content.contains("3 results:"));
    assert_eq!(server.requests(), ["/html/?q=OpenAI%20API%20documentation"]);
}
