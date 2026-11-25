//! End to end (D116): `abhoth --hostui` on a fresh home (no config.json, no model) starts; a
//! prompt gets a "no model" notice instead of a crash; once the settings page sets the model
//! server and a model, the same session's next prompt runs on it (orca-testkit's fake server).

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use orca_testkit::{FakeOpenAi, Reply};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;

const TOKEN: &str = "e2e-fresh-token";

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn read(socket: &mut Socket) -> Value {
    loop {
        let msg = match tokio::time::timeout(Duration::from_secs(60), socket.next()).await {
            Err(_) => return json!({"type": "failure", "error": "timed out"}),
            Ok(None) => return json!({"type": "failure", "error": "socket closed"}),
            Ok(Some(Err(e))) => return json!({"type": "failure", "error": e.to_string()}),
            Ok(Some(Ok(m))) => m,
        };
        if let Message::Text(t) = msg {
            return serde_json::from_str(&t).unwrap();
        }
    }
}

fn collect(v: &Value, events: &mut Vec<Value>) {
    if v["type"] == "event" && v["method"] == "events" {
        events.extend(v["args"][1].as_array().cloned().unwrap_or_default());
    }
}

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

/// Sends a prompt and returns the events of its turn (up to and with `turnEnd`).
async fn turn(socket: &mut Socket, id: &str, pid: &str, text: &str) -> Result<Vec<Value>, String> {
    let mut events = Vec::new();
    invoke(socket, id, "SendPrompt", json!([pid, text]), &mut events).await?;
    while !events.iter().any(|e| e["type"] == "turnEnd") {
        let v = read(socket).await;
        if v["type"] == "failure" {
            return Err(format!("{v}; events so far: {events:?}"));
        }
        collect(&v, &mut events);
    }
    Ok(events)
}

fn outcome(events: &[Value]) -> Value {
    events
        .iter()
        .find(|e| e["type"] == "turnEnd")
        .map(|e| e["data"]["outcome"].clone())
        .unwrap_or(Value::Null)
}

#[tokio::test(flavor = "multi_thread")]
async fn hostui_starts_without_a_model_and_picks_one_up_from_the_settings() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_abhoth"))
        .args(["--hostui=127.0.0.1:0", &format!("--hostui-token={TOKEN}")])
        .current_dir(work.path())
        .env("OPENORCA_HOME", home.path())
        // An admin exists (D140): the token then signs in as them.
        .env("ABHOTH_ADMIN_USER", "admin")
        .env("ABHOTH_ADMIN_PASSWORD_FILE", {
            let f = home.path().join("admin-password");
            std::fs::write(&f, "e2e admin password").unwrap();
            f
        })
        .env("ABHOTH_HOME", home.path())
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
    let fake = FakeOpenAi::start(vec![Reply::text("MODEL-SET-OK")]).await;
    let result = async {
        let http = reqwest::Client::new();
        let info: Value = http
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

        // No model: a clear notice, the turn ends as an error, the host keeps running.
        let events = turn(&mut socket, "1", &pid, "hello").await?;
        let notice = events
            .iter()
            .find(|e| e["type"] == "notice")
            .ok_or(format!("no notice: {events:?}"))?;
        assert_eq!(notice["data"]["level"], "error");
        let text = notice["data"]["text"].as_str().unwrap_or_default();
        assert!(text.contains("No model is configured"), "{text}");
        assert_eq!(outcome(&events), "error");

        // The settings page without a project (config.json) sets the server and a model.
        for (key, value) in [
            ("lmStudio.baseUrl", json!(fake.base_url())),
            ("lmStudio.models", json!(["fake-model"])),
        ] {
            let r: Value = http
                .put(format!("{base}/api/settings"))
                .header("X-OpenOrca-Token", TOKEN)
                .json(&json!({"key": key, "value": value}))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            assert_eq!(r["ok"], true, "{key}: {r}");
        }
        let events = turn(&mut socket, "2", &pid, "hello again").await?;
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
    let text: String = events
        .iter()
        .filter(|e| e["type"] == "assistantDelta")
        .filter_map(|e| e["data"]["text"].as_str())
        .collect();
    assert!(text.contains("MODEL-SET-OK"), "{text}; {events:?}");
    assert_eq!(outcome(&events), "done");
    let sent = fake.requests();
    assert!(
        sent.iter().any(|r| r.body["model"] == "fake-model"),
        "{:?}",
        sent.iter().map(|r| &r.body["model"]).collect::<Vec<_>>()
    );
}
