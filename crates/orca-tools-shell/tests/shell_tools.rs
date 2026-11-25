//! Ported from `OpenOrca.Tools.Tests/ShellToolTests.cs` (the cd tests live with the file
//! tools, in `orca-tools-fs`).

use std::sync::Mutex;
use std::time::{Duration, Instant};

use orca_abstractions::{Tool, ToolContext, ToolResult};
use orca_tools_shell::{
    BashTool, GetProcessOutputTool, ListProcessesTool, StartBackgroundProcessTool, StopProcessTool,
    process,
};
use serde_json::{Value, json};

/// Tests that start background processes run one at a time: `get_process_output` with "last"
/// picks the newest process of the whole (shared) manager.
static PROCESS_TESTS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn ctx() -> ToolContext {
    ToolContext::new(std::env::current_dir().unwrap())
}

async fn run(tool: &BashTool, args: Value) -> ToolResult {
    tool.execute(&args, &ctx()).await
}

fn win() -> bool {
    cfg!(windows)
}

fn slow_command() -> &'static str {
    if win() {
        "ping -n 30 127.0.0.1"
    } else {
        "sleep 30"
    }
}

#[tokio::test]
async fn simple_command_returns_output_with_header() {
    let r = run(&BashTool::default(), json!({"command": "echo hello"})).await;
    assert!(!r.is_error, "{}", r.content);
    let cwd = std::env::current_dir().unwrap();
    assert!(
        r.content.starts_with(&format!(
            "Working directory: {}\nExit code: 0\n\n",
            cwd.display()
        )),
        "{}",
        r.content
    );
    assert!(r.content.contains("hello"));
}

#[tokio::test]
async fn failing_command_is_an_error_with_exit_code() {
    let cmd = if win() { "cmd /c exit 1" } else { "false" };
    let r = run(&BashTool::default(), json!({"command": cmd})).await;
    // As in C#: cmd.exe fed through stdin exits 0 at end of input, so only the header is checked.
    assert!(r.content.to_lowercase().contains("exit code"));
    if !win() {
        assert!(r.is_error);
    }
}

#[tokio::test]
async fn stderr_lines_are_prefixed() {
    let cmd = if win() {
        "echo oops 1>&2"
    } else {
        "echo oops >&2"
    };
    let r = run(&BashTool::default(), json!({"command": cmd})).await;
    assert!(r.content.contains("[stderr] oops"), "{}", r.content);
}

#[tokio::test]
async fn working_directory_is_used() {
    let dir = tempfile::tempdir().unwrap();
    let cmd = if win() { "cd" } else { "pwd" };
    let r = run(
        &BashTool::default(),
        json!({"command": cmd, "working_directory": dir.path().display().to_string()}),
    )
    .await;
    assert!(!r.is_error, "{}", r.content);
    let name = dir
        .path()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();
    assert!(r.content.contains(&name));
    let r = run(
        &BashTool::default(),
        json!({"command": "echo x", "working_directory": "no/such/dir"}),
    )
    .await;
    assert!(r.is_error && r.content.starts_with("Working directory not found"));
}

#[tokio::test]
async fn long_output_keeps_head_and_tail() {
    let cmd = if win() {
        "for /L %i in (0,1,899) do @echo row-%i-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\r\necho SUMMARY: 1 failed, 899 passed"
    } else {
        "for i in $(seq 0 899); do echo row-$i-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx; done; echo 'SUMMARY: 1 failed, 899 passed'"
    };
    let r = run(&BashTool::default(), json!({"command": cmd})).await;
    assert!(
        r.content.contains("SUMMARY: 1 failed, 899 passed"),
        "{}",
        &r.content[r.content.len().saturating_sub(300)..]
    );
    assert!(r.content.contains("omitted"));
    assert!(r.content.chars().count() < 30_000);
}

#[tokio::test]
async fn multiline_output_and_streaming_callback() {
    let cmd = if win() {
        "echo alpha && echo beta && echo gamma"
    } else {
        "echo alpha; echo beta; echo gamma"
    };
    let streamed = Mutex::new(Vec::<String>::new());
    let r = BashTool::default()
        .execute_streaming(&json!({"command": cmd}), &ctx(), &|l| {
            streamed.lock().unwrap().push(l.to_string())
        })
        .await;
    assert!(!r.is_error, "{}", r.content);
    let lines = streamed.into_inner().unwrap();
    for w in ["alpha", "beta", "gamma"] {
        assert!(r.content.contains(w));
        assert!(lines.iter().any(|l| l.contains(w)), "{lines:?}");
    }
}

#[tokio::test]
async fn slow_command_returns_process_id_and_can_be_stopped() {
    let _serial = PROCESS_TESTS.lock().await;
    // A scope of its own: "last" then can't pick a process another test started meanwhile.
    let scope: Option<std::sync::Arc<str>> = Some("slow-command-test".into());
    let started = Instant::now();
    let r = run(
        &BashTool {
            idle_timeout_seconds: 2,
            scope: scope.clone(),
        },
        json!({"command": slow_command()}),
    )
    .await;
    assert!(!r.is_error, "{}", r.content);
    assert!(
        r.content.contains("still running")
            && r.content.contains("process ID")
            && r.content.contains("get_process_output")
    );
    assert!(started.elapsed() < Duration::from_secs(15));
    let id = r.content.split('"').nth(1).unwrap().to_string();

    let out = GetProcessOutputTool {
        scope: scope.clone(),
    }
    .execute(&json!({"process_id": "last"}), &ctx())
    .await;
    assert!(out.content.contains("Status: Running"), "{}", out.content);
    let stop = StopProcessTool {
        scope: scope.clone(),
    }
    .execute(&json!({"process_id": id}), &ctx())
    .await;
    assert!(
        stop.content.starts_with(&format!("Stopped process {id}")),
        "{}",
        stop.content
    );
    let p = process::get_in(&id, scope.as_deref()).unwrap();
    assert!(p.wait_for_exit(Duration::from_secs(5)).await);
}

#[tokio::test]
async fn timeout_override_respects_parameter() {
    let started = Instant::now();
    let r = run(
        &BashTool::default(),
        json!({"command": slow_command(), "timeout_seconds": 1}),
    )
    .await;
    assert!(r.content.contains("still running"));
    assert!(started.elapsed() < Duration::from_secs(15));
    process::stop(r.content.split('"').nth(1).unwrap());
}

#[tokio::test]
async fn completed_within_timeout_returns_full_output() {
    let r = run(
        &BashTool {
            idle_timeout_seconds: 30,
            scope: None,
        },
        json!({"command": "echo completed_ok"}),
    )
    .await;
    assert!(
        r.content.contains("completed_ok")
            && r.content.contains("Exit code: 0")
            && !r.content.contains("still running")
    );
}

#[tokio::test]
async fn manager_start_stop_tail_and_new_lines() {
    let _serial = PROCESS_TESTS.lock().await;
    let cwd = std::env::current_dir().unwrap();
    let long = if win() {
        "ping -t 127.0.0.1"
    } else {
        "sleep 60"
    };
    let m = process::start(long, &cwd).unwrap();
    assert!(!m.has_exited() && !m.id.is_empty());
    assert!(!m.wait_for_exit(Duration::from_millis(200)).await);
    assert!(process::stop(&m.id));
    assert!(m.wait_for_exit(Duration::from_secs(5)).await);
    assert!(!process::stop("nonexistent_process_id"));

    let cmd = if win() {
        "echo first && echo second && echo third"
    } else {
        "echo first; echo second; echo third"
    };
    let m = process::start(cmd, &cwd).unwrap();
    assert!(m.wait_for_exit(Duration::from_secs(5)).await);
    m.wait_for_output(Duration::from_secs(2)).await;
    let (lines, cursor) = m.new_lines(0);
    assert!(
        lines.len() >= 3 && lines.iter().any(|l| l.contains("first")),
        "{lines:?}"
    );
    let (again, cursor2) = m.new_lines(cursor);
    assert!(again.is_empty() && cursor2 == cursor);
    assert!(m.tail_lines(10).iter().any(|l| l.contains("third")));
}

#[tokio::test]
async fn start_background_process_same_command_already_running_blocked() {
    let _serial = PROCESS_TESTS.lock().await;
    // Regression: a model that cannot observe a GUI app relaunched the same command 8 times in
    // one session, leaking a process each time. A nonce keeps this command distinct.
    let nonce = format!("{:x}", std::process::id() ^ 0x5eed);
    let long_running = if win() {
        format!("ping -n 30 127.0.0.1 & rem {nonce}")
    } else {
        format!("sleep 30 # {nonce}")
    };
    let args = json!({"command": long_running});
    let first = StartBackgroundProcessTool::default()
        .execute(&args, &ctx())
        .await;
    assert!(!first.is_error, "{}", first.content);
    let id = first
        .content
        .strip_prefix("Started background process ")
        .and_then(|r| r.split(':').next())
        .unwrap()
        .to_string();
    let second = StartBackgroundProcessTool::default()
        .execute(&args, &ctx())
        .await;
    process::stop(&id);
    assert!(second.is_error);
    assert_eq!(second.error_code, orca_abstractions::ToolErrorCode::Blocked);
    assert!(second.content.contains("ALREADY RUNNING"));
    assert!(second.content.contains(&id));
}

#[tokio::test]
async fn start_background_process_missing_directory_and_listing() {
    let _serial = PROCESS_TESTS.lock().await;
    let r = StartBackgroundProcessTool::default()
        .execute(
            &json!({"command": "echo hi", "working_directory": "no_such_dir_xyz"}),
            &ctx(),
        )
        .await;
    assert!(r.is_error);
    assert!(r.content.starts_with("Working directory not found: "));

    let cmd = "echo listed";
    let started = StartBackgroundProcessTool::default()
        .execute(&json!({"command": cmd}), &ctx())
        .await;
    assert!(
        started
            .content
            .contains("Use get_process_output with process_id")
    );
    let list = ListProcessesTool::default()
        .execute(&json!({}), &ctx())
        .await;
    assert!(!list.is_error);
    assert!(
        list.content.contains("background process(es):"),
        "{}",
        list.content
    );
    assert!(list.content.contains("  Command: echo listed"));
}

/// Web UI sessions (D140): tools of one scope can't list, read or stop another scope's
/// processes, nor the process-wide ones.
#[tokio::test]
async fn process_tools_only_see_their_own_scope() {
    let _serial = PROCESS_TESTS.lock().await;
    let ann: std::sync::Arc<str> = std::sync::Arc::from("scope-ann");
    let bob: std::sync::Arc<str> = std::sync::Arc::from("scope-bob");
    let tools = |s: &std::sync::Arc<str>| {
        (
            StartBackgroundProcessTool {
                scope: Some(s.clone()),
            },
            ListProcessesTool {
                scope: Some(s.clone()),
            },
            GetProcessOutputTool {
                scope: Some(s.clone()),
            },
            StopProcessTool {
                scope: Some(s.clone()),
            },
        )
    };
    let (start_a, list_a, _, _) = tools(&ann);
    let (_, list_b, get_b, stop_b) = tools(&bob);
    let r = start_a
        .execute(&json!({"command": slow_command()}), &ctx())
        .await;
    assert!(!r.is_error, "{}", r.content);
    let id = r.content.split('"').nth(1).unwrap().to_string();
    assert!(
        list_a
            .execute(&json!({}), &ctx())
            .await
            .content
            .contains(&id)
    );
    assert!(
        !list_b
            .execute(&json!({}), &ctx())
            .await
            .content
            .contains(&id)
    );
    let unscoped = ListProcessesTool::default()
        .execute(&json!({}), &ctx())
        .await;
    assert!(!unscoped.content.contains(&id));
    let read = get_b.execute(&json!({"process_id": id}), &ctx()).await;
    assert!(read.is_error, "{}", read.content);
    let stop = stop_b.execute(&json!({"process_id": id}), &ctx()).await;
    assert!(stop.is_error, "{}", stop.content);
    let p = process::get_in(&id, Some("scope-ann")).unwrap();
    assert!(!p.has_exited(), "Bob could not stop it");
    assert!(process::get(&id).is_none());
    assert!(process::stop_in(&id, Some("scope-ann")));
}
