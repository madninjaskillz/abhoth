//! End to end: the real `abhoth --hostui` with a temp home switches the session to the Claude
//! Code bridge over the hub and runs one bridged turn, played by orca-testkit's fake-claude
//! (never the real CLI). The cross-check can't drive the web host on both builds (D108).

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;

const TOKEN: &str = "e2e-bridge-token";

/// fake-claude next to abhoth (a workspace build builds both).
fn fake_claude() -> Option<PathBuf> {
    let dir = PathBuf::from(env!("CARGO_BIN_EXE_abhoth"))
        .parent()?
        .to_path_buf();
    let exe = dir.join(format!("fake-claude{}", std::env::consts::EXE_SUFFIX));
    exe.exists().then_some(exe)
}

const SCRIPT: &str = r#"{"type":"system","subtype":"init","cwd":"<CWD>","session_id":"00000000-0000-4000-8000-0000000000bb","tools":["Read"],"mcp_servers":[],"model":"claude-fake","permissionMode":"default"}
{"type":"assistant","message":{"model":"claude-fake","id":"m1","type":"message","role":"assistant","content":[{"type":"text","text":"Hello from the fake bridge. WEB-BRIDGE-DONE"}],"usage":{"input_tokens":2,"output_tokens":11}},"parent_tool_use_id":null,"session_id":"00000000-0000-4000-8000-0000000000bb"}
{"type":"result","subtype":"success","is_error":false,"duration_ms":20,"num_turns":1,"result":"Hello from the fake bridge. WEB-BRIDGE-DONE","session_id":"00000000-0000-4000-8000-0000000000bb","total_cost_usd":0.0,"usage":{"input_tokens":2,"output_tokens":11}}
"#;

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// The next text message; a failure is reported with the host's stderr by the test.
async fn read(socket: &mut Socket) -> Value {
    loop {
        let msg = match tokio::time::timeout(Duration::from_secs(60), socket.next()).await {
            Err(_) => {
                return json!({"type": "failure", "error": "timed out waiting for a hub message"});
            }
            Ok(None) => return json!({"type": "failure", "error": "socket closed"}),
            Ok(Some(Err(e))) => return json!({"type": "failure", "error": e.to_string()}),
            Ok(Some(Ok(m))) => m,
        };
        if let Message::Text(t) = msg {
            return serde_json::from_str(&t).unwrap();
        }
    }
}

/// Call a hub method and return its result; live events seen meanwhile go to `events`.
async fn invoke(
    socket: &mut Socket,
    id: &str,
    method: &str,
    args: Value,
    events: &mut Vec<Value>,
) -> Result<Value, String> {
    socket
        .send(Message::Text(
            json!({"type": "invoke", "id": id, "method": method, "args": args})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    loop {
        let v = read(socket).await;
        if v["type"] == "failure" {
            return Err(format!("{method}: {v}"));
        }
        if v["type"] == "result" && v["id"] == id {
            if !v.get("error").is_none_or(Value::is_null) {
                return Err(format!("{method}: {v}"));
            }
            return Ok(v["result"].clone());
        }
        collect(&v, events);
    }
}

fn collect(v: &Value, events: &mut Vec<Value>) {
    if v["type"] == "event" && v["method"] == "events" {
        events.extend(v["args"][1].as_array().cloned().unwrap_or_default());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn hostui_switches_to_the_bridge_and_runs_a_turn() {
    let Some(fake) = fake_claude() else {
        eprintln!("fake-claude is not built (cargo build -p orca-testkit): skipped");
        return;
    };
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    std::fs::write(work.path().join("claude-script.jsonl"), SCRIPT).unwrap();
    std::fs::write(
        home.path().join("config.json"),
        json!({
            "lmStudio": {"baseUrl": "http://127.0.0.1:9/v1", "apiKey": "k", "models": ["fake-model"]},
            "bridges": {"claude": {"path": fake.display().to_string(), "progressBars": false}},
            "permissions": {"autoApproveAll": true},
        })
        .to_string(),
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_abhoth"))
        .args(["--hostui=127.0.0.1:0", &format!("--hostui-token={TOKEN}")])
        .current_dir(work.path())
        .env("OPENORCA_HOME", home.path())
        .env("ABHOTH_HOME", home.path())
        .env("FAKE_CLAUDE_SCRIPT", "claude-script.jsonl")
        .env_remove("RUST_LOG")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let url = tokio::task::spawn_blocking(move || {
        BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
            .find_map(|l| l.strip_prefix("● Web UI ").map(|u| u.trim().to_string()))
    })
    .await
    .unwrap();
    let Some(url) = url else {
        let _ = child.kill();
        let out = child.wait_with_output().unwrap();
        panic!("no URL; stderr: {}", String::from_utf8_lossy(&out.stderr));
    };
    let base = url
        .split("/?")
        .next()
        .unwrap()
        .trim_end_matches('/')
        .to_string();
    let result = async {
        let info: Value = reqwest::Client::new()
            .get(format!("{base}/api/info"))
            .header("X-OpenOrca-Token", TOKEN)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let pid = info["primary"].as_str().unwrap().to_string();
        let ws_url = format!("{}/hub/ws?token={TOKEN}", base.replacen("http", "ws", 1));
        let (mut socket, _) = tokio_tungstenite::connect_async(ws_url).await.unwrap();
        loop {
            let v = read(&mut socket).await;
            if v["type"] == "welcome" {
                break;
            }
            if v["type"] == "failure" {
                return Err(format!("before welcome: {v}"));
            }
        }
        let mut events = Vec::new();

        let snap = invoke(&mut socket, "1", "GetSnapshot", json!([pid]), &mut events).await?;
        assert_eq!(snap["engine"], "native");
        let r = invoke(
            &mut socket,
            "2",
            "SetEngine",
            json!([pid, "claude"]),
            &mut events,
        )
        .await?;
        assert_eq!(r["ok"], true, "{r}");
        assert_eq!(r["engine"], "claude");
        let snap = invoke(&mut socket, "3", "GetSnapshot", json!([pid]), &mut events).await?;
        assert_eq!(snap["engine"], "claude");
        let lead = &snap["agents"]["agents"][0];
        assert_eq!(lead["slot"], "claude", "{snap}");
        assert_eq!(lead["model"], "claude-code");

        invoke(
            &mut socket,
            "4",
            "SendPrompt",
            json!([pid, "say hello"]),
            &mut events,
        )
        .await?;
        while !events.iter().any(|e| e["type"] == "turnEnd") {
            let v = read(&mut socket).await;
            if v["type"] == "failure" {
                return Err(format!("{v}; events so far: {events:?}"));
            }
            collect(&v, &mut events);
        }
        Ok::<Vec<Value>, String>(events)
    };
    let events = tokio::time::timeout(Duration::from_secs(120), result).await;
    let _ = child.kill();
    let out = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    let events = match events {
        Ok(Ok(e)) => e,
        Ok(Err(e)) => panic!("{e}; stderr: {stderr}"),
        Err(_) => panic!("timed out; stderr: {stderr}"),
    };
    assert!(
        events
            .iter()
            .any(|e| e["type"] == "engine" && e["data"]["engine"] == "claude")
    );
    let text: String = events
        .iter()
        .filter(|e| e["type"] == "assistantDelta")
        .filter_map(|e| e["data"]["text"].as_str())
        .collect();
    assert!(text.contains("WEB-BRIDGE-DONE"), "{text}");
    let end = events.iter().find(|e| e["type"] == "turnEnd").unwrap();
    assert_eq!(end["data"]["outcome"], "done", "{end}");
    assert_eq!(end["data"]["tokens"], 11);
}
