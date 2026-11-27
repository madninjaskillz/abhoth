//! Port of tests/OpenOrca.Tools.Tests/PiJobTests.cs: bash background jobs and the job tool
//! (wait on exit / regex / timeout, list, kill, session cleanup), plus bash's timeout handling
//! and the kill-by-name warning.

use std::sync::Arc;
use std::time::{Duration, Instant};

use orca_abstractions::{Tool, ToolContext, ToolErrorCode, ToolResult};
use orca_tools_pi::bash::{KILL_BY_NAME_WARNING, KILL_GUIDELINE, kills_by_name};
use orca_tools_pi::shell::{self, PiShellConfig};
use orca_tools_pi::{PiBashTool, PiJobManager, PiJobTool};
use regex::Regex;
use serde_json::{Value, json};

struct Fixture {
    dir: tempfile::TempDir,
    jobs: Arc<PiJobManager>,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let jobs = Arc::new(PiJobManager::with_log_directory(dir.path().join("logs")));
        Self { dir, jobs }
    }

    fn ctx(&self) -> ToolContext {
        ToolContext::new(self.dir.path())
    }

    fn bash_tool(&self, shell: &PiShellConfig, default_timeout: i32) -> PiBashTool {
        PiBashTool::new(
            Some(shell.clone()),
            None,
            default_timeout,
            Vec::new(),
            self.jobs.clone(),
        )
    }

    fn job_tool(&self) -> PiJobTool {
        PiJobTool::new(self.jobs.clone())
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.jobs.kill_all();
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// The shell, started once before any test uses it. Several tests measure 1-second windows
/// (bash's start-up wait for a job that fails at once, a 1 s timeout that must keep the output
/// printed before it). On Windows CI the first bash starts in this binary were now and then
/// all held up by about a second together (later starts were normal), which made the first
/// batch of those tests fail and pass on a rerun. That one-time stall is not what they test,
/// so it is paid here; every test waits for this one warm-up.
fn bash() -> Option<PiShellConfig> {
    static SHELL: std::sync::OnceLock<Option<PiShellConfig>> = std::sync::OnceLock::new();
    SHELL
        .get_or_init(|| {
            let s = shell::resolve(None)
                .ok()
                .filter(|s| !cfg!(windows) || std::path::Path::new(&s.path).is_file())?;
            let _ = std::process::Command::new(&s.path)
                .args(["-c", "true"])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
            Some(s)
        })
        .clone()
}

async fn run(tool: &dyn Tool, args: Value, ctx: &ToolContext) -> ToolResult {
    let prepared = match tool.prepare_arguments(&args.to_string()) {
        Some(Ok(json)) => serde_json::from_str(&json).unwrap(),
        Some(Err(e)) => return ToolResult::error(e),
        None => args,
    };
    tool.execute(&prepared, ctx).await
}

fn job_id(started: &ToolResult) -> String {
    let re = Regex::new(r"^Started job (\d+) \(pid (\d+)\)").unwrap();
    let m = re
        .captures(&started.content)
        .unwrap_or_else(|| panic!("{}", started.content));
    m[1].to_string()
}

async fn wait_until(condition: impl Fn() -> bool, ms: u64) {
    let sw = Instant::now();
    while !condition() && sw.elapsed() < Duration::from_millis(ms) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[cfg(windows)]
fn process_exists(pid: u32) -> bool {
    let out = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).contains(&format!("\"{pid}\""))
}

#[cfg(unix)]
fn process_exists(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

#[tokio::test(flavor = "multi_thread")]
async fn background_returns_a_job_id_and_wait_returns_the_exit_code_and_output() {
    let Some(shell) = bash() else { return }; // no bash available
    let f = Fixture::new();
    let ctx = f.ctx();
    let started = run(
        &f.bash_tool(&shell, 0),
        json!({"command": "echo one; sleep 2; echo two; exit 3", "background": true}),
        &ctx,
    )
    .await;
    assert!(!started.is_error, "{}", started.content);
    let id = job_id(&started);
    assert!(started.content.contains("job wait"));

    let sw = Instant::now();
    let waited = run(&f.job_tool(), json!({"action": "wait", "id": id}), &ctx).await;
    assert!(sw.elapsed() < Duration::from_secs(30));
    assert!(waited.is_error, "{}", waited.content); // non-zero exit, like bash
    let text = waited.content.replace('\r', "");
    assert!(
        text.starts_with(&format!("Job {id} exited with code 3 after")),
        "{text}"
    );
    assert!(text.contains("one\ntwo"), "{text}");

    // The log file holds everything.
    let job = f.jobs.get(Some(&id)).unwrap();
    assert!(
        std::fs::read_to_string(&job.log_path)
            .unwrap()
            .contains("two")
    );
    assert_eq!("exited 3", job.status());
}

#[tokio::test(flavor = "multi_thread")]
async fn background_that_fails_at_once_reports_the_exit_right_away() {
    let Some(shell) = bash() else { return }; // no bash available
    let f = Fixture::new();
    let result = run(
        &f.bash_tool(&shell, 0),
        json!({"command": "echo nope; exit 4", "background": true}),
        &f.ctx(),
    )
    .await;
    assert!(result.is_error);
    assert!(
        result.content.contains("already exited with code 4"),
        "{}",
        result.content
    );
    assert!(result.content.contains("nope"));
}

#[tokio::test(flavor = "multi_thread")]
async fn wait_until_returns_the_matched_line_and_only_new_lines_count_next_time() {
    let Some(shell) = bash() else { return }; // no bash available
    let f = Fixture::new();
    let ctx = f.ctx();
    let id = job_id(
        &run(
            &f.bash_tool(&shell, 0),
            json!({"command": "echo booting; sleep 1; echo 'Listening on 5000'; sleep 60", "background": true}),
            &ctx,
        )
        .await,
    );

    let matched = run(
        &f.job_tool(),
        json!({"action": "wait", "id": id, "until": "Listening on \\d+", "timeout": 30}),
        &ctx,
    )
    .await;
    assert!(!matched.is_error, "{}", matched.content);
    let text = matched.content.replace('\r', "");
    assert!(
        text.starts_with(&format!("Job {id} matched at line 2 after")),
        "{text}"
    );
    assert!(
        text.contains(": Listening on 5000\nIt is still running."),
        "{text}"
    );

    // The same line does not match a second time: this wait times out.
    let again = run(
        &f.job_tool(),
        json!({"action": "wait", "id": id, "until": "Listening", "timeout": 1}),
        &ctx,
    )
    .await;
    assert!(!again.is_error, "{}", again.content);
    assert!(
        again.content.contains("still running after waiting 1s"),
        "{}",
        again.content
    );
    assert!(again.content.contains("no new line matched"));
}

#[tokio::test(flavor = "multi_thread")]
async fn wait_timeout_returns_with_the_job_still_running() {
    let Some(shell) = bash() else { return }; // no bash available
    let f = Fixture::new();
    let ctx = f.ctx();
    let id = job_id(
        &run(
            &f.bash_tool(&shell, 0),
            json!({"command": "echo hi; sleep 60", "background": true}),
            &ctx,
        )
        .await,
    );
    let sw = Instant::now();
    let result = run(
        &f.job_tool(),
        json!({"action": "wait", "id": id, "timeout": 1.5}),
        &ctx,
    )
    .await;
    let secs = sw.elapsed().as_secs_f64();
    assert!((1.0..=15.0).contains(&secs), "{secs}");
    assert!(!result.is_error, "{}", result.content);
    assert!(
        result
            .content
            .starts_with(&format!("Job {id} is still running after waiting")),
        "{}",
        result.content
    );
    assert!(result.content.contains("hi"));
    assert!(!f.jobs.get(Some(&id)).unwrap().has_exited());
}

// Git Bash's `sleep 120 &` runs in a process whose Windows parent (a Cygwin fork stub) has
// already exited, so no tree kill (taskkill /T here, Process.Kill(entireProcessTree) in C#)
// reaches it; as in C#, only the shell itself is checked. That orphan keeps the job's pipes
// open, so this test's runtime is shut down in the background rather than waiting on it.
#[test]
fn kill_stops_the_jobs_process_tree_and_list_shows_it() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(kill_stops_the_jobs_process_tree_and_list_shows_it_body());
    rt.shutdown_background();
}

async fn kill_stops_the_jobs_process_tree_and_list_shows_it_body() {
    let Some(shell) = bash() else { return }; // no bash available
    let f = Fixture::new();
    let ctx = f.ctx();
    // A child process that would outlive a plain kill of the shell.
    let id = job_id(
        &run(
            &f.bash_tool(&shell, 0),
            json!({"command": "sleep 120 & sleep 120; wait", "background": true}),
            &ctx,
        )
        .await,
    );
    let job = f.jobs.get(Some(&id)).unwrap();

    let list = run(&f.job_tool(), json!({"action": "list"}), &ctx).await;
    assert!(
        list.content
            .contains(&format!("{id}  pid {}  running", job.pid)),
        "{}",
        list.content
    );

    let killed = run(&f.job_tool(), json!({"action": "kill", "id": id}), &ctx).await;
    assert!(!killed.is_error, "{}", killed.content);
    assert!(
        killed.content.starts_with(&format!(
            "Killed job {id} (pid {}) and its child processes.",
            job.pid
        )),
        "{}",
        killed.content
    );
    wait_until(|| job.has_exited(), 10_000).await;
    assert!(job.has_exited());
    assert_eq!("killed", job.status());
    assert!(!process_exists(job.pid)); // gone

    // A wait on a killed job returns at once and says so; a second kill is a no-op.
    let waited = run(&f.job_tool(), json!({"action": "wait", "id": id}), &ctx).await;
    assert!(
        waited
            .content
            .starts_with(&format!("Job {id} was killed after")),
        "{}",
        waited.content
    );
    let again = run(
        &f.job_tool(),
        json!({"action": "kill", "id": format!("job {id}")}),
        &ctx,
    )
    .await;
    assert!(
        again.content.contains("had already killed"),
        "{}",
        again.content
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn kill_all_at_session_end_stops_every_running_job() {
    let Some(shell) = bash() else { return }; // no bash available
    let f = Fixture::new();
    let dir = f.dir.path();
    let a = f.jobs.start(&shell, "sleep 120", dir, &[]).await.unwrap();
    let b = f.jobs.start(&shell, "sleep 120", dir, &[]).await.unwrap();
    let done = f.jobs.start(&shell, "true", dir, &[]).await.unwrap();
    wait_until(|| done.has_exited(), 10_000).await;

    f.jobs.kill_all();
    wait_until(|| a.has_exited() && b.has_exited(), 10_000).await;
    assert!(a.has_exited() && b.has_exited());
    assert_eq!("killed", a.status());
    assert_eq!("exited 0", done.status());
}

#[tokio::test(flavor = "multi_thread")]
async fn job_tool_rejects_bad_input() {
    let f = Fixture::new();
    let ctx = f.ctx();
    let tool = f.job_tool();
    assert!(
        run(&tool, json!({"action": "explode"}), &ctx)
            .await
            .is_error
    );
    let missing = run(&tool, json!({"action": "wait", "id": "99"}), &ctx).await;
    assert!(missing.is_error);
    assert!(
        missing.content.contains("No job \"99\""),
        "{}",
        missing.content
    );
    assert_eq!(
        "No background jobs.",
        run(&tool, json!({"action": "list"}), &ctx).await.content
    );

    let Some(shell) = bash() else { return }; // no bash available
    let id = job_id(
        &run(
            &f.bash_tool(&shell, 0),
            json!({"command": "sleep 30", "background": true}),
            &ctx,
        )
        .await,
    );
    let bad_regex = run(
        &tool,
        json!({"action": "wait", "id": id, "until": "("}),
        &ctx,
    )
    .await;
    assert!(bad_regex.is_error);
    assert!(
        bad_regex.content.starts_with("Invalid until regex"),
        "{}",
        bad_regex.content
    );
}

// ---- bash timeouts ----------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn bash_default_timeout_kills_the_command_and_keeps_its_output() {
    let Some(shell) = bash() else { return }; // no bash available
    let f = Fixture::new();
    let result = run(
        &f.bash_tool(&shell, 1),
        json!({"command": "echo partial; sleep 30"}),
        &f.ctx(),
    )
    .await;
    assert!(result.is_error);
    assert_eq!(ToolErrorCode::Timeout, result.error_code);
    let text = result.content.replace('\r', "");
    assert!(text.starts_with("partial\n"), "{text}");
    assert!(
        text.contains("timed out after 1 seconds and was killed"),
        "{text}"
    );
    assert!(text.contains("background: true"));
}

#[tokio::test(flavor = "multi_thread")]
async fn bash_timeout_above_the_maximum_runs_at_the_cap() {
    let Some(shell) = bash() else { return }; // no bash available
    let f = Fixture::new();
    let result = run(
        &f.bash_tool(&shell, 0),
        json!({"command": "echo ok", "timeout": 5000}),
        &f.ctx(),
    )
    .await;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!("ok\n", result.content.replace('\r', ""));
}

#[test]
fn bash_description_and_schema_state_the_timeouts_and_background() {
    let jobs = Arc::new(PiJobManager::new());
    let tool = PiBashTool::new(None, None, 300, Vec::new(), jobs.clone());
    assert!(
        tool.description()
            .contains("Timeout: 300 seconds unless you pass one (max 600).")
    );
    assert!(tool.description().contains("never sleep to wait"));
    let schema = tool.parameter_schema();
    let props = &schema["properties"];
    assert_eq!(
        "Timeout in seconds (default 300, max 600)",
        props["timeout"]["description"].as_str().unwrap()
    );
    assert_eq!("boolean", props["background"]["type"].as_str().unwrap());

    let none = PiBashTool::new(None, None, 0, Vec::new(), jobs.clone());
    assert!(
        none.description()
            .contains("No timeout unless you pass one (max 600 seconds).")
    );
    assert_eq!(
        "Timeout in seconds (max 600)",
        none.parameter_schema()["properties"]["timeout"]["description"]
            .as_str()
            .unwrap()
    );
    assert!(
        PiBashTool::new(None, None, 1200, Vec::new(), jobs)
            .description()
            .contains("Timeout: 600 seconds")
    );
}

// ---- kill by name -----------------------------------------------------------------------------

#[test]
fn kills_by_name_flags_name_based_kills() {
    let cases: [(&str, bool); 14] = [
        ("taskkill //IM EzmuzeStudio.Desktop.DX.exe //F", true),
        ("taskkill /F /IM app.exe", true),
        ("TASKKILL /im app.exe", true),
        ("pkill -f dotnet", true),
        ("sleep 1; killall node", true),
        ("powershell -c \"Stop-Process -Name app -Force\"", true),
        ("Get-Process app | Stop-Process", true),
        ("wmic process where name='app.exe' delete", true),
        ("taskkill //PID 1234 //T //F", false),
        ("kill 1234", false),
        ("Stop-Process -Id 1234", false),
        ("Get-Process -Id 1234 | Stop-Process", false),
        ("grep -r pkill_helper src", false),
        ("echo skillall", false),
    ];
    for (command, expected) in cases {
        assert_eq!(expected, kills_by_name(command), "{command}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn bash_kill_by_name_still_runs_but_warns() {
    let Some(shell) = bash() else { return }; // no bash available
    let f = Fixture::new();
    let ctx = f.ctx();
    let result = run(
        &f.bash_tool(&shell, 0),
        json!({"command": "echo ran || pkill no-such-process-xyz"}),
        &ctx,
    )
    .await;
    assert!(!result.is_error, "{}", result.content);
    assert!(result.content.starts_with("ran"));
    assert!(
        result.content.ends_with(KILL_BY_NAME_WARNING),
        "{}",
        result.content
    );

    let clean = run(
        &f.bash_tool(&shell, 0),
        json!({"command": "echo ran"}),
        &ctx,
    )
    .await;
    assert!(!clean.content.contains("Warning"));
}

#[test]
fn toolset_advertises_job_and_prompt_carries_the_kill_rule() {
    assert_eq!(
        vec!["read", "bash", "edit", "write", "job"],
        orca_tools_pi::advertised_names(false, true)
    );
    assert_eq!(
        vec!["read", "bash", "edit", "write"],
        orca_tools_pi::advertised_names(false, false)
    );
    let jobs = Arc::new(PiJobManager::new());
    let with = orca_tools_pi::create_toolset(&orca_tools_pi::PiToolsetOptions {
        job_manager: Some(jobs.clone()),
        ..Default::default()
    });
    assert!(with.iter().any(|t| t.name() == "job"));
    let without = orca_tools_pi::create_toolset(&orca_tools_pi::PiToolsetOptions {
        jobs: false,
        job_manager: Some(jobs),
        ..Default::default()
    });
    assert!(!without.iter().any(|t| t.name() == "job"));

    let names: Vec<String> = orca_tools_pi::advertised_names(false, true)
        .into_iter()
        .map(String::from)
        .collect();
    let prompt = orca_tools_pi::system_prompt::build(
        &names,
        &orca_tools_pi::prompt_snippets(),
        &orca_tools_pi::prompt_guidelines(),
        &[],
        "/w",
        None,
        false,
    );
    assert!(
        prompt.contains("- job: Wait on, list or kill background jobs"),
        "{prompt}"
    );
    assert!(prompt.contains(&format!("- {KILL_GUIDELINE}")));
}
