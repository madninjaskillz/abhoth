//! A tiny language server over stdio (`Content-Length` framed JSON-RPC) for tests and the
//! cross-check. Its answers are derived from the request, so they are deterministic:
//!
//! - `didOpen`: publishes an Error (code "E1", source "fake") for every line containing `bad`
//!   and a Warning (code 7) for every line containing `todo`.
//! - hover: `hover at L:C` (markdown); line 50 fails with an error, lines >= 100 give null.
//! - definition: line 1 col 3 of the same file (`[]` for lines >= 100).
//! - references: lines 6 and 2 of the same file and line 3 of `other.rs` next to it.
//! - documentSymbol: a struct `Foo` with a method `bar`, then a flat `helper` function.
//! - workspace/symbol, prepareCallHierarchy, incoming/outgoing calls: one item each.
//!
//! `--log <file>` appends every received message to a file.

use std::io::{BufRead, Write};

use serde_json::{Value, json};

fn read_message(r: &mut impl BufRead) -> Option<Value> {
    let mut len = None;
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(v) = line
            .split_once(':')
            .filter(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        {
            len = v.1.trim().parse::<usize>().ok();
        }
    }
    let mut body = vec![0u8; len?];
    r.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}

fn write_message(w: &mut impl Write, v: &Value) -> std::io::Result<()> {
    let body = v.to_string();
    write!(w, "Content-Length: {}\r\n\r\n{body}", body.len())?;
    w.flush()
}

fn range(line: i64, ch: i64) -> Value {
    json!({"start": {"line": line, "character": ch}, "end": {"line": line, "character": ch + 3}})
}

fn sibling(uri: &str, name: &str) -> String {
    match uri.rfind('/') {
        Some(i) => format!("{}{name}", &uri[..=i]),
        None => name.to_string(),
    }
}

fn diagnostics(text: &str) -> Vec<Value> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let i = i as i64;
        if line.contains("bad") {
            out.push(json!({"range": range(i, 4), "severity": 1, "code": "E1", "source": "fake", "message": "bad thing here"}));
        }
        if line.contains("todo") {
            out.push(json!({"range": range(i, 0), "severity": 2, "code": 7, "message": "unfinished work"}));
        }
    }
    out
}

fn item(name: &str, kind: i64, uri: &str, line: i64) -> Value {
    json!({"name": name, "kind": kind, "uri": uri, "range": range(line, 0), "selectionRange": range(line, 3)})
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let log = args
        .iter()
        .position(|a| a == "--log")
        .and_then(|i| args.get(i + 1))
        .cloned();
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    while let Some(msg) = read_message(&mut input) {
        if let Some(path) = &log
            && let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
        {
            let _ = writeln!(f, "{msg}");
        }
        let method = msg["method"].as_str().unwrap_or_default().to_string();
        let params = &msg["params"];
        let uri = params["textDocument"]["uri"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let line = params["position"]["line"].as_i64().unwrap_or(0);
        let ch = params["position"]["character"].as_i64().unwrap_or(0);
        let Some(id) = msg.get("id").cloned() else {
            // Notifications.
            match method.as_str() {
                "textDocument/didOpen" => {
                    let text = params["textDocument"]["text"].as_str().unwrap_or_default();
                    let note = json!({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics",
                        "params": {"uri": uri, "diagnostics": diagnostics(text)}});
                    if write_message(&mut out, &note).is_err() {
                        return;
                    }
                }
                "exit" => return,
                _ => {}
            }
            continue;
        };
        if msg.get("method").is_none() {
            continue; // a response to a request we never sent
        }
        let result: Result<Value, (i64, &str)> = match method.as_str() {
            "initialize" => Ok(
                json!({"capabilities": {"hoverProvider": true, "definitionProvider": true}, "serverInfo": {"name": "fake-lsp"}}),
            ),
            "shutdown" => Ok(Value::Null),
            "textDocument/hover" if line == 49 => Err((-32603, "fake hover failure")),
            "textDocument/hover" if line >= 99 => Ok(Value::Null),
            "textDocument/hover" => Ok(
                json!({"contents": {"kind": "markdown", "value": format!("hover at {}:{ch}", line + 1)}}),
            ),
            "textDocument/definition" if line >= 99 => Ok(json!([])),
            "textDocument/definition" => Ok(json!([{"uri": uri, "range": range(0, 3)}])),
            "textDocument/references" => Ok(json!([
                {"uri": uri, "range": range(5, 2)},
                {"uri": uri, "range": range(1, 4)},
                {"uri": sibling(&uri, "other.rs"), "range": range(2, 0)}
            ])),
            "textDocument/documentSymbol" => Ok(json!([
                {"name": "Foo", "kind": 23, "detail": "struct Foo", "range": range(0, 0), "selectionRange": range(0, 7),
                 "children": [{"name": "bar", "kind": 6, "range": range(2, 4), "selectionRange": range(2, 7), "children": []}]},
                {"name": "helper", "kind": 12, "location": {"uri": uri, "range": range(9, 0)}}
            ])),
            "workspace/symbol" => Ok(json!([
                {"name": params["query"].as_str().unwrap_or_default(), "kind": 5, "containerName": "crate",
                 "location": {"uri": "file:///w/lib.rs", "range": range(4, 0)}}
            ])),
            "textDocument/prepareCallHierarchy" => Ok(json!([item("main", 12, &uri, line)])),
            "callHierarchy/incomingCalls" => {
                let uri = params["item"]["uri"].as_str().unwrap_or_default();
                Ok(json!([{"from": item("caller", 12, uri, 9), "fromRanges": []}]))
            }
            "callHierarchy/outgoingCalls" => {
                let uri = params["item"]["uri"].as_str().unwrap_or_default();
                Ok(
                    json!([{"to": item("helper", 12, &sibling(uri, "other.rs"), 19), "fromRanges": []}]),
                )
            }
            _ => Err((-32601, "method not found")),
        };
        let reply = match result {
            Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
            Err((code, message)) => {
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
            }
        };
        if write_message(&mut out, &reply).is_err() {
            return;
        }
    }
}
