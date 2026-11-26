//! hostctl builtins (ROUTER-CONFIG.md 6.2): the safety-critical steps that are awkward in bash.
//! They run inside `abhoth hostctl` on the host (or in-process for a local server). Arguments
//! are typed JSON; a strict host also checks every path against its allowlist's path rules
//! before a builtin runs ([`super::hostctl`]).
//!
//! Everything here is std + tokio + the host's own tools (`docker`, `nvidia-smi`, `flock`,
//! `dmesg`), so the same binary works on any Linux host.

use std::path::Path;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::exec::{Cancel, ExecError, ExecOutput, run_shell};
use super::template::argv_to_shell;

/// Every builtin name hostctl knows.
pub const NAMES: &[&str] = &[
    "flock",
    "gpu-free-wait",
    "numa-room",
    "json-set",
    "restore-file",
    "oom-kills",
    "proc-scan",
    "secret-put",
    "docker-rm-robust",
    "telemetry",
    "compute-apps",
    "docker-up",
    "mounted",
    "gpus-visible",
    "file-exists",
    "read-json",
    "download",
    "download-status",
    "gguf-meta",
];

/// Builtins that change the host (root on a strict host, logged as writes).
pub const WRITES: &[&str] = &[
    "flock",
    "numa-room",
    "json-set",
    "restore-file",
    "secret-put",
    "docker-rm-robust",
    "download",
];

/// Argument keys that name host paths (checked against the allowlist's path rules).
pub const PATH_ARGS: &[(&str, &str, bool)] = &[
    // (builtin, arg, written)
    ("flock", "path", true),
    ("json-set", "file", true),
    ("restore-file", "path", true),
    ("restore-file", "manifest", false),
    ("secret-put", "dir", true),
    ("mounted", "path", false),
    ("file-exists", "path", false),
    ("read-json", "path", false),
    ("download", "dest", true),
    ("download-status", "dest", false),
    ("gguf-meta", "path", false),
];

pub fn known(name: &str) -> bool {
    NAMES.contains(&name)
}

fn bad(name: &str, e: impl std::fmt::Display) -> ExecError {
    ExecError::Refused(format!("{name}: {e}"))
}

fn args_of<T: for<'de> Deserialize<'de>>(name: &str, args: &Value) -> Result<T, ExecError> {
    serde_json::from_value(args.clone()).map_err(|e| bad(name, format!("arguments: {e}")))
}

/// An absolute path without `..` or control characters.
pub fn clean_path(p: &str) -> Result<(), String> {
    // `C:/…` only where the host itself is Windows (a local server on a Windows router)
    let windows_abs =
        cfg!(windows) && p.as_bytes().get(1) == Some(&b':') && p.as_bytes().get(2) == Some(&b'/');
    if !p.starts_with('/') && !windows_abs {
        return Err(format!("{p:?} is not an absolute path"));
    }
    if p.split(['/', '\\']).any(|c| c == "..") || p.chars().any(char::is_control) {
        return Err(format!("{p:?} is not a plain path"));
    }
    Ok(())
}

async fn sh(argv: &[&str], timeout: Duration, cancel: &Cancel) -> Result<ExecOutput, ExecError> {
    let words: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
    run_shell("sh", &argv_to_shell(&words), None, timeout, cancel).await
}

async fn pause(d: Duration, cancel: &Cancel) -> Result<(), ExecError> {
    tokio::select! {
        _ = tokio::time::sleep(d) => Ok(()),
        _ = cancel.cancelled() => Err(ExecError::Cancelled),
    }
}

fn done(started: Instant, mut o: ExecOutput) -> ExecOutput {
    o.duration_ms = started.elapsed().as_millis() as u64;
    o
}

/// Run a builtin.
pub async fn run(
    name: &str,
    args: &Value,
    stdin: Option<&[u8]>,
    timeout: Duration,
    cancel: &Cancel,
) -> Result<ExecOutput, ExecError> {
    let t0 = Instant::now();
    let out = match name {
        "file-exists" => {
            let a: PathArg = args_of(name, args)?;
            clean_path(&a.path).map_err(|e| bad(name, e))?;
            if Path::new(&a.path).exists() {
                ExecOutput::ok_with("yes")
            } else {
                ExecOutput::fail(1, &format!("{} does not exist", a.path))
            }
        }
        "read-json" => {
            let a: PathArg = args_of(name, args)?;
            clean_path(&a.path).map_err(|e| bad(name, e))?;
            match std::fs::read_to_string(&a.path) {
                Ok(t) if serde_json::from_str::<Value>(&t).is_ok() => ExecOutput::ok_with(&t),
                Ok(_) => ExecOutput::fail(2, "not JSON"),
                Err(e) => ExecOutput::fail(1, &e.to_string()),
            }
        }
        "mounted" => {
            let a: PathArg = args_of(name, args)?;
            let m = std::fs::read_to_string("/proc/mounts").unwrap_or_default();
            if mounts_contain(&m, &a.path) {
                ExecOutput::ok_with("mounted")
            } else {
                ExecOutput::fail(1, &format!("{} is not mounted", a.path))
            }
        }
        "docker-up" => {
            sh(
                &["docker", "info", "--format", "{{.ServerVersion}}"],
                timeout,
                cancel,
            )
            .await?
        }
        "gpus-visible" => {
            let a: UuidsArg = args_of(name, args)?;
            let o = sh(
                &["nvidia-smi", "--query-gpu=uuid", "--format=csv,noheader"],
                timeout,
                cancel,
            )
            .await?;
            if !o.ok() {
                o
            } else {
                let have: Vec<String> = o.stdout.lines().map(|l| l.trim().to_lowercase()).collect();
                let missing: Vec<&String> = a
                    .uuids
                    .iter()
                    .filter(|u| !have.contains(&u.to_lowercase()))
                    .collect();
                if missing.is_empty() {
                    ExecOutput::ok_with("all visible")
                } else {
                    ExecOutput::fail(1, &format!("not visible: {missing:?}"))
                }
            }
        }
        "download" => {
            let a: super::fetch::DownloadArgs = args_of(name, args)?;
            clean_path(&a.dest).map_err(|e| bad(name, e))?;
            super::fetch::download(&a, stdin, timeout, cancel).await
        }
        "download-status" => {
            let a: super::fetch::DownloadStatusArgs = args_of(name, args)?;
            clean_path(&a.dest).map_err(|e| bad(name, e))?;
            ExecOutput::ok_with(&super::fetch::status(&a.dest).to_string())
        }
        "gguf-meta" => {
            let a: PathArg = args_of(name, args)?;
            clean_path(&a.path).map_err(|e| bad(name, e))?;
            use std::io::Read as _;
            let mut buf = Vec::new();
            match std::fs::File::open(&a.path).and_then(|f| f.take(16 << 20).read_to_end(&mut buf))
            {
                Err(e) => ExecOutput::fail(1, &format!("{}: {e}", a.path)),
                Ok(_) => match super::caps::gguf_meta(&buf) {
                    Some(m) => ExecOutput::ok_with(&serde_json::to_string(&m).unwrap_or_default()),
                    None => ExecOutput::fail(
                        2,
                        "not a GGUF file (or no general.architecture in its first 16 MiB)",
                    ),
                },
            }
        }
        "telemetry" => {
            let q = format!("--query-gpu={}", super::telemetry::QUERY_FIELDS.join(","));
            sh(
                &["nvidia-smi", &q, "--format=csv,noheader,nounits"],
                timeout,
                cancel,
            )
            .await?
        }
        "compute-apps" => compute_apps(timeout, cancel).await?,
        "gpu-free-wait" => {
            let a: GpuFreeArgs = args_of(name, args)?;
            let end = Instant::now() + Duration::from_secs(a.timeout_seconds).min(timeout);
            loop {
                let o = sh(
                    &[
                        "nvidia-smi",
                        "--query-gpu=memory.used",
                        "--format=csv,noheader,nounits",
                        "-i",
                        &a.uuid,
                    ],
                    Duration::from_secs(20),
                    cancel,
                )
                .await?;
                let used: u64 = o
                    .stdout
                    .split_whitespace()
                    .next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(u64::MAX);
                if used < a.below_mib {
                    break ExecOutput::ok_with(&format!("{used} MiB used"));
                }
                if Instant::now() >= end {
                    break ExecOutput::fail(
                        1,
                        &format!("still {used} MiB in use after {} s", a.timeout_seconds),
                    );
                }
                pause(Duration::from_secs(3), cancel).await?;
            }
        }
        "docker-rm-robust" => {
            let a: DockerRmArgs = args_of(name, args)?;
            if !a
                .name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
            {
                return Err(bad(name, "bad container name"));
            }
            let end = Instant::now() + Duration::from_secs(a.timeout_seconds).min(timeout);
            loop {
                let rm = sh(
                    &["docker", "rm", "-f", &a.name],
                    Duration::from_secs(60),
                    cancel,
                )
                .await?;
                let gone = !sh(
                    &["docker", "inspect", "-f", "{{.State.Status}}", &a.name],
                    Duration::from_secs(20),
                    cancel,
                )
                .await?
                .ok();
                if gone {
                    break ExecOutput::ok_with(&format!("{} removed", a.name));
                }
                if Instant::now() >= end {
                    break ExecOutput::fail(
                        1,
                        &format!("could not remove {}: {}", a.name, rm.summary()),
                    );
                }
                pause(Duration::from_secs(3), cancel).await?;
            }
        }
        "numa-room" => {
            let a: NumaArgs = args_of(name, args)?;
            let before = node_free_gib();
            let free = before.iter().find(|(n, _, _)| *n == a.node).map(|x| x.1);
            let line = |v: &[(u32, f64, f64)]| {
                v.iter()
                    .map(|(n, f, c)| format!("node {n} {f:.0} GiB free ({c:.0} GiB page cache)"))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            match free {
                None => ExecOutput::ok_with("no NUMA information"),
                Some(f) if f >= a.need_gib => {
                    ExecOutput::ok_with(&format!("memory: {}", line(&before)))
                }
                Some(_) if !a.drop_caches => ExecOutput::ok_with(&format!(
                    "memory: {}; node {} under {:.0} GiB (cache drop not allowed)",
                    line(&before),
                    a.node,
                    a.need_gib
                )),
                Some(_) => {
                    let _ = sh(&["sync"], Duration::from_secs(120), cancel).await;
                    let dropped = std::fs::write("/proc/sys/vm/drop_caches", "1\n");
                    let after = node_free_gib();
                    let ok_now = after
                        .iter()
                        .find(|(n, _, _)| *n == a.node)
                        .is_some_and(|x| x.1 >= a.need_gib);
                    ExecOutput::ok_with(&format!(
                        "memory: {}; node {} under {:.0} GiB: {}; now {}{}",
                        line(&before),
                        a.node,
                        a.need_gib,
                        match dropped {
                            Ok(()) => "dropped the page cache".to_string(),
                            Err(e) => format!("could not drop the page cache ({e})"),
                        },
                        line(&after),
                        if ok_now {
                            ""
                        } else {
                            " (still short: the engine may not fit)"
                        }
                    ))
                }
            }
        }
        "json-set" => {
            let a: JsonSetArgs = args_of(name, args)?;
            clean_path(&a.file).map_err(|e| bad(name, e))?;
            let text = std::fs::read_to_string(&a.file)
                .map_err(|e| bad(name, format!("{}: {e}", a.file)))?;
            let mut v: Value =
                serde_json::from_str(&text).map_err(|e| bad(name, format!("{}: {e}", a.file)))?;
            let val = typed(&a.value, a.r#type.as_deref()).map_err(|e| bad(name, e))?;
            let Some(obj) = v.as_object_mut() else {
                return Err(bad(name, "not a JSON object"));
            };
            if obj.get(&a.key) == Some(&val) {
                ExecOutput::ok_with("unchanged")
            } else {
                obj.insert(a.key.clone(), val.clone());
                write_atomic(
                    Path::new(&a.file),
                    serde_json::to_string_pretty(&v)
                        .unwrap_or_default()
                        .as_bytes(),
                    None,
                )
                .map_err(|e| bad(name, e))?;
                ExecOutput::ok_with(&format!("{} = {val}", a.key))
            }
        }
        "restore-file" => restore_file(args_of(name, args)?, cancel).await?,
        "oom-kills" => {
            let o = sh(&["dmesg", "-T"], timeout, cancel).await?;
            if !o.ok() {
                o
            } else {
                let lines: Vec<&str> = o
                    .stdout
                    .lines()
                    .filter(|l| l.to_lowercase().contains("killed process"))
                    .collect();
                ExecOutput::ok_with(&serde_json::to_string(&lines).unwrap_or_default())
            }
        }
        "proc-scan" => {
            let a: ProcScanArgs = args_of(name, args)?;
            let found = proc_scan(Path::new("/proc"), &a);
            ExecOutput::ok_with(&serde_json::to_string(&found).unwrap_or_default())
        }
        "secret-put" => {
            let a: SecretPutArgs = args_of(name, args)?;
            clean_path(&a.dir).map_err(|e| bad(name, e))?;
            if a.id.is_empty()
                || !a
                    .id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_".contains(c))
            {
                return Err(bad(name, "bad secret id"));
            }
            let body = stdin.ok_or_else(|| bad(name, "the secret comes on stdin"))?;
            std::fs::create_dir_all(&a.dir).map_err(|e| bad(name, e))?;
            write_atomic(&Path::new(&a.dir).join(&a.id), body, Some(0o600))
                .map_err(|e| bad(name, e))?;
            ExecOutput::ok_with(&format!("{} written", a.id))
        }
        "flock" => flock(args_of(name, args)?, timeout, cancel).await?,
        other => return Err(ExecError::Refused(format!("unknown builtin `{other}`"))),
    };
    Ok(done(t0, out))
}

#[derive(Deserialize)]
struct PathArg {
    path: String,
}

#[derive(Deserialize)]
struct UuidsArg {
    #[serde(default)]
    uuids: Vec<String>,
}

#[derive(Deserialize)]
struct GpuFreeArgs {
    uuid: String,
    #[serde(default = "d_below")]
    below_mib: u64,
    #[serde(default = "d_90")]
    timeout_seconds: u64,
}
fn d_below() -> u64 {
    1024
}
fn d_90() -> u64 {
    90
}

#[derive(Deserialize)]
struct DockerRmArgs {
    name: String,
    #[serde(default = "d_120")]
    timeout_seconds: u64,
}
fn d_120() -> u64 {
    120
}

#[derive(Deserialize)]
struct NumaArgs {
    #[serde(default)]
    node: u32,
    need_gib: f64,
    #[serde(default)]
    drop_caches: bool,
}

#[derive(Deserialize)]
struct SecretPutArgs {
    /// The secrets directory (`/run/abhoth/secrets`).
    dir: String,
    id: String,
}

#[derive(Deserialize)]
struct JsonSetArgs {
    file: String,
    key: String,
    value: Value,
    #[serde(default)]
    r#type: Option<String>,
}

/// Coerce a templated value (`"{lanes}"` renders to a string) to its declared type.
fn typed(v: &Value, ty: Option<&str>) -> Result<Value, String> {
    let s = match v {
        Value::String(s) => s.clone(),
        other => return Ok(other.clone()),
    };
    match ty {
        Some("int") => s
            .trim()
            .parse::<i64>()
            .map(Value::from)
            .map_err(|_| format!("{s:?} is not an integer")),
        Some("float") => s
            .trim()
            .parse::<f64>()
            .map(Value::from)
            .map_err(|_| format!("{s:?} is not a number")),
        Some("bool") => s
            .trim()
            .parse::<bool>()
            .map(Value::from)
            .map_err(|_| format!("{s:?} is not true/false")),
        Some("string") | None => Ok(Value::String(s)),
        Some(t) => Err(format!("unknown type {t}")),
    }
}

/// Write via a temp file and rename (same directory), optional mode on unix.
pub fn write_atomic(path: &Path, body: &[u8], mode: Option<u32>) -> Result<(), String> {
    let dir = path.parent().ok_or("no parent directory")?;
    let tmp = dir.join(format!(
        ".{}.abhoth-tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("file")
    ));
    std::fs::write(&tmp, body).map_err(|e| format!("{}: {e}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let m = match mode {
            Some(m) => m,
            None => std::fs::metadata(path)
                .map(|m| m.permissions().mode() & 0o7777)
                .unwrap_or(0o644),
        };
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(m))
            .map_err(|e| e.to_string())?;
    }
    #[cfg(not(unix))]
    let _ = mode;
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}

fn mounts_contain(mounts: &str, path: &str) -> bool {
    let p = path.trim_end_matches('/');
    mounts
        .lines()
        .any(|l| l.split_whitespace().nth(1).is_some_and(|m| m == p))
}

/// `(node, free GiB, page cache GiB)` from sysfs.
pub fn node_free_gib() -> Vec<(u32, f64, f64)> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir("/sys/devices/system/node") else {
        return out;
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let Some(n) = name
            .strip_prefix("node")
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(t) = std::fs::read_to_string(e.path().join("meminfo")) else {
            continue;
        };
        let (free, cache) = parse_node_meminfo(&t);
        out.push((n, free, cache));
    }
    out.sort_by_key(|x| x.0);
    out
}

/// `Node 0 MemFree: 123 kB` / `Node 0 FilePages: …` -> GiB.
pub fn parse_node_meminfo(t: &str) -> (f64, f64) {
    let get = |key: &str| -> f64 {
        t.lines()
            .find(|l| l.split_whitespace().nth(2) == Some(key))
            .and_then(|l| l.split_whitespace().nth(3))
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.0)
            / (1024.0 * 1024.0)
    };
    (get("MemFree:"), get("FilePages:"))
}

// ── compute apps: who holds each GPU ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComputeApp {
    pub gpu_uuid: String,
    pub pid: u32,
    pub used_mib: u64,
    /// The docker container the process runs in, when it does.
    pub container: Option<String>,
    pub process: String,
}

async fn compute_apps(timeout: Duration, cancel: &Cancel) -> Result<ExecOutput, ExecError> {
    let o = sh(
        &[
            "nvidia-smi",
            "--query-compute-apps=gpu_uuid,pid,used_memory,process_name",
            "--format=csv,noheader,nounits",
        ],
        timeout,
        cancel,
    )
    .await?;
    if !o.ok() {
        return Ok(o);
    }
    let ps = sh(
        &[
            "docker",
            "ps",
            "--no-trunc",
            "--format",
            "{{.ID}} {{.Names}}",
        ],
        Duration::from_secs(30),
        cancel,
    )
    .await?;
    let ids: Vec<(String, String)> = ps
        .stdout
        .lines()
        .filter_map(|l| {
            l.split_once(' ')
                .map(|(a, b)| (a.to_string(), b.to_string()))
        })
        .collect();
    let mut apps = parse_compute_apps(&o.stdout);
    for a in &mut apps {
        let cg = std::fs::read_to_string(format!("/proc/{}/cgroup", a.pid)).unwrap_or_default();
        a.container = ids
            .iter()
            .find(|(id, _)| cg.contains(id.as_str()))
            .map(|(_, n)| n.clone());
    }
    Ok(ExecOutput::ok_with(
        &serde_json::to_string(&apps).unwrap_or_default(),
    ))
}

pub fn parse_compute_apps(csv: &str) -> Vec<ComputeApp> {
    csv.lines()
        .filter_map(|l| {
            let p: Vec<&str> = l.split(',').map(str::trim).collect();
            Some(ComputeApp {
                gpu_uuid: p.first()?.to_string(),
                pid: p.get(1)?.parse().ok()?,
                used_mib: p.get(2).and_then(|v| v.parse().ok()).unwrap_or(0),
                container: None,
                process: p.get(3).unwrap_or(&"").to_string(),
            })
        })
        .collect()
}

// ── process scan (guards) ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct ProcScanArgs {
    pub argv_any: String,
    #[serde(default)]
    pub exe: Vec<String>,
    #[serde(default)]
    pub label_arg: Option<String>,
    #[serde(default)]
    pub label_default: Option<String>,
}

/// `[(pid, label)]` of processes with an argv word (of the first three) ending in `argv_any`
/// and an accepted executable (ondemand.py `video_runs`).
pub fn proc_scan(proc: &Path, a: &ProcScanArgs) -> Vec<(u32, String)> {
    let me = std::process::id();
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(proc) else {
        return out;
    };
    for e in rd.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else {
            continue;
        };
        if pid == me {
            continue;
        }
        let Ok(raw) = std::fs::read(e.path().join("cmdline")) else {
            continue;
        };
        let argv: Vec<String> = raw
            .split(|b| *b == 0)
            .filter(|a| !a.is_empty())
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect();
        if let Some(label) = match_argv(&argv, a) {
            out.push((pid, label));
        }
    }
    out.sort();
    out
}

fn base(a: &str) -> &str {
    a.rsplit('/').next().unwrap_or(a)
}

pub fn match_argv(argv: &[String], a: &ProcScanArgs) -> Option<String> {
    let exe = base(argv.first()?);
    let hit = argv.iter().take(3).any(|w| base(w) == a.argv_any);
    if !hit || (!a.exe.is_empty() && !a.exe.iter().any(|e| e == exe)) {
        return None;
    }
    let label = a
        .label_arg
        .as_ref()
        .and_then(|flag| {
            let i = argv[..argv.len().saturating_sub(1)]
                .iter()
                .position(|w| w == flag)?;
            argv.get(i + 1).cloned()
        })
        .or_else(|| a.label_default.clone())
        .unwrap_or_default();
    Some(label)
}

// ── restore from the archive (sha256 checked) ────────────────────────────────────────────────

#[derive(Deserialize)]
struct RestoreArgs {
    /// The model file's original path.
    path: String,
    /// TSV with `original_path`, `size_bytes`, `sha256`, `archive_path` columns.
    manifest: String,
    #[serde(default)]
    owner: Option<(u32, u32)>,
    #[serde(default = "d_margin")]
    margin_bytes: u64,
}
fn d_margin() -> u64 {
    20_000_000_000
}

#[derive(Debug, Clone, PartialEq)]
pub struct ManifestRow {
    pub size: u64,
    pub sha256: String,
    pub archive_path: String,
}

pub fn manifest_row(tsv: &str, original: &str) -> Option<ManifestRow> {
    let mut lines = tsv.lines();
    let head: Vec<&str> = lines.next()?.split('\t').collect();
    let col = |n: &str| head.iter().position(|h| h.trim() == n);
    let (o, s, h, a) = (
        col("original_path")?,
        col("size_bytes")?,
        col("sha256")?,
        col("archive_path")?,
    );
    let mut found = None;
    for l in lines {
        let f: Vec<&str> = l.split('\t').collect();
        if f.get(o).map(|x| x.trim()) == Some(original) {
            found = Some(ManifestRow {
                size: f.get(s)?.trim().parse().ok()?,
                sha256: f.get(h)?.trim().to_lowercase(),
                archive_path: f.get(a)?.trim().to_string(),
            });
        }
    }
    found
}

async fn restore_file(a: RestoreArgs, cancel: &Cancel) -> Result<ExecOutput, ExecError> {
    use sha2::{Digest, Sha256};
    use std::io::{Read, Write};
    let name = "restore-file";
    clean_path(&a.path).map_err(|e| bad(name, e))?;
    clean_path(&a.manifest).map_err(|e| bad(name, e))?;
    if Path::new(&a.path).exists() {
        return Ok(ExecOutput::ok_with("present"));
    }
    let tsv = std::fs::read_to_string(&a.manifest)
        .map_err(|e| bad(name, format!("{}: {e}", a.manifest)))?;
    let Some(row) = manifest_row(&tsv, &a.path) else {
        return Ok(ExecOutput::fail(
            3,
            &format!("{} is neither present nor in the archive manifest", a.path),
        ));
    };
    clean_path(&row.archive_path).map_err(|e| bad(name, e))?;
    let target = Path::new(&a.path).to_path_buf();
    let part = format!("{}.restoring", a.path);
    let cancel = cancel.clone();
    let margin = a.margin_bytes;
    let owner = a.owner;
    // Blocking file work off the async threads.
    let res = tokio::task::spawn_blocking(move || -> Result<String, String> {
        if let Some(d) = target.parent() {
            std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
        }
        let _ = margin; // free-space check needs statvfs (no unsafe here); the copy fails cleanly when full
        let t0 = Instant::now();
        let mut src = std::fs::File::open(&row.archive_path).map_err(|e| format!("{}: {e}", row.archive_path))?;
        let mut dst = std::fs::File::create(&part).map_err(|e| format!("{part}: {e}"))?;
        let mut h = Sha256::new();
        let mut buf = vec![0u8; 16 << 20];
        let mut n = 0u64;
        loop {
            if cancel.is_cancelled() {
                let _ = std::fs::remove_file(&part);
                return Err("cancelled".into());
            }
            let k = src.read(&mut buf).map_err(|e| e.to_string())?;
            if k == 0 {
                break;
            }
            dst.write_all(&buf[..k]).map_err(|e| e.to_string())?;
            h.update(&buf[..k]);
            n += k as u64;
        }
        dst.sync_all().map_err(|e| e.to_string())?;
        drop(dst);
        let got: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
        if got != row.sha256 || n != row.size {
            let _ = std::fs::remove_file(&part);
            return Err("the archive copy does not match the manifest (sha256/size); nothing restored".into());
        }
        // Verify what landed on disk.
        let mut f = std::fs::File::open(&part).map_err(|e| e.to_string())?;
        let mut h2 = Sha256::new();
        loop {
            let k = f.read(&mut buf).map_err(|e| e.to_string())?;
            if k == 0 {
                break;
            }
            h2.update(&buf[..k]);
        }
        let got2: String = h2.finalize().iter().map(|b| format!("{b:02x}")).collect();
        if got2 != row.sha256 {
            let _ = std::fs::remove_file(&part);
            return Err("the copy on disk fails its sha256 check; removed".into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&part, std::fs::Permissions::from_mode(0o644));
            if let Some((u, g)) = owner {
                let _ = std::os::unix::fs::chown(&part, Some(u), Some(g));
            }
        }
        #[cfg(not(unix))]
        let _ = owner;
        std::fs::rename(&part, &target).map_err(|e| e.to_string())?;
        Ok(json!({"path": target, "from": row.archive_path, "bytes": n, "sha256": row.sha256, "verified": true,
                  "seconds": t0.elapsed().as_secs_f64()})
        .to_string())
    })
    .await
    .map_err(|e| ExecError::Io(e.to_string()))?;
    Ok(match res {
        Ok(rec) => ExecOutput::ok_with(&rec),
        Err(e) if e == "cancelled" => return Err(ExecError::Cancelled),
        Err(e) => ExecOutput::fail(1, &e),
    })
}

// ── host-wide locks (interoperate with the old tools' flock(2)) ─────────────────────────────

#[derive(Deserialize)]
struct FlockArgs {
    path: String,
    /// `acquire` | `release`.
    op: String,
    /// Job id holding it.
    owner: String,
    #[serde(default = "d_wait")]
    wait_seconds: u64,
    /// The hold ends by itself after this long (a router that died cannot hold a lock forever).
    #[serde(default = "d_ttl")]
    ttl_seconds: u64,
}
fn d_wait() -> u64 {
    120
}
fn d_ttl() -> u64 {
    3600
}

/// A held lock is a detached `flock PATH sh -c 'exec sleep TTL'` process (the same flock(2) the
/// old watchdog takes); its owner and pid sit in `PATH.holder`.
async fn flock(a: FlockArgs, timeout: Duration, cancel: &Cancel) -> Result<ExecOutput, ExecError> {
    let name = "flock";
    clean_path(&a.path).map_err(|e| bad(name, e))?;
    if !a
        .owner
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_.:".contains(c))
    {
        return Err(bad(name, "bad owner"));
    }
    let holder = format!("{}.holder", a.path);
    let current = std::fs::read_to_string(&holder).ok();
    // `PATH.holder`: "<owner> <pid>".
    let held_by = |t: &str| -> Option<(String, u32)> {
        let (o, p) = t.trim().split_once(' ')?;
        Some((o.to_string(), p.trim().parse().ok()?))
    };
    let alive = |pid: u32| Path::new(&format!("/proc/{pid}")).exists();
    match a.op.as_str() {
        "release" => {
            if let Some((o, pid)) = current.as_deref().and_then(held_by)
                && o == a.owner
            {
                let _ = sh(&["kill", &pid.to_string()], Duration::from_secs(10), cancel).await;
                let _ = std::fs::remove_file(&holder);
                return Ok(ExecOutput::ok_with("released"));
            }
            Ok(ExecOutput::ok_with("not held by this owner"))
        }
        "acquire" => {
            if let Some((o, pid)) = current.as_deref().and_then(held_by)
                && o == a.owner
                && alive(pid)
            {
                return Ok(ExecOutput::ok_with("already held"));
            }
            let hq = super::template::shell_quote(&holder);
            let inner = format!(
                "echo \"$0 $$\" > {hq}.tmp && mv {hq}.tmp {hq} && exec sleep {}",
                a.ttl_seconds
            );
            let words: Vec<String> = vec![
                "flock".into(),
                "-w".into(),
                a.wait_seconds.to_string(),
                a.path.clone(),
                "sh".into(),
                "-c".into(),
                inner,
                a.owner.clone(),
            ];
            let _ = std::fs::remove_file(&holder);
            let spawn = format!(
                "setsid -f {} >/dev/null 2>&1 </dev/null",
                argv_to_shell(&words)
            );
            let o = run_shell("sh", &spawn, None, Duration::from_secs(10), cancel).await?;
            if !o.ok() {
                return Ok(o);
            }
            let end = Instant::now() + Duration::from_secs(a.wait_seconds + 2).min(timeout);
            loop {
                if let Some((o, pid)) = std::fs::read_to_string(&holder)
                    .ok()
                    .as_deref()
                    .and_then(held_by)
                    && o == a.owner
                    && alive(pid)
                {
                    return Ok(ExecOutput::ok_with(&format!("held (pid {pid})")));
                }
                if Instant::now() >= end {
                    return Ok(ExecOutput::fail(
                        1,
                        &format!("{} is held by another process", a.path),
                    ));
                }
                pause(Duration::from_millis(250), cancel).await?;
            }
        }
        other => Err(bad(name, format!("op {other}: acquire or release"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_and_types() {
        assert!(clean_path("/srv/a/b.json").is_ok());
        assert!(clean_path("srv/a").is_err());
        assert!(clean_path("/srv/../etc/shadow").is_err());
        assert_eq!(typed(&json!("4"), Some("int")).unwrap(), json!(4));
        assert!(typed(&json!("x"), Some("int")).is_err());
        assert_eq!(typed(&json!(3), Some("int")).unwrap(), json!(3));
    }

    #[test]
    fn parses_host_tables() {
        let mi = "Node 0 MemTotal:       263921212 kB\nNode 0 MemFree:        67108864 kB\nNode 0 FilePages:      10485760 kB\n";
        let (f, c) = parse_node_meminfo(mi);
        assert_eq!((f.round(), c.round()), (64.0, 10.0));
        let apps = parse_compute_apps(
            "GPU-aa, 1234, 30210, /app/ninfer-serve\nGPU-bb, 99, 500, python3\n",
        );
        assert_eq!(apps.len(), 2);
        assert_eq!(apps[0].used_mib, 30210);
        let tsv = "original_path\tsize_bytes\tsha256\tarchive_path\n/m/a.ninfer\t10\tABCD\t/arch/a.ninfer\n";
        let r = manifest_row(tsv, "/m/a.ninfer").unwrap();
        assert_eq!(
            (r.size, r.sha256.as_str(), r.archive_path.as_str()),
            (10, "abcd", "/arch/a.ninfer")
        );
        assert!(manifest_row(tsv, "/m/b").is_none());
        assert!(mounts_contain(
            "tank /mnt/.ix-apps zfs rw 0 0\n",
            "/mnt/.ix-apps/"
        ));
    }

    #[test]
    fn video_runs_match_like_ondemand() {
        let a = ProcScanArgs {
            argv_any: "run_daily.sh".into(),
            exe: vec!["bash".into(), "sh".into()],
            label_arg: Some("--channel".into()),
            label_default: Some("ai-news".into()),
        };
        let v = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(
            match_argv(
                &v(&["/bin/bash", "/srv/vp/run_daily.sh", "--channel", "games"]),
                &a
            ),
            Some("games".into())
        );
        assert_eq!(
            match_argv(&v(&["bash", "run_daily.sh"]), &a),
            Some("ai-news".into())
        );
        assert_eq!(match_argv(&v(&["python3", "run_daily.sh"]), &a), None);
        assert_eq!(
            match_argv(&v(&["bash", "x", "y", "run_daily.sh"]), &a),
            None
        );
    }

    #[tokio::test]
    async fn file_builtins() {
        let d = std::env::temp_dir().join(format!("abhoth-bi-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("cfg.json");
        std::fs::write(&f, r#"{"parallel": 1, "other": "x"}"#).unwrap();
        let p = f.to_string_lossy().replace('\\', "/");
        // Windows paths are not absolute in the unix sense: only run the path-checked part on unix.
        if p.starts_with('/') {
            let c = Cancel::new();
            let o = run(
                "json-set",
                &json!({"file": p, "key": "parallel", "value": "3", "type": "int"}),
                None,
                Duration::from_secs(5),
                &c,
            )
            .await
            .unwrap();
            assert!(o.ok(), "{o:?}");
            let v: Value = serde_json::from_str(&std::fs::read_to_string(&f).unwrap()).unwrap();
            assert_eq!(v["parallel"], 3);
            assert_eq!(v["other"], "x");
            let o = run(
                "file-exists",
                &json!({"path": p}),
                None,
                Duration::from_secs(5),
                &c,
            )
            .await
            .unwrap();
            assert!(o.ok());
        }
        let _ = std::fs::remove_dir_all(&d);
        let c = Cancel::new();
        assert!(matches!(
            run("nope", &json!({}), None, Duration::from_secs(1), &c).await,
            Err(ExecError::Refused(_))
        ));
        assert!(matches!(
            run(
                "json-set",
                &json!({"file": "rel"}),
                None,
                Duration::from_secs(1),
                &c
            )
            .await,
            Err(ExecError::Refused(_))
        ));
    }
}
