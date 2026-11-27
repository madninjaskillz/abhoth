//! Host-local sources the Python dash polled, as feed sections (SUPERVISOR.md 2.5), so Glance
//! needs nothing from the old dash:
//!
//! - **host.json** (the dash's root collector, `collector/hoststat.py`): `storage` (ZFS pools),
//!   `containers`, `ooms`, and the engine containers with their safe command-line hints
//!   (port, model, `--max-concurrency`, `--request-log-jsonl`) for `models`;
//! - **ninfer request logs** (JSONL, read incrementally): `models` per container: tok/s,
//!   running, queued (the latest `throughput` record, if under 15 s old), request totals since
//!   the server started, and the last 8 requests. Each server's tok/s is also a reading,
//!   `engine.<container>.tok_s` (and `engine.all.tok_s`), so it has history;
//!   Request log paths are host paths; `path_map` says where they are mounted here. host.json's
//!   GPU processes are also `gpu_procs`, by card;
//! - **daily job run logs** (`pipelines`: the video pipelines' newest `run_daily_<date>.log`):
//!   `pipelines`, status as the old dash read it (done / running / stopped, errors, title);
//! - **the image API** (`GET <image_url>`): `image`;
//! - **service checks** (HTTP below 500, or a TCP connect, or the fan feed's freshness):
//!   `services`.
//!
//! Engines other than ninfer (Strata `/metrics`, llama.cpp `/slots`) are listed in `models`
//! without live metrics in this build.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use orca_sensors::feed::{TelemetryFeed, TelemetrySink, sections};
use orca_sensors::{Namespace, Reading, SensorId, Unit, now_ms};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::TelemetryHub;

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LocalConfig {
    /// The dash collector's `host.json` (pools, containers, OOM kills, engine hints).
    #[serde(default)]
    pub host_json: Option<PathBuf>,
    /// Older than this, `storage.stale` is true (and containers / models are still shown).
    #[serde(default = "d_stale")]
    pub host_json_stale_seconds: u64,
    /// The image API's health URL (`http://127.0.0.1:8189/health`).
    #[serde(default)]
    pub image_url: Option<String>,
    #[serde(default)]
    pub services: Vec<ServiceCheck>,
    #[serde(default = "d_interval")]
    pub interval_seconds: u64,
    /// `[host prefix, path here]` pairs for the paths host.json names (the ninfer request
    /// logs are host paths: `["/mnt/Storage/ai/ninfer/logs", "/ninfer-logs"]` in a container).
    #[serde(default)]
    pub path_map: Vec<(String, String)>,
    /// Daily jobs whose latest run log is shown (the video pipelines), as `pipelines`.
    #[serde(default)]
    pub pipelines: Vec<PipelineLog>,
}

/// A daily job's run logs: `logs` is a directory and a file pattern with one `*`
/// (`/video-logs/ai-news/run_daily_*.log`); the newest dated file (`…YYYY-MM-DD.log`) counts.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PipelineLog {
    pub name: String,
    pub logs: String,
}

fn d_stale() -> u64 {
    120
}
fn d_interval() -> u64 {
    5
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CheckKind {
    /// `GET http://<host>:<port><path>`: up on any answer below 500.
    Http,
    /// A TCP connect.
    Tcp,
    /// The fan feed: up while every fan in `fans` is fresh (35 s) and not failsafe.
    Fans,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ServiceCheck {
    pub name: String,
    #[serde(default)]
    pub group: Option<String>,
    pub kind: CheckKind,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub path: Option<String>,
    /// Default `127.0.0.1`.
    #[serde(default)]
    pub host: Option<String>,
}

impl LocalConfig {
    pub fn check(&self) -> Vec<String> {
        let mut out = Vec::new();
        for s in &self.services {
            match s.kind {
                CheckKind::Http if s.port.is_none() || s.path.is_none() => out.push(format!(
                    "local.services.{}: an http check needs port and path",
                    s.name
                )),
                CheckKind::Tcp if s.port.is_none() => out.push(format!(
                    "local.services.{}: a tcp check needs a port",
                    s.name
                )),
                _ => {}
            }
        }
        if self.interval_seconds == 0 {
            out.push("local.interval_seconds must be at least 1".into());
        }
        for p in &self.pipelines {
            let name = Path::new(&p.logs)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");
            if name.matches('*').count() != 1 || p.logs.matches('*').count() != 1 {
                out.push(format!(
                    "local.pipelines.{}: logs needs one * in the file name",
                    p.name
                ));
            }
        }
        out
    }
}

/// host.json → `storage`, `containers` and `ooms`. `gpu_labels` is GPU UUID → card label.
pub fn host_json_sections(
    hj: &Value,
    now_s: f64,
    stale_s: u64,
    gpu_labels: &BTreeMap<String, String>,
) -> Vec<(&'static str, Value)> {
    let age = hj["time"].as_f64().map(|t| (now_s - t).max(0.0));
    let stale = age.is_none_or(|a| a > stale_s as f64);
    let containers: Vec<Value> = hj["containers"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|c| {
                    let gpus: Vec<&str> = c["gpus"]
                        .as_array()
                        .map(|g| {
                            g.iter()
                                .filter_map(|u| u.as_str())
                                .map(|u| gpu_labels.get(u).map(String::as_str).unwrap_or(u))
                                .collect()
                        })
                        .unwrap_or_default();
                    json!({"name": c["name"], "image": c["image"], "state": c["state"],
                        "health": c["health"], "restarts": c["restarts"], "ports": c["ports"],
                        "started_at": c["started_at"], "oom_killed": c["oom_killed"], "gpus": gpus})
                })
                .collect()
        })
        .unwrap_or_default();
    let ooms: Vec<Value> = hj["ooms"]
        .as_array()
        .map(|a| a.iter().rev().take(12).rev().cloned().collect())
        .unwrap_or_default();
    let mut procs = serde_json::Map::new();
    for p in hj["gpu_procs"].as_array().into_iter().flatten() {
        let Some(u) = p["gpu"].as_str() else {
            continue;
        };
        let card = gpu_labels.get(u).cloned().unwrap_or_else(|| u.to_string());
        if let Value::Array(a) = procs.entry(card).or_insert_with(|| json!([])) {
            a.push(json!({"pid": p["pid"], "process": p["process"],
                "container": p["container"], "mem_mib": p["mem_mib"]}));
        }
    }
    vec![
        (sections::GPU_PROCS, Value::Object(procs)),
        (
            sections::STORAGE,
            json!({"pools": hj["pools"].as_array().cloned().unwrap_or_default(),
                "age_s": age.map(|a| a.round()), "stale": stale}),
        ),
        (sections::CONTAINERS, Value::Array(containers)),
        (sections::OOMS, Value::Array(ooms)),
    ]
}

/// The image API's `/health` → `image`.
pub fn image_section(d: &Value) -> Value {
    const KEYS: [&str; 11] = [
        "name",
        "enabled",
        "healthy",
        "inflight",
        "unet",
        "steps",
        "served",
        "failed",
        "last_ok_age_s",
        "queue_running",
        "queue_pending",
    ];
    let backends: Vec<Value> = d["backends"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|b| {
                    Value::Object(
                        KEYS.iter()
                            .filter_map(|k| b.get(*k).map(|v| (k.to_string(), v.clone())))
                            .collect(),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    json!({"status": d["status"], "backends": backends})
}

/// One ninfer request log, read incrementally (`--request-log-jsonl`).
#[derive(Debug, Default)]
pub struct NinferLog {
    pos: u64,
    server: Option<String>,
    since_s: Option<f64>,
    requests: u64,
    output_tokens: u64,
    prompt_tokens: u64,
    reused: u64,
    decode_s: f64,
    recent: VecDeque<Value>,
    /// The latest throughput record: (ts ms, decode tok/s, running, waiting).
    throughput: Option<(i64, f64, u64, u64)>,
}

/// At most this much of a log per poll (a long log catches up over several polls).
const CHUNK: u64 = 16 * 1024 * 1024;
const FRESH_MS: i64 = 15_000;

impl NinferLog {
    /// Reads what was appended since the last call. A shorter file (rotated) starts over.
    pub fn read(&mut self, path: &Path) -> std::io::Result<()> {
        let len = std::fs::metadata(path)?.len();
        if len < self.pos {
            *self = NinferLog::default();
        }
        if len == self.pos {
            return Ok(());
        }
        let mut f = std::fs::File::open(path)?;
        f.seek(SeekFrom::Start(self.pos))?;
        let mut buf = Vec::new();
        f.take(CHUNK).read_to_end(&mut buf)?;
        let end = buf.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
        self.feed(&buf[..end]);
        self.pos += end as u64;
        Ok(())
    }

    /// Complete lines only.
    pub fn feed(&mut self, chunk: &[u8]) {
        for line in chunk.split(|b| *b == b'\n') {
            let has = |n: &[u8]| line.windows(n.len()).any(|w| w == n);
            if !(has(b"\"request_done\"") || has(b"\"server_start\"") || has(b"\"throughput")) {
                continue;
            }
            let Ok(e) = serde_json::from_slice::<Value>(line) else {
                continue;
            };
            let ts = e["timestamp_unix_ms"].as_i64().unwrap_or(0);
            if let Some(tp) = e["throughput_tokens_per_second"].as_object() {
                let sch = &e["scheduler"];
                self.throughput = Some((
                    ts,
                    tp.get("decode").and_then(Value::as_f64).unwrap_or(0.0),
                    sch["running"].as_u64().unwrap_or(0),
                    sch["waiting"].as_u64().unwrap_or(0),
                ));
            }
            let ev = e["event"].as_str().unwrap_or("");
            if ev != "request_done" && ev != "server_start" {
                continue;
            }
            let sid = e["server_instance_id"].as_str().map(String::from);
            if ev == "server_start" || sid != self.server {
                self.server = sid;
                self.since_s = (ts > 0).then_some(ts as f64 / 1000.0);
                self.requests = 0;
                self.output_tokens = 0;
                self.prompt_tokens = 0;
                self.reused = 0;
                self.decode_s = 0.0;
                self.recent.clear();
                if ev == "server_start" {
                    continue;
                }
            }
            let r = &e["result"];
            let t = &e["timings_seconds"];
            let out = r["completion_tokens"].as_u64().unwrap_or(0);
            let decode = t["decode"].as_f64().unwrap_or(0.0);
            self.requests += 1;
            self.output_tokens += out;
            self.prompt_tokens += r["prompt_tokens"].as_u64().unwrap_or(0);
            self.reused += r["prefix_cache_hit_tokens"].as_u64().unwrap_or(0);
            self.decode_s += decode;
            self.recent.push_front(json!({
                "time": ts as f64 / 1000.0,
                "duration_s": t["total"],
                "prompt": r["prompt_tokens"],
                "reused": r["prefix_cache_hit_tokens"],
                "out": out,
                "tok_s": (decode > 0.0).then(|| (out as f64 / decode * 10.0).round() / 10.0),
                "ttft_ms": t["ttft"].as_f64().map(|x| (x * 1000.0).round()),
                "finish": r["finish_reason"],
            }));
            self.recent.truncate(8);
        }
    }

    /// (tok/s, running, queued) now: the latest throughput record if fresh, else idle.
    pub fn live(&self, now: i64) -> (f64, u64, u64) {
        match self.throughput {
            Some((ts, tok, run, wait)) if now - ts < FRESH_MS => (tok, run, wait),
            _ => (0.0, 0, 0),
        }
    }

    pub fn summary(&self, now: i64) -> Value {
        let (tok, running, queued) = self.live(now);
        json!({
            "tok_s": (tok * 10.0).round() / 10.0,
            "running": running,
            "queued": queued,
            "state": if running > 0 || tok > 0.0 { "generating" } else { "idle" },
            "since": self.since_s,
            "requests": self.requests,
            "output_tokens": self.output_tokens,
            "prompt_tokens": self.prompt_tokens,
            "prefix_reused": self.reused,
            "decode_tok_s": (self.decode_s > 0.0)
                .then(|| (self.output_tokens as f64 / self.decode_s * 10.0).round() / 10.0),
            "recent": self.recent.iter().cloned().collect::<Vec<_>>(),
        })
    }
}

/// A host path as seen here: the first `path_map` prefix that matches (whole segments) is
/// replaced.
pub fn map_path(path: &str, map: &[(String, String)]) -> PathBuf {
    for (from, to) in map {
        let from = from.trim_end_matches('/');
        if let Some(rest) = path.strip_prefix(from)
            && (rest.is_empty() || rest.starts_with('/'))
        {
            return PathBuf::from(format!("{}{rest}", to.trim_end_matches('/')));
        }
    }
    PathBuf::from(path)
}

/// The engine containers in host.json: `(container, port, hints)`, those with a model hint or a
/// request log.
fn engine_containers(hj: &Value) -> Vec<(String, Option<u64>, Value)> {
    hj["containers"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|c| {
                    let h = &c["hints"];
                    h["model_id"].is_string()
                        || h["model_file"].is_string()
                        || h["request_log"].is_string()
                })
                .filter_map(|c| {
                    Some((
                        c["name"].as_str()?.to_string(),
                        c["ports"].as_array().and_then(|p| p.first()?.as_u64()),
                        c["hints"].clone(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// host.json + the request logs → the `models` section and the tok/s readings. Reads the
/// logs (blocking).
pub fn models_section(
    hj: &Value,
    logs: &mut HashMap<String, NinferLog>,
    gpu_labels: &BTreeMap<String, String>,
    path_map: &[(String, String)],
    now: i64,
) -> (Value, Vec<(SensorId, Reading)>) {
    let mut servers = serde_json::Map::new();
    let mut readings = Vec::new();
    let mut total = 0.0;
    let procs = hj["gpu_procs"].as_array().cloned().unwrap_or_default();
    let started = |name: &str| {
        hj["containers"]
            .as_array()
            .and_then(|a| a.iter().find(|c| c["name"].as_str() == Some(name)))
            .map(|c| c["started_at"].clone())
            .unwrap_or(Value::Null)
    };
    let mut seen = Vec::new();
    for (name, port, hints) in engine_containers(hj) {
        let mut cards: Vec<String> = procs
            .iter()
            .filter(|p| p["container"].as_str() == Some(name.as_str()))
            .filter_map(|p| p["gpu"].as_str())
            .map(|u| gpu_labels.get(u).cloned().unwrap_or_else(|| u.to_string()))
            .collect();
        cards.sort();
        cards.dedup();
        let mut m = json!({
            "port": port,
            "model": hints.get("model_id").or(hints.get("model_file")),
            "lanes": hints["max_concurrency"].as_str().and_then(|s| s.parse::<u64>().ok()),
            "cards": cards,
            "started_at": started(&name),
        });
        if let Some(path) = hints["request_log"].as_str() {
            let log = logs.entry(name.clone()).or_default();
            if let Err(e) = log.read(&map_path(path, path_map)) {
                m["error"] = json!(format!("request log: {e}"));
            }
            m["engine"] = json!("ninfer");
            if let (Value::Object(o), Value::Object(s)) = (&mut m, log.summary(now)) {
                o.extend(s);
            }
            let tok = log.live(now).0;
            total += tok;
            readings.push((
                SensorId::of(Namespace::Engine, &[name.as_str(), "tok_s"]),
                Reading::new(tok, Unit::None, now, "supervisor/ninfer-log"),
            ));
        } else {
            m["engine"] = json!(if name.starts_with("strata") {
                "strata"
            } else {
                "unknown"
            });
        }
        seen.push(name.clone());
        servers.insert(name, m);
    }
    logs.retain(|k, _| seen.contains(k));
    readings.push((
        SensorId::of(Namespace::Engine, &["all", "tok_s"]),
        Reading::new(total, Unit::None, now, "supervisor/ninfer-log"),
    ));
    (
        json!({"servers": servers, "total_tok_s": (total * 10.0).round() / 10.0}),
        readings,
    )
}

/// `YYYY-MM-DD` at the end of a log file name (`run_daily_2026-11-26.log`).
fn log_date(file: &str) -> Option<&str> {
    let stem = file.strip_suffix(".log")?;
    let d = stem.get(stem.len().checked_sub(10)?..)?;
    let ok = d.bytes().enumerate().all(|(i, c)| {
        if i == 4 || i == 7 {
            c == b'-'
        } else {
            c.is_ascii_digit()
        }
    });
    ok.then_some(d)
}

/// The newest dated run log matching `pattern` (a directory and a file name with one `*`).
pub fn latest_log(pattern: &str) -> Option<PathBuf> {
    let p = Path::new(pattern);
    let (pre, post) = p.file_name()?.to_str()?.split_once('*')?;
    let mut best: Option<(String, String, PathBuf)> = None;
    for e in std::fs::read_dir(p.parent()?).ok()?.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.len() < pre.len() + post.len() || !name.starts_with(pre) || !name.ends_with(post) {
            continue;
        }
        let Some(date) = log_date(name) else { continue };
        let key = (date.to_string(), name.to_string());
        if best
            .as_ref()
            .is_none_or(|(d, n, _)| (d.as_str(), n.as_str()) < (key.0.as_str(), key.1.as_str()))
        {
            best = Some((key.0, key.1, e.path()));
        }
    }
    best.map(|(_, _, p)| p)
}

/// The last `n` characters.
fn tail_chars(s: &str, n: usize) -> &str {
    match s.char_indices().rev().nth(n.saturating_sub(1)) {
        Some((i, _)) if s.chars().count() > n => &s[i..],
        _ => s,
    }
}

/// An error line, as the old dash counted them (`Traceback|\bERROR\b|FAILED|\[fail`).
fn is_error_line(l: &str) -> bool {
    if l.contains("Traceback") || l.contains("FAILED") || l.contains("[fail") {
        return true;
    }
    let word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
    l.match_indices("ERROR")
        .any(|(i, m)| !word(l[..i].chars().next_back()) && !word(l[i + m.len()..].chars().next()))
}

/// One run log's text → the `pipelines` entry (without `status`, which depends on the clock).
pub fn parse_run_log(name: &str, file: &str, text: &str, mtime: f64) -> Value {
    let lines: Vec<&str> = text.lines().collect();
    let done = lines.iter().rev().find(|l| l.contains("=== done")).copied();
    let errs: Vec<&str> = lines.iter().copied().filter(|l| is_error_line(l)).collect();
    let title = lines
        .iter()
        .filter_map(|l| {
            let (_, rest) = l.split_once("notify [ok] ")?;
            Some(rest.split_once('|')?.0.trim_end())
        })
        .next_back();
    const STUDIO: &str = "https://studio.youtube.com/video/";
    let video = lines.iter().find_map(|l| {
        let (_, rest) = l.split_once(STUDIO)?;
        let id: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
            .collect();
        (!id.is_empty()).then_some(id)
    });
    let minutes = done.and_then(|d| {
        let (_, rest) = d.split_once("done in ")?;
        rest.split_once(" min")?.0.trim().parse::<u64>().ok()
    });
    json!({
        "name": name,
        "file": file,
        "date": log_date(file),
        "done": done.is_some(),
        "minutes": minutes,
        "mtime": mtime,
        "errors": errs.len(),
        "last_error": errs.last().map(|l| tail_chars(l, 200)),
        "title": title,
        "video": video,
        "last_line": lines.last().map(|l| tail_chars(l, 200)).unwrap_or(""),
    })
}

/// `done`; else `running` while the log was written in the last 10 minutes; else `stopped`.
pub fn pipeline_status(entry: &Value, now_s: f64) -> &'static str {
    if entry["done"] == Value::Bool(true) {
        "done"
    } else if entry["mtime"].as_f64().is_some_and(|m| now_s - m < 600.0) {
        "running"
    } else {
        "stopped"
    }
}

/// The parsed run logs, re-read only when a file changes.
#[derive(Default)]
pub struct PipelineCache {
    seen: HashMap<String, (PathBuf, u64, f64, Value)>,
}

impl PipelineCache {
    /// The `pipelines` section: one entry per configured job (`status: "none"` without logs).
    pub fn section(&mut self, jobs: &[PipelineLog], now_s: f64) -> Value {
        let mut out = Vec::new();
        for j in jobs {
            let Some(path) = latest_log(&j.logs) else {
                out.push(json!({"name": j.name, "status": "none"}));
                continue;
            };
            let meta = std::fs::metadata(&path).ok();
            let len = meta.as_ref().map_or(0, |m| m.len());
            let mtime = meta
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0.0, |d| d.as_secs_f64());
            let fresh = self
                .seen
                .get(&j.name)
                .is_some_and(|(p, l, m, _)| *p == path && *l == len && *m == mtime);
            if !fresh {
                let file = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_string();
                let text = read_tail(&path, 200_000).unwrap_or_default();
                let v = parse_run_log(&j.name, &file, &text, mtime);
                self.seen.insert(j.name.clone(), (path, len, mtime, v));
            }
            let mut v = self.seen[&j.name].3.clone();
            v["status"] = json!(pipeline_status(&v, now_s));
            if let Some(o) = v.as_object_mut() {
                o.remove("done");
            }
            out.push(v);
        }
        self.seen.retain(|k, _| jobs.iter().any(|j| &j.name == k));
        Value::Array(out)
    }
}

/// The last `n` bytes of a file as text (the first, partial line dropped when cut).
fn read_tail(path: &Path, n: u64) -> std::io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    let start = len.saturating_sub(n);
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    let text = String::from_utf8_lossy(&buf).into_owned();
    Ok(if start > 0 {
        text.split_once('\n')
            .map(|(_, r)| r.to_string())
            .unwrap_or_default()
    } else {
        text
    })
}

/// `gpu.<card>.` in a sensor id or expression → the card.
fn gpu_card(s: &str) -> Option<String> {
    s.match_indices("gpu.").find_map(|(i, m)| {
        if s[..i]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
        {
            return None;
        }
        let rest = &s[i + m.len()..];
        let card: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
            .collect();
        (!card.is_empty() && rest[card.len()..].starts_with('.')).then_some(card)
    })
}

/// The policy tables' fans → `fan_policy`: each fan's card (from its load or temperature
/// input, through a `sensor.*` expression when it names one) and its curve numbers.
pub fn fan_policy_section(policy_toml: &str) -> Value {
    let Ok(t) = toml::from_str::<toml::Table>(policy_toml) else {
        return Value::Null;
    };
    let table = |k: &str| t.get(k).and_then(toml::Value::as_table);
    let defaults = table("fan_defaults").cloned().unwrap_or_default();
    let card_of = |s: &str| {
        gpu_card(s).or_else(|| {
            let expr = table("sensors")?
                .get(s.strip_prefix("sensor.")?)?
                .get("expr")?
                .as_str()?;
            gpu_card(expr)
        })
    };
    let mut out = serde_json::Map::new();
    for (id, f) in table("fans").into_iter().flatten() {
        let Some(f) = f.as_table() else { continue };
        let mut m = defaults.clone();
        if let Some(p) = f
            .get("profile")
            .and_then(toml::Value::as_str)
            .and_then(|p| table("fan_profiles")?.get(p)?.as_table())
        {
            m.extend(p.clone());
        }
        m.extend(f.clone());
        let card = ["load", "temp"]
            .iter()
            .filter_map(|k| m.get(*k)?.as_str())
            .find_map(card_of);
        let pick = |k: &str| {
            m.get(k)
                .and_then(|v| serde_json::to_value(v).ok())
                .unwrap_or(Value::Null)
        };
        out.insert(
            id.clone(),
            json!({"card": card, "backend": pick("backend"), "channel": pick("channel"),
                "mode": pick("mode"), "curve": pick("curve"), "min_duty": pick("min_duty"),
                "max_temp": pick("max_temp"), "feed_forward": pick("feed_forward"),
                "load_gate": pick("load_gate")}),
        );
    }
    Value::Object(out)
}

/// The fan feed is up while it has fans, each fresh (35 s) and not failsafe.
pub fn fans_up(fans: &Value, now: i64) -> bool {
    fans.as_object().is_some_and(|m| {
        !m.is_empty()
            && m.values().all(|f| {
                f["failsafe"].as_bool() != Some(true)
                    && f["ts"].as_i64().is_some_and(|t| now - t < 35_000)
            })
    })
}

async fn run_check(http: &reqwest::Client, s: &ServiceCheck, state: &Value) -> Value {
    let host = s.host.as_deref().unwrap_or("127.0.0.1");
    let t0 = std::time::Instant::now();
    let up = match s.kind {
        CheckKind::Fans => fans_up(&state["fans"], now_ms()),
        CheckKind::Tcp => {
            let addr = format!("{host}:{}", s.port.unwrap_or(0));
            matches!(
                tokio::time::timeout(Duration::from_secs(2), tokio::net::TcpStream::connect(addr))
                    .await,
                Ok(Ok(_))
            )
        }
        CheckKind::Http => {
            let url = format!(
                "http://{host}:{}{}",
                s.port.unwrap_or(0),
                s.path.as_deref().unwrap_or("/")
            );
            match http.get(url).send().await {
                Ok(r) => r.status().as_u16() < 500,
                Err(_) => false,
            }
        }
    };
    let ms = (up && s.kind != CheckKind::Fans).then(|| t0.elapsed().as_millis() as u64);
    json!({"name": s.name, "group": s.group, "kind": s.kind_name(), "port": s.port, "up": up, "ms": ms})
}

impl ServiceCheck {
    fn kind_name(&self) -> &'static str {
        match self.kind {
            CheckKind::Http => "http",
            CheckKind::Tcp => "tcp",
            CheckKind::Fans => "fans",
        }
    }
}

/// GPU UUID → card label from the `gpus` section.
fn gpu_labels(state: &Value) -> BTreeMap<String, String> {
    state["gpus"]
        .as_object()
        .map(|m| {
            m.iter()
                .filter_map(|(l, g)| Some((g["uuid"].as_str()?.to_string(), l.clone())))
                .collect()
        })
        .unwrap_or_default()
}

/// Polls the local sources every `interval_seconds` into the hub.
pub fn spawn(cfg: LocalConfig, hub: Arc<TelemetryHub>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_millis(2500))
            .build()
            .unwrap_or_default();
        let mut logs: HashMap<String, NinferLog> = HashMap::new();
        let mut runs = PipelineCache::default();
        let mut t = tokio::time::interval(Duration::from_secs(cfg.interval_seconds.max(1)));
        t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let feed: Arc<dyn TelemetryFeed> = hub.clone();
        loop {
            t.tick().await;
            let state = feed.snapshot().state;
            let labels = gpu_labels(&state);
            if let Some(p) = cfg.host_json.clone() {
                let stale = cfg.host_json_stale_seconds;
                let l2 = labels.clone();
                let map = cfg.path_map.clone();
                let mut lg = std::mem::take(&mut logs);
                let res = tokio::task::spawn_blocking(move || {
                    let hj: Value = std::fs::read(&p)
                        .ok()
                        .and_then(|b| serde_json::from_slice(&b).ok())
                        .unwrap_or(Value::Null);
                    let now = now_ms();
                    let secs = host_json_sections(&hj, now as f64 / 1000.0, stale, &l2);
                    let models = models_section(&hj, &mut lg, &l2, &map, now);
                    (hj.is_null(), secs, models, lg)
                })
                .await;
                match res {
                    Ok((missing, secs, (models, readings), lg)) => {
                        logs = lg;
                        if missing {
                            tracing::warn!("local: host.json unreadable");
                        }
                        for (name, v) in secs {
                            hub.set_section(name, v);
                        }
                        hub.set_section(sections::MODELS, models);
                        hub.put_readings(&readings);
                    }
                    Err(e) => tracing::warn!("local: {e}"),
                }
            }
            if !cfg.pipelines.is_empty() {
                let jobs = cfg.pipelines.clone();
                let mut c = std::mem::take(&mut runs);
                if let Ok((v, c)) = tokio::task::spawn_blocking(move || {
                    let v = c.section(&jobs, now_ms() as f64 / 1000.0);
                    (v, c)
                })
                .await
                {
                    runs = c;
                    hub.set_section(sections::PIPELINES, v);
                }
            }
            if let Some(u) = &cfg.image_url {
                let v = match http.get(u).send().await {
                    Ok(r) => match r.json::<Value>().await {
                        Ok(d) => image_section(&d),
                        Err(e) => json!({"status": "error", "error": e.to_string()}),
                    },
                    Err(e) => json!({"status": "down", "error": e.to_string()}),
                };
                hub.set_section(sections::IMAGE, v);
            }
            if !cfg.services.is_empty() {
                let checks = cfg.services.iter().map(|s| run_check(&http, s, &state));
                let out = futures_util::future::join_all(checks).await;
                hub.set_section(sections::SERVICES, Value::Array(out));
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels() -> BTreeMap<String, String> {
        [("GPU-a".to_string(), "A".to_string())].into()
    }

    #[test]
    fn host_json_to_sections() {
        let hj = json!({"time": 1000.0,
            "containers": [{"id": "x", "name": "ninfer-a", "image": "i", "state": "running", "health": "healthy",
                "restarts": 0, "oom_killed": false, "ports": [8094], "gpus": ["GPU-a"], "hints": {}}],
            "pools": [{"name": "Apps", "cap": 91, "health": "ONLINE"}],
            "ooms": (0..20).map(|i| json!({"n": i})).collect::<Vec<_>>()});
        let s: BTreeMap<_, _> = host_json_sections(&hj, 1030.0, 120, &labels())
            .into_iter()
            .collect();
        assert_eq!(s["storage"]["pools"][0]["name"], "Apps");
        assert_eq!(s["storage"]["stale"], false);
        assert_eq!(s["storage"]["age_s"], 30.0);
        assert_eq!(s["containers"][0]["gpus"], json!(["A"]));
        assert!(s["containers"][0].get("hints").is_none());
        assert_eq!(s["ooms"].as_array().unwrap().len(), 12);
        assert_eq!(s["ooms"][11]["n"], 19);
        let old: BTreeMap<_, _> = host_json_sections(&hj, 2000.0, 120, &labels())
            .into_iter()
            .collect();
        assert_eq!(old["storage"]["stale"], true);
    }

    #[test]
    fn ninfer_log_totals_live_and_restart() {
        let mut l = NinferLog::default();
        let lines = [
            r#"{"event":"server_start","server_instance_id":"s1","timestamp_unix_ms":1000}"#,
            r#"{"event":"request_done","server_instance_id":"s1","timestamp_unix_ms":2000,"result":{"completion_tokens":100,"prompt_tokens":50,"prefix_cache_hit_tokens":10,"finish_reason":"stop"},"timings_seconds":{"decode":2.0,"total":2.5,"ttft":0.1}}"#,
            r#"{"artifact_type":"ninfer_serve_request_log","timestamp_unix_ms":3000,"throughput_tokens_per_second":{"decode":42.5},"scheduler":{"running":2,"waiting":1}}"#,
            "not json",
        ];
        l.feed(lines.join("\n").as_bytes());
        let s = l.summary(4000);
        assert_eq!(s["requests"], 1);
        assert_eq!(s["output_tokens"], 100);
        assert_eq!(s["decode_tok_s"], 50.0);
        assert_eq!(s["tok_s"], 42.5);
        assert_eq!(s["running"], 2);
        assert_eq!(s["queued"], 1);
        assert_eq!(s["state"], "generating");
        assert_eq!(s["recent"][0]["ttft_ms"], 100.0);
        assert_eq!(s["since"], 1.0);
        // Stale throughput: idle.
        assert_eq!(l.live(3000 + FRESH_MS), (0.0, 0, 0));
        // A new server instance resets the totals.
        l.feed(br#"{"event":"request_done","server_instance_id":"s2","timestamp_unix_ms":5000,"result":{"completion_tokens":7},"timings_seconds":{}}"#);
        assert_eq!(l.summary(5000)["requests"], 1);
        assert_eq!(l.summary(5000)["output_tokens"], 7);
    }

    #[test]
    fn ninfer_log_reads_incrementally_and_restarts_on_truncation() {
        let dir = std::env::temp_dir().join(format!("orca-sv-local-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("req.jsonl");
        let done = |n: u64| {
            format!(
                "{{\"event\":\"request_done\",\"server_instance_id\":\"s\",\"timestamp_unix_ms\":1,\"result\":{{\"completion_tokens\":{n}}},\"timings_seconds\":{{}}}}\n"
            )
        };
        std::fs::write(&p, done(1) + &done(2) + "{\"event\":\"request_do").unwrap();
        let mut l = NinferLog::default();
        l.read(&p).unwrap();
        assert_eq!(l.summary(0)["output_tokens"], 3);
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        use std::io::Write;
        f.write_all(b"ne\",\"server_instance_id\":\"s\",\"result\":{\"completion_tokens\":4}}\n")
            .unwrap();
        l.read(&p).unwrap();
        assert_eq!(l.summary(0)["output_tokens"], 7);
        std::fs::write(&p, done(5)).unwrap();
        l.read(&p).unwrap();
        assert_eq!(l.summary(0)["output_tokens"], 5);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn models_from_host_json() {
        let hj = json!({"containers": [
            {"name": "ninfer-a", "ports": [8094], "hints": {"model_id": "m", "max_concurrency": "4",
                "request_log": "/nonexistent/req.jsonl"}},
            {"name": "abhoth-router-router-1", "ports": [8010], "hints": {"config": "router.json"}}],
            "gpu_procs": [{"gpu": "GPU-a", "container": "ninfer-a"}, {"gpu": "GPU-a", "container": "ninfer-a"}]});
        let mut logs = HashMap::new();
        let (m, r) = models_section(&hj, &mut logs, &labels(), &[], 1000);
        let a = &m["servers"]["ninfer-a"];
        assert_eq!(a["engine"], "ninfer");
        assert_eq!(a["lanes"], 4);
        assert_eq!(a["cards"], json!(["A"]));
        assert_eq!(a["port"], 8094);
        assert!(a["error"].as_str().unwrap().starts_with("request log"));
        assert!(m["servers"].get("abhoth-router-router-1").is_none());
        assert_eq!(m["total_tok_s"], 0.0);
        let ids: Vec<String> = r.iter().map(|(i, _)| i.to_string()).collect();
        assert_eq!(ids, vec!["engine.ninfer-a.tok_s", "engine.all.tok_s"]);
    }

    #[test]
    fn paths_map_by_whole_segments() {
        let map = vec![(
            "/mnt/Storage/ai/ninfer/logs/".to_string(),
            "/ninfer-logs".to_string(),
        )];
        assert_eq!(
            map_path("/mnt/Storage/ai/ninfer/logs/req-a.jsonl", &map),
            PathBuf::from("/ninfer-logs/req-a.jsonl")
        );
        assert_eq!(
            map_path("/mnt/Storage/ai/ninfer/logsX/r", &map),
            PathBuf::from("/mnt/Storage/ai/ninfer/logsX/r")
        );
        assert_eq!(map_path("/other", &[]), PathBuf::from("/other"));
    }

    #[test]
    fn gpu_procs_and_started_at() {
        let hj = json!({"time": 1.0,
            "containers": [{"name": "ninfer-a", "ports": [8094], "started_at": "2026-11-24T09:23:20Z",
                "hints": {"model_id": "m"}}],
            "gpu_procs": [{"pid": 7, "gpu": "GPU-a", "container": "ninfer-a", "process": "ninfer-serve", "mem_mib": 100},
                {"pid": 8, "gpu": "GPU-z", "container": "comfy", "process": "python", "mem_mib": 5}]});
        let s: BTreeMap<_, _> = host_json_sections(&hj, 2.0, 120, &labels())
            .into_iter()
            .collect();
        assert_eq!(s["gpu_procs"]["A"][0]["process"], "ninfer-serve");
        assert_eq!(s["gpu_procs"]["GPU-z"][0]["mem_mib"], 5);
        let (m, _) = models_section(&hj, &mut HashMap::new(), &labels(), &[], 0);
        assert_eq!(
            m["servers"]["ninfer-a"]["started_at"],
            "2026-11-24T09:23:20Z"
        );
    }

    #[test]
    fn run_logs_parse_like_the_old_dash() {
        let text = "2026-11-26 07:00:00 start\n\
            2026-11-26 07:10:00 step research ERROR: timeout\n\
            2026-11-26 07:11:00 ERRORS are not errors\n\
            2026-11-26 07:38:34 Main: https://studio.youtube.com/video/HbfXk78-IKg/edit (PUBLIC)\n\
            2026-11-26 07:38:34 notify [ok] AI News 2026-11-26 published | headline\n\
            2026-11-26 07:38:34 === done in 38 min ===\n";
        let v = parse_run_log("AI News", "run_daily_2026-11-26.log", text, 100.0);
        assert_eq!(v["date"], "2026-11-26");
        assert_eq!(v["minutes"], 38);
        assert_eq!(v["errors"], 1);
        assert!(v["last_error"].as_str().unwrap().contains("timeout"));
        assert_eq!(v["title"], "AI News 2026-11-26 published");
        assert_eq!(v["video"], "HbfXk78-IKg");
        assert_eq!(pipeline_status(&v, 1e9), "done");
        let r = parse_run_log("x", "run_daily_2026-11-27.log", "step render\n", 1000.0);
        assert_eq!(pipeline_status(&r, 1100.0), "running");
        assert_eq!(pipeline_status(&r, 2000.0), "stopped");
        assert_eq!(tail_chars("aé€b", 2), "€b");
        assert_eq!(log_date("run_daily_locked.log"), None);
    }

    #[test]
    fn the_newest_dated_log_is_read_and_cached() {
        let dir = std::env::temp_dir().join(format!("orca-sv-runs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (n, t) in [
            ("run_daily_2026-11-25.log", "=== done in 5 min ===\n"),
            (
                "run_daily_2026-11-26.log",
                "notify [ok] T | x\n=== done in 7 min ===\n",
            ),
            ("run_daily_locked.log", "nope\n"),
            ("other_2026-12-01.log", "nope\n"),
        ] {
            std::fs::write(dir.join(n), t).unwrap();
        }
        let pat = format!("{}/run_daily_*.log", dir.display());
        assert!(
            latest_log(&pat)
                .unwrap()
                .ends_with("run_daily_2026-11-26.log")
        );
        let jobs = vec![
            PipelineLog {
                name: "AI News".into(),
                logs: pat,
            },
            PipelineLog {
                name: "Gone".into(),
                logs: format!("{}/none_*.log", dir.display()),
            },
        ];
        let mut c = PipelineCache::default();
        let s = c.section(&jobs, 0.0);
        assert_eq!(s[0]["minutes"], 7);
        assert_eq!(s[0]["status"], "done");
        assert!(s[0].get("done").is_none());
        assert_eq!(s[1]["status"], "none");
        assert_eq!(
            c.section(&jobs, 0.0),
            s,
            "unchanged files: the cached entry"
        );
        let _ = std::fs::remove_dir_all(&dir);
        let bad: LocalConfig =
            toml::from_str("pipelines = [{ name = \"x\", logs = \"/a/*/b.log\" }]").unwrap();
        assert_eq!(bad.check().len(), 1);
    }

    #[test]
    fn fan_policy_names_each_fans_card() {
        let p = r#"
            [sensors.ctrl_B]
            expr = "ema(first(gpu.B.hbm, gpu.B.core), 4s)"
            [fan_defaults]
            mode = "device_curve"
            curve = [[60, 30], [82, 100]]
            min_duty = 30
            max_temp = 85
            feed_forward = { lo = 50, hi = 100, max_duty = 60 }
            [fans.duct0]
            backend = "esp"
            channel = 0
            temp = "sensor.ctrl_B"
            [fans.duct1]
            backend = "esp"
            channel = 1
            temp = "sensor.ctrl_A"
            load = "gpu.A.power_pct"
            min_duty = 40
        "#;
        let v = fan_policy_section(p);
        assert_eq!(v["duct0"]["card"], "B");
        assert_eq!(v["duct0"]["curve"], json!([[60, 30], [82, 100]]));
        assert_eq!(v["duct1"]["card"], "A");
        assert_eq!(v["duct1"]["min_duty"], 40);
        assert_eq!(v["duct1"]["feed_forward"]["max_duty"], 60);
        assert_eq!(gpu_card("xgpu.A.core"), None);
        assert_eq!(fan_policy_section("not toml ["), Value::Null);
    }

    #[test]
    fn image_fans_and_config_checks() {
        let d = json!({"status": "ok", "backends": [{"name": "comfy", "healthy": true, "secret": "x", "inflight": 1}]});
        let i = image_section(&d);
        assert_eq!(i["backends"][0]["name"], "comfy");
        assert!(i["backends"][0].get("secret").is_none());
        let fans = json!({"duct0": {"ts": 1000, "failsafe": false}, "duct1": {"ts": 1000, "failsafe": false}});
        assert!(fans_up(&fans, 2000));
        assert!(!fans_up(&fans, 40_000));
        assert!(!fans_up(
            &json!({"duct0": {"ts": 1000, "failsafe": true}}),
            2000
        ));
        assert!(!fans_up(&json!({}), 2000));
        let c: LocalConfig = toml::from_str(
            "services = [{ name = \"x\", kind = \"http\", port = 1 }, { name = \"y\", kind = \"tcp\" }, { name = \"z\", kind = \"fans\" }]",
        )
        .unwrap();
        assert_eq!(c.check().len(), 2);
        assert_eq!(c.interval_seconds, 5);
    }
}
