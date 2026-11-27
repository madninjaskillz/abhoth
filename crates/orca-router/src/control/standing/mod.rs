//! Standing engines (D153 phase 5, ROUTER-MEDIA-PLAN.md 3.1 and 14): `lifecycle = "standing"`
//! package engines the router keeps running on each card of their `port`, through the server's
//! Docker API (`servers.x.docker_api`, a loopback docker socket proxy).
//!
//! Every tick, for each (engine, card): inspect the container. Missing: render the package's
//! container method into a `docker run`, check it against the router-side rules ([`check`]),
//! pull the image when it has a registry, create and start it. Stopped: start it again, with
//! backoff. Running: probe its health (HTTP path or TCP) and report `starting`, `ready` or
//! `unhealthy`. The supervisor never stops, removes or replaces a container (so no pipeline run
//! or layout switch can lose one through it), and a container of that name it did not create
//! (no `abhoth.standing` label) is left alone and reported as `foreign`.
//!
//! Every create, start and pull is appended to `<state_dir>/standing-audit.jsonl`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use orca_engines::manifest::{GpuKind, MethodType};
use orca_engines::render::{StartPlan, plan_start};
use orca_engines::{Approvals, Installed, Manifest};
use serde::Serialize;
use serde_json::{Value, json};

use super::driver::Resolved;
use super::schema::{ControlConfig, DriverKind};
use crate::state::AppState;

/// How often each standing engine is looked at.
pub const TICK: Duration = Duration::from_secs(15);
/// Seconds between start tries after failures (then the last, repeated).
const BACKOFF: [i64; 5] = [10, 30, 120, 600, 1800];
/// The label on every container the supervisor creates.
pub const LABEL: &str = "abhoth.standing";

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// What the admin sees for one (engine, card).
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub engine: String,
    pub card: String,
    pub server: String,
    pub container: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// `starting`, `ready`, `unhealthy`, `stopped`, `backoff`, `failed`, `foreign`, `blocked`.
    pub state: String,
    pub detail: String,
    /// Starts after the first, since the router started.
    pub restarts: u32,
    /// When the container last started (Unix seconds).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_try: Option<i64>,
    pub checked: i64,
    #[serde(skip)]
    fails: u32,
}

static STATE: LazyLock<Mutex<BTreeMap<String, Status>>> = LazyLock::new(Default::default);

/// `GET /admin/api/standing`.
pub fn view() -> Value {
    let s = STATE.lock().unwrap();
    json!({"engines": s.values().collect::<Vec<_>>()})
}

/// One standing engine's state, for the inventory view.
pub fn status_of(engine: &str, card: &str) -> Option<Status> {
    STATE
        .lock()
        .unwrap()
        .get(&format!("{engine}/{card}"))
        .cloned()
}

// ── docker run words → Docker API ────────────────────────────────────────────────────────────

/// A `docker run` as the package renders it, split into what the Docker API takes.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RunArgs {
    pub name: String,
    pub image: String,
    pub cmd: Vec<String>,
    pub entrypoint: Option<String>,
    pub env: Vec<String>,
    pub labels: BTreeMap<String, String>,
    /// (host ip, host port, container port, protocol)
    pub ports: Vec<(String, u16, u16, String)>,
    /// (source, target, read only)
    pub binds: Vec<(String, String, bool)>,
    pub gpus: Option<String>,
    pub restart: Option<String>,
    pub shm_size: Option<i64>,
    pub privileged: bool,
    pub network: Option<String>,
    pub pid: Option<String>,
    pub ipc: Option<String>,
    pub devices: Vec<String>,
    pub cap_add: Vec<String>,
    pub user: Option<String>,
    pub workdir: Option<String>,
    pub cpus: Option<f64>,
    pub memory: Option<i64>,
    pub cpuset: Option<String>,
    pub runtime: Option<String>,
}

/// `512m`, `2g`, `1024` → bytes.
fn bytes(s: &str) -> Option<i64> {
    let s = s.trim().to_ascii_lowercase();
    let (n, mul) = match s.chars().last()? {
        'k' => (&s[..s.len() - 1], 1i64 << 10),
        'm' => (&s[..s.len() - 1], 1 << 20),
        'g' => (&s[..s.len() - 1], 1 << 30),
        'b' => (&s[..s.len() - 1], 1),
        _ => (s.as_str(), 1),
    };
    n.parse::<f64>().ok().map(|x| (x * mul as f64) as i64)
}

/// Parse `docker run …` words. Unknown options are refused rather than dropped.
pub fn parse_run(argv: &[String]) -> Result<RunArgs, String> {
    let mut a = RunArgs::default();
    let mut it = argv.iter().peekable();
    if it.next().map(String::as_str) != Some("docker")
        || it.next().map(String::as_str) != Some("run")
    {
        return Err("not a docker run".into());
    }
    while let Some(w) = it.next() {
        if !w.starts_with('-') {
            a.image = w.clone();
            a.cmd = it.by_ref().cloned().collect();
            break;
        }
        let (flag, inline) = match w.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f.to_string(), Some(v.to_string())),
            _ => (w.clone(), None),
        };
        let boolean = matches!(flag.as_str(), "-d" | "--detach" | "--privileged" | "--init");
        let val = if boolean {
            String::new()
        } else {
            match inline {
                Some(v) => v,
                None => it
                    .next()
                    .cloned()
                    .ok_or_else(|| format!("{flag} has no value"))?,
            }
        };
        match flag.as_str() {
            "-d" | "--detach" | "--init" => {}
            "--privileged" => a.privileged = true,
            "--name" => a.name = val,
            "--restart" => a.restart = Some(val),
            "--label" | "-l" => {
                let (k, v) = val.split_once('=').unwrap_or((val.as_str(), ""));
                a.labels.insert(k.to_string(), v.to_string());
            }
            "-e" | "--env" => a.env.push(val),
            "-p" | "--publish" => {
                let (spec, proto) = val.split_once('/').unwrap_or((val.as_str(), "tcp"));
                let parts: Vec<&str> = spec.rsplitn(3, ':').collect();
                let (ip, hp, cp) = match parts.as_slice() {
                    [cp, hp, ip] => (ip.to_string(), *hp, *cp),
                    [cp, hp] => (String::new(), *hp, *cp),
                    _ => return Err(format!("-p {val}: give host and container ports")),
                };
                let hp = hp.parse().map_err(|_| format!("-p {val}: bad host port"))?;
                let cp = cp
                    .parse()
                    .map_err(|_| format!("-p {val}: bad container port"))?;
                a.ports.push((ip, hp, cp, proto.to_string()));
            }
            "-v" | "--volume" => {
                let parts: Vec<&str> = val.split(':').collect();
                match parts.as_slice() {
                    [s, t] => a.binds.push((s.to_string(), t.to_string(), false)),
                    [s, t, o] => a.binds.push((
                        s.to_string(),
                        t.to_string(),
                        o.split(',').any(|x| x == "ro"),
                    )),
                    _ => return Err(format!("-v {val}: give source and target")),
                }
            }
            "--gpus" => a.gpus = Some(val),
            "--network" | "--net" => a.network = Some(val),
            "--pid" => a.pid = Some(val),
            "--ipc" => a.ipc = Some(val),
            "--device" => a.devices.push(val),
            "--cap-add" => a.cap_add.push(val),
            "--entrypoint" => a.entrypoint = Some(val),
            "--shm-size" => a.shm_size = Some(bytes(&val).ok_or("--shm-size: bad size")?),
            "-u" | "--user" => a.user = Some(val),
            "-w" | "--workdir" => a.workdir = Some(val),
            "--cpus" => a.cpus = Some(val.parse().map_err(|_| "--cpus: bad number")?),
            "-m" | "--memory" => a.memory = Some(bytes(&val).ok_or("--memory: bad size")?),
            "--cpuset-cpus" => a.cpuset = Some(val),
            "--runtime" => a.runtime = Some(val),
            f => {
                return Err(format!(
                    "the docker option {f} is not supported for standing engines"
                ));
            }
        }
    }
    if a.image.is_empty() {
        return Err("the docker run names no image".into());
    }
    if a.name.is_empty() {
        return Err("the docker run names no container".into());
    }
    Ok(a)
}

/// Whether `path` is `root` or inside it (no `..` tricks: both are plain absolute paths).
fn inside(path: &str, root: &str) -> bool {
    let root = root.trim_end_matches('/');
    !root.is_empty()
        && path.starts_with('/')
        && !path.split('/').any(|x| x == "..")
        && (path == root || path.starts_with(&format!("{root}/")))
}

/// The router-side rules for a standing engine's container, checked before every create
/// (the socket proxy cannot look inside a create body): no privileged container, no host
/// network, PID or IPC namespace, no added capabilities or raw devices, binds only under
/// `roots`, GPUs only the card's own (none on a CPU card), ports only on loopback.
pub fn check(a: &RunArgs, roots: &[String], gpu_uuid: Option<&str>) -> Result<(), String> {
    if a.privileged {
        return Err("a privileged container".into());
    }
    if let Some(n) = &a.network
        && n != "bridge"
    {
        return Err(format!("the {n} network"));
    }
    if a.pid.is_some() {
        return Err("a shared PID namespace".into());
    }
    if a.ipc
        .as_deref()
        .is_some_and(|x| x != "private" && x != "none")
    {
        return Err("a shared IPC namespace".into());
    }
    if !a.cap_add.is_empty() {
        return Err(format!("added capabilities ({})", a.cap_add.join(", ")));
    }
    if !a.devices.is_empty() {
        return Err(format!("raw devices ({})", a.devices.join(", ")));
    }
    for (src, _, _) in &a.binds {
        if !roots.iter().any(|r| inside(src, r)) {
            return Err(format!("the mount {src} is outside the allowed folders"));
        }
        if src == "/var/run/docker.sock" || src == "/run/docker.sock" {
            return Err("the docker socket".into());
        }
    }
    match (&a.gpus, gpu_uuid) {
        (None, _) => {}
        (Some(g), Some(u)) if !u.is_empty() && g.eq_ignore_ascii_case(&format!("device={u}")) => {}
        (Some(g), _) => return Err(format!("GPUs `{g}` (only the card's own GPU is allowed)")),
    }
    for (ip, ..) in &a.ports {
        if ip != "127.0.0.1" && ip != "::1" {
            return Err(format!(
                "a port published on {} (standing engines listen on loopback; the router fronts them)",
                if ip.is_empty() { "every address" } else { ip }
            ));
        }
    }
    Ok(())
}

/// The Docker API create body.
pub fn create_body(a: &RunArgs) -> Value {
    let mut exposed = serde_json::Map::new();
    let mut bindings = serde_json::Map::new();
    for (ip, hp, cp, proto) in &a.ports {
        let k = format!("{cp}/{proto}");
        exposed.insert(k.clone(), json!({}));
        bindings.insert(k, json!([{"HostIp": ip, "HostPort": hp.to_string()}]));
    }
    let restart = a.restart.as_deref().unwrap_or("unless-stopped");
    let (rname, rmax) = match restart.split_once(':') {
        Some((n, m)) => (n.to_string(), m.parse::<i64>().unwrap_or(0)),
        None => (restart.to_string(), 0),
    };
    let mut host = json!({
        "Binds": a.binds.iter().map(|(s, t, ro)| format!("{s}:{t}{}", if *ro { ":ro" } else { "" })).collect::<Vec<_>>(),
        "PortBindings": bindings,
        "RestartPolicy": {"Name": rname, "MaximumRetryCount": rmax},
        "Privileged": false,
        "NetworkMode": a.network.clone().unwrap_or_else(|| "bridge".into()),
    });
    if let Some(g) = &a.gpus {
        let ids: Vec<String> = g
            .trim_start_matches("device=")
            .split(',')
            .map(String::from)
            .collect();
        host["DeviceRequests"] =
            json!([{"Driver": "nvidia", "DeviceIDs": ids, "Capabilities": [["gpu"]]}]);
    }
    if let Some(x) = a.shm_size {
        host["ShmSize"] = json!(x);
    }
    if let Some(x) = a.cpus {
        host["NanoCpus"] = json!((x * 1e9) as i64);
    }
    if let Some(x) = a.memory {
        host["Memory"] = json!(x);
    }
    if let Some(x) = &a.cpuset {
        host["CpusetCpus"] = json!(x);
    }
    if let Some(x) = &a.runtime {
        host["Runtime"] = json!(x);
    }
    let mut b = json!({
        "Image": a.image,
        "Env": a.env,
        "Labels": a.labels,
        "ExposedPorts": exposed,
        "HostConfig": host,
    });
    if !a.cmd.is_empty() {
        b["Cmd"] = json!(a.cmd);
    }
    if let Some(e) = &a.entrypoint {
        b["Entrypoint"] = json!([e]);
    }
    if let Some(u) = &a.user {
        b["User"] = json!(u);
    }
    if let Some(w) = &a.workdir {
        b["WorkingDir"] = json!(w);
    }
    b
}

// ── the Docker API ───────────────────────────────────────────────────────────────────────────

/// A small client for the Docker Engine API (through the socket proxy).
pub struct Docker {
    base: String,
    http: reqwest::Client,
}

fn enc(s: &str) -> String {
    let mut o = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            o.push(b as char);
        } else {
            o.push_str(&format!("%{b:02X}"));
        }
    }
    o
}

impl Docker {
    pub fn new(base: &str) -> Self {
        Docker {
            base: base.trim_end_matches('/').to_string(),
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .build()
                .unwrap_or_default(),
        }
    }
    fn url(&self, p: &str) -> String {
        format!("{}{p}", self.base)
    }
    /// `GET /containers/<name>/json`: `None` when there is none.
    pub async fn inspect(&self, name: &str) -> Result<Option<Value>, String> {
        let r = self
            .http
            .get(self.url(&format!("/containers/{}/json", enc(name))))
            .timeout(Duration::from_secs(20))
            .send()
            .await
            .map_err(|e| format!("the Docker API: {e}"))?;
        match r.status().as_u16() {
            200 => r.json().await.map(Some).map_err(|e| e.to_string()),
            404 => Ok(None),
            s => Err(format!("inspect {name}: {s}")),
        }
    }
    pub async fn has_image(&self, image: &str) -> Result<bool, String> {
        let r = self
            .http
            .get(self.url(&format!("/images/{}/json", enc(image))))
            .timeout(Duration::from_secs(20))
            .send()
            .await
            .map_err(|e| format!("the Docker API: {e}"))?;
        match r.status().as_u16() {
            200 => Ok(true),
            404 => Ok(false),
            s => Err(format!("image {image}: {s}")),
        }
    }
    /// `POST /images/create?fromImage=…`; the stream's `error` lines become the error.
    pub async fn pull(&self, image: &str) -> Result<(), String> {
        let r = self
            .http
            .post(self.url(&format!("/images/create?fromImage={}", enc(image))))
            .timeout(Duration::from_secs(3600))
            .send()
            .await
            .map_err(|e| format!("pull {image}: {e}"))?;
        let ok = r.status().is_success();
        let text = r.text().await.unwrap_or_default();
        for line in text.lines() {
            if let Ok(v) = serde_json::from_str::<Value>(line)
                && let Some(e) = v.get("error").and_then(Value::as_str)
            {
                return Err(format!("pull {image}: {e}"));
            }
        }
        if ok {
            Ok(())
        } else {
            Err(format!("pull {image}: {}", text.trim()))
        }
    }
    pub async fn create(&self, name: &str, body: &Value) -> Result<(), String> {
        let r = self
            .http
            .post(self.url(&format!("/containers/create?name={}", enc(name))))
            .timeout(Duration::from_secs(60))
            .json(body)
            .send()
            .await
            .map_err(|e| format!("create {name}: {e}"))?;
        if r.status().is_success() {
            Ok(())
        } else {
            let s = r.status().as_u16();
            Err(format!(
                "create {name}: {s} {}",
                r.text().await.unwrap_or_default().trim()
            ))
        }
    }
    pub async fn start(&self, name: &str) -> Result<(), String> {
        let r = self
            .http
            .post(self.url(&format!("/containers/{}/start", enc(name))))
            .timeout(Duration::from_secs(60))
            .send()
            .await
            .map_err(|e| format!("start {name}: {e}"))?;
        match r.status().as_u16() {
            204 | 304 => Ok(()),
            s => Err(format!(
                "start {name}: {s} {}",
                r.text().await.unwrap_or_default().trim()
            )),
        }
    }
}

// ── rendering a standing engine ──────────────────────────────────────────────────────────────

/// What a standing (engine, card) runs.
pub struct Plan {
    pub run: RunArgs,
    pub health_http: Option<String>,
    pub health_tcp: bool,
    pub start_seconds: u64,
    pub port: u16,
}

/// The default store's manifest for a package reference.
fn manifest_of(id: &str, version: &str) -> Result<Manifest, String> {
    orca_engines::store::embedded::packages()
        .into_iter()
        .map(|p| p.manifest)
        .find(|m| m.id == id && m.version == version)
        .ok_or_else(|| format!("package {id}@{version} is not in the default store"))
}

/// The folders a standing engine on `server` may mount.
pub fn mount_roots(c: &ControlConfig, server: &str) -> Vec<String> {
    let mut r = Vec::new();
    if let Some(s) = c.servers.get(server) {
        r.extend(s.docker_mount_roots.iter().cloned());
        r.extend(s.model_dir.iter().cloned());
        r.extend(s.comfy_root.iter().cloned());
        let es = s
            .vars
            .get("engine_state")
            .cloned()
            .unwrap_or_else(|| super::packages::DEFAULT_ENGINE_STATE.to_string());
        r.push(es);
    }
    r
}

/// Render `engine` on `card` into a checked `docker run`.
pub fn plan(c: &ControlConfig, engine: &str, card: &str, state_dir: &Path) -> Result<Plan, String> {
    let e = c
        .engines
        .get(engine)
        .ok_or_else(|| format!("no engine `{engine}`"))?;
    if e.driver != DriverKind::Package {
        return Err("a standing engine is an engine package".into());
    }
    let pr = e.package.as_ref().ok_or("no package = { id, version }")?;
    let m = manifest_of(&pr.id, &pr.version)?;
    let cd = c
        .cards
        .get(card)
        .ok_or_else(|| format!("no card `{card}`"))?;
    let want = if cd.is_cpu() {
        GpuKind::Cpu
    } else {
        GpuKind::Cuda
    };
    let (k, meth) = m
        .methods
        .iter()
        .enumerate()
        .find(|(_, x)| {
            x.is_container()
                && x.r#type != MethodType::Compose
                && pr.method.as_deref().is_none_or(|t| x.r#type.as_str() == t)
                && m.method_gpus(x).contains(&want)
        })
        .ok_or_else(|| {
            format!(
                "package {} has no docker method for a {} card",
                m.key(),
                if cd.is_cpu() { "CPU" } else { "CUDA" }
            )
        })?;
    let mut iref = super::resolve::instance_ref(c, card, engine)?;
    if let Some(md) = &e.model_dir {
        iref.vars.insert(
            "model_dir".into(),
            super::template::TplValue::Word(md.clone()),
        );
    }
    let port = *e
        .port
        .get(card)
        .ok_or_else(|| format!("no port for card `{card}`"))?;
    let r = Resolved {
        iref,
        port,
        spec_json: json!({"engine": engine}).to_string(),
        ..Default::default()
    };
    let pkg = super::packages::Package::from(e);
    let spec = pkg.run_spec(&r, &m).map_err(|x| x.0)?;
    // the declared host mounts, rendered with the engine's params (the rules check each one)
    let params: indexmap::IndexMap<String, Value> = {
        let mut p: indexmap::IndexMap<String, Value> = m
            .params
            .iter()
            .filter_map(|(k, x)| x.default.clone().map(|d| (k.clone(), d)))
            .collect();
        p.extend(pr.params.clone());
        p
    };
    let mounts = m
        .security
        .host_mounts
        .iter()
        .map(|h| {
            let mut s = h.source.clone();
            for (k, v) in &params {
                let v = v
                    .as_str()
                    .map(String::from)
                    .unwrap_or_else(|| v.to_string());
                s = s.replace(&format!("{{{{param.{k}}}}}"), &v);
            }
            s
        })
        .collect();
    let inst = Installed {
        id: m.id.clone(),
        version: m.version.clone(),
        method: k,
        method_type: meth.r#type,
        dir: state_dir.join("standing").join(engine),
        os: "linux".into(),
        venv: None,
        install: None,
        bin: None,
        image: None,
        pins: vec![],
        found_version: None,
        at: now(),
        approvals: Approvals {
            host_mounts: mounts,
            by: "router:standing".into(),
            ..Default::default()
        },
        store: pr.store.clone().unwrap_or_else(|| "default".into()),
    };
    let argv = match plan_start(&m, &inst, &spec, &inst.approvals)? {
        StartPlan::Docker { argv, .. } => argv,
        _ => return Err("the package method is not a docker run".into()),
    };
    let mut run = parse_run(&argv)?;
    run.labels.insert(LABEL.into(), "1".into());
    run.labels.insert("abhoth.card".into(), card.into());
    let uuid = (!cd.is_cpu()).then_some(cd.gpu_uuid.as_str());
    check(&run, &mount_roots(c, &cd.server), uuid).map_err(|why| format!("refused: {why}"))?;
    Ok(Plan {
        run,
        health_http: meth.health.http.clone(),
        health_tcp: meth.health.tcp,
        start_seconds: meth.health.start_seconds.unwrap_or(300),
        port,
    })
}

// ── the supervisor ───────────────────────────────────────────────────────────────────────────

fn audit(state_dir: &Path, op: &str, engine: &str, card: &str, detail: &Value) {
    let line = json!({"at": now(), "op": op, "engine": engine, "card": card, "detail": detail});
    tracing::info!("standing: {op} {engine}/{card}: {detail}");
    let p = state_dir.join("standing-audit.jsonl");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(p)
    {
        use std::io::Write;
        let _ = writeln!(f, "{line}");
    }
}

async fn healthy(p: &Plan) -> bool {
    let addr = format!("127.0.0.1:{}", p.port);
    if let Some(path) = &p.health_http {
        let url = format!("http://{addr}{path}");
        return reqwest::Client::new()
            .get(url)
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .is_ok_and(|r| r.status().is_success());
    }
    tokio::time::timeout(
        Duration::from_secs(5),
        tokio::net::TcpStream::connect(&addr),
    )
    .await
    .is_ok_and(|r| r.is_ok())
}

fn set(key: &str, f: impl FnOnce(&mut Status)) {
    let mut s = STATE.lock().unwrap();
    if let Some(x) = s.get_mut(key) {
        f(x);
        x.checked = now();
    }
}

/// Look at one (engine, card) and do what it needs.
pub async fn tend(c: &ControlConfig, engine: &str, card: &str, state_dir: &Path) {
    let key = format!("{engine}/{card}");
    let server = c
        .cards
        .get(card)
        .map(|x| x.server.clone())
        .unwrap_or_default();
    let name = super::resolve::instance_ref(c, card, engine)
        .map(|i| i.name)
        .unwrap_or_default();
    STATE
        .lock()
        .unwrap()
        .entry(key.clone())
        .or_insert_with(|| Status {
            engine: engine.into(),
            card: card.into(),
            server: server.clone(),
            container: name.clone(),
            image: None,
            state: "starting".into(),
            detail: String::new(),
            restarts: 0,
            started: None,
            next_try: None,
            checked: now(),
            fails: 0,
        });
    let Some(api) = c.servers.get(&server).and_then(|s| s.docker_api.clone()) else {
        set(&key, |x| {
            x.state = "blocked".into();
            x.detail = format!("server `{server}` has no docker_api");
        });
        return;
    };
    let p = match plan(c, engine, card, state_dir) {
        Ok(p) => p,
        Err(why) => {
            set(&key, |x| {
                x.state = "blocked".into();
                x.detail = why;
            });
            return;
        }
    };
    let d = Docker::new(&api);
    set(&key, |x| x.image = Some(p.run.image.clone()));
    let found = match d.inspect(&p.run.name).await {
        Ok(f) => f,
        Err(why) => {
            set(&key, |x| {
                x.state = "failed".into();
                x.detail = why;
            });
            return;
        }
    };
    let (due, restarts) = {
        let s = STATE.lock().unwrap();
        let x = &s[&key];
        (x.next_try.is_none_or(|t| now() >= t), x.restarts)
    };
    match found {
        Some(info) => {
            if info["Config"]["Labels"][LABEL].as_str() != Some("1") {
                set(&key, |x| {
                    x.state = "foreign".into();
                    x.detail = format!(
                        "a container named {} that the router did not create runs there; left alone",
                        p.run.name
                    );
                });
                return;
            }
            if info["State"]["Running"].as_bool() == Some(true) {
                let started = info["State"]["StartedAt"]
                    .as_str()
                    .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                    .map(|t| t.timestamp());
                let ok = healthy(&p).await;
                let young = started.is_some_and(|t| now() - t < p.start_seconds as i64);
                set(&key, |x| {
                    x.started = started;
                    x.next_try = None;
                    if ok {
                        x.fails = 0;
                    }
                    x.state = if ok {
                        "ready"
                    } else if young {
                        "starting"
                    } else {
                        "unhealthy"
                    }
                    .into();
                    x.detail = if ok {
                        String::new()
                    } else {
                        "its health check does not pass".into()
                    };
                });
                return;
            }
            if !due {
                set(&key, |x| x.state = "backoff".into());
                return;
            }
            let r = d.start(&p.run.name).await;
            audit(
                state_dir,
                "start",
                engine,
                card,
                &json!({"container": p.run.name, "ok": r.is_ok(), "error": r.as_ref().err()}),
            );
            after_start(&key, r, restarts + 1);
        }
        None => {
            if !due {
                set(&key, |x| x.state = "backoff".into());
                return;
            }
            let local = p.run.image.starts_with("local/") || !p.run.image.contains('/');
            match d.has_image(&p.run.image).await {
                Ok(true) => {}
                Ok(false) if local => {
                    let why = format!(
                        "the image {} is not on the host (it is built there)",
                        p.run.image
                    );
                    after_start(&key, Err(why), restarts);
                    return;
                }
                Ok(false) => {
                    let r = d.pull(&p.run.image).await;
                    audit(
                        state_dir,
                        "pull",
                        engine,
                        card,
                        &json!({"image": p.run.image, "ok": r.is_ok(), "error": r.as_ref().err()}),
                    );
                    if let Err(why) = r {
                        after_start(&key, Err(why), restarts);
                        return;
                    }
                }
                Err(why) => {
                    after_start(&key, Err(why), restarts);
                    return;
                }
            }
            let body = create_body(&p.run);
            let r = d.create(&p.run.name, &body).await;
            audit(
                state_dir,
                "create",
                engine,
                card,
                &json!({"container": p.run.name, "image": p.run.image, "binds": body["HostConfig"]["Binds"], "gpus": p.run.gpus, "ok": r.is_ok(), "error": r.as_ref().err()}),
            );
            if let Err(why) = r {
                after_start(&key, Err(why), restarts);
                return;
            }
            let r = d.start(&p.run.name).await;
            audit(
                state_dir,
                "start",
                engine,
                card,
                &json!({"container": p.run.name, "ok": r.is_ok(), "error": r.as_ref().err()}),
            );
            after_start(&key, r, restarts);
        }
    }
}

fn after_start(key: &str, r: Result<(), String>, restarts: u32) {
    set(key, |x| match r {
        Ok(()) => {
            x.state = "starting".into();
            x.detail = String::new();
            x.restarts = restarts;
            x.started = Some(now());
            x.next_try = Some(now() + BACKOFF[0]);
        }
        Err(why) => {
            let k = (x.fails as usize).min(BACKOFF.len() - 1);
            x.fails += 1;
            x.state = "failed".into();
            x.detail = why;
            x.next_try = Some(now() + BACKOFF[k]);
        }
    });
}

/// The standing (engine, card) pairs in `c`.
pub fn pairs(c: &ControlConfig) -> Vec<(String, String)> {
    let mut v = Vec::new();
    for (id, e) in &c.engines {
        if !e.is_standing() {
            continue;
        }
        for card in e.port.cards() {
            if c.cards.get(card).is_some_and(|x| {
                // the router's own servers only (a bridge runs its engines itself)
                c.servers.get(&x.server).is_some_and(|s| s.origin.is_none())
            }) {
                v.push((id.clone(), card.to_string()));
            }
        }
    }
    v
}

/// The supervisor: tends every standing engine of the live config each [`TICK`].
pub async fn run(st: Arc<AppState>, fallback: Arc<ControlConfig>, state_dir: PathBuf) {
    tokio::time::sleep(Duration::from_secs(10)).await;
    loop {
        let c = match st.bridges.get() {
            Some(h) => Arc::new(h.config()),
            None => fallback.clone(),
        };
        let now_pairs = pairs(&c);
        STATE
            .lock()
            .unwrap()
            .retain(|k, _| now_pairs.iter().any(|(e, cd)| *k == format!("{e}/{cd}")));
        for (e, cd) in &now_pairs {
            tend(&c, e, cd, &state_dir).await;
        }
        tokio::time::sleep(TICK).await;
    }
}

#[cfg(test)]
mod tests;
