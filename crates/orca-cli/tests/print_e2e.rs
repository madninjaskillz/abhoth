//! End to end: the `openorca` binary, `--print`, against the fake OpenAI server. The script
//! is the one used to capture the C# build's behaviour (read a file, answer briefly, get asked
//! for a summary), and the output is compared field by field with what C# printed.

use std::process::Command;

use orca_testkit::{FakeOpenAi, Reply};
use serde_json::{Value, json};

fn write_config(home: &std::path::Path, base_url: &str, extra: Value) {
    let mut config = json!({
        "lmStudio": {"baseUrl": base_url, "apiKey": "test-key", "models": ["fake-model"], "nativeToolCalling": true, "temperature": 0.7},
    });
    if let (Some(c), Some(e)) = (config.as_object_mut(), extra.as_object()) {
        for (k, v) in e {
            c.insert(k.clone(), v.clone());
        }
    }
    std::fs::write(home.join("config.json"), config.to_string()).unwrap();
}

async fn run_print(
    home: &std::path::Path,
    work: &std::path::Path,
    prompt: &str,
) -> (Value, String) {
    let exe = env!("CARGO_BIN_EXE_openorca").to_string();
    let (home, work, prompt) = (home.to_path_buf(), work.to_path_buf(), prompt.to_string());
    let out = tokio::task::spawn_blocking(move || {
        Command::new(exe)
            .args(["--print", &prompt])
            .current_dir(work)
            .env("OPENORCA_HOME", home)
            .env_remove("RUST_LOG")
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        out.status.success(),
        "exit {:?}\nstdout: {stdout}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    (
        serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("{e}: {stdout}")),
        stdout,
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn print_reads_a_file_and_reports_like_csharp() {
    let server = FakeOpenAi::start(vec![
        Reply::tool("read_file", json!({"path": "hello.txt"})).with_reasoning("I should read it."),
        Reply::text("The file says hi."),
        Reply::text("The file says hi."),
    ])
    .await;
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    std::fs::write(work.path().join("hello.txt"), "hi there\n").unwrap();
    write_config(home.path(), &server.base_url(), json!({}));

    let (v, raw) = run_print(home.path(), work.path(), "What does hello.txt say?").await;

    // Output shape and formatting (JsonDefaults: indented, JavaScriptEncoder escaping).
    let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        ["response", "tokens", "duration_ms", "tool_calls", "success"]
    );
    assert!(raw.contains("\n  \"response\": \"The file says hi.\","));
    assert_eq!(v["response"], "The file says hi.");
    assert_eq!(
        v["tokens"], 40,
        "completion tokens summed over the three requests (20 + 10 + 10)"
    );
    assert_eq!(v["success"], true);
    let call = &v["tool_calls"][0];
    assert_eq!(call["name"], "read_file");
    let nl = if cfg!(windows) { "\r\n" } else { "\n" };
    assert_eq!(
        call["arguments"],
        format!("{{{nl}  \"path\": \"hello.txt\"{nl}}}")
    );
    let file = work.path().join("hello.txt");
    assert_eq!(
        call["result"],
        format!(
            "File: {} (1 lines, showing 1-1){nl}{nl}1\thi there{nl}",
            file.display()
        )
    );
    assert_eq!(call["is_error"], false);

    // The conversation the server saw, as the C# client sent it.
    let reqs = server.requests();
    assert_eq!(reqs.len(), 3, "tool call, short answer, summary request");
    assert_eq!(reqs[0].header("authorization"), Some("Bearer test-key"));
    let b0 = &reqs[0].body;
    assert_eq!(b0["model"], "fake-model");
    assert_eq!(b0["stream"], true);
    assert_eq!(b0["tool_choice"], "auto");
    assert_eq!(b0["messages"][0]["role"], "system");
    assert_eq!(
        b0["messages"][1],
        json!({"role": "user", "content": "What does hello.txt say?"})
    );
    let m1 = &reqs[1].body["messages"];
    assert_eq!(m1[2]["role"], "assistant");
    assert_eq!(m1[2]["content"], "");
    assert!(
        m1[2].get("reasoning_content").is_none(),
        "no reasoning echo without preserve-thinking"
    );
    assert_eq!(m1[2]["tool_calls"][0]["function"]["name"], "read_file");
    assert_eq!(m1[3]["role"], "tool");
    assert_eq!(m1[3]["tool_call_id"], m1[2]["tool_calls"][0]["id"]);
    let m2 = reqs[2].body["messages"].as_array().unwrap();
    let last = m2.last().unwrap();
    assert_eq!(last["role"], "user");
    assert!(
        last["content"]
            .as_str()
            .unwrap()
            .starts_with("All tool calls are complete.")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn print_write_then_denied_bash_reports_files_and_failure() {
    let server = FakeOpenAi::start(vec![
        Reply::tool(
            "write_file",
            json!({"path": "out.txt", "content": "made by the agent"}),
        ),
        Reply::tool("bash", json!({"command": "echo hi"})),
        Reply::text("I wrote out.txt, but running the command was not allowed in this session."),
    ])
    .await;
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    // write_file is Moderate: approve it; bash stays unapproved (no prompter: denied).
    write_config(
        home.path(),
        &server.base_url(),
        json!({"permissions": {"autoApproveModerate": true}}),
    );

    let (v, _) = run_print(home.path(), work.path(), "make out.txt").await;
    assert_eq!(
        std::fs::read_to_string(work.path().join("out.txt")).unwrap(),
        "made by the agent"
    );
    assert_eq!(v["files_modified"], json!(["out.txt"]));
    assert_eq!(
        v["tool_calls"].as_array().unwrap().len(),
        1,
        "a denied call never ran, so it is not recorded"
    );
    assert_eq!(v["success"], true);
    let reqs = server.requests();
    // write, denied bash, answer; then (a file changed and nothing ran since) the
    // verification step, its retry when the reply used no tools, and the final answer.
    assert_eq!(reqs.len(), 5);
    let last = reqs[4].body["messages"].as_array().unwrap();
    assert!(
        last[last.len() - 1]["content"]
            .as_str()
            .unwrap()
            .starts_with("You said \"Verified\"")
    );
    let msgs = reqs[2].body["messages"].as_array().unwrap();
    let denied = &msgs[msgs.len() - 1];
    assert_eq!(denied["role"], "tool");
    // C# returns the denial text as is (prelude errors skip the "ERROR: " prefix).
    assert_eq!(denied["content"], "Permission denied by user.");
}

#[test]
fn version_flag() {
    let out = Command::new(env!("CARGO_BIN_EXE_openorca"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("OpenOrca v"));
}
