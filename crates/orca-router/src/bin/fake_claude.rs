//! A stand-in for the `claude` CLI in the Claude engine's tests (built with `claude-engine`).
//!
//! `--fake-scenario <file>`: JSONL played in order. A line is printed to stdout as it is,
//! unless it is a directive `{"fake": ...}`:
//! - `read_user`: read stdin until the user turn (answering `initialize` on the way);
//! - `mcp` + `message`: send an `mcp_message` control request, wait for its response;
//! - `sleep` + `ms`; `stderr` + `text`; `exit` + `code`; `hang` (sleep for an hour).
//!
//! `--fake-record <file>`: append what it saw (argv, cwd, env of interest, stdin lines) as
//! JSONL, flushed per line, so a test can read it even after the process was killed.
//! Every other argument is accepted and ignored, like the real CLI's flags.

use std::io::{BufRead, Write};
use std::time::Duration;

use serde_json::{Value, json};

struct Rec(Option<std::fs::File>);

impl Rec {
    fn put(&mut self, v: Value) {
        if let Some(f) = self.0.as_mut() {
            let _ = writeln!(f, "{v}");
            let _ = f.flush();
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arg = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let mut rec = Rec(arg("--fake-record").and_then(|p| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(p)
            .ok()
    }));
    let env = |k: &str| std::env::var(k).ok();
    rec.put(json!({
        "argv": args,
        "cwd": std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default(),
        "env": {
            "CLAUDE_CODE_OAUTH_TOKEN": env("CLAUDE_CODE_OAUTH_TOKEN"),
            "ANTHROPIC_API_KEY": env("ANTHROPIC_API_KEY"),
            "CLAUDE_CODE_MAX_OUTPUT_TOKENS": env("CLAUDE_CODE_MAX_OUTPUT_TOKENS"),
        },
    }));
    // the system prompt file, as the real CLI would read it
    if let Some(p) = arg("--system-prompt-file") {
        rec.put(json!({"system": std::fs::read_to_string(p).ok()}));
    }
    let Some(scenario) = arg("--fake-scenario") else {
        eprintln!("fake claude: no --fake-scenario");
        std::process::exit(2);
    };
    let text = std::fs::read_to_string(&scenario).expect("scenario");
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut out = std::io::stdout();
    let mut read_line = |rec: &mut Rec| -> Option<Value> {
        let mut l = String::new();
        if input.read_line(&mut l).ok()? == 0 {
            return None;
        }
        let v: Value = serde_json::from_str(l.trim()).unwrap_or(Value::Null);
        rec.put(json!({"stdin": v}));
        Some(v)
    };
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let v: Value = serde_json::from_str(line).expect("scenario line");
        let Some(d) = v.get("fake").and_then(Value::as_str) else {
            let _ = writeln!(out, "{line}");
            let _ = out.flush();
            continue;
        };
        match d {
            "read_user" => {
                while let Some(m) = read_line(&mut rec) {
                    match m.get("type").and_then(Value::as_str) {
                        Some("user") => break,
                        Some("control_request") => {
                            let r = json!({"type": "control_response", "response": {
                                "subtype": "success", "request_id": m["request_id"], "response": {}}});
                            let _ = writeln!(out, "{r}");
                            let _ = out.flush();
                        }
                        _ => {}
                    }
                }
            }
            "mcp" => {
                let rid = format!("mcp-{}", rand_id());
                let r = json!({"type": "control_request", "request_id": rid, "request": {
                    "subtype": "mcp_message", "server_name": "c", "message": v["message"]}});
                let _ = writeln!(out, "{r}");
                let _ = out.flush();
                while let Some(m) = read_line(&mut rec) {
                    if m.pointer("/response/request_id").and_then(Value::as_str) == Some(&rid) {
                        break;
                    }
                }
            }
            "sleep" => std::thread::sleep(Duration::from_millis(v["ms"].as_u64().unwrap_or(0))),
            "stderr" => eprintln!("{}", v["text"].as_str().unwrap_or("")),
            "exit" => std::process::exit(v["code"].as_i64().unwrap_or(0) as i32),
            "hang" => std::thread::sleep(Duration::from_secs(3600)),
            other => panic!("unknown directive {other}"),
        }
    }
}

fn rand_id() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}
