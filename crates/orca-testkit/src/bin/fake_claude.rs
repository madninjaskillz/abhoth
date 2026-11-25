//! A stand-in for the Claude Code CLI (tests and the cross-check; never the real CLI).
//!
//! - `--version`: prints `2.1.326 (Claude Code)`.
//! - `auth status --json`: `{"loggedIn": true}`, or false when `FAKE_CLAUDE_LOGGED_OUT` is set.
//! - Anything else is the stream-json bridge mode: answers the `initialize` and `interrupt`
//!   control requests and, for each `user` message, writes the next turn of the script in
//!   `FAKE_CLAUDE_SCRIPT` (a JSONL file, relative to the working directory; a turn ends with
//!   its `"type":"result"` line). In the script, `<CWD>` is the working directory (JSON-escaped)
//!   and `<RESUMED>` is the `--resume` id it was started with (or `none`).
//!
//! Every argument list and stdin line it received is appended to `FAKE_CLAUDE_LOG` when set.

use std::io::{BufRead, Write};

use serde_json::{Value, json};

fn log(line: &str) {
    if let Ok(path) = std::env::var("FAKE_CLAUDE_LOG")
        && let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
    {
        let _ = writeln!(f, "{line}");
    }
}

fn turns(resumed: &str) -> Vec<Vec<String>> {
    let Ok(path) = std::env::var("FAKE_CLAUDE_SCRIPT") else {
        return Vec::new();
    };
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let cwd = std::env::current_dir()
        .map(|d| d.display().to_string())
        .unwrap_or_default();
    let cwd_json = serde_json::to_string(&cwd).unwrap_or_default();
    let cwd_json = &cwd_json[1..cwd_json.len() - 1];
    let mut out = Vec::new();
    let mut current = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let line = line
            .replace("<CWD>", cwd_json)
            .replace("<RESUMED>", resumed);
        let end = line.contains("\"type\":\"result\"");
        current.push(line);
        if end {
            out.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    log(&format!("args: {}", args.join(" ")));
    if args.iter().any(|a| a == "--version") {
        println!("2.1.326 (Claude Code)");
        return;
    }
    if args.first().map(String::as_str) == Some("auth") {
        let logged_in = std::env::var_os("FAKE_CLAUDE_LOGGED_OUT").is_none();
        println!("{}", json!({"loggedIn": logged_in, "authMethod": "fake"}));
        return;
    }
    let resumed = args
        .iter()
        .position(|a| a == "--resume")
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| "none".into());
    let mut script = turns(&resumed).into_iter();
    let stdout = std::io::stdout();
    let emit = |line: &str| {
        let mut out = stdout.lock();
        let _ = writeln!(out, "{line}");
        let _ = out.flush();
    };
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        log(&format!("stdin: {line}"));
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        match msg["type"].as_str() {
            Some("control_request") => {
                let id = msg["request_id"].as_str().unwrap_or_default();
                let response = match msg["request"]["subtype"].as_str() {
                    Some("initialize") => json!({"commands": []}),
                    _ => json!({}),
                };
                emit(
                    &json!({"type": "control_response", "response": {"subtype": "success", "request_id": id, "response": response}})
                        .to_string(),
                );
            }
            Some("user") => {
                for l in script.next().unwrap_or_default() {
                    emit(&l);
                }
            }
            _ => {}
        }
    }
}
