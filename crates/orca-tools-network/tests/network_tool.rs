//! network_diagnostics against local endpoints only (C# has no tests for it).

use orca_abstractions::{Tool, ToolContext, ToolResult};
use orca_tools_network::NetworkDiagnosticsTool;
use serde_json::{Value, json};

async fn run(args: Value) -> ToolResult {
    NetworkDiagnosticsTool
        .execute(&args, &ToolContext::new(std::env::temp_dir()))
        .await
}

#[tokio::test]
async fn check_connection_reports_status_and_headers() {
    let app = axum::Router::new().route(
        "/",
        axum::routing::get(|| async {
            ([(axum::http::header::SERVER, "TestServer/1.0")], "hello")
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let target = format!("http://{addr}/");
    let r = run(json!({"action": "check_connection", "target": target})).await;
    assert!(!r.is_error, "{}", r.content);
    assert_eq!(
        r.content,
        format!(
            "Connection check for {target}:\n  Status: 200 OK\n  Server: TestServer/1.0\n  Content-Type: text/plain; charset=utf-8"
        )
    );
}

#[tokio::test]
async fn dns_lookup_localhost_and_errors() {
    let r = run(json!({"action": "dns_lookup", "target": "http://localhost:1234/x"})).await;
    assert!(!r.is_error, "{}", r.content);
    assert!(r.content.starts_with("DNS lookup for localhost:"));
    assert!(
        r.content.contains("127.0.0.1 (InterNetwork)")
            || r.content.contains("::1 (InterNetworkV6)"),
        "{}",
        r.content
    );
    let r = run(json!({"action": "frob", "target": "x"})).await;
    assert_eq!(
        r.content,
        "Unknown action: frob. Use 'ping', 'dns_lookup', or 'check_connection'."
    );
}
