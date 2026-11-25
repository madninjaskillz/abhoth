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
//! - the files in the work folder afterwards.
//!
//! Known, documented gaps are ignored by default: the system prompt text and the advertised
//! tool list (phase 1 has a compact prompt and only the core tools). `--strict` compares them too.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use orca_testkit::{FakeOpenAi, Reply};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Debug, Deserialize)]
struct Scenario {
    #[serde(default)]
    name: String,
    prompt: String,
    #[serde(default)]
    files: BTreeMap<String, String>,
    /// Extra config.json keys (merged over the harness defaults, one level deep).
    #[serde(default)]
    config: serde_json::Map<String, Value>,
    replies: Vec<ScriptReply>,
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
    fn to_reply(&self) -> Reply {
        Reply {
            text: self.text.clone(),
            reasoning: self.reasoning.clone(),
            tool_calls: self
                .tools
                .iter()
                .map(|t| {
                    (
                        t.name.clone(),
                        t.raw.clone().unwrap_or_else(|| t.args.to_string()),
                    )
                })
                .collect(),
            finish_reason: self.finish_reason.clone(),
            prompt_tokens: 100,
            completion_tokens: if self.tools.is_empty() { 10 } else { 20 },
            ..Reply::default()
        }
    }
}

struct RunResult {
    output: Value,
    requests: Vec<Value>,
    files: BTreeMap<String, String>,
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
    let server =
        FakeOpenAi::start(scenario.replies.iter().map(ScriptReply::to_reply).collect()).await;
    let home = tempfile::tempdir()?;
    let work = tempfile::tempdir()?;
    let mut config = base_config(&server.base_url());
    merge(&mut config, &scenario.config);
    std::fs::write(
        home.path().join("config.json"),
        serde_json::to_string_pretty(&config)?,
    )?;
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

    let (exe_owned, work_path, home_path, prompt) = (
        exe.to_path_buf(),
        work.path().to_path_buf(),
        home.path().to_path_buf(),
        scenario.prompt.clone(),
    );
    let out = tokio::task::spawn_blocking(move || {
        Command::new(exe_owned)
            .args(["--print", &prompt])
            .current_dir(&work_path)
            .env("OPENORCA_HOME", &home_path)
            .env_remove("RUST_LOG")
            .output()
    })
    .await??;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let json_start = stdout.find('{').unwrap_or(0);
    let output: Value = serde_json::from_str(stdout[json_start..].trim()).with_context(|| {
        format!(
            "{} printed no JSON.\nstdout: {stdout}\nstderr: {stderr}",
            exe.display()
        )
    })?;

    let work_str = work.path().display().to_string();
    let home_str = home.path().display().to_string();
    let norm = |v: Value| normalize(v, &[(&work_str, "<WORK>"), (&home_str, "<HOME>")]);
    let mut files = BTreeMap::new();
    for e in walkdir::WalkDir::new(work.path())
        .into_iter()
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
    Ok(RunResult {
        output: norm(output),
        requests: server
            .requests()
            .into_iter()
            .map(|r| norm(r.body))
            .collect(),
        files,
        stderr,
    })
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

/// Remove what phase 1 knowingly does differently (see PORTING.md, "Known gaps").
fn mask_known_gaps(mut request: Value) -> Value {
    if let Some(o) = request.as_object_mut() {
        o.remove("tools");
        if let Some(Value::Array(msgs)) = o.get_mut("messages")
            && let Some(first) = msgs.first_mut()
            && first["role"] == "system"
        {
            first["content"] = json!("<system prompt>");
        }
    }
    request
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
                let dump = json!({"output": r.output, "requests": r.requests, "files": r.files, "stderr": r.stderr});
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
