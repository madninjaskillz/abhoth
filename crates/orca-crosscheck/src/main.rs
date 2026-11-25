//! Cross-check harness: the same scripted session through the C# and the Rust `openorca`.
//!
//! ```text
//! cargo run -p orca-crosscheck -- --csharp ..\src\OpenOrca.Cli\bin\Release\net10.0\openorca.exe \
//!     --rust target\debug\abhoth.exe [--scenarios crosscheck\scenarios] [--only name] [--out dir]
//! ```
//!
//! For each scenario (`crosscheck/scenarios/*.json`) both binaries run `--print <prompt>` in a
//! fresh work folder and config home, against a fake OpenAI server that plays the scenario's
//! replies. Compared after normalising (durations, temp paths):
//! - the `--print` JSON (response, tokens, tool calls with arguments and results, files, success),
//! - every request body the server received (messages, sampling fields, ...),
//! - the files in the work folder afterwards,
//! - the checkpoints in the config home (`checkpoints/`: manifests with times blanked and the
//!   backup names checked against the path hash, backup contents),
//! - the exit code and the session notices on stderr (resumed / not found / none to continue),
//! - the "Slot usage" / "Provider usage" lines of the config home's `logs/*.log` (times and
//!   durations masked; `usage_log: false` skips them). Replies can report `cached_tokens`,
//!   `reasoning_tokens`, `completion_tokens` and llama.cpp `timings` for them.
//!
//! Config-home files outside those are compared when the scenario names them (`compare_home`,
//! e.g. `["memory/"]`), and `file_masks` (`[regex, replacement]`) blank volatile text (an
//! export's time stamp) in them and in the work files.
//!
//! A scenario can plant files in the config home first (`home_files`, e.g. a saved session;
//! `<WORK_JSON>` in them becomes the work folder as a JSON string body) and pass extra
//! arguments (`args`, e.g. `["-r", "s1"]`). The fake server can answer other endpoints
//! (`server_routes`: `{"GET /health": [200, "{...}"]}`); every such request (`probes`, e.g. the
//! startup context-window probes) is compared too. A reply can stream its reasoning in small
//! chunks (`reasoning_chunk_chars`), open with a null-content role chunk
//! (`null_role_content`), break off after some events (`die_after_events`) or be an HTTP error
//! (`error`: `[status, body]`). A reply with `for_model` only answers requests for that model,
//! and `unordered_requests` compares the request bodies sorted instead of in arrival order:
//! together they make concurrent requests (pair programming) deterministic.
//!
//! For the git tools a scenario can run `setup` commands in the work folder after its files
//! are written (e.g. `["git", "init"]`) with extra environment variables (`env`, also given to
//! both runs; `<HOME>` in a value becomes the config home), so fixed author/committer dates
//! give the same commit hashes in both runs. `.git/` is left out of the file comparison.
//!
//! Config values can name the fake server (`<SERVER>`: its `/v1` base URL, `<SERVER_ROOT>`:
//! without `/v1`, e.g. a slot copy or the profiles API) and the config home (`<HOME_JSON>`,
//! JSON-escaped, e.g. `slots.localStateFile`) or the work folder (`<WORK_JSON>`, e.g. a fake
//! bridge CLI's path). Profile, mode and engine errors on stderr are
//! compared with the session notices.
//!
//! For the web tools a scenario can serve pages from a local server (`web`: path -> status,
//! content type, body); `<WEB>` in the prompt and the scripted tool arguments becomes its base
//! URL, and `<CLOSED>` becomes `127.0.0.1:<port>` of a port nothing listens on. Both are
//! normalised back in the results.
//!
//! A scenario can also put binary files in the work folder (`binary_files`, base64). In its
//! config, `<FAKE_MCP>` / `<FAKE_LSP>` name the fake MCP and LSP servers built with this
//! program and `<IMAGE_URL>` the fake server's images API; images API requests are compared
//! too, and image files by their pixels (see `describe_image`).
//!
//! Known, documented gaps are ignored by default: the advertised tool list (the `tools` array
//! and the tool-derived parts of the system prompt: the AVAILABLE TOOLS lines and the
//! lead_mode tool's LEAD MODE section; a text-mode sub-agent's TOOLS AVAILABLE list), since Rust
//! does not have every C# tool yet. The
//! rest of the system prompt is compared. `--strict` compares everything; a scenario with
//! `"strict": true` (the Pi profile's) is always compared in full.
//!
//! ## Interactive scenarios
//!
//! A scenario with `"interactive": {...}` runs each binary without `--print` (with `args`, the
//! same environment and work folder) inside a pseudo-terminal (ConPTY on Windows), with a
//! `vt100` screen model as the terminal (it answers the cursor-position query `ESC[6n`):
//!
//! ```text
//! "interactive": {
//!   "cols": 100, "rows": 32,           // terminal size (defaults)
//!   "exitTimeoutMs": 10000,            // after the last step: wait this long, then kill
//!   "compareScreen": ["Goodbye!", "Compacted: \\d+ messages"],
//!   "steps": [
//!     {"action": "waitFor", "text": "❯", "timeoutMs": 45000},  // visible screen contains it
//!     {"action": "waitGone", "text": "..."},                     // ... no longer contains it
//!     {"action": "type", "text": "say hello"},                   // `<WEB>` is substituted
//!     {"action": "key", "name": "enter", "count": 1},  // enter esc tab shift-tab up down left
//!                                       // right home end pgup pgdn backspace delete
//!                                       // ctrl-c/d/g/o/r/t alt-v
//!     {"action": "wait", "ms": 300},
//!     {"action": "waitQuiet", "ms": 600},          // no output for this long (timeoutMs)
//!     {"action": "snap", "label": "after"},        // the screen, written with --out
//!     {"action": "expect", "text": "..."},         // screen or scrollback contains it
//!     {"action": "expectNot", "text": "..."},
//!     {"action": "exit", "timeoutMs": 15000}       // the process exits (e.g. after /exit)
//!   ]
//! }
//! ```
//!
//! `timeoutMs` defaults to 30 s. A failed step is reported for that binary (`step N: timed out
//! waiting for ...`), and every failed step makes the scenario differ. Compared: the steps, the
//! request bodies, work files and checkpoints as above, the exit code (none when killed), and
//! the saved sessions (`sessions/*.json`): sessions the run created get the ids `<S1>`, `<S2>`,
//! ... in creation order (in the file names, `id`, `parentSessionId` and the screen text),
//! `createdAt` / `updatedAt` are masked and the system prompt is masked like the requests'.
//! The rendering differs by design, so the screen is compared only where `compareScreen`
//! says: a line containing one of the texts is compared whole; otherwise a pattern that
//! matches a line as a regex compares just the matched part. With `--out`, each binary's
//! semantic screen (scrollback + final screen, trimmed, blank lines dropped) is written as
//! `<name>.<side>.screen.txt`, with its snaps and the raw VT stream. A reply can stream
//! slowly (`chunk_delay_ms` before each event), e.g. to cancel it with Ctrl+C.

mod ctrl_c;
mod pty;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use orca_testkit::{FakeOpenAi, Page, Reply, ServerOptions, StaticServer};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Debug, Deserialize)]
struct Scenario {
    #[serde(default)]
    name: String,
    /// The `--print` prompt (unused by interactive scenarios).
    #[serde(default)]
    prompt: String,
    /// Run without `--print` in a pseudo-terminal, driven by these steps.
    #[serde(default)]
    interactive: Option<pty::Interactive>,
    #[serde(default)]
    files: BTreeMap<String, String>,
    /// Files to create in the config home (relative paths), e.g. `sessions/<id>.json`.
    #[serde(default)]
    home_files: BTreeMap<String, String>,
    /// Arguments after `--print <prompt>`.
    #[serde(default)]
    args: Vec<String>,
    /// Binary files to create in the work folder: relative path -> base64 content.
    #[serde(default)]
    binary_files: BTreeMap<String, String>,
    /// Extra config.json keys (merged over the harness defaults, one level deep).
    #[serde(default)]
    config: serde_json::Map<String, Value>,
    /// Compare this scenario without the known-gap masks (`--strict` for one scenario): the
    /// tool list and the whole system prompt (the Pi profile's tools are fully ported).
    #[serde(default)]
    strict: bool,
    /// Commands (program and arguments) run in the work folder before the prompt.
    #[serde(default)]
    setup: Vec<Vec<String>>,
    /// Environment variables for the setup commands and both runs (`<HOME>`: config home).
    #[serde(default)]
    env: BTreeMap<String, String>,
    /// Pages served at `<WEB>` (path -> page).
    #[serde(default)]
    web: BTreeMap<String, WebPage>,
    /// Answers for other endpoints: `"METHOD /path"` -> `[status, body]`.
    #[serde(default)]
    server_routes: BTreeMap<String, (u16, String)>,
    /// Compare the request bodies as a set (sorted) instead of in arrival order: requests sent
    /// concurrently (pair programming) reach the server in either order.
    #[serde(default)]
    unordered_requests: bool,
    /// Compare the per-request usage lines of the file log (default on; off for scenarios
    /// whose request ends at a timing-dependent point, such as a Ctrl+C mid-stream).
    #[serde(default = "yes")]
    usage_log: bool,
    /// Config-home files compared too (relative paths; one ending in `/` takes the files under
    /// that folder), e.g. `memory/` or `config.json`. Paths and masks applied as for work files.
    #[serde(default)]
    compare_home: Vec<String>,
    /// `[regex, replacement]` pairs applied to work files and `compare_home` files (e.g. a time
    /// stamp written into an exported file).
    #[serde(default)]
    file_masks: Vec<(String, String)>,
    replies: Vec<ScriptReply>,
}

#[derive(Debug, Deserialize)]
struct WebPage {
    #[serde(default = "ok_status")]
    status: u16,
    #[serde(default = "html_type")]
    content_type: String,
    #[serde(default)]
    body: String,
}

fn yes() -> bool {
    true
}

fn ok_status() -> u16 {
    200
}

fn html_type() -> String {
    "text/html; charset=utf-8".to_string()
}

#[derive(Debug, Deserialize)]
struct ScriptReply {
    #[serde(default)]
    text: String,
    #[serde(default)]
    reasoning: String,
    /// `{"name": ..., "args": {...}}`, or several.
    #[serde(default)]
    tools: Vec<ScriptTool>,
    #[serde(default)]
    finish_reason: Option<String>,
    /// The prompt size the usage reports (default 100): drives the context estimate.
    #[serde(default)]
    prompt_tokens: Option<i64>,
    /// Usage off: no usage chunk (with no text either, the stream is empty).
    #[serde(default)]
    no_usage: bool,
    #[serde(default)]
    reasoning_chunk_chars: usize,
    #[serde(default)]
    null_role_content: bool,
    #[serde(default)]
    die_after_events: Option<usize>,
    /// `[status, body]`: answer with an HTTP error instead.
    #[serde(default)]
    error: Option<(u16, String)>,
    /// Answer only a request for this model (concurrent requests to several models).
    #[serde(default)]
    for_model: Option<String>,
    /// Pause before each streamed event (a slow reply, e.g. to cancel with Ctrl+C).
    #[serde(default)]
    chunk_delay_ms: u64,
    /// Usage details and llama.cpp timings for the usage chunk.
    #[serde(default)]
    cached_tokens: Option<i64>,
    #[serde(default)]
    reasoning_tokens: Option<i64>,
    #[serde(default)]
    timings: Option<Value>,
    /// The completion tokens the usage reports (default 10, or 20 with tool calls).
    #[serde(default)]
    completion_tokens: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct ScriptTool {
    name: String,
    #[serde(default)]
    args: Value,
    /// Raw argument text, for malformed-JSON scenarios.
    #[serde(default)]
    raw: Option<String>,
}

impl ScriptReply {
    fn to_reply(&self, subst: &dyn Fn(&str) -> String) -> Reply {
        Reply {
            text: self.text.clone(),
            reasoning: self.reasoning.clone(),
            tool_calls: self
                .tools
                .iter()
                .map(|t| {
                    (
                        t.name.clone(),
                        subst(&t.raw.clone().unwrap_or_else(|| t.args.to_string())),
                    )
                })
                .collect(),
            finish_reason: self.finish_reason.clone(),
            prompt_tokens: if self.no_usage {
                0
            } else {
                self.prompt_tokens.unwrap_or(100)
            },
            completion_tokens: match (self.no_usage, self.tools.is_empty()) {
                (true, _) => 0,
                (false, true) => self.completion_tokens.unwrap_or(10),
                (false, false) => self.completion_tokens.unwrap_or(20),
            },
            cached_tokens: self.cached_tokens,
            reasoning_tokens: self.reasoning_tokens,
            timings: self.timings.clone(),
            reasoning_chunk_chars: self.reasoning_chunk_chars,
            null_role_content: self.null_role_content,
            die_after_events: self.die_after_events,
            error: self.error.clone(),
            for_model: self.for_model.clone(),
            chunk_delay_ms: self.chunk_delay_ms,
            ..Reply::default()
        }
    }
}

struct RunResult {
    output: Value,
    requests: Vec<Value>,
    image_requests: Vec<Value>,
    files: BTreeMap<String, String>,
    home_files: BTreeMap<String, Value>,
    exit_code: Option<i32>,
    notices: Vec<String>,
    probes: Vec<String>,
    stderr: String,
    /// Interactive runs: the failed steps (empty when every step passed).
    steps: Vec<String>,
    /// The file log's usage lines, normalised (see [`usage_lines`]).
    usage_log: Vec<String>,
    /// Interactive runs: the semantic screen (scrollback + screen, trimmed lines).
    screen: Vec<String>,
    /// Interactive runs: `snap` steps' screens, and the raw VT stream.
    snaps: Vec<(String, String)>,
    raw: Vec<u8>,
}

fn base_config(base_url: &str) -> Value {
    json!({
        "lmStudio": {"baseUrl": base_url, "apiKey": "test-key", "models": ["fake-model"], "nativeToolCalling": true, "maxTokens": 2048},
        "context": {"autoDetectContextWindow": false},
        "session": {"autoSave": false},
        "permissions": {"autoApproveAll": true}
    })
}

fn merge(base: &mut Value, extra: &serde_json::Map<String, Value>) {
    for (k, v) in extra {
        match (base.get_mut(k), v) {
            (Some(Value::Object(b)), Value::Object(e)) => {
                for (ek, ev) in e {
                    b.insert(ek.clone(), ev.clone());
                }
            }
            _ => {
                base[k] = v.clone();
            }
        }
    }
}

async fn run_one(exe: &Path, scenario: &Scenario) -> Result<RunResult> {
    let web = StaticServer::start(scenario.web.iter().map(|(path, p)| {
        (
            path.clone(),
            Page::new(p.status, p.content_type.clone(), p.body.clone()),
        )
    }))
    .await;
    let web_url = web.base_url();
    let closed = {
        let l = std::net::TcpListener::bind("127.0.0.1:0")?;
        format!("127.0.0.1:{}", l.local_addr()?.port())
    };
    let subst = |s: &str| s.replace("<WEB>", &web_url).replace("<CLOSED>", &closed);
    let mut options = ServerOptions::default();
    for (key, (status, body)) in &scenario.server_routes {
        options = options.route(key, *status, body.clone());
    }
    let server = FakeOpenAi::start_with(
        scenario
            .replies
            .iter()
            .map(|r| r.to_reply(&subst))
            .collect(),
        options,
    )
    .await;
    let home = tempfile::tempdir()?;
    let work = tempfile::tempdir()?;
    let mut config = base_config(&server.base_url());
    merge(&mut config, &scenario.config);
    // `<SERVER>` / `<SERVER_ROOT>` / `<HOME_JSON>` / `<WORK_JSON>` in config values: the fake
    // server's base URL (with and without `/v1`), the config home (the local slot state file)
    // and the work folder; then the fake MCP / LSP / images placeholders.
    let server_url = server.base_url();
    let server_root = server_url.trim_end_matches("/v1").to_string();
    let home_json = serde_json::to_string(&home.path().display().to_string())?;
    let work_json = serde_json::to_string(&work.path().display().to_string())?;
    let config_text = fill_placeholders(&serde_json::to_string_pretty(&config)?, &server_url)
        .replace("<SERVER_ROOT>", &server_root)
        .replace("<SERVER>", &server_url)
        .replace("<HOME_JSON>", &home_json[1..home_json.len() - 1])
        .replace("<WORK_JSON>", &work_json[1..work_json.len() - 1]);
    std::fs::write(home.path().join("config.json"), config_text)?;
    let work_json = serde_json::to_string(&work.path().display().to_string())?;
    let work_json = &work_json[1..work_json.len() - 1];
    for (rel, content) in &scenario.home_files {
        let p = home.path().join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&p, content.replace("<WORK_JSON>", work_json))?;
    }
    // Fixed, distinct mtimes (one second apart, in name order, ten and a half minutes ago) so
    // tools that sort or describe by age give the same answer in both runs: "10m ago" however
    // fast the binary starts (at exactly ten minutes a quick run still saw "9m").
    let base = std::time::SystemTime::now() - std::time::Duration::from_secs(630);
    let binary: Vec<(String, Vec<u8>)> = scenario
        .binary_files
        .iter()
        .map(|(rel, b64)| {
            use base64::Engine as _;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(b64)
                .with_context(|| format!("binary_files.{rel} is not base64"))?;
            Ok((rel.clone(), bytes))
        })
        .collect::<Result<_>>()?;
    let all_files = scenario
        .files
        .iter()
        .map(|(rel, c)| (rel.clone(), c.as_bytes().to_vec()))
        .chain(binary);
    for (i, (rel, content)) in all_files.enumerate() {
        let p = work.path().join(&rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&p, content)?;
        std::fs::File::options()
            .write(true)
            .open(&p)?
            .set_modified(base + std::time::Duration::from_secs(i as u64))?;
    }

    let home_str = home.path().display().to_string();
    let env: Vec<(String, String)> = scenario
        .env
        .iter()
        .map(|(k, v)| (k.clone(), v.replace("<HOME>", &home_str)))
        .collect();
    for argv in &scenario.setup {
        let (program, rest) = argv.split_first().context("empty setup command")?;
        let out = Command::new(program)
            .args(rest)
            .current_dir(work.path())
            .envs(env.iter().cloned())
            .output()
            .with_context(|| format!("setup: {argv:?}"))?;
        if !out.status.success() {
            bail!(
                "setup {argv:?} failed: {}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    let (exe_owned, work_path, home_path, prompt, extra) = (
        exe.to_path_buf(),
        work.path().to_path_buf(),
        home.path().to_path_buf(),
        subst(&scenario.prompt),
        scenario.args.clone(),
    );
    let mut pty_run = None;
    let (stdout, stderr, exit_code) = if let Some(inter) = &scenario.interactive {
        // The steps' typed text can name the web server too.
        let steps = inter
            .steps
            .iter()
            .map(|s| pty::Step {
                text: s.text.as_deref().map(subst),
                ..s.clone()
            })
            .collect();
        let inter = pty::Interactive {
            steps,
            ..inter.clone()
        };
        let mut env = env.clone();
        env.push(("OPENORCA_HOME".into(), home_str.clone()));
        let run = tokio::task::spawn_blocking(move || {
            pty::run(&exe_owned, &extra, &work_path, &env, &inter)
        })
        .await??;
        let code = run.exit_code;
        pty_run = Some(run);
        (String::new(), String::new(), code)
    } else {
        let out = tokio::task::spawn_blocking(move || {
            Command::new(exe_owned)
                .args(["--print", &prompt])
                .args(&extra)
                .current_dir(&work_path)
                .env("OPENORCA_HOME", &home_path)
                .envs(env)
                .env_remove("RUST_LOG")
                .output()
        })
        .await??;
        (
            String::from_utf8_lossy(&out.stdout).to_string(),
            String::from_utf8_lossy(&out.stderr).to_string(),
            out.status.code(),
        )
    };
    // A run that stops early (an unknown --resume id) prints nothing to stdout.
    let output: Value = if stdout.trim().is_empty() {
        Value::Null
    } else {
        let json_start = stdout.find('{').unwrap_or(0);
        serde_json::from_str(stdout[json_start..].trim()).with_context(|| {
            format!(
                "{} printed no JSON.\nstdout: {stdout}\nstderr: {stderr}",
                exe.display()
            )
        })?
    };

    let work_str = work.path().display().to_string();
    let norm = |v: Value| {
        normalize(
            v,
            &[
                (&work_str, "<WORK>"),
                (&home_str, "<HOME>"),
                (&web_url, "<WEB>"),
                (&closed, "<CLOSED>"),
                (&server_root, "<SERVER_ROOT>"),
            ],
        )
    };
    let masks = scenario
        .file_masks
        .iter()
        .map(|(re, to)| Ok((regex::Regex::new(re)?, to.clone())))
        .collect::<Result<Vec<_>>>()?;
    let mut files = BTreeMap::new();
    for e in walkdir::WalkDir::new(work.path())
        .into_iter()
        .filter_entry(|e| e.file_name() != ".git")
        .filter_map(Result::ok)
    {
        if e.file_type().is_file() {
            let rel = e
                .path()
                .strip_prefix(work.path())
                .unwrap()
                .display()
                .to_string()
                .replace('\\', "/");
            let bytes = std::fs::read(e.path())?;
            let content = describe_image(&rel, &bytes)
                .unwrap_or_else(|| String::from_utf8_lossy(&bytes).to_string());
            files.insert(rel, mask_file(&norm(json!(content)), &masks));
        }
    }
    let mut home_files = BTreeMap::new();
    for e in walkdir::WalkDir::new(home.path())
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
    {
        let rel = e
            .path()
            .strip_prefix(home.path())
            .unwrap()
            .display()
            .to_string()
            .replace('\\', "/");
        let wanted = scenario.compare_home.iter().any(|w| {
            if w.ends_with('/') {
                rel.starts_with(w.as_str())
            } else {
                rel == *w
            }
        });
        if wanted {
            let text = String::from_utf8_lossy(&std::fs::read(e.path())?).into_owned();
            home_files.insert(rel, json!(mask_file(&norm(json!(text)), &masks)));
        }
    }
    let checkpoints = home.path().join("checkpoints");
    // Backup file name -> the (normalised) file it backs up, from the manifests: several
    // backups made in the same second differ only in the path hash, so they are keyed by
    // their file instead.
    let mut backup_of: BTreeMap<String, String> = BTreeMap::new();
    for e in walkdir::WalkDir::new(&checkpoints)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_name() == "manifest.json")
    {
        let manifest: Value = serde_json::from_slice(&std::fs::read(e.path())?)
            .with_context(|| format!("{} wrote a bad manifest", exe.display()))?;
        for entry in manifest.as_array().into_iter().flatten() {
            if let (Some(name), Some(path)) =
                (entry["backupFile"].as_str(), entry["filePath"].as_str())
            {
                let shown = norm(json!(path)).as_str().unwrap_or_default().to_string();
                backup_of.insert(name.to_string(), shown);
            }
        }
    }
    for e in walkdir::WalkDir::new(&checkpoints)
        .into_iter()
        .filter_map(Result::ok)
    {
        if !e.file_type().is_file() {
            continue;
        }
        let rel = e
            .path()
            .strip_prefix(home.path())
            .unwrap()
            .display()
            .to_string()
            .replace('\\', "/");
        let bytes = std::fs::read(e.path())?;
        let (key, value) = if rel.ends_with("/manifest.json") {
            let mut manifest: Value = serde_json::from_slice(&bytes)
                .with_context(|| format!("{} wrote a bad {rel}", exe.display()))?;
            for entry in manifest.as_array_mut().into_iter().flatten() {
                entry["timestamp"] = json!("<TIME>");
                let path = entry["filePath"].as_str().unwrap_or_default().to_string();
                let name = entry["backupFile"].as_str().unwrap_or_default().to_string();
                entry["backupFile"] = json!(normalize_backup_name(&name, Some(&path)));
            }
            (rel, norm(manifest))
        } else {
            let name = rel.rsplit('/').next().unwrap_or_default();
            let dir = &rel[..rel.len() - name.len()];
            let key = match backup_of.get(name) {
                Some(file) => format!("{dir}<STAMP>_<{file}>.bak"),
                None => format!("{dir}{}", normalize_backup_name(name, None)),
            };
            (key, json!(String::from_utf8_lossy(&bytes)))
        };
        home_files.insert(key, value);
    }
    // Interactive runs: the saved sessions, with new session ids (not the scenario's planted
    // ones) renamed `<S1>`, `<S2>`, ... in creation order and the times masked.
    let mut session_ids: Vec<(String, String)> = Vec::new();
    if scenario.interactive.is_some() {
        let mut sessions: Vec<(String, Value)> = Vec::new();
        for e in std::fs::read_dir(home.path().join("sessions"))
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
        {
            let name = e.file_name().to_string_lossy().into_owned();
            let Some(id) = name.strip_suffix(".json") else {
                continue;
            };
            let v: Value = serde_json::from_slice(&std::fs::read(e.path())?)
                .with_context(|| format!("{} wrote a bad sessions/{name}", exe.display()))?;
            sessions.push((id.to_string(), v));
        }
        let planted = |id: &str| {
            scenario
                .home_files
                .contains_key(&format!("sessions/{id}.json"))
        };
        let mut fresh: Vec<&(String, Value)> =
            sessions.iter().filter(|(id, _)| !planted(id)).collect();
        fresh.sort_by_key(|(id, v)| {
            (
                v["createdAt"].as_str().unwrap_or_default().to_string(),
                v["title"].as_str().unwrap_or_default().to_string(),
                id.clone(),
            )
        });
        session_ids = fresh
            .iter()
            .enumerate()
            .map(|(i, (id, _))| (id.clone(), format!("<S{}>", i + 1)))
            .collect();
        let rename = |id: &str| {
            session_ids
                .iter()
                .find(|(from, _)| from == id)
                .map_or_else(|| id.to_string(), |(_, to)| to.clone())
        };
        for (id, mut v) in sessions {
            for key in ["id", "parentSessionId"] {
                if let Some(s) = v[key].as_str() {
                    v[key] = json!(rename(s));
                }
            }
            for key in ["createdAt", "updatedAt"] {
                if v.get(key).is_some() {
                    v[key] = json!("<TIME>");
                }
            }
            home_files.insert(format!("sessions/{}.json", rename(&id)), norm(v));
        }
    }
    let notices = stderr
        .lines()
        .map(str::trim)
        .filter(|l| {
            [
                "Resumed session:",
                "Session not found:",
                "No saved sessions",
                "No session for this directory",
                "Unknown profile",
                "Profile '",
                "Unknown mode",
                "Unknown agent.engine",
                "Unknown --engine",
            ]
            .iter()
            .any(|p| l.starts_with(p))
        })
        .map(|l| {
            l.replace(&server_root, "<SERVER_ROOT>")
                .replace(&home_str, "<HOME>")
        })
        .collect();

    let (steps, screen, snaps, raw) = match pty_run {
        Some(r) => {
            // Paths and new session ids in the screen text, as in the files.
            let fix = |l: &str| {
                let mut l = norm(json!(l)).as_str().unwrap_or_default().to_string();
                for (from, to) in &session_ids {
                    l = l.replace(from.as_str(), to);
                }
                l
            };
            let screen = r.screen.iter().map(|l| fix(l)).collect();
            (r.failures, screen, r.snaps, r.raw)
        }
        None => Default::default(),
    };
    let usage_log = usage_lines(&home.path().join("logs"));
    Ok(RunResult {
        output: norm(output),
        usage_log,
        home_files,
        exit_code,
        notices,
        probes: server.probes(),
        requests: server
            .requests()
            .into_iter()
            .map(|r| norm(r.body))
            .collect(),
        image_requests: server.image_requests().into_iter().map(norm).collect(),
        files,
        stderr,
        steps,
        screen,
        snaps,
        raw,
    })
}

/// A normalised file body (a JSON string) with the scenario's `file_masks` applied.
fn mask_file(v: &Value, masks: &[(regex::Regex, String)]) -> String {
    let mut text = v.as_str().unwrap_or_default().to_string();
    for (re, to) in masks {
        text = re.replace_all(&text, to.as_str()).into_owned();
    }
    text
}

/// The "Slot usage" and "Provider usage" lines of the logs in `dir`, in file order, without
/// the time stamp and with the measured times masked (`<MS>`) and sub-agent ids numbered.
fn usage_lines(dir: &Path) -> Vec<String> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "log"))
        .collect();
    files.sort();
    let stamp = regex::Regex::new(r"^\d\d:\d\d:\d\d\.\d{3} ").unwrap();
    let ms = regex::Regex::new(r", \d+ ms \|").unwrap();
    let first = regex::Regex::new(r"(first_think_ms|first_answer_ms)=\d+").unwrap();
    // Sub-agent ids are random: numbered in order of appearance (`<AGENT1>`, ...).
    let agent = regex::Regex::new(r"Slot usage ([0-9a-f]{8}) on").unwrap();
    let mut agents: Vec<String> = Vec::new();
    let mut out = Vec::new();
    for f in files {
        let text = String::from_utf8_lossy(&std::fs::read(&f).unwrap_or_default()).into_owned();
        for line in text.lines() {
            if !line.contains("Slot usage ") && !line.contains("Provider usage ") {
                continue;
            }
            let l = stamp.replace(line, "");
            let l = ms.replace_all(&l, ", <MS> ms |");
            let l = first.replace_all(&l, "$1=<MS>");
            let l = agent.replace_all(&l, |c: &regex::Captures| {
                let id = c[1].to_string();
                let n = match agents.iter().position(|a| *a == id) {
                    Some(i) => i + 1,
                    None => {
                        agents.push(id);
                        agents.len()
                    }
                };
                format!("Slot usage <AGENT{n}> on")
            });
            out.push(l.into_owned());
        }
    }
    out
}

/// Scenario placeholders in the config: `<FAKE_MCP>` / `<FAKE_LSP>` (the fake servers built
/// next to this program, as JSON string bodies) and `<IMAGE_URL>` (the fake server's `/v1`,
/// which also answers the images API).
fn fill_placeholders(text: &str, base_url: &str) -> String {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_default();
    let json_path = |name: &str| {
        let p = exe_dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
        let s = serde_json::to_string(&p.display().to_string()).unwrap_or_default();
        s[1..s.len() - 1].to_string()
    };
    text.replace("<FAKE_MCP>", &json_path("fake-mcp-server"))
        .replace("<FAKE_LSP>", &json_path("fake-lsp-server"))
        .replace("<IMAGE_URL>", base_url)
}

/// Image files are compared by content, not bytes (the two builds use different encoders):
/// `<image WxH rgba:SHA>` for the lossless formats, `<image FORMAT WxH>` for JPEG and WebP
/// (lossy in C#, lossless in Rust). `None` for files that are not decodable images.
fn describe_image(rel: &str, bytes: &[u8]) -> Option<String> {
    let ext = rel.rsplit('.').next()?.to_ascii_lowercase();
    if !["png", "bmp", "gif", "jpg", "jpeg", "webp"].contains(&ext.as_str()) {
        return None;
    }
    let format = image::guess_format(bytes).ok()?;
    let img = image::load_from_memory_with_format(bytes, format).ok()?;
    let (w, h) = (img.width(), img.height());
    let name = format!("{format:?}").to_ascii_uppercase();
    if matches!(format, image::ImageFormat::Jpeg | image::ImageFormat::WebP) {
        return Some(format!("<image {name} {w}x{h}>"));
    }
    use sha2::Digest;
    let digest = sha2::Sha256::digest(img.to_rgba8().as_raw());
    let hex: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    Some(format!("<image {name} {w}x{h} rgba:{hex}>"))
}

/// `yyyyMMdd_HHmmss_<hash16>.bak` with the time masked; with the file path, the hash is
/// checked to be the first 16 hex digits of the path's SHA-256 (as C# names backups).
fn normalize_backup_name(name: &str, path: Option<&str>) -> String {
    let Some(rest) = name.strip_suffix(".bak") else {
        return name.to_string();
    };
    let (stamp, hash) = rest.split_at(rest.len().min(16));
    let stamp_ok = stamp.len() == 16
        && stamp.chars().enumerate().all(|(i, c)| {
            if i == 8 || i == 15 {
                c == '_'
            } else {
                c.is_ascii_digit()
            }
        });
    if !stamp_ok || hash.len() != 16 {
        return name.to_string();
    }
    let hash = match path {
        Some(p) => {
            use sha2::Digest;
            let digest = sha2::Sha256::digest(p.as_bytes());
            let expected: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
            if expected == hash {
                "<HASH-OK>".to_string()
            } else {
                format!("<HASH-MISMATCH {hash}>")
            }
        }
        None => "<HASH>".to_string(),
    };
    format!("<STAMP>_{hash}.bak")
}

/// `/benchmark` run folders are named `orca-benchmark-<model>-run<N>-<yyyyMMdd-HHmmss>`: the
/// time stamp becomes `<TS>`.
fn mask_benchmark_stamps(text: &str) -> String {
    const MARK: &str = "orca-benchmark-";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find(MARK) {
        let (head, tail) = rest.split_at(i + MARK.len());
        out.push_str(head);
        rest = tail;
        // The stamp: "-run<digits>-" then 8 digits, '-', 6 digits.
        let Some(run) = rest.find("-run") else {
            continue;
        };
        let after_run = &rest[run + 4..];
        let digits = after_run.bytes().take_while(u8::is_ascii_digit).count();
        let stamp = &after_run[digits..];
        let b = stamp.as_bytes();
        let is_stamp = digits > 0
            && b.len() >= 16
            && b[0] == b'-'
            && b[1..9].iter().all(u8::is_ascii_digit)
            && b[9] == b'-'
            && b[10..16].iter().all(u8::is_ascii_digit);
        if is_stamp {
            let keep = run + 4 + digits;
            out.push_str(&rest[..keep]);
            out.push_str("-<TS>");
            rest = &rest[keep + 16..];
        }
    }
    out.push_str(rest);
    out
}

/// The Rust build is called Abhoth (D88): its product name reads as the C# build's.
fn mask_product_name(text: &str) -> String {
    text.replace("Abhoth", "OpenOrca")
        .replace("abhoth", "openorca")
}

/// Replace temp paths (in both slash styles and JSON-escaped forms) and zero the timings.
fn normalize(v: Value, paths: &[(&str, &str)]) -> Value {
    match v {
        Value::String(mut s) => {
            for (from, to) in paths {
                for variant in [
                    from.to_string(),
                    from.replace('\\', "/"),
                    from.replace('\\', "\\\\"),
                ] {
                    s = s.replace(&variant, to);
                }
            }
            Value::String(mask_benchmark_stamps(&mask_product_name(&s)))
        }
        Value::Array(a) => Value::Array(a.into_iter().map(|x| normalize(x, paths)).collect()),
        Value::Object(o) => Value::Object(
            o.into_iter()
                .map(|(k, x)| {
                    let x = if k == "duration_ms" {
                        json!(0)
                    } else {
                        normalize(x, paths)
                    };
                    (k, x)
                })
                .collect(),
        ),
        other => other,
    }
}

/// Remove what the port knowingly does differently (see PORTING.md, "Known gaps"): the tool
/// list. The system prompt is compared with its tool-derived parts masked.
fn mask_known_gaps(mut request: Value) -> Value {
    if let Some(o) = request.as_object_mut() {
        o.remove("tools");
        if let Some(Value::Array(msgs)) = o.get_mut("messages")
            && let Some(first) = msgs.first_mut()
            && first["role"] == "system"
            && let Some(text) = first["content"].as_str()
        {
            first["content"] = json!(mask_tool_derived_prompt(text));
        }
    }
    request
}

/// The system prompt with the AVAILABLE TOOLS lines replaced by `<tool list>` and the LEAD MODE
/// section (present only when a `lead_mode` tool is advertised) removed.
fn mask_tool_derived_prompt(text: &str) -> String {
    let mut out = text.to_string();
    const LIST: &str = "AVAILABLE TOOLS:
";
    if let Some(start) = out.find(LIST).map(|i| i + LIST.len()) {
        // Tool descriptions can hold blank lines: the list ends at the next section header.
        let end = out[start..]
            .find(
                "

AGENT DELEGATION",
            )
            .or_else(|| {
                out[start..].find(
                    "

PROGRESS:",
                )
            })
            .or_else(|| {
                out[start..].find(
                    "

",
                )
            })
            .map_or(out.len(), |e| start + e);
        out.replace_range(start..end, "<tool list>");
    }
    // A text-mode sub-agent's prompt lists its tools too ("TOOLS AVAILABLE:", platform lines).
    for (head, gap) in [
        ("TOOLS AVAILABLE:\r\n", "\r\n\r\n"),
        ("TOOLS AVAILABLE:\n", "\n\n"),
    ] {
        if let Some(start) = out.find(head).map(|i| i + head.len()) {
            let end = out[start..].find(gap).map_or(out.len(), |e| start + e);
            out.replace_range(start..end, "<tool list>");
            break;
        }
    }
    const LEAD: &str = "

LEAD MODE (auto):
";
    if let Some(start) = out.find(LEAD) {
        let end = out[start + LEAD.len()..]
            .find(
                "

",
            )
            .map_or(out.len(), |e| start + LEAD.len() + e);
        out.replace_range(start..end, "");
    }
    out
}

fn diff(path: &str, a: &Value, b: &Value, out: &mut Vec<String>) {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            let keys: std::collections::BTreeSet<&String> = x.keys().chain(y.keys()).collect();
            for k in keys {
                diff(
                    &format!("{path}.{k}"),
                    x.get(k).unwrap_or(&Value::Null),
                    y.get(k).unwrap_or(&Value::Null),
                    out,
                );
            }
            let (kx, ky): (Vec<_>, Vec<_>) = (x.keys().collect(), y.keys().collect());
            if kx != ky && out.is_empty() {
                out.push(format!("{path}: key order differs: {kx:?} vs {ky:?}"));
            }
        }
        (Value::Array(x), Value::Array(y)) => {
            if x.len() != y.len() {
                out.push(format!("{path}: length {} vs {}", x.len(), y.len()));
            }
            for (i, (p, q)) in x.iter().zip(y).enumerate() {
                diff(&format!("{path}[{i}]"), p, q, out);
            }
        }
        _ if a != b => out.push(format!(
            "{path}:\n    C#:   {}\n    Rust: {}",
            short(a),
            short(b)
        )),
        _ => {}
    }
}

fn short(v: &Value) -> String {
    let s = v.to_string();
    if s.chars().count() > 400 {
        format!("{}...", s.chars().take(400).collect::<String>())
    } else {
        s
    }
}

/// The screen text an interactive scenario compares: each line that contains one of the
/// patterns (the whole line), or else matches one as a regex (only the matched part, so a
/// pattern can leave out numbers that legitimately differ).
fn screen_lines(screen: &[String], patterns: &[String]) -> Vec<String> {
    let regexes: Vec<regex::Regex> = patterns
        .iter()
        .filter_map(|p| regex::Regex::new(p).ok())
        .collect();
    screen
        .iter()
        .filter_map(|l| {
            if patterns.iter().any(|p| l.contains(p.as_str())) {
                return Some(l.clone());
            }
            regexes
                .iter()
                .find_map(|r| r.find(l).map(|m| m.as_str().to_string()))
        })
        .collect()
}

fn compare(
    cs: &RunResult,
    rs: &RunResult,
    strict: bool,
    interactive: Option<&pty::Interactive>,
    usage_log: bool,
    unordered: bool,
) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(inter) = interactive {
        // Every step must pass on both sides (a step failing on both sides still fails).
        for (side, r) in [("C#", cs), ("Rust", rs)] {
            for f in &r.steps {
                out.push(format!("{side}: {f}"));
            }
        }
        diff(
            "screen",
            &json!(screen_lines(&cs.screen, &inter.compare_screen)),
            &json!(screen_lines(&rs.screen, &inter.compare_screen)),
            &mut out,
        );
    }
    diff("output", &cs.output, &rs.output, &mut out);
    if cs.requests.len() != rs.requests.len() {
        out.push(format!(
            "requests: C# sent {}, Rust sent {}",
            cs.requests.len(),
            rs.requests.len()
        ));
    }
    let prepare = |requests: &[Value]| {
        let mut v: Vec<Value> = requests
            .iter()
            .map(|r| {
                if strict {
                    r.clone()
                } else {
                    mask_known_gaps(r.clone())
                }
            })
            .collect();
        if unordered {
            v.sort_by_cached_key(|r| r.to_string());
        }
        v
    };
    let (c_requests, r_requests) = (prepare(&cs.requests), prepare(&rs.requests));
    for (i, (a, b)) in c_requests.iter().zip(&r_requests).enumerate() {
        diff(&format!("request[{i}]"), a, b, &mut out);
    }
    diff(
        "image_requests",
        &json!(cs.image_requests),
        &json!(rs.image_requests),
        &mut out,
    );
    let files = |r: &RunResult| serde_json::to_value(&r.files).unwrap();
    diff("files", &files(cs), &files(rs), &mut out);
    let home = |r: &RunResult| {
        let mut v = serde_json::to_value(&r.home_files).unwrap();
        // Saved sessions carry the system prompt: masked like the requests' one.
        for (key, session) in v.as_object_mut().into_iter().flatten() {
            if !strict
                && key.starts_with("sessions/")
                && let Some(text) = session["systemPrompt"].as_str()
            {
                session["systemPrompt"] = json!(mask_tool_derived_prompt(text));
            }
        }
        v
    };
    diff("home", &home(cs), &home(rs), &mut out);
    if cs.exit_code != rs.exit_code {
        out.push(format!(
            "exit code: C# {:?}, Rust {:?}",
            cs.exit_code, rs.exit_code
        ));
    }
    diff("notices", &json!(cs.notices), &json!(rs.notices), &mut out);
    diff("probes", &json!(cs.probes), &json!(rs.probes), &mut out);
    if usage_log {
        diff(
            "usage_log",
            &json!(cs.usage_log),
            &json!(rs.usage_log),
            &mut out,
        );
    }
    out
}

fn arg_value(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    ctrl_c::stop_ignoring();
    let csharp =
        PathBuf::from(arg_value(&args, "--csharp").context("--csharp <openorca.exe> is required")?);
    let rust = PathBuf::from(arg_value(&args, "--rust").context("--rust <abhoth> is required")?);
    let dir = PathBuf::from(
        arg_value(&args, "--scenarios").unwrap_or_else(|| "crosscheck/scenarios".into()),
    );
    let only = arg_value(&args, "--only");
    let out_dir = arg_value(&args, "--out").map(PathBuf::from);
    let strict = args.iter().any(|a| a == "--strict");

    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    paths.sort();

    let (mut passed, mut failed) = (0, 0);
    for path in paths {
        let mut scenario: Scenario = serde_json::from_str(&std::fs::read_to_string(&path)?)
            .with_context(|| format!("parsing {}", path.display()))?;
        if scenario.name.is_empty() {
            scenario.name = path.file_stem().unwrap().to_string_lossy().into_owned();
        }
        if only.as_deref().is_some_and(|o| o != scenario.name) {
            continue;
        }
        let cs = run_one(&csharp, &scenario)
            .await
            .with_context(|| format!("{}: C# run", scenario.name))?;
        let rs = run_one(&rust, &scenario)
            .await
            .with_context(|| format!("{}: Rust run", scenario.name))?;
        if let Some(out) = &out_dir {
            std::fs::create_dir_all(out)?;
            for (tag, r) in [("csharp", &cs), ("rust", &rs)] {
                let mut dump = json!({"output": r.output, "requests": r.requests, "files": r.files, "home_files": r.home_files, "exit_code": r.exit_code, "image_requests": r.image_requests, "notices": r.notices, "probes": r.probes, "stderr": r.stderr, "usage_log": r.usage_log});
                if scenario.interactive.is_some() {
                    dump["failed_steps"] = json!(r.steps);
                    // The semantic screen, each `snap`, and the raw VT stream.
                    let base = format!("{}.{tag}", scenario.name);
                    std::fs::write(
                        out.join(format!("{base}.screen.txt")),
                        r.screen.join("\n") + "\n",
                    )?;
                    for (i, (label, shown)) in r.snaps.iter().enumerate() {
                        std::fs::write(
                            out.join(format!("{base}.snap{:02}-{label}.txt", i + 1)),
                            shown,
                        )?;
                    }
                    std::fs::write(out.join(format!("{base}.raw.ansi")), &r.raw)?;
                }
                std::fs::write(
                    out.join(format!("{}.{tag}.json", scenario.name)),
                    serde_json::to_string_pretty(&dump)?,
                )?;
            }
        }
        let diffs = compare(
            &cs,
            &rs,
            strict || scenario.strict,
            scenario.interactive.as_ref(),
            scenario.usage_log,
            scenario.unordered_requests,
        );
        if diffs.is_empty() {
            passed += 1;
            println!("PASS  {}", scenario.name);
        } else {
            failed += 1;
            println!("DIFF  {}", scenario.name);
            for d in diffs.iter().take(20) {
                println!("  {d}");
            }
            if diffs.len() > 20 {
                println!("  ... {} more", diffs.len() - 20);
            }
        }
    }
    println!("\n{passed} passed, {failed} differ");
    if failed > 0 {
        bail!("{failed} scenario(s) differ");
    }
    Ok(())
}
