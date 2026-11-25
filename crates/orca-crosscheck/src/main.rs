//! Cross-check harness: the same scripted session through the C# and the Rust `openorca`.
//!
//! ```text
//! cargo run -p orca-crosscheck -- --csharp ..\src\OpenOrca.Cli\bin\Release\net10.0\openorca.exe \
//!     --rust target\debug\openorca.exe [--scenarios crosscheck\scenarios] [--only name] [--out dir]
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
//! - the exit code and the session notices on stderr (resumed / not found / none to continue).
//!
//! A scenario can plant files in the config home first (`home_files`, e.g. a saved session;
//! `<WORK_JSON>` in them becomes the work folder as a JSON string body) and pass extra
//! arguments (`args`, e.g. `["-r", "s1"]`). The fake server can answer other endpoints
//! (`server_routes`: `{"GET /health": [200, "{...}"]}`); every such request (`probes`, e.g. the
//! startup context-window probes) is compared too. A reply can stream its reasoning in small
//! chunks (`reasoning_chunk_chars`), open with a null-content role chunk
//! (`null_role_content`), break off after some events (`die_after_events`) or be an HTTP error
//! (`error`: `[status, body]`).
//!
//! For the git tools a scenario can run `setup` commands in the work folder after its files
//! are written (e.g. `["git", "init"]`) with extra environment variables (`env`, also given to
//! both runs; `<HOME>` in a value becomes the config home), so fixed author/committer dates
//! give the same commit hashes in both runs. `.git/` is left out of the file comparison.
//!
//! For the web tools a scenario can serve pages from a local server (`web`: path -> status,
//! content type, body); `<WEB>` in the prompt and the scripted tool arguments becomes its base
//! URL, and `<CLOSED>` becomes `127.0.0.1:<port>` of a port nothing listens on. Both are
//! normalised back in the results.
//!
//! Known, documented gaps are ignored by default: the advertised tool list (the `tools` array
//! and the tool-derived parts of the system prompt: the AVAILABLE TOOLS lines and the
//! lead_mode tool's LEAD MODE section), since Rust has only the core tools until phase 3. The
//! rest of the system prompt is compared. `--strict` compares everything.

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
    prompt: String,
    #[serde(default)]
    files: BTreeMap<String, String>,
    /// Files to create in the config home (relative paths), e.g. `sessions/<id>.json`.
    #[serde(default)]
    home_files: BTreeMap<String, String>,
    /// Arguments after `--print <prompt>`.
    #[serde(default)]
    args: Vec<String>,
    /// Extra config.json keys (merged over the harness defaults, one level deep).
    #[serde(default)]
    config: serde_json::Map<String, Value>,
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
                (false, true) => 10,
                (false, false) => 20,
            },
            reasoning_chunk_chars: self.reasoning_chunk_chars,
            null_role_content: self.null_role_content,
            die_after_events: self.die_after_events,
            error: self.error.clone(),
            ..Reply::default()
        }
    }
}

struct RunResult {
    output: Value,
    requests: Vec<Value>,
    files: BTreeMap<String, String>,
    home_files: BTreeMap<String, Value>,
    exit_code: Option<i32>,
    notices: Vec<String>,
    probes: Vec<String>,
    stderr: String,
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
    std::fs::write(
        home.path().join("config.json"),
        serde_json::to_string_pretty(&config)?,
    )?;
    let work_json = serde_json::to_string(&work.path().display().to_string())?;
    let work_json = &work_json[1..work_json.len() - 1];
    for (rel, content) in &scenario.home_files {
        let p = home.path().join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&p, content.replace("<WORK_JSON>", work_json))?;
    }
    // Fixed, distinct mtimes (one second apart, in name order, ten minutes ago) so tools that
    // sort or describe by age give the same answer in both runs.
    let base = std::time::SystemTime::now() - std::time::Duration::from_secs(600);
    for (i, (rel, content)) in scenario.files.iter().enumerate() {
        let p = work.path().join(rel);
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
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
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
            ],
        )
    };
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
            files.insert(
                rel,
                String::from_utf8_lossy(&std::fs::read(e.path())?).to_string(),
            );
        }
    }
    let mut home_files = BTreeMap::new();
    let checkpoints = home.path().join("checkpoints");
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
            (
                format!("{dir}{}", normalize_backup_name(name, None)),
                json!(String::from_utf8_lossy(&bytes)),
            )
        };
        home_files.insert(key, value);
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
            ]
            .iter()
            .any(|p| l.starts_with(p))
        })
        .map(str::to_string)
        .collect();

    Ok(RunResult {
        output: norm(output),
        home_files,
        exit_code: out.status.code(),
        notices,
        probes: server.probes(),
        requests: server
            .requests()
            .into_iter()
            .map(|r| norm(r.body))
            .collect(),
        files,
        stderr,
    })
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
            Value::String(s)
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

fn compare(cs: &RunResult, rs: &RunResult, strict: bool) -> Vec<String> {
    let mut out = Vec::new();
    diff("output", &cs.output, &rs.output, &mut out);
    if cs.requests.len() != rs.requests.len() {
        out.push(format!(
            "requests: C# sent {}, Rust sent {}",
            cs.requests.len(),
            rs.requests.len()
        ));
    }
    for (i, (a, b)) in cs.requests.iter().zip(&rs.requests).enumerate() {
        let (a, b) = if strict {
            (a.clone(), b.clone())
        } else {
            (mask_known_gaps(a.clone()), mask_known_gaps(b.clone()))
        };
        diff(&format!("request[{i}]"), &a, &b, &mut out);
    }
    let files = |r: &RunResult| serde_json::to_value(&r.files).unwrap();
    diff("files", &files(cs), &files(rs), &mut out);
    let home = |r: &RunResult| serde_json::to_value(&r.home_files).unwrap();
    diff("home", &home(cs), &home(rs), &mut out);
    if cs.exit_code != rs.exit_code {
        out.push(format!(
            "exit code: C# {:?}, Rust {:?}",
            cs.exit_code, rs.exit_code
        ));
    }
    diff("notices", &json!(cs.notices), &json!(rs.notices), &mut out);
    diff("probes", &json!(cs.probes), &json!(rs.probes), &mut out);
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
    let csharp =
        PathBuf::from(arg_value(&args, "--csharp").context("--csharp <openorca.exe> is required")?);
    let rust = PathBuf::from(arg_value(&args, "--rust").context("--rust <openorca> is required")?);
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
                let dump = json!({"output": r.output, "requests": r.requests, "files": r.files, "home_files": r.home_files, "exit_code": r.exit_code, "notices": r.notices, "probes": r.probes, "stderr": r.stderr});
                std::fs::write(
                    out.join(format!("{}.{tag}.json", scenario.name)),
                    serde_json::to_string_pretty(&dump)?,
                )?;
            }
        }
        let diffs = compare(&cs, &rs, strict);
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
