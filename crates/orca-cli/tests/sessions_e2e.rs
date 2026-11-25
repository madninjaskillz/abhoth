//! End to end: `--print` with `-r` / `-c`, checkpoints, hooks and the formatter, against the
//! fake OpenAI server. The resumed session is one the C# build wrote
//! (`orca-core/tests/fixtures/csharp/sessions/csharp01.json`).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use orca_testkit::{FakeOpenAi, Reply};
use serde_json::{Value, json};

fn fixture(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../orca-core/tests/fixtures/csharp")
        .join(rel)
}

fn write_config(home: &Path, base_url: &str, extra: Value) {
    let mut config = json!({
        "lmStudio": {"baseUrl": base_url, "apiKey": "test-key", "models": ["fake-model"], "nativeToolCalling": true, "maxTokens": 4096},
        "permissions": {"autoApproveAll": true},
    });
    if let (Some(c), Some(e)) = (config.as_object_mut(), extra.as_object()) {
        for (k, v) in e {
            c.insert(k.clone(), v.clone());
        }
    }
    std::fs::write(home.join("config.json"), config.to_string()).unwrap();
}

fn plant_session(home: &Path, id: &str, content: &str) {
    std::fs::create_dir_all(home.join("sessions")).unwrap();
    std::fs::write(home.join("sessions").join(format!("{id}.json")), content).unwrap();
}

async fn run(home: &Path, work: &Path, args: &[&str]) -> Output {
    let exe = env!("CARGO_BIN_EXE_abhoth").to_string();
    let (home, work) = (home.to_path_buf(), work.to_path_buf());
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    tokio::task::spawn_blocking(move || {
        Command::new(exe)
            .args(&args)
            .current_dir(work)
            .env("OPENORCA_HOME", home)
            .env_remove("RUST_LOG")
            .output()
            .unwrap()
    })
    .await
    .unwrap()
}

fn json_of(out: &Output) -> Value {
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "exit {:?}\nstdout: {stdout}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("{e}: {stdout}"))
}

fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn resume_sends_the_saved_conversation_and_does_not_autosave() {
    let server = FakeOpenAi::start(vec![Reply::text("Hello again.")]).await;
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    // autoSave on (the default): --print still does not save.
    write_config(home.path(), &server.base_url(), json!({}));
    let saved = std::fs::read_to_string(fixture("sessions/csharp01.json")).unwrap();
    plant_session(home.path(), "csharp01", &saved);

    let out = run(
        home.path(),
        work.path(),
        &["--print", "And now?", "-r", "csharp01"],
    )
    .await;
    let v = json_of(&out);
    assert_eq!(v["response"], "Hello again.");
    assert!(
        stderr_of(&out).contains("Resumed session: Fix the 'bug' in a+b <now>"),
        "{}",
        stderr_of(&out)
    );
    assert!(stderr_of(&out).contains("(7 messages)"));

    let reqs = server.requests();
    assert_eq!(reqs.len(), 1);
    let msgs = reqs[0].body["messages"].as_array().unwrap();
    let roles: Vec<&str> = msgs.iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert_eq!(
        roles,
        [
            "system",
            "user",
            "assistant",
            "tool",
            "assistant",
            "tool",
            "tool",
            "assistant",
            "user"
        ]
    );
    // The current system prompt replaces the saved one.
    assert_ne!(msgs[0]["content"], "You are Orca.\nBe brief & exact <ok>.");
    assert_eq!(
        msgs[1]["content"],
        "Fix the 'bug' in a+b <now> \u{2014} caf\u{e9} \u{1F600}\r\nplease"
    );
    assert_eq!(msgs[2]["tool_calls"][0]["id"], "call_1");
    assert_eq!(msgs[2]["tool_calls"][0]["function"]["name"], "read_file");
    assert_eq!(msgs[4]["content"], "");
    assert_eq!(msgs[4]["tool_calls"].as_array().unwrap().len(), 2);
    assert_eq!(msgs[5]["tool_call_id"], "call_2");
    assert_eq!(msgs[5]["content"], "a&b");
    assert_eq!(msgs[8], json!({"role": "user", "content": "And now?"}));

    // Nothing was written back.
    let files: Vec<_> = std::fs::read_dir(home.path().join("sessions"))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(files.len(), 1);
    assert_eq!(
        std::fs::read_to_string(home.path().join("sessions/csharp01.json")).unwrap(),
        saved
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn resume_unknown_id_stops_with_exit_code_zero() {
    let server = FakeOpenAi::start(vec![Reply::text("unused")]).await;
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    write_config(home.path(), &server.base_url(), json!({}));

    let out = run(home.path(), work.path(), &["--print", "hi", "-r", "nope"]).await;
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stdout.is_empty());
    assert_eq!(stderr_of(&out).trim(), "Session not found: nope");
    assert!(server.requests().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn continue_prefers_this_directory_then_any() {
    let server = FakeOpenAi::start(vec![Reply::text("one"), Reply::text("two")]).await;
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    write_config(home.path(), &server.base_url(), json!({}));
    let session = |id: &str, dir: &str, updated: &str, text: &str| {
        json!({
            "id": id, "title": text, "createdAt": updated, "updatedAt": updated,
            "workingDirectory": dir,
            "messages": [{"role": "user", "text": text}, {"role": "assistant", "text": "ok"}]
        })
        .to_string()
    };
    let here = work.path().display().to_string();
    plant_session(
        home.path(),
        "here",
        &session(
            "here",
            &here.to_uppercase(),
            "2026-01-01T00:00:00Z",
            "from here",
        ),
    );
    plant_session(
        home.path(),
        "other",
        &session(
            "other",
            "/elsewhere",
            "2026-06-01T00:00:00Z",
            "from elsewhere",
        ),
    );

    let out = run(home.path(), work.path(), &["--print", "go on", "-c"]).await;
    json_of(&out);
    let reqs = server.requests();
    assert_eq!(reqs[0].body["messages"][1]["content"], "from here");
    assert!(!stderr_of(&out).contains("elsewhere"));

    // From another directory the most recent session is taken, with a notice.
    let other_work = tempfile::tempdir().unwrap();
    let out = run(home.path(), other_work.path(), &["--print", "go on", "-c"]).await;
    json_of(&out);
    assert_eq!(
        server.requests()[1].body["messages"][1]["content"],
        "from elsewhere"
    );
    assert!(stderr_of(&out).contains("resuming most recent from elsewhere"));
}

#[tokio::test(flavor = "multi_thread")]
async fn continue_without_sessions_starts_fresh() {
    let server = FakeOpenAi::start(vec![Reply::text("fresh")]).await;
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    write_config(home.path(), &server.base_url(), json!({}));
    let out = run(home.path(), work.path(), &["--print", "hi", "--continue"]).await;
    assert_eq!(json_of(&out)["response"], "fresh");
    assert!(stderr_of(&out).contains("No saved sessions to continue."));
    assert_eq!(
        server.requests()[0].body["messages"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

fn manifest(home: &Path, session: &str) -> Vec<Value> {
    let text =
        std::fs::read_to_string(home.join("checkpoints").join(session).join("manifest.json"))
            .unwrap();
    serde_json::from_str(&text).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn file_changes_are_checkpointed_under_the_session() {
    let script = || {
        vec![
            Reply::tool("read_file", json!({"path": "a.txt"})),
            Reply::tool("write_file", json!({"path": "a.txt", "content": "new\n"})),
            Reply::tool("write_file", json!({"path": "b.txt", "content": "fresh\n"})),
            Reply::text("I rewrote a.txt with new content and created b.txt as asked."),
            Reply::text("Verified: a.txt and b.txt have the expected content now."),
            Reply::text("Verified: a.txt and b.txt have the expected content now."),
        ]
    };
    for resume in [false, true] {
        let server = FakeOpenAi::start(script()).await;
        let home = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        write_config(home.path(), &server.base_url(), json!({}));
        std::fs::write(work.path().join("a.txt"), "old\n").unwrap();
        let mut args = vec!["--print", "rewrite a.txt"];
        if resume {
            plant_session(
                home.path(),
                "s1",
                &json!({"id": "s1", "title": "t", "messages": []}).to_string(),
            );
            args.extend(["-r", "s1"]);
        }
        json_of(&run(home.path(), work.path(), &args).await);
        assert_eq!(
            std::fs::read_to_string(work.path().join("a.txt")).unwrap(),
            "new\n"
        );

        let sid = if resume { "s1" } else { "default" };
        let entries = manifest(home.path(), sid);
        // Only a.txt existed before it was written; b.txt was new.
        assert_eq!(entries.len(), 1, "{entries:?}");
        let a = work.path().join("a.txt").display().to_string();
        assert_eq!(entries[0]["filePath"], a.as_str());
        assert_eq!(entries[0]["sizeBytes"], 4);
        let backup = home
            .path()
            .join("checkpoints")
            .join(sid)
            .join(entries[0]["backupFile"].as_str().unwrap());
        assert_eq!(std::fs::read_to_string(backup).unwrap(), "old\n");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn pre_hook_blocks_the_call() {
    let server = FakeOpenAi::start(vec![
        Reply::tool("bash", json!({"command": "echo hi"})),
        Reply::text("The command was blocked by a hook, so nothing ran this time."),
        Reply::text("The command was blocked by a hook, so nothing ran this time."),
    ])
    .await;
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    write_config(
        home.path(),
        &server.base_url(),
        json!({"hooks": {"preToolHooks": {"bash": "exit 1"}}}),
    );
    let v = json_of(&run(home.path(), work.path(), &["--print", "run it"]).await);
    assert!(
        v.get("tool_calls").is_none(),
        "a blocked call is not recorded"
    );
    let reqs = server.requests();
    let msgs = reqs[1].body["messages"].as_array().unwrap();
    let last = msgs.last().unwrap();
    assert_eq!(last["role"], "tool");
    assert_eq!(last["content"], "Tool blocked by hook.");
}

#[tokio::test(flavor = "multi_thread")]
async fn formatter_output_is_appended_to_the_result() {
    let server = FakeOpenAi::start(vec![
        Reply::tool("write_file", json!({"path": "n.txt", "content": "x"})),
        Reply::text("Created n.txt with the requested content, and it was formatted."),
        Reply::text("Created n.txt with the requested content, and it was formatted."),
        Reply::text("Created n.txt with the requested content, and it was formatted."),
    ])
    .await;
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    write_config(
        home.path(),
        &server.base_url(),
        json!({"formatter": {"enabled": true, "commands": {".txt": "echo formatted"}}}),
    );
    let v = json_of(&run(home.path(), work.path(), &["--print", "make n.txt"]).await);
    // The record keeps the tool's own result; the model sees the formatter output too.
    assert!(
        !v["tool_calls"][0]["result"]
            .as_str()
            .unwrap()
            .contains("Auto-format")
    );
    let reqs = server.requests();
    let msgs = reqs[1].body["messages"].as_array().unwrap();
    let content = msgs.last().unwrap()["content"].as_str().unwrap();
    assert!(
        content.ends_with("\n\n[Auto-format] formatted"),
        "{content:?}"
    );
}
