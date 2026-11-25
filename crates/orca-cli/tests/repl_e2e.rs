//! End to end: the interactive REPL with input piped in (the non-terminal fallback that reads
//! stdin lines), against the fake OpenAI server. The pty-driven runs are cross-check
//! scenarios 95-107 (orca-crosscheck `interactive`).

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use orca_testkit::{FakeOpenAi, Reply};
use serde_json::json;

fn write_config(home: &Path, base_url: &str) {
    let config = json!({
        "lmStudio": {"baseUrl": base_url, "apiKey": "test-key", "models": ["fake-model"], "nativeToolCalling": true, "maxTokens": 4096},
        "permissions": {"autoApproveAll": true},
        "memory": {"autoMemoryEnabled": false},
    });
    std::fs::write(home.join("config.json"), config.to_string()).unwrap();
}

async fn run_repl(home: &Path, work: &Path, input: &str) -> Output {
    let exe = env!("CARGO_BIN_EXE_abhoth").to_string();
    let (home, work, input) = (home.to_path_buf(), work.to_path_buf(), input.to_string());
    tokio::task::spawn_blocking(move || {
        let mut child = Command::new(exe)
            .current_dir(work)
            .env("OPENORCA_HOME", home)
            .env_remove("RUST_LOG")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn piped_input_runs_a_turn_commands_and_exits() {
    let server = FakeOpenAi::start(vec![Reply::text("Hello from the fake model.")]).await;
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    write_config(home.path(), &server.base_url());

    let out = run_repl(
        home.path(),
        work.path(),
        "say hi\n/rewind\n/hlep\n/sesion\n!echo shell-says-hi\n/exit\n",
    )
    .await;
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "exit {:?}\n{stdout}", out.status);
    for expected in [
        "❯ say hi",
        "Hello from the fake model.",
        "✻ ",
        "Rewound 1 turn(s) (removed 2 messages).",
        "Unknown command /hlep — /help lists commands",
        "Unknown command /sesion — did you mean /session?",
        "⎿  shell-says-hi",
        "Goodbye!",
    ] {
        assert!(
            stdout.contains(expected),
            "missing {expected:?} in:\n{stdout}"
        );
    }
    // autoSave (on by default) wrote the session after the turn.
    let sessions: Vec<_> = std::fs::read_dir(home.path().join("sessions"))
        .unwrap()
        .collect();
    assert_eq!(sessions.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn end_of_input_ends_the_repl() {
    let server = FakeOpenAi::start(vec![]).await;
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    write_config(home.path(), &server.base_url());
    let out = run_repl(home.path(), work.path(), "/help\n").await;
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "exit {:?}\n{stdout}", out.status);
    assert!(stdout.contains("/session"), "{stdout}");
    assert!(server.requests().is_empty());
}

/// `/retry` runs the last prompt again on the next model of `lmStudio.models`, and the request
/// names that model (C# drops the prompt and keeps sending the first model; D127).
#[tokio::test(flavor = "multi_thread")]
async fn retry_resends_the_last_prompt_to_the_backup_model() {
    let server = FakeOpenAi::start(vec![
        Reply::text("Fact one from the first model."),
        Reply::text("Fact two from the backup model."),
    ])
    .await;
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let config = json!({
        "lmStudio": {"baseUrl": server.base_url(), "apiKey": "test-key", "models": ["fake-model", "backup-model"], "nativeToolCalling": true, "maxTokens": 4096},
        "permissions": {"autoApproveAll": true},
        "session": {"autoSave": false},
    });
    std::fs::write(home.path().join("config.json"), config.to_string()).unwrap();
    let out = run_repl(home.path(), work.path(), "tell me a fact\n/retry\n/exit\n").await;
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Retrying last turn with backup model: backup-model"),
        "{stdout}"
    );
    let reqs = server.requests();
    let dump: Vec<String> = reqs
        .iter()
        .map(|r| {
            format!(
                "{} {}",
                r.body["model"],
                r.body["messages"].as_array().unwrap().len()
            )
        })
        .collect();
    assert_eq!(
        reqs.len(),
        2,
        "{dump:?}
{stdout}"
    );
    assert_eq!(reqs[1].body["model"], "backup-model");
    let users: Vec<_> = reqs[1].body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "user")
        .collect();
    assert_eq!(users.len(), 1);
    assert_eq!(users[0]["content"], "tell me a fact");
}

/// Custom markdown commands run (project over global, {{ARGS}} / {{ARGn}}); the C# REPL
/// answers "Unknown command" for them (D127), so this is not a cross-check scenario.
#[tokio::test(flavor = "multi_thread")]
async fn custom_commands_expand_their_template() {
    let server = FakeOpenAi::start(vec![Reply::text("Hello to the world.")]).await;
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    write_config(home.path(), &server.base_url());
    std::fs::create_dir_all(home.path().join("commands")).unwrap();
    std::fs::write(home.path().join("commands/greet.md"), "Global {{ARGS}}").unwrap();
    std::fs::write(work.path().join("app.sln"), "").unwrap();
    std::fs::create_dir_all(work.path().join(".orca/commands")).unwrap();
    std::fs::write(
        work.path().join(".orca/commands/greet.md"),
        "Say hello to {{ARGS}} (first: {{ARG1}}, second: {{ARG2}}).",
    )
    .unwrap();
    let out = run_repl(home.path(), work.path(), "/greet big world\n/exit\n").await;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let reqs = server.requests();
    assert_eq!(reqs.len(), 1, "{stdout}");
    let last = reqs[0].body["messages"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    assert_eq!(
        last["content"],
        "Say hello to big world (first: big, second: world)."
    );
}
