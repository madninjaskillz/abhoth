//! `/benchmark [count=N] [models=all|name1,name2] [mode=pair]` (`CommandHandler.HandleBenchmarkAsync`)
//! and `BenchmarkRunner`: per model, a scripted three-prompt Python task run by the agent loop in
//! a fresh temp folder, then checked by running the script (`python main.py` must write the
//! primes up to 100 to `output.txt`). Pair mode runs every ordered model pair once.
//!
//! The runs use the REPL's own session (as C# uses its agent loop): the conversation, working
//! folder, model list and pair switch are saved before and put back afterwards, also when the
//! benchmark is cancelled with Ctrl+C.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use orca_core::agent::AgentSession;
use orca_core::agent::system_prompt::SystemPromptBuilder;

use super::CommandContext;
use crate::repl::observer::TurnObserver;
use crate::repl::state::ReplState;
use crate::ui::Console;
use crate::ui::markup::escape;
use crate::ui::widgets::{Table, write_rule};

const PYTHON_TIMEOUT_SECONDS: u64 = 30;
const MODEL_TIMEOUT_SECONDS: u64 = 90;

const EXPECTED_PRIMES: [i32; 25] = [
    2, 3, 5, 7, 11, 13, 17, 19, 23, 29, 31, 37, 41, 43, 47, 53, 59, 61, 67, 71, 73, 79, 83, 89, 97,
];

pub const BENCHMARK_PROMPTS: [&str; 3] = [
    "Use the write_file tool to create a file called main.py in the current directory. It should be a minimal Python script with a main function and an `if __name__ == '__main__'` block that prints 'hello'.",
    "Update main.py to calculate all prime numbers up to 50 and write them to a file called 'output.txt', one number per line.",
    "Update main.py so it produces all prime numbers up to 100 instead of 50.",
];

/// What `/benchmark` was asked to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BenchmarkArgs {
    pub runs_per_model: i32,
    /// `None` = the current model only; `Some(empty)` = all models.
    pub model_filter: Option<Vec<String>>,
    pub mode_pair: bool,
}

fn strip_prefix_ci<'a>(arg: &'a str, prefix: &str) -> Option<&'a str> {
    arg.get(..prefix.len())
        .filter(|p| p.eq_ignore_ascii_case(prefix))
        .map(|_| &arg[prefix.len()..])
}

/// Parse the arguments; `Err` holds the markup lines to print.
pub fn parse_args(args: &[String]) -> Result<BenchmarkArgs, Vec<String>> {
    let mut parsed = BenchmarkArgs {
        runs_per_model: 3,
        model_filter: None,
        mode_pair: false,
    };
    for arg in args {
        if let Some(value) = strip_prefix_ci(arg, "count=") {
            match value.trim().parse::<i32>() {
                Ok(n) if n >= 1 => parsed.runs_per_model = n,
                _ => {
                    return Err(vec![
                        "[red]Invalid count — must be a positive integer (e.g. count=5).[/]".into(),
                    ]);
                }
            }
        } else if let Some(value) = strip_prefix_ci(arg, "models=") {
            if value.eq_ignore_ascii_case("all") {
                parsed.model_filter = Some(Vec::new());
            } else {
                let models: Vec<String> = value
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect();
                if models.is_empty() {
                    return Err(vec![
                        "[red]Invalid models — provide comma-separated names or 'all' (e.g. models=all).[/]".into(),
                    ]);
                }
                parsed.model_filter = Some(models);
            }
        } else if let Some(value) = strip_prefix_ci(arg, "mode=") {
            if value.eq_ignore_ascii_case("pair") {
                parsed.mode_pair = true;
            } else {
                return Err(vec![format!(
                    "[red]Unknown mode: {}. Supported: pair[/]",
                    escape(value)
                )]);
            }
        } else {
            return Err(vec![
                format!("[red]Unknown argument: {}[/]", escape(arg)),
                "[grey]Usage: /benchmark [[count=N]] [[models=all|name1,name2]] [[mode=pair]][/]"
                    .into(),
            ]);
        }
    }
    // Pair mode defaults to all models if none specified.
    if parsed.mode_pair && parsed.model_filter.is_none() {
        parsed.model_filter = Some(Vec::new());
    }
    Ok(parsed)
}

/// `BenchmarkRunResult`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RunResult {
    pub llm_time_ms: i64,
    pub python_exec_time_ms: i64,
    pub output_correct: Option<bool>,
    pub notes: Option<String>,
}

/// `PairBenchmarkRunResult`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PairRunResult {
    pub primary_model: String,
    pub secondary_model: String,
    pub run: RunResult,
    pub pair_primary_wins: i32,
    pub pair_alternate_wins: i32,
    pub pair_disagreements: i32,
    pub pair_backup_failures: i32,
}

/// `BenchmarkResult`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelResult {
    pub model_name: String,
    pub runs: Vec<RunResult>,
}

/// `/benchmark`: check the arguments, then run (Ctrl+C cancels).
pub async fn handle(ctx: &mut CommandContext<'_>, args: &[String]) {
    let console = ctx.console.clone();
    let Some(parsed) = parse_or_report(&console, args) else {
        return;
    };
    let snapshot = Snapshot::capture(ctx.session);
    let state = ctx.state.clone();
    let outcome = {
        let run = run(&console, ctx.session, &state, &parsed);
        tokio::select! {
            r = run => Some(r),
            _ = tokio::signal::ctrl_c() => None,
        }
    };
    snapshot.restore(ctx.session);
    match outcome {
        None => console.markup_line("[yellow]Benchmark cancelled.[/]"),
        Some(Err(e)) => {
            tracing::error!("Benchmark failed: {e}");
            console.markup_line(&format!("[red]Benchmark failed: {}[/]", escape(&e)));
        }
        Some(Ok(())) => {}
    }
}

fn parse_or_report(console: &Console, args: &[String]) -> Option<BenchmarkArgs> {
    match parse_args(args) {
        Err(lines) => {
            for l in lines {
                console.markup_line(&l);
            }
            None
        }
        Ok(parsed) => Some(parsed),
    }
}

/// `RunAsync` / `RunPairBenchmarkAsync`.
pub async fn run(
    console: &Console,
    session: &mut AgentSession,
    state: &ReplState,
    args: &BenchmarkArgs,
) -> Result<(), String> {
    if args.mode_pair {
        let filter = args.model_filter.clone().unwrap_or_default();
        run_pair(console, session, state, &filter).await;
    } else {
        run_models(
            console,
            session,
            state,
            args.runs_per_model,
            args.model_filter.as_deref(),
        )
        .await;
    }
    Ok(())
}

/// What the benchmark changes on the session (`CaptureConfigSnapshot`, plus the session state
/// the Rust runs use in place of the process's current folder and a new conversation).
struct Snapshot {
    models: Vec<String>,
    pair_mode: bool,
    cwd: PathBuf,
    messages: Vec<orca_core::client::ChatMessage>,
    default_model: Option<String>,
    model_index: usize,
    prompt_builder: Arc<SystemPromptBuilder>,
}

impl Snapshot {
    fn capture(session: &AgentSession) -> Self {
        Self {
            models: session.config.lm_studio.models.clone(),
            pair_mode: session.config.lm_studio.pair_model_programming,
            cwd: session.ctx.cwd.clone(),
            messages: session.messages.clone(),
            default_model: session.client.settings().default_model.clone(),
            model_index: session.model_index,
            prompt_builder: session.prompt_builder.clone(),
        }
    }

    fn restore(self, session: &mut AgentSession) {
        session.config.lm_studio.models = self.models;
        session.config.lm_studio.pair_model_programming = self.pair_mode;
        session.ctx.cwd = self.cwd;
        session.messages = self.messages;
        session.client.settings_mut().default_model = self.default_model;
        session.model_index = self.model_index;
        session.prompt_builder = self.prompt_builder;
    }
}

async fn run_models(
    console: &Console,
    session: &mut AgentSession,
    state: &ReplState,
    runs_per_model: i32,
    model_filter: Option<&[String]>,
) {
    console.markup_line(&format!(
        "[bold cyan]Starting model benchmark ({runs_per_model} run(s) per model)...[/]"
    ));
    console.blank();
    let Some(models) = discover_and_filter(console, &session.config, model_filter, 1).await else {
        return;
    };
    console.blank();
    let snapshot = Snapshot::capture(session);
    let mut results = Vec::new();
    for model in &models {
        let mut model_result = ModelResult {
            model_name: model.clone(),
            runs: Vec::new(),
        };
        for run in 1..=runs_per_model {
            write_rule(
                console,
                &format!(
                    "[bold yellow]{} — Run {run}/{runs_per_model}[/]",
                    escape(model)
                ),
                true,
            );
            let r = single_run(console, session, state, model, run).await;
            model_result.runs.push(r);
        }
        results.push(model_result);
    }
    snapshot.restore(session);
    render_results_table(console, &results);
}

async fn run_pair(
    console: &Console,
    session: &mut AgentSession,
    state: &ReplState,
    model_filter: &[String],
) {
    console
        .markup_line("[bold cyan]Starting pair benchmark (every ordered model combination)...[/]");
    console.blank();
    let Some(models) = discover_and_filter(console, &session.config, Some(model_filter), 2).await
    else {
        return;
    };
    let pairs = generate_ordered_pairs(&models);
    console.markup_line(&format!(
        "[grey]Testing {} ordered pair(s) from {} model(s)[/]",
        pairs.len(),
        models.len()
    ));
    console.blank();
    let snapshot = Snapshot::capture(session);
    let mut results = Vec::new();
    for (i, (primary, backup)) in pairs.iter().enumerate() {
        write_rule(
            console,
            &format!(
                "[bold yellow]Pair {}/{}: {} + {}[/]",
                i + 1,
                pairs.len(),
                escape(primary),
                escape(backup)
            ),
            true,
        );
        session.config.lm_studio.models = vec![primary.clone(), backup.clone()];
        session.config.lm_studio.pair_model_programming = true;
        session.pair_stats = Default::default();
        let run = single_run(console, session, state, primary, 1).await;
        let stats = &session.pair_stats;
        results.push(PairRunResult {
            primary_model: primary.clone(),
            secondary_model: backup.clone(),
            pair_primary_wins: stats.primary_wins,
            pair_alternate_wins: stats.backup_wins,
            pair_disagreements: stats.disagreements,
            pair_backup_failures: stats.backup_failures,
            run,
        });
    }
    snapshot.restore(session);
    render_pair_results_table(console, &results);
}

/// `BenchmarkSingleRunAsync`: one run in its own temp folder, cut off after 90 s.
async fn single_run(
    console: &Console,
    session: &mut AgentSession,
    state: &ReplState,
    model: &str,
    run_number: i32,
) -> RunResult {
    let timestamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let temp_dir = std::env::temp_dir().join(format!(
        "orca-benchmark-{}-run{run_number}-{timestamp}",
        sanitize_model_name(model)
    ));
    let _ = std::fs::create_dir_all(&temp_dir);
    let original_cwd = session.ctx.cwd.clone();
    let mut result = RunResult::default();
    let work = run_core(console, session, state, model, &temp_dir, &mut result);
    if tokio::time::timeout(Duration::from_secs(MODEL_TIMEOUT_SECONDS), work)
        .await
        .is_err()
    {
        // Hard timeout: the turn is dropped (the agent loop may be stuck) and the run moves on.
        session.close_dangling_tool_calls();
        result.output_correct = Some(false);
        result.notes = Some(format!("Timed out after {MODEL_TIMEOUT_SECONDS}s"));
        console.markup_line(&format!(
            "\n[red]Run timed out after {MODEL_TIMEOUT_SECONDS}s — skipping.[/]"
        ));
    }
    session.ctx.cwd = original_cwd;
    // Best effort: file locks or permissions may prevent cleanup.
    let _ = std::fs::remove_dir_all(&temp_dir);
    result
}

/// `RunBenchmarkCoreAsync`.
async fn run_core(
    console: &Console,
    session: &mut AgentSession,
    state: &ReplState,
    model: &str,
    temp_dir: &Path,
    result: &mut RunResult,
) {
    session.ctx.cwd = temp_dir.to_path_buf();
    session.prompt_builder = Arc::new(SystemPromptBuilder::with_defaults(
        &session.config,
        temp_dir,
    ));
    session.config.lm_studio.models = vec![model.to_string()];
    session.client.settings_mut().default_model = Some(model.to_string());
    session.model_index = 0;

    let llm = Instant::now();
    session.messages.clear();
    let prompt = session.default_system_prompt();
    session.set_system_prompt(prompt);
    for (i, prompt) in BENCHMARK_PROMPTS.iter().enumerate() {
        console.markup_line(&format!(
            "  [grey]Prompt {}/{}[/]",
            i + 1,
            BENCHMARK_PROMPTS.len()
        ));
        let before = session.tool_call_history.len();
        let started = Instant::now();
        // Model text is not shown, tool call summaries are.
        let mut observer =
            TurnObserver::new(console.clone(), state.clone(), session.registry.all()).suppressed();
        let outcome = session.run_turn(prompt, &mut observer).await;
        observer.finish();
        if let Err(e) = outcome {
            crate::repl::render_loop_error(console, &e, &session.config.lm_studio.base_url);
        }
        let calls = &session.tool_call_history[before.min(session.tool_call_history.len())..];
        let errors = calls.iter().filter(|c| c.is_error).count();
        let errors = if errors > 0 {
            format!(", [red]{errors} error(s)[/]")
        } else {
            String::new()
        };
        console.markup_line(&format!(
            "  [dim]({:.1}s, {} tool call(s){errors})[/]",
            started.elapsed().as_secs_f64(),
            calls.len()
        ));
    }
    result.llm_time_ms = llm.elapsed().as_millis() as i64;

    let Some(entry) = find_python_entry_point(temp_dir) else {
        result.notes = Some("No Python entry point found".into());
        console.markup_line("[red]No Python entry point found.[/]");
        return;
    };
    let file_name = entry
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    console.markup_line(&format!("[grey]Running: python {}[/]", escape(&file_name)));
    let (exit_code, python_ms, python_error) = match run_python(&entry).await {
        Ok(r) => r,
        Err(e) => {
            result.notes = Some(format!("Error: {e}"));
            tracing::error!("Benchmark error for model {model}: {e}");
            console.markup_line(&format!("[red]Error: {}[/]", escape(&e.to_string())));
            return;
        }
    };
    result.python_exec_time_ms = python_ms;
    if exit_code != 0 {
        result.output_correct = Some(false);
        result.notes = Some(format!(
            "Python exited with code {exit_code}: {python_error}"
        ));
        console.markup_line(&format!(
            "[red]Python failed (exit {exit_code}): {}[/]",
            escape(&python_error)
        ));
        return;
    }
    let output_path = temp_dir.join("output.txt");
    if !output_path.is_file() {
        result.output_correct = Some(false);
        result.notes = Some("output.txt not found".into());
        console.markup_line("[red]output.txt not found.[/]");
        return;
    }
    let content = match std::fs::read(&output_path) {
        Ok(bytes) => {
            let text = String::from_utf8_lossy(&bytes).to_string();
            text.strip_prefix('\u{feff}')
                .map(str::to_string)
                .unwrap_or(text)
        }
        Err(e) => {
            result.notes = Some(format!("Error: {e}"));
            console.markup_line(&format!("[red]Error: {}[/]", escape(&e.to_string())));
            return;
        }
    };
    let (correct, notes) = validate_output(&content);
    result.output_correct = Some(correct);
    let (color, text) = if correct {
        ("green", "PASS")
    } else {
        ("red", "FAIL")
    };
    console.markup_line(&format!("[{color}]{text}[/] — {}", escape(&notes)));
    result.notes = Some(notes);
}

/// `RunPythonAsync`: exit code, elapsed ms and the first 200 characters of stderr.
async fn run_python(script: &Path) -> std::io::Result<(i32, i64, String)> {
    let python = if cfg!(windows) { "python" } else { "python3" };
    let dir = script.parent().unwrap_or(Path::new("."));
    let started = Instant::now();
    let child = tokio::process::Command::new(python)
        .arg(script)
        .current_dir(dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    match tokio::time::timeout(
        Duration::from_secs(PYTHON_TIMEOUT_SECONDS),
        child.wait_with_output(),
    )
    .await
    {
        Ok(out) => {
            let out = out?;
            let stderr = String::from_utf8_lossy(&out.stderr).to_string();
            let stderr: String = if stderr.chars().count() > 200 {
                stderr.chars().take(200).collect()
            } else {
                stderr
            };
            Ok((
                out.status.code().unwrap_or(-1),
                started.elapsed().as_millis() as i64,
                stderr,
            ))
        }
        // The child is killed when its future is dropped (kill_on_drop).
        Err(_) => Ok((
            -1,
            started.elapsed().as_millis() as i64,
            format!("Timed out after {PYTHON_TIMEOUT_SECONDS}s"),
        )),
    }
}

/// `FindPythonEntryPoint`: main.py, else the only .py file that is not a test or setup file.
pub fn find_python_entry_point(directory: &Path) -> Option<PathBuf> {
    let main = directory.join("main.py");
    if main.is_file() {
        return Some(main);
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(directory)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .filter(|p| {
            p.extension()
                .is_some_and(|e| e.to_string_lossy().eq_ignore_ascii_case("py"))
        })
        .filter(|p| {
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            name != "test.py"
                && !name.starts_with("test_")
                && !name.ends_with("_test.py")
                && name != "setup.py"
                && name != "conftest.py"
        })
        .collect();
    if files.len() == 1 { files.pop() } else { None }
}

/// `ValidateOutput`: the numbers in `output.txt` must be exactly the primes up to 100.
pub fn validate_output(content: &str) -> (bool, String) {
    if content.trim().is_empty() {
        return (false, "output.txt is empty".into());
    }
    let numbers: Vec<i32> = content
        .split('\n')
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter_map(|l| l.parse::<i32>().ok())
        .collect();
    if numbers.is_empty() {
        return (false, "No numbers found in output".into());
    }
    // Enumerable.Except: distinct, in first-seen order.
    let mut missing: Vec<i32> = Vec::new();
    for p in EXPECTED_PRIMES {
        if !numbers.contains(&p) && !missing.contains(&p) {
            missing.push(p);
        }
    }
    let mut extra: Vec<i32> = Vec::new();
    for n in &numbers {
        if !EXPECTED_PRIMES.contains(n) && !extra.contains(n) {
            extra.push(*n);
        }
    }
    if missing.is_empty() && extra.is_empty() {
        return (
            true,
            format!("All {} primes correct", EXPECTED_PRIMES.len()),
        );
    }
    let join = |v: &[i32]| v.iter().map(i32::to_string).collect::<Vec<_>>().join(", ");
    let mut notes = Vec::new();
    if !missing.is_empty() {
        notes.push(format!("Missing: {}", join(&missing)));
    }
    if !extra.is_empty() {
        notes.push(format!("Extra: {}", join(&extra)));
    }
    (false, notes.join("; "))
}

/// `SanitizeModelName`: lower case, letters/digits/`-` kept, everything else `_`, 50 chars max.
pub fn sanitize_model_name(model: &str) -> String {
    model
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(50)
        .collect()
}

/// `GenerateOrderedPairs`: every (primary, backup) with primary != backup.
pub fn generate_ordered_pairs(models: &[String]) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    for (i, a) in models.iter().enumerate() {
        for (j, b) in models.iter().enumerate() {
            if i != j {
                pairs.push((a.clone(), b.clone()));
            }
        }
    }
    pairs
}

/// `DiscoverAndFilterModelsAsync`: `None` when the requirements aren't met (already said why).
async fn discover_and_filter(
    console: &Console,
    config: &orca_core::config::OrcaConfig,
    model_filter: Option<&[String]>,
    min_required: usize,
) -> Option<Vec<String>> {
    let Some(filter) = model_filter else {
        // The current model only.
        let current = config.lm_studio.primary_model().unwrap_or_default();
        if current.trim().is_empty() {
            console.markup_line(
                "[red]No model configured. Set a model with /model or use models=all.[/]",
            );
            return None;
        }
        console.markup_line(&format!(
            "[grey]Benchmarking current model: {}[/]",
            escape(current)
        ));
        return Some(vec![current.to_string()]);
    };
    crate::model_discovery::invalidate_cache();
    let available = crate::model_discovery::available_models(config).await;
    select_models(console, &available, filter, min_required)
}

/// The filtering half of `DiscoverAndFilterModelsAsync`.
fn select_models(
    console: &Console,
    available: &[String],
    filter: &[String],
    min_required: usize,
) -> Option<Vec<String>> {
    if available.len() < min_required {
        console.markup_line(&if min_required > 1 {
            format!(
                "[red]Requires at least {min_required} models. Found: {}[/]",
                available.len()
            )
        } else {
            "[red]No models found on the server. Is the LLM server running with models loaded?[/]"
                .to_string()
        });
        return None;
    }
    if filter.is_empty() {
        console.markup_line(&format!(
            "[grey]Benchmarking {} model(s): {}[/]",
            available.len(),
            escape(&available.join(", "))
        ));
        return Some(available.to_vec());
    }
    let mut models = Vec::new();
    for requested in filter {
        match available
            .iter()
            .find(|m| m.to_lowercase() == requested.to_lowercase())
        {
            Some(m) => models.push(m.clone()),
            None => console.markup_line(&format!(
                "[yellow]Model not found: {} — skipping[/]",
                escape(requested)
            )),
        }
    }
    if models.len() < min_required {
        console.markup_line(if min_required > 1 {
            "[red]Not enough matched models for this benchmark mode.[/]"
        } else {
            "[red]None of the requested models are available.[/]"
        });
        console.markup_line(&format!(
            "[grey]Available: {}[/]",
            escape(&available.join(", "))
        ));
        return None;
    }
    console.markup_line(&format!(
        "[grey]Benchmarking {} model(s): {}[/]",
        models.len(),
        escape(&models.join(", "))
    ));
    Some(models)
}

fn llm_time(ms: i64) -> String {
    if ms > 0 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        "-".into()
    }
}

fn py_time(ms: i64) -> String {
    if ms > 0 {
        format!("{ms}ms")
    } else {
        "-".into()
    }
}

fn outcome(correct: Option<bool>) -> &'static str {
    match correct {
        Some(true) => "[green]PASS[/]",
        Some(false) => "[red]FAIL[/]",
        None => "[grey]N/A[/]",
    }
}

/// `RenderResultsTable`.
pub fn render_results_table(console: &Console, results: &[ModelResult]) {
    console.blank();
    write_rule(console, "[bold cyan]Benchmark Results[/]", false);
    let mut table = Table::new()
        .expand()
        .column("[bold]Model[/]")
        .column("[bold]Run[/]")
        .column("[bold]LLM Time[/]")
        .column("[bold]Python Time[/]")
        .column("[bold]Result[/]")
        .column("[bold]Notes[/]");
    for model in results {
        for (i, r) in model.runs.iter().enumerate() {
            let model_col = if i == 0 {
                escape(&model.model_name)
            } else {
                String::new()
            };
            table.row(&[
                model_col,
                format!("#{}", i + 1),
                llm_time(r.llm_time_ms),
                py_time(r.python_exec_time_ms),
                outcome(r.output_correct).to_string(),
                escape(r.notes.as_deref().unwrap_or_default()),
            ]);
        }
        // Average row.
        let completed: Vec<&RunResult> = model.runs.iter().filter(|r| r.llm_time_ms > 0).collect();
        let passes = model
            .runs
            .iter()
            .filter(|r| r.output_correct == Some(true))
            .count();
        let avg_llm = if completed.is_empty() {
            "-".to_string()
        } else {
            let avg = completed.iter().map(|r| r.llm_time_ms as f64).sum::<f64>()
                / completed.len() as f64;
            format!("[bold]{:.1}s[/]", avg / 1000.0)
        };
        let py: Vec<f64> = completed
            .iter()
            .filter(|r| r.python_exec_time_ms > 0)
            .map(|r| r.python_exec_time_ms as f64)
            .collect();
        let avg_py = if py.is_empty() {
            "-".to_string()
        } else {
            format!("[bold]{:.0}ms[/]", py.iter().sum::<f64>() / py.len() as f64)
        };
        table.row(&[
            String::new(),
            "[bold]Avg[/]".into(),
            avg_llm,
            avg_py,
            format!("[bold]{passes}/{}[/]", model.runs.len()),
            String::new(),
        ]);
        table.row(&[""; 6]);
    }
    table.write(console);
}

/// `RenderPairResultsTable`.
pub fn render_pair_results_table(console: &Console, results: &[PairRunResult]) {
    console.blank();
    write_rule(console, "[bold cyan]Pair Benchmark Results[/]", false);
    let mut table = Table::new()
        .expand()
        .column("[bold]Primary[/]")
        .column("[bold]Secondary[/]")
        .column("[bold]LLM Time[/]")
        .column("[bold]Python Time[/]")
        .column("[bold]Result[/]")
        .column("[bold]Votes (P/A/D)[/]")
        .column("[bold]Notes[/]");
    for r in results {
        let mut votes = format!(
            "{}/{}/{}",
            r.pair_primary_wins, r.pair_alternate_wins, r.pair_disagreements
        );
        if r.pair_backup_failures > 0 {
            votes += &format!(" [red]({} fail)[/]", r.pair_backup_failures);
        }
        table.row(&[
            escape(&r.primary_model),
            escape(&r.secondary_model),
            llm_time(r.run.llm_time_ms),
            py_time(r.run.python_exec_time_ms),
            outcome(r.run.output_correct).to_string(),
            votes,
            escape(r.run.notes.as_deref().unwrap_or_default()),
        ]);
    }
    table.write(console);
    // Summary: the fastest passing pair.
    match results
        .iter()
        .filter(|r| r.run.output_correct == Some(true))
        .min_by_key(|r| r.run.llm_time_ms)
    {
        Some(best) => console.markup_line(&format!(
            "\n[bold green]Best pair:[/] {} + {} ({:.1}s)",
            escape(&best.primary_model),
            escape(&best.secondary_model),
            best.run.llm_time_ms as f64 / 1000.0
        )),
        None => console.markup_line("\n[yellow]No pairs produced a passing result.[/]"),
    }
}

#[cfg(test)]
#[path = "benchmark_tests.rs"]
mod tests;
