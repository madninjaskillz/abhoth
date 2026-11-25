//! A tiny MCP server over stdio (newline-delimited JSON-RPC) for tests and the cross-check.
//!
//! Tools: `echo` (read-only), `add`, `fail` (an `isError` result), `image` (text + a PNG
//! block), `mixed` (text + an embedded resource), `env` (the `FAKE_MCP_VAR` variable) and
//! `slow` (`{"ms": n}`). Arguments: `--exit` quits before the handshake (a server that fails
//! to start); `--log <file>` appends every received message to a file.

use std::io::{BufRead, Write};

use serde_json::{Value, json};

fn tools() -> Value {
    json!([
        {"name": "echo", "description": "Echoes the text back",
         "inputSchema": {"type": "object", "properties": {"text": {"type": "string", "description": "Text to echo"}}, "required": ["text"]},
         "annotations": {"readOnlyHint": true}},
        {"name": "add", "description": "Adds two numbers",
         "inputSchema": {"type": "object", "properties": {"a": {"type": "number"}, "b": {"type": "number"}}, "required": ["a", "b"]}},
        {"name": "fail", "description": "Always fails",
         "inputSchema": {"type": "object", "properties": {}}},
        {"name": "image", "description": "Returns a tiny image",
         "inputSchema": {"type": "object", "properties": {}}},
        {"name": "mixed", "description": "Returns text and an embedded resource",
         "inputSchema": {"type": "object", "properties": {}}},
        {"name": "env", "description": "Shows FAKE_MCP_VAR",
         "inputSchema": {"type": "object", "properties": {}}},
        {"name": "slow", "description": "Sleeps for ms milliseconds",
         "inputSchema": {"type": "object", "properties": {"ms": {"type": "integer"}}}}
    ])
}

fn text(t: impl Into<String>) -> Value {
    json!({"type": "text", "text": t.into()})
}

fn call(name: &str, args: &Value) -> Result<Value, (i64, String)> {
    let content = match name {
        "echo" => vec![text(format!(
            "echo: {}",
            args["text"].as_str().unwrap_or_default()
        ))],
        "add" => {
            let sum = args["a"].as_f64().unwrap_or(0.0) + args["b"].as_f64().unwrap_or(0.0);
            vec![text(format!("{sum}"))]
        }
        "fail" => {
            return Ok(json!({"content": [text("the fake tool failed")], "isError": true}));
        }
        // "iVBORw==" is the base64 of the first four PNG signature bytes.
        "image" => vec![
            text("an image:"),
            json!({"type": "image", "data": "iVBORw==", "mimeType": "image/png"}),
        ],
        "mixed" => vec![
            text("see the resource"),
            json!({"type": "resource", "resource": {"uri": "file:///x.txt", "mimeType": "text/plain", "text": "x"}}),
        ],
        "env" => vec![text(format!(
            "FAKE_MCP_VAR={}",
            std::env::var("FAKE_MCP_VAR").unwrap_or_default()
        ))],
        "slow" => {
            let ms = args["ms"].as_u64().unwrap_or(1000);
            std::thread::sleep(std::time::Duration::from_millis(ms));
            vec![text("done")]
        }
        other => return Err((-32602, format!("Unknown tool: {other}"))),
    };
    Ok(json!({"content": content}))
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--exit") {
        std::process::exit(3);
    }
    let log = args
        .iter()
        .position(|a| a == "--log")
        .and_then(|i| args.get(i + 1))
        .cloned();
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        if let Some(path) = &log
            && let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
        {
            let _ = writeln!(f, "{line}");
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(id) = msg.get("id").cloned() else {
            continue; // a notification
        };
        let Some(method) = msg["method"].as_str() else {
            continue; // a response to something we never send
        };
        let params = &msg["params"];
        let result = match method {
            "initialize" => Ok(json!({
                "protocolVersion": params["protocolVersion"].as_str().unwrap_or("2025-06-18"),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "fake-mcp", "version": "1.0.0"}
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": tools()})),
            "tools/call" => call(
                params["name"].as_str().unwrap_or_default(),
                &params["arguments"],
            ),
            other => Err((-32601, format!("Method not found: {other}"))),
        };
        let reply = match result {
            Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
            Err((code, message)) => {
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
            }
        };
        if writeln!(stdout, "{reply}").is_err() || stdout.flush().is_err() {
            break;
        }
    }
}
