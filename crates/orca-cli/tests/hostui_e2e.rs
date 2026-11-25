//! End to end: `openorca --hostui` starts the web UI headless, prints its URL, and serves the
//! API with the token; bad --hostui flags exit 2 with the C# messages.

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
        (
            vec!["--hostui=127.0.0.1:0", "--hostui-bench"],
            "--hostui-bench (the Benchmarks page) is not ported to Rust yet",
        ),
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
