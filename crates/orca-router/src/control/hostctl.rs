//! `abhoth-router hostctl`: the host side of execution (ROUTER-CONFIG.md 6.2).
//!
//! The router reaches a remote server over SSH with a key whose `authorized_keys` line forces
//! `hostctl serve` (see `deploy/hostctl/README.md`). Each connection carries **one framed
//! request** (a JSON line on stdin) and gets one JSON line back. The host decides, from its own
//! root-owned allowlist, whether to run it:
//!
//! - **commands** must match an allowlist entry's id and **exact text** (`hostctl export`
//!   renders every command the router's config can produce; a changed config means a re-export
//!   the admin reviews and installs). Root commands also need `root = true` on the entry.
//! - **builtins** must be allowed by name; path arguments must sit under the allowlist's
//!   `paths.read` / `paths.write` prefixes.
//! - **hostd** calls go to the host daemon's socket (SUPERVISOR.md), methods allowed by name.
//!
//! Root requests go through the single sudo entry `hostctl exec-root --allowlist <file>`, which
//! re-checks the request against the (root-owned, not group/world-writable) allowlist itself.
//! Every request is appended to the host audit log. `mode = "trusted"` skips the command text
//! check (development hosts only; validation warns).
//!
//! The allowlist lives under the apps pool (`<apps>/abhoth/etc/hostctl.toml`): `/etc` does
//! not survive TrueNAS updates.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::builtins;
use super::driver::Cmd;
use super::exec::{Cancel, ExecError, ExecOutput, ExecRequest, Executor, LocalExecutor, Op};
use super::schema::{ControlConfig, HostctlMode};

/// Protocol version on the wire.
pub const PROTOCOL: u32 = 1;

/// The request frame (one JSON line).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Frame {
    pub v: u32,
    pub req: ExecRequest,
}

/// The reply frame (one JSON line).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Reply {
    Ok(ExecOutput),
    Err(ExecError),
}

// ── the allowlist ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Allowlist {
    pub version: u32,
    #[serde(default)]
    pub mode: HostctlMode,
    /// The server id this file was exported for (shown in refusals).
    #[serde(default)]
    pub server: String,
    /// JSON-lines audit log of every request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audit_log: Option<String>,
    #[serde(default = "d_max_timeout")]
    pub max_timeout_seconds: u64,
    #[serde(default)]
    pub paths: PathRules,
    #[serde(default)]
    pub builtins: BuiltinRules,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostd: Option<HostdRules>,
    #[serde(default, rename = "command", skip_serializing_if = "Vec::is_empty")]
    pub commands: Vec<AllowedCommand>,
}

fn d_max_timeout() -> u64 {
    7200
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PathRules {
    #[serde(default)]
    pub read: Vec<String>,
    #[serde(default)]
    pub write: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct BuiltinRules {
    /// Builtins that may run (default: all).
    #[serde(default = "all_builtins")]
    pub allow: Vec<String>,
    /// Builtins that may run as root.
    #[serde(default = "root_builtins")]
    pub root: Vec<String>,
}

impl Default for BuiltinRules {
    fn default() -> Self {
        Self {
            allow: all_builtins(),
            root: root_builtins(),
        }
    }
}

fn all_builtins() -> Vec<String> {
    builtins::NAMES.iter().map(|s| s.to_string()).collect()
}

fn root_builtins() -> Vec<String> {
    [
        "flock",
        "numa-room",
        "json-set",
        "restore-file",
        "secret-put",
        "docker-rm-robust",
        "docker-up",
        "compute-apps",
        "oom-kills",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HostdRules {
    pub socket: String,
    #[serde(default)]
    pub methods: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AllowedCommand {
    pub id: String,
    #[serde(default)]
    pub root: bool,
    /// What it is for (review aid).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub text: String,
}

impl Allowlist {
    pub fn parse(text: &str) -> Result<Self, String> {
        let a: Allowlist = toml::from_str(text).map_err(|e| format!("allowlist: {e}"))?;
        if a.version != 1 {
            return Err(format!(
                "allowlist version {} (this hostctl knows 1)",
                a.version
            ));
        }
        Ok(a)
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Self::parse(&text)
    }

    fn path_ok(&self, p: &str, write: bool) -> bool {
        let under = |pre: &String| {
            p == pre.trim_end_matches('/')
                || p.starts_with(&format!("{}/", pre.trim_end_matches('/')))
        };
        if write {
            self.paths.write.iter().any(under)
        } else {
            self.paths.read.iter().chain(&self.paths.write).any(under)
        }
    }

    /// Whether the request may run. `Err` is the refusal text.
    pub fn check(&self, req: &ExecRequest) -> Result<(), String> {
        if req.timeout > Duration::from_secs(self.max_timeout_seconds) {
            return Err(format!(
                "timeout over the host's {} s limit",
                self.max_timeout_seconds
            ));
        }
        match &req.op {
            Op::Command { id, text, .. } => {
                if self.mode == HostctlMode::Trusted {
                    return Ok(());
                }
                let Some(e) = self.commands.iter().find(|c| c.id == *id) else {
                    return Err(format!(
                        "command {id} is not in the allowlist of {} (re-export and install it)",
                        self.server
                    ));
                };
                if e.text != *text {
                    return Err(format!(
                        "command {id}: the text differs from the allowlist (re-export and install it)"
                    ));
                }
                if req.root && !e.root {
                    return Err(format!("command {id} may not run as root"));
                }
                Ok(())
            }
            Op::Builtin { name, args } => {
                if !builtins::known(name) || !self.builtins.allow.contains(name) {
                    return Err(format!("builtin {name} is not allowed"));
                }
                if req.root && !self.builtins.root.contains(name) {
                    return Err(format!("builtin {name} may not run as root"));
                }
                for (b, arg, write) in builtins::PATH_ARGS {
                    if b != name {
                        continue;
                    }
                    if let Some(p) = args.get(*arg).and_then(Value::as_str) {
                        builtins::clean_path(p)?;
                        if !self.path_ok(p, *write) {
                            return Err(format!(
                                "{name}: {arg} {p} is outside the allowed {} paths",
                                if *write { "write" } else { "read" }
                            ));
                        }
                    }
                }
                Ok(())
            }
            Op::Hostd { method, .. } => {
                let Some(h) = &self.hostd else {
                    return Err("no host daemon configured".into());
                };
                if !h.methods.iter().any(|m| m == method) {
                    return Err(format!("hostd method {method} is not allowed"));
                }
                Ok(())
            }
        }
    }
}

// ── serving ──────────────────────────────────────────────────────────────────────────────────

fn audit(al: &Allowlist, req: &ExecRequest, result: &Result<ExecOutput, ExecError>) {
    let Some(path) = &al.audit_log else {
        return;
    };
    let line = serde_json::json!({
        "at": chrono::Utc::now().to_rfc3339(),
        "user": std::env::var("USER").or_else(|_| std::env::var("SUDO_USER")).unwrap_or_default(),
        "op": req.op.label(),
        "kind": match &req.op { Op::Command { .. } => "command", Op::Builtin { .. } => "builtin", Op::Hostd { .. } => "hostd" },
        "root": req.root,
        "result": match result {
            Ok(o) => serde_json::json!({"exit": o.exit_code, "timed_out": o.timed_out, "ms": o.duration_ms}),
            Err(e) => serde_json::json!({"error": e.to_string()}),
        },
    });
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{line}");
    }
}

/// Is this process root? (`id -u`, no unsafe.)
fn is_root() -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // /proc/self is owned by the effective uid.
        std::fs::metadata("/proc/self")
            .map(|m| m.uid() == 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    false
}

/// The allowlist file must be root-owned and not writable by group or others.
pub fn check_owner(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let m = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
        if m.uid() != 0 || m.mode() & 0o022 != 0 {
            return Err(format!(
                "{} must be owned by root and not group/world-writable",
                path.display()
            ));
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

/// Run one checked request on this host.
pub async fn execute(
    al: &Allowlist,
    allowlist_path: &Path,
    req: ExecRequest,
    cancel: &Cancel,
) -> Result<ExecOutput, ExecError> {
    al.check(&req).map_err(ExecError::Refused)?;
    if req.root && !is_root() {
        return via_sudo(allowlist_path, &req).await;
    }
    match &req.op {
        Op::Hostd { method, params } => hostd_call(al, method, params, req.timeout).await,
        _ => LocalExecutor::default().run(req, cancel).await,
    }
}

/// Hand a root request to `sudo -n <self> hostctl exec-root --allowlist <file>`.
async fn via_sudo(allowlist_path: &Path, req: &ExecRequest) -> Result<ExecOutput, ExecError> {
    use tokio::io::AsyncWriteExt;
    let exe = std::env::current_exe().map_err(|e| ExecError::Io(e.to_string()))?;
    let mut child = tokio::process::Command::new("sudo")
        .arg("-n")
        .arg(exe)
        .args(["hostctl", "exec-root", "--allowlist"])
        .arg(allowlist_path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| ExecError::Io(format!("sudo: {e}")))?;
    let frame = serde_json::to_string(&Frame {
        v: PROTOCOL,
        req: req.clone(),
    })
    .map_err(|e| ExecError::Io(e.to_string()))?;
    if let Some(mut si) = child.stdin.take() {
        si.write_all(frame.as_bytes())
            .await
            .map_err(|e| ExecError::Io(e.to_string()))?;
        si.write_all(b"\n")
            .await
            .map_err(|e| ExecError::Io(e.to_string()))?;
    }
    let out = tokio::time::timeout(
        req.timeout + Duration::from_secs(30),
        child.wait_with_output(),
    )
    .await
    .map_err(|_| ExecError::Io("root helper did not answer".into()))?
    .map_err(|e| ExecError::Io(e.to_string()))?;
    parse_reply(&String::from_utf8_lossy(&out.stdout)).unwrap_or_else(|| {
        Err(ExecError::Refused(format!(
            "root helper: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )))
    })
}

/// The last JSON reply line in `text`.
pub fn parse_reply(text: &str) -> Option<Result<ExecOutput, ExecError>> {
    let r: Reply = text
        .lines()
        .rev()
        .find_map(|l| serde_json::from_str(l.trim()).ok())?;
    Some(match r {
        Reply::Ok(o) => Ok(o),
        Reply::Err(e) => Err(e),
    })
}

/// A hostd call: one JSON line to its unix socket, one line back.
/// The frames for one hostd call (`orca-hostd::protocol`, NDJSON envelopes): `hello` (id 0),
/// then the message: `type` = `method`, the other fields from `params`, id 1.
pub fn hostd_frames(method: &str, params: &Value) -> [Value; 2] {
    let hello = serde_json::json!({"id": 0, "type": "hello", "proto": 1,
        "version": env!("CARGO_PKG_VERSION"), "client": "router"});
    let mut msg = params.as_object().cloned().unwrap_or_default();
    msg.insert("type".into(), Value::from(method));
    msg.insert("id".into(), Value::from(1));
    [hello, Value::Object(msg)]
}

/// The reply to a call among hostd's frames: id 1, or an `error` to the hello (id 0). Stream
/// frames (telemetry, events) carry no id and are skipped.
pub fn hostd_reply(line: &str) -> Option<String> {
    let v: Value = serde_json::from_str(line.trim()).ok()?;
    match v.get("id").and_then(Value::as_u64) {
        Some(1) => Some(line.trim().to_string()),
        Some(0) if v["type"] == "error" => Some(line.trim().to_string()),
        _ => None,
    }
}

async fn hostd_call(
    al: &Allowlist,
    method: &str,
    params: &Value,
    timeout: Duration,
) -> Result<ExecOutput, ExecError> {
    #[cfg(unix)]
    {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
        let h = al
            .hostd
            .as_ref()
            .ok_or_else(|| ExecError::Refused("no host daemon".into()))?;
        let fut = async {
            let s = tokio::net::UnixStream::connect(&h.socket)
                .await
                .map_err(|e| ExecError::Io(format!("hostd: {e}")))?;
            let (rd, mut wr) = s.into_split();
            let mut r = BufReader::new(rd);
            for frame in hostd_frames(method, params) {
                let mut line = frame.to_string().into_bytes();
                line.push(b'\n');
                wr.write_all(&line)
                    .await
                    .map_err(|e| ExecError::Io(e.to_string()))?;
            }
            wr.flush().await.map_err(|e| ExecError::Io(e.to_string()))?;
            let mut line = String::new();
            loop {
                line.clear();
                let n = (&mut r)
                    .take(4 * 1024 * 1024)
                    .read_line(&mut line)
                    .await
                    .map_err(|e| ExecError::Io(e.to_string()))?;
                if n == 0 {
                    return Err(ExecError::Io("hostd closed the connection".into()));
                }
                if let Some(reply) = hostd_reply(&line) {
                    return Ok(ExecOutput::ok_with(&reply));
                }
            }
        };
        tokio::time::timeout(timeout, fut)
            .await
            .map_err(|_| ExecError::Io("hostd timed out".into()))?
    }
    #[cfg(not(unix))]
    {
        let _ = (al, params, timeout);
        Err(ExecError::Refused(format!(
            "hostd call {method}: unix only"
        )))
    }
}

/// `hostctl serve` / `hostctl exec-root`: read one frame from stdin, answer on stdout.
/// `exec_root` additionally insists the allowlist is root-owned.
pub async fn serve_stdio(allowlist_path: &Path, exec_root: bool) -> i32 {
    let mut line = String::new();
    let reply = async {
        if exec_root {
            check_owner(allowlist_path).map_err(ExecError::Refused)?;
        }
        let al = Allowlist::load(allowlist_path).map_err(ExecError::Refused)?;
        std::io::stdin()
            .lock()
            .read_line(&mut line)
            .map_err(|e| ExecError::Io(e.to_string()))?;
        let mut frame: Frame = serde_json::from_str(line.trim())
            .map_err(|e| ExecError::Refused(format!("bad request: {e}")))?;
        if frame.v != PROTOCOL {
            return Err(ExecError::Refused(format!(
                "protocol {} (this host speaks {PROTOCOL})",
                frame.v
            )));
        }
        if exec_root && !frame.req.root {
            frame.req.root = true;
        }
        let req = frame.req;
        let res = execute(&al, allowlist_path, req.clone(), &Cancel::new()).await;
        audit(&al, &req, &res);
        res
    }
    .await;
    let r = match reply {
        Ok(o) => Reply::Ok(o),
        Err(e) => Reply::Err(e),
    };
    println!("{}", serde_json::to_string(&r).unwrap_or_default());
    0
}

// ── export ───────────────────────────────────────────────────────────────────────────────────

/// The lines `logs` asks for (fixed, so the command text is fixed).
pub const LOG_LINES: u32 = 200;

/// Every command the config can send to `server`: start/stop/status/logs of every known spec
/// on every managed card, plus the server's template hooks. Sorted by id, deduplicated.
pub fn commands_for(c: &ControlConfig, server: &str) -> Result<Vec<Cmd>, String> {
    use super::{drivers, resolve};
    let mut out: BTreeMap<String, Cmd> = BTreeMap::new();
    let mut add = |cmd: Cmd| {
        if matches!(cmd.op, Op::Command { .. }) {
            out.entry(cmd.id.clone()).or_insert(cmd);
        }
    };
    for card in c.managed_cards(server) {
        for spec in resolve::known_specs(c, card) {
            let Some(e) = c.engines.get(&spec.engine) else {
                continue;
            };
            let d = drivers::for_engine(e);
            if let Ok(r) = resolve::resolve(c, card, &spec) {
                for cmd in d.start(&r).map_err(|x| x.to_string())? {
                    add(cmd);
                }
            }
            let iref = resolve::instance_ref(c, card, &spec.engine)?;
            for cmd in d
                .stop(&iref, Duration::from_secs(e.stop.timeout_seconds))
                .map_err(|x| x.to_string())?
            {
                add(cmd);
            }
            add(d.status(&iref).map_err(|x| x.to_string())?);
            add(d.logs(&iref, LOG_LINES).map_err(|x| x.to_string())?);
        }
    }
    if let Some(s) = c.servers.get(server) {
        for (name, text) in &s.templates {
            add(Cmd::shell(
                &format!("template:{name}"),
                text.clone(),
                false,
                Duration::from_secs(60),
            ));
        }
    }
    Ok(out.into_values().collect())
}

/// The allowlist file for `server` (the admin reviews it, then installs it root-owned).
pub fn export(c: &ControlConfig, server: &str, paths: PathRules) -> Result<String, String> {
    let s = c
        .servers
        .get(server)
        .ok_or_else(|| format!("no server {server}"))?;
    let cmds = commands_for(c, server)?;
    let roots: BTreeSet<&str> = cmds
        .iter()
        .filter(|c| c.root)
        .map(|c| c.id.as_str())
        .collect();
    let al = Allowlist {
        version: 1,
        mode: s.hostctl,
        server: server.into(),
        audit_log: Some("/var/log/abhoth-hostctl.jsonl".into()),
        max_timeout_seconds: d_max_timeout(),
        paths,
        builtins: BuiltinRules::default(),
        hostd: None,
        commands: cmds
            .iter()
            .map(|c| AllowedCommand {
                id: c.id.clone(),
                root: roots.contains(c.id.as_str()),
                note: None,
                text: c.text().to_string(),
            })
            .collect(),
    };
    let body = toml::to_string_pretty(&al).map_err(|e| e.to_string())?;
    Ok(format!(
        "# hostctl allowlist for server `{server}`: generated by `abhoth-router hostctl export`.\n\
         # Review every command, then install it root-owned 0644 (deploy/hostctl/README.md).\n\
         # A config change that alters a command needs a new export: the host refuses changed text.\n\n{body}"
    ))
}

/// Write paths a config's builtins need on `server` (json-set files, model dirs for restores,
/// locks), as allowlist prefixes. The admin narrows or widens them on review.
pub fn suggested_paths(c: &ControlConfig, server: &str) -> PathRules {
    let mut write: BTreeSet<String> = BTreeSet::new();
    let mut read: BTreeSet<String> = BTreeSet::new();
    let dir = |p: &str| {
        p.rsplit_once('/')
            .map(|(d, _)| d.to_string())
            .filter(|d| !d.is_empty())
    };
    let vars = c
        .servers
        .get(server)
        .map(|s| s.vars.clone())
        .unwrap_or_default();
    let expand = |t: &str| -> String {
        let mut s = t.to_string();
        for (k, v) in &vars {
            s = s.replace(&format!("{{var:{k}}}"), v);
        }
        s
    };
    if let Some(s) = c.servers.get(server) {
        // The lock files themselves (their `.holder` sidecar is written by flock itself).
        for l in &s.locks {
            write.insert(l.clone());
        }
    }
    for e in c.engines.values() {
        for h in e.before_start.iter().chain(&e.prepare) {
            if let Some(f) = h.params.get("file").and_then(Value::as_str) {
                let f = expand(f);
                if let Some(d) = dir(&f).filter(|d| !d.contains('{')) {
                    write.insert(d);
                }
            }
        }
        if let Some(md) = &e.model_dir {
            let md = expand(md);
            if !md.contains('{') {
                write.insert(md);
            }
        }
    }
    if let Some(m) = vars.get("archive_manifest")
        && let Some(d) = dir(m)
    {
        read.insert(d);
    }
    // What the ready hooks and the guards read (file_exists, json_file).
    if let Some(s) = c.servers.get(server) {
        for h in &s.ready {
            if let Some(p) = h.params.get("path").and_then(Value::as_str) {
                read.insert(expand(p));
            }
        }
    }
    for g in c.guards.values() {
        if g.server.as_deref().is_some_and(|s| s != server) {
            continue;
        }
        if let Some(p) = &g.path {
            let p = expand(p);
            if !p.contains('{') {
                read.insert(p);
            }
        }
    }
    read.insert("/run".into());
    PathRules {
        read: read.into_iter().collect(),
        write: write.into_iter().collect(),
    }
}

/// `hostctl export --config router.toml --server ID [--out FILE]`.
pub fn export_cli(config: &Path, server: &str, out: Option<&PathBuf>) -> Result<String, String> {
    let loaded = super::load::load(config).map_err(|is| {
        is.iter()
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    })?;
    let text = export(
        &loaded.config,
        server,
        suggested_paths(&loaded.config, server),
    )?;
    if let Some(o) = out {
        std::fs::write(o, &text).map_err(|e| format!("{}: {e}", o.display()))?;
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::validate::tests::sample;
    use serde_json::json;

    fn req(op: Op, root: bool) -> ExecRequest {
        ExecRequest {
            server: "gpu".into(),
            op,
            timeout: Duration::from_secs(10),
            root,
            stdin: None,
        }
    }

    #[test]
    fn export_round_trips_and_checks_exact_text() {
        let c = sample();
        let text = export(&c, "gpu", suggested_paths(&c, "gpu")).unwrap();
        let al = Allowlist::parse(&text).unwrap();
        assert!(!al.commands.is_empty());
        let docker_run = al
            .commands
            .iter()
            .find(|c| c.id.starts_with("docker-run:"))
            .expect("a docker run");
        assert!(docker_run.root);
        let ok = req(
            Op::Command {
                id: docker_run.id.clone(),
                text: docker_run.text.clone(),
                params: Default::default(),
            },
            true,
        );
        assert_eq!(al.check(&ok), Ok(()));
        let changed = req(
            Op::Command {
                id: docker_run.id.clone(),
                text: format!("{} --privileged", docker_run.text),
                params: Default::default(),
            },
            true,
        );
        assert!(al.check(&changed).unwrap_err().contains("differs"));
        let unknown = req(
            Op::Command {
                id: "x:1".into(),
                text: "rm -rf /".into(),
                params: Default::default(),
            },
            false,
        );
        assert!(
            al.check(&unknown)
                .unwrap_err()
                .contains("not in the allowlist")
        );
        // Every command the lifecycle can send is in the export.
        for card in ["A", "B"] {
            for spec in crate::control::resolve::known_specs(&c, card) {
                if let Ok(cmds) = crate::control::resolve::start_commands(&c, card, &spec) {
                    for cmd in cmds
                        .into_iter()
                        .filter(|c| matches!(c.op, Op::Command { .. }))
                    {
                        assert_eq!(al.check(&cmd.request("gpu")), Ok(()), "{}", cmd.id);
                    }
                }
            }
        }
    }

    #[test]
    fn builtins_are_path_checked() {
        let al = Allowlist {
            version: 1,
            max_timeout_seconds: 600,
            paths: PathRules {
                read: vec!["/srv/ro".into()],
                write: vec!["/srv/ai/qfn/".into()],
            },
            ..Default::default()
        };
        let b = |name: &str, args: Value, root: bool| {
            req(
                Op::Builtin {
                    name: name.into(),
                    args,
                },
                root,
            )
        };
        assert_eq!(
            al.check(&b("json-set", json!({"file": "/srv/ai/qfn/x.json"}), true)),
            Ok(())
        );
        assert!(
            al.check(&b("json-set", json!({"file": "/etc/passwd"}), true))
                .is_err()
        );
        assert!(
            al.check(&b(
                "json-set",
                json!({"file": "/srv/ai/qfn/../../etc/x"}),
                true
            ))
            .is_err()
        );
        assert!(
            al.check(&b("json-set", json!({"file": "/srv/ai/qfnX/x"}), true))
                .is_err()
        );
        assert_eq!(
            al.check(&b("read-json", json!({"path": "/srv/ro/a.json"}), false)),
            Ok(())
        );
        assert!(
            al.check(&b("read-json", json!({"path": "/srv/ro/a.json"}), true))
                .is_err(),
            "not a root builtin"
        );
        assert!(al.check(&b("bogus", json!({}), false)).is_err());
        assert!(
            al.check(&req(
                Op::Hostd {
                    method: "fans.set".into(),
                    params: json!({})
                },
                false
            ))
            .is_err()
        );
    }

    #[test]
    fn replies_round_trip() {
        let r = serde_json::to_string(&Reply::Err(ExecError::Refused("no".into()))).unwrap();
        assert_eq!(
            parse_reply(&format!("noise\n{r}\n")),
            Some(Err(ExecError::Refused("no".into())))
        );
        let r = serde_json::to_string(&Reply::Ok(ExecOutput::ok_with("hi"))).unwrap();
        assert_eq!(parse_reply(&r).unwrap().unwrap().stdout, "hi");
    }
}
