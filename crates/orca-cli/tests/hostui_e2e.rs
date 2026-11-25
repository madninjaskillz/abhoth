//! End to end: `openorca --hostui` starts the web UI headless, prints its URL, and serves the
//! API with the token; `--hostui-bench` runs a suite through a fake task kit against the fake
//! model server; bad --hostui flags exit 2 with the C# messages.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

use orca_testkit::{FakeOpenAi, Reply};
use serde_json::{Value, json};

#[tokio::test(flavor = "multi_thread")]
async fn hostui_prints_the_url_and_serves_the_api() {
    let server = FakeOpenAi::start(vec![Reply::text("unused")]).await;
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("config.json"),
        json!({"lmStudio": {"baseUrl": server.base_url(), "apiKey": "k", "models": ["fake-model"]}})
            .to_string(),
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_openorca"))
        .args(["--hostui=127.0.0.1:0", "--hostui-token=e2e-token-123"])
        .current_dir(work.path())
        .env("OPENORCA_HOME", home.path())
        .env_remove("RUST_LOG")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let url = tokio::task::spawn_blocking(move || {
        let mut lines = BufReader::new(stdout).lines();
        let mut seen = Vec::new();
        while let Some(Ok(line)) = lines.next() {
            if let Some(u) = line.strip_prefix("● Web UI ") {
                return Ok(u.trim().to_string());
            }
            seen.push(line);
        }
        Err(seen)
    })
    .await
    .unwrap();
    let url = match url {
        Ok(u) => u,
        Err(seen) => {
            let _ = child.kill();
            let out = child.wait_with_output().unwrap();
            panic!(
                "no URL; stdout: {seen:?}; stderr: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    };
    assert!(url.ends_with("/?token=e2e-token-123"), "{url}");
    let base = url.trim_end_matches("?token=e2e-token-123").to_string();
    let c = reqwest::Client::new();
    let info: Value = c
        .get(format!("{base}api/info"))
        .header("X-OpenOrca-Token", "e2e-token-123")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let denied = c.get(format!("{base}api/info")).send().await.unwrap();
    let _ = child.kill();
    let _ = child.wait();
    assert_eq!(info["mode"], "headless");
    assert_eq!(info["hub"], "ws");
    assert_eq!(denied.status().as_u16(), 401);
}

#[test]
fn bad_hostui_flags_exit_2() {
    for (args, message) in [
        (
            vec!["--hostui", "--hostui-token=abc"],
            "--hostui-token must be at least 8 characters.",
        ),
        (
            vec!["--hostui-open"],
            "--hostui-token and --hostui-open need --hostui.",
        ),
        (vec!["--hostui=notahost:1"], "Not an IP address: notahost."),
    ] {
        let home = tempfile::tempdir().unwrap();
        let out = Command::new(env!("CARGO_BIN_EXE_openorca"))
            .args(&args)
            .env("OPENORCA_HOME", home.path())
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains(message), "{args:?}: {err}");
    }
}

/// benchkit.py's stand-in: one task, "demo"; passes when answer.txt says 42.
const FAKE_KIT: &str = r#"import json, os, sys
kit, cmd, *rest = sys.argv[1:]
if cmd == "list":
    print(json.dumps([{"name": "demo", "timeout": 120}]))
elif cmd == "setup":
    os.makedirs(rest[1], exist_ok=True)
    print(json.dumps({"prompt": "Write 42 to answer.txt", "timeout": 120}))
elif cmd == "check":
    os.makedirs(rest[2], exist_ok=True)
    f = os.path.join(rest[1], "answer.txt")
    ok = os.path.exists(f) and open(f).read().strip() == "42"
    print(json.dumps({"pass": ok, "reason": "ok" if ok else "wrong", "tests_passed": 1 if ok else 0, "tests_total": 1}))
"#;

#[tokio::test(flavor = "multi_thread")]
async fn hostui_bench_runs_a_suite_in_its_own_home_and_records_the_row() {
    let python = if cfg!(windows) { "python" } else { "python3" };
    if !Command::new(python)
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
    {
        eprintln!("python is not on PATH: skipped");
        return;
    }
    let window = orca_webui::bench::time::in_quiet_window(
        std::time::SystemTime::now(),
        std::time::Duration::from_secs(300),
        6 * 60 + 45,
        7 * 60 + 30,
    );
    if window {
        eprintln!("inside the bench's quiet window (06:45-07:30 UK): skipped");
        return;
    }
    let server = FakeOpenAi::start(vec![
        // The run's session starts with a probe ("hi").
        Reply::text("hello"),
        Reply::tool("write_file", json!({"path": "answer.txt", "content": "42"})),
        Reply::text("Wrote 42 to answer.txt."),
        Reply::text("Done."),
        Reply::text("Done."),
    ])
    .await;
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let bench = home.path().join("bench");
    std::fs::create_dir_all(bench.join("suites")).unwrap();
    std::fs::write(
        bench.join("suites").join("e2e.json"),
        r#"{"arms":[{"name":"auto","leadMode":"auto"}],"tasks":["demo"],"reps":1}"#,
    )
    .unwrap();
    let kit = home.path().join("benchkit.py");
    std::fs::write(&kit, FAKE_KIT).unwrap();
    std::fs::write(
        home.path().join("config.json"),
        json!({
            "lmStudio": {"baseUrl": server.base_url(), "apiKey": "k", "models": ["fake-model"], "nativeToolCalling": true},
            "agent": {"verificationMode": "off"},
        })
        .to_string(),
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_openorca"))
        .args([
            "--hostui=127.0.0.1:0",
            "--hostui-token=e2e-token-123",
            &format!("--hostui-bench={}", bench.display()),
        ])
        .current_dir(work.path())
        .env("OPENORCA_HOME", home.path())
        .env("OPENORCA_BENCHKIT", &kit)
        .env_remove("RUST_LOG")
        .env_remove("REALTASKS_REPO")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let url = tokio::task::spawn_blocking(move || {
        let lines = BufReader::new(stdout).lines();
        for line in lines.map_while(Result::ok) {
            if let Some(u) = line.strip_prefix("● Web UI ") {
                return Some(u.trim().to_string());
            }
        }
        None
    })
    .await
    .unwrap();
    let Some(url) = url else {
        let _ = child.kill();
        let out = child.wait_with_output().unwrap();
        panic!("no URL; stderr: {}", String::from_utf8_lossy(&out.stderr));
    };
    let base = url.trim_end_matches("?token=e2e-token-123").to_string();
    let c = reqwest::Client::new();
    let get = |path: &str| {
        c.get(format!("{base}{path}"))
            .header("X-OpenOrca-Token", "e2e-token-123")
            .send()
    };
    let info: Value = get("api/info").await.unwrap().json().await.unwrap();
    assert_eq!(info["bench"], true);
    let state: Value = get("api/bench").await.unwrap().json().await.unwrap();
    assert!(
        state["suites"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["name"] == "e2e")
    );
    let started: Value = c
        .post(format!("{base}api/bench/start"))
        .header("X-OpenOrca-Token", "e2e-token-123")
        .json(&json!({"suite": "e2e"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(started["queued"], 1, "{started}");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    let row = loop {
        let rows: Value = get("api/bench/results")
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if let Some(r) = rows.as_array().and_then(|a| a.first()) {
            break r.clone();
        }
        if std::time::Instant::now() > deadline {
            let state: Value = get("api/bench").await.unwrap().json().await.unwrap();
            let _ = child.kill();
            panic!("no results row; state: {state}");
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    };
    let _ = child.kill();
    let out = child.wait_with_output().unwrap();
    let ws = bench
        .join("runs")
        .join(row["id"].as_str().unwrap())
        .join("ws");
    let listing: Vec<String> = std::fs::read_dir(&ws)
        .map(|r| {
            r.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(
        row["status"],
        "passed",
        "{row}
requests: {}
ws: {listing:?}
stderr: {}",
        server
            .requests()
            .iter()
            .map(|r| r.body["messages"]
                .as_array()
                .map(|m| m.last().cloned())
                .unwrap_or_default()
                .map(|m| m.to_string())
                .unwrap_or_default())
            .collect::<Vec<_>>()
            .join(" || "),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(row["suite"], "e2e");
    assert_eq!(row["lead_mode"], "auto");
    assert_eq!(row["tests_passed"], 1);
    assert_eq!(row["settings"]["agent"]["leadMode"], "auto");
    // The run's home: the base config with the arm's lead mode, tools auto-approved.
    let run = row["id"].as_str().unwrap();
    let cfg: Value = serde_json::from_str(
        &std::fs::read_to_string(
            bench
                .join("runs")
                .join(run)
                .join("home")
                .join("config.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(cfg["permissions"]["autoApproveAll"], true);
    assert_eq!(cfg["lmStudio"]["baseUrl"], server.base_url());
    let line = std::fs::read_to_string(bench.join("results.jsonl")).unwrap();
    assert_eq!(line.lines().count(), 1);
}
