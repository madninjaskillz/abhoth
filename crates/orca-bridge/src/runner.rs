//! Starting and stopping engines on the bridge's host (ROUTER-MEDIA-PLAN.md 13.3).
//!
//! [`Runner`] is the seam the shared engine runner (`orca-engines`, engine packages) replaces;
//! until it lands, [`DockerRunner`] (the `docker` CLI) and [`ProcessRunner`] (a child process)
//! cover manual engines. [`Supervisor`] owns the engine instances (one per engine and card):
//! on-demand start when a request arrives, the idle stop, and what `status` and `logs` report.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::Log;
use crate::config::{BridgeConfig, EngineConfig};
use crate::detect::Gpu;

/// What to start: one engine on one card.
#[derive(Debug, Clone)]
pub struct RunSpec {
    pub bridge: String,
    pub engine: String,
    pub card: String,
    pub port: u16,
    pub gpu: Option<Gpu>,
    pub cfg: EngineConfig,
    pub model_dir: Option<String>,
    pub state_dir: PathBuf,
}

impl RunSpec {
    /// The container name (only names with this prefix are ever removed).
    pub fn container(&self) -> String {
        format!(
            "{CONTAINER_PREFIX}{}-{}-{}",
            self.bridge,
            self.engine,
            self.card.to_ascii_lowercase()
        )
    }
    fn subst(&self, s: &str) -> String {
        let gi = self.gpu.as_ref().map(|g| g.index.to_string());
        let gu = self.gpu.as_ref().map(|g| g.uuid.clone());
        s.replace("{port}", &self.port.to_string())
            .replace("{card}", &self.card)
            .replace("{gpu_index}", gi.as_deref().unwrap_or(""))
            .replace("{gpu_uuid}", gu.as_deref().unwrap_or(""))
            .replace("{model_dir}", self.model_dir.as_deref().unwrap_or(""))
            .replace("{state_dir}", &self.state_dir.to_string_lossy())
    }
}

pub const CONTAINER_PREFIX: &str = "abhoth-bridge-";

/// How to find a running instance again (kept in the state dir, so a restarted bridge adopts
/// what is already running).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Handle {
    Container {
        name: String,
    },
    Process {
        pid: u32,
    },
    External,
    /// Started by the shared engine runner (an engine package).
    Package {
        handle: Box<orca_engines::Handle>,
    },
}

#[async_trait]
pub trait Runner: Send + Sync {
    async fn start(&self, spec: &RunSpec) -> Result<Handle, String>;
    async fn stop(&self, h: &Handle, grace: Duration) -> Result<(), String>;
    async fn running(&self, h: &Handle) -> bool;
    async fn logs(&self, h: &Handle, lines: u32) -> Result<String, String>;
    /// An instance left running by an earlier bridge process, if there is one.
    async fn adopt(&self, spec: &RunSpec) -> Option<Handle>;
    /// Fetch what a start needs (an image, a package install) ahead of the first request.
    /// `Ok(what)`: ready; `Err`: why it cannot start (shown in the bridge's status).
    async fn prepare(&self, _spec: &RunSpec) -> Result<String, String> {
        Ok("nothing to fetch".into())
    }
}

// ── docker ───────────────────────────────────────────────────────────────────────────────────

/// Containers through the `docker` CLI (`DOCKER` overrides the binary).
pub struct DockerRunner {
    pub docker: String,
}

impl Default for DockerRunner {
    fn default() -> Self {
        Self {
            docker: crate::config::env_var("ABHOTH_BRIDGE_DOCKER")
                .unwrap_or_else(|| "docker".into()),
        }
    }
}

/// `docker run` arguments for a spec (pure, for tests).
pub fn docker_args(spec: &RunSpec) -> Result<Vec<String>, String> {
    let d = spec
        .cfg
        .docker
        .as_ref()
        .ok_or_else(|| format!("engine {} has no [docker] section", spec.engine))?;
    let bind = if d.publish_all_interfaces {
        "0.0.0.0"
    } else {
        "127.0.0.1"
    };
    let mut a: Vec<String> = vec![
        "run".into(),
        "-d".into(),
        "--name".into(),
        spec.container(),
        "--label".into(),
        format!("abhoth.bridge={}", spec.bridge),
        "--label".into(),
        format!("abhoth.engine={}", spec.engine),
        "--restart".into(),
        "no".into(),
        "-p".into(),
        format!("{bind}:{}:{}", spec.port, d.container_port),
    ];
    match &spec.gpu {
        Some(g) => {
            a.push("--gpus".into());
            a.push(format!("device={}", g.uuid));
        }
        None => {
            // A CPU card: hide the GPUs even where nvidia is docker's default runtime.
            a.push("-e".into());
            a.push("NVIDIA_VISIBLE_DEVICES=void".into());
        }
    }
    if let Some(c) = &d.cpus {
        a.push(format!("--cpus={c}"));
    }
    if let Some(m) = &d.memory {
        a.push(format!("--memory={m}"));
    }
    for (k, v) in &d.env {
        a.push("-e".into());
        a.push(format!("{k}={}", spec.subst(v)));
    }
    for m in &d.mounts {
        a.push("-v".into());
        a.push(spec.subst(m));
    }
    a.extend(d.flags.iter().map(|f| spec.subst(f)));
    a.push(d.image.clone());
    a.extend(d.args.iter().map(|x| spec.subst(x)));
    Ok(a)
}

impl DockerRunner {
    async fn docker(&self, args: &[String], timeout: Duration) -> Result<String, String> {
        let mut cmd = tokio::process::Command::new(&self.docker);
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(windows)]
        cmd.creation_flags(0x0800_0000);
        let o = tokio::time::timeout(timeout, cmd.output())
            .await
            .map_err(|_| format!("docker {} timed out", args.first().map_or("", |s| s)))?
            .map_err(|e| format!("docker: {e}"))?;
        if o.status.success() {
            Ok(String::from_utf8_lossy(&o.stdout).trim().to_string())
        } else {
            Err(format!(
                "docker {}: {}",
                args.first().map_or("", |s| s),
                String::from_utf8_lossy(&o.stderr).trim()
            ))
        }
    }
}

#[async_trait]
impl Runner for DockerRunner {
    async fn start(&self, spec: &RunSpec) -> Result<Handle, String> {
        let name = spec.container();
        // A stopped leftover of ours with the same name.
        let _ = self
            .docker(
                &["rm".into(), "-f".into(), name.clone()],
                Duration::from_secs(60),
            )
            .await;
        self.docker(&docker_args(spec)?, Duration::from_secs(900))
            .await?;
        Ok(Handle::Container { name })
    }
    async fn stop(&self, h: &Handle, grace: Duration) -> Result<(), String> {
        let Handle::Container { name } = h else {
            return Ok(());
        };
        if !name.starts_with(CONTAINER_PREFIX) {
            return Err(format!("{name} is not a bridge container"));
        }
        let t = grace.as_secs().to_string();
        let _ = self
            .docker(
                &["stop".into(), "-t".into(), t, name.clone()],
                grace + Duration::from_secs(30),
            )
            .await;
        self.docker(
            &["rm".into(), "-f".into(), name.clone()],
            Duration::from_secs(60),
        )
        .await
        .map(|_| ())
    }
    async fn running(&self, h: &Handle) -> bool {
        let Handle::Container { name } = h else {
            return false;
        };
        self.docker(
            &[
                "inspect".into(),
                "-f".into(),
                "{{.State.Running}}".into(),
                name.clone(),
            ],
            Duration::from_secs(20),
        )
        .await
        .is_ok_and(|s| s == "true")
    }
    async fn logs(&self, h: &Handle, lines: u32) -> Result<String, String> {
        let Handle::Container { name } = h else {
            return Ok(String::new());
        };
        // docker logs writes the container's stderr to its stderr: take both.
        let mut cmd = tokio::process::Command::new(&self.docker);
        cmd.args(["logs", "--tail", &lines.to_string(), name])
            .stdin(Stdio::null())
            .kill_on_drop(true);
        #[cfg(windows)]
        cmd.creation_flags(0x0800_0000);
        let o = tokio::time::timeout(Duration::from_secs(30), cmd.output())
            .await
            .map_err(|_| "docker logs timed out".to_string())?
            .map_err(|e| format!("docker: {e}"))?;
        let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
        s.push_str(&String::from_utf8_lossy(&o.stderr));
        Ok(s)
    }
    async fn adopt(&self, spec: &RunSpec) -> Option<Handle> {
        let h = Handle::Container {
            name: spec.container(),
        };
        self.running(&h).await.then_some(h)
    }
    async fn prepare(&self, spec: &RunSpec) -> Result<String, String> {
        let Some(d) = &spec.cfg.docker else {
            return Ok("nothing to fetch".into());
        };
        let image = d.image.clone();
        if self
            .docker(
                &[
                    "image".into(),
                    "inspect".into(),
                    "-f".into(),
                    "{{.Id}}".into(),
                    image.clone(),
                ],
                Duration::from_secs(30),
            )
            .await
            .is_ok()
        {
            return Ok(format!("image {image} is here"));
        }
        self.docker(&["pull".into(), image.clone()], Duration::from_secs(3600))
            .await
            .map(|_| format!("pulled {image}"))
            .map_err(|e| {
                format!("image {image} is not on this host and pulling it failed ({e}); load it with `docker load` or check the registry")
            })
    }
}

// ── processes ────────────────────────────────────────────────────────────────────────────────

/// Child processes of the bridge, killed by their exact PID (and when the bridge exits).
#[derive(Default)]
pub struct ProcessRunner {
    children: tokio::sync::Mutex<HashMap<u32, (tokio::process::Child, PathBuf)>>,
}

fn log_path(spec: &RunSpec) -> PathBuf {
    spec.state_dir.join("logs").join(format!(
        "{}-{}.log",
        spec.engine,
        spec.card.to_ascii_lowercase()
    ))
}

#[async_trait]
impl Runner for ProcessRunner {
    async fn start(&self, spec: &RunSpec) -> Result<Handle, String> {
        let p = spec
            .cfg
            .process
            .as_ref()
            .ok_or_else(|| format!("engine {} has no [process] section", spec.engine))?;
        let argv: Vec<String> = p.command.iter().map(|a| spec.subst(a)).collect();
        let (bin, args) = argv
            .split_first()
            .ok_or_else(|| format!("engine {}: empty command", spec.engine))?;
        let log = log_path(spec);
        if let Some(d) = log.parent() {
            std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
        }
        let out = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)
            .map_err(|e| format!("{}: {e}", log.display()))?;
        let err = out.try_clone().map_err(|e| e.to_string())?;
        let mut cmd = tokio::process::Command::new(bin);
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(err))
            .kill_on_drop(true);
        #[cfg(windows)]
        cmd.creation_flags(0x0800_0000);
        if let Some(d) = &p.cwd {
            cmd.current_dir(spec.subst(d));
        }
        match &spec.gpu {
            Some(g) => {
                cmd.env("CUDA_VISIBLE_DEVICES", g.index.to_string());
            }
            None => {
                cmd.env("CUDA_VISIBLE_DEVICES", "");
            }
        }
        for (k, v) in &p.env {
            cmd.env(k, spec.subst(v));
        }
        let child = cmd.spawn().map_err(|e| format!("starting {bin}: {e}"))?;
        let pid = child.id().ok_or("the process ended at once")?;
        self.children.lock().await.insert(pid, (child, log));
        Ok(Handle::Process { pid })
    }
    async fn stop(&self, h: &Handle, grace: Duration) -> Result<(), String> {
        let Handle::Process { pid } = h else {
            return Ok(());
        };
        let Some((mut child, _)) = self.children.lock().await.remove(pid) else {
            return Ok(()); // not ours (never kill a PID we did not start)
        };
        #[cfg(unix)]
        {
            // SIGTERM first, through `kill` (no unsafe code), then wait out the grace.
            let _ = tokio::process::Command::new("kill")
                .args(["-TERM", &pid.to_string()])
                .status()
                .await;
            if tokio::time::timeout(grace, child.wait()).await.is_ok() {
                return Ok(());
            }
        }
        #[cfg(not(unix))]
        let _ = grace;
        child.kill().await.map_err(|e| format!("kill {pid}: {e}"))
    }
    async fn running(&self, h: &Handle) -> bool {
        let Handle::Process { pid } = h else {
            return false;
        };
        let mut g = self.children.lock().await;
        match g.get_mut(pid) {
            Some((c, _)) => matches!(c.try_wait(), Ok(None)),
            None => false,
        }
    }
    async fn logs(&self, h: &Handle, lines: u32) -> Result<String, String> {
        let Handle::Process { pid } = h else {
            return Ok(String::new());
        };
        let path = match self.children.lock().await.get(pid) {
            Some((_, p)) => p.clone(),
            None => return Ok(String::new()),
        };
        Ok(tail(&path, lines as usize))
    }
    async fn adopt(&self, _spec: &RunSpec) -> Option<Handle> {
        None // children die with the bridge (kill_on_drop)
    }
}

fn tail(path: &Path, lines: usize) -> String {
    let s = std::fs::read_to_string(path).unwrap_or_default();
    let v: Vec<&str> = s.lines().collect();
    v[v.len().saturating_sub(lines)..].join("\n")
}

// ── the supervisor ───────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Stopped,
    Starting,
    Running,
    Failed,
}

struct Inst {
    state: State,
    error: Option<String>,
    handle: Option<Handle>,
    active: u32,
    last_used: Instant,
    since: Option<Instant>,
    starts: u32,
}

impl Default for Inst {
    fn default() -> Self {
        Self {
            state: State::Stopped,
            error: None,
            handle: None,
            active: 0,
            last_used: Instant::now(),
            since: None,
            starts: 0,
        }
    }
}

type Key = (String, String);

/// A change of an instance's state, for the router (`event` `engine-state`).
pub type OnState = Arc<dyn Fn(Value) + Send + Sync>;

pub struct Supervisor {
    cfg: BridgeConfig,
    gpus: Vec<Gpu>,
    docker: Arc<dyn Runner>,
    process: Arc<dyn Runner>,
    package: Arc<dyn Runner>,
    inst: Mutex<HashMap<Key, Inst>>,
    locks: Mutex<HashMap<Key, Arc<tokio::sync::Mutex<()>>>>,
    http: reqwest::Client,
    log: Log,
    on_state: Mutex<Option<OnState>>,
    /// Per engine, what [`Supervisor::prepare_all`] found (`ready`, `fetching`, `failed`).
    prepared: Mutex<BTreeMap<String, Value>>,
    /// Health polling interval while starting (tests make it short).
    pub poll: Duration,
}

/// A port is ready when a connection stays open: docker's port proxy accepts a connection to a
/// published port at once and closes it while nothing listens in the container yet.
pub async fn tcp_ready(addr: &str) -> bool {
    use tokio::io::AsyncReadExt;
    let Ok(Ok(mut c)) =
        tokio::time::timeout(Duration::from_secs(3), tokio::net::TcpStream::connect(addr)).await
    else {
        return false;
    };
    let mut b = [0u8; 1];
    match tokio::time::timeout(Duration::from_millis(500), c.read(&mut b)).await {
        // still open and quiet: a server waiting for its client
        Err(_) => true,
        // it spoke first, or closed (0 bytes)
        Ok(Ok(n)) => n > 0,
        Ok(Err(_)) => false,
    }
}

/// Held while a request uses an instance; dropping it starts the idle clock.
pub struct Lease {
    sup: Arc<Supervisor>,
    key: Key,
    pub port: u16,
    pub host: String,
}

impl Drop for Lease {
    fn drop(&mut self) {
        let mut g = self.sup.inst.lock().unwrap();
        if let Some(i) = g.get_mut(&self.key) {
            i.active = i.active.saturating_sub(1);
            i.last_used = Instant::now();
        }
    }
}

/// RAM an engine could have now (Linux): `MemAvailable`, plus the ZFS ARC above its floor,
/// which the kernel gives back under pressure but `MemAvailable` does not count (TrueNAS).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct FreeRam {
    pub available_gib: f64,
    /// ARC above `max(c_min, arc_meta_min)`, less the margin (0 without ZFS or when off).
    pub arc_gib: f64,
}

impl FreeRam {
    pub fn total(&self) -> f64 {
        self.available_gib + self.arc_gib
    }
    /// `1.8 GiB available + 5.1 GiB reclaimable ARC`, or `1.8 GiB available`.
    pub fn describe(&self) -> String {
        if self.arc_gib > 0.0 {
            format!(
                "{:.1} GiB available + {:.1} GiB reclaimable ARC",
                self.available_gib, self.arc_gib
            )
        } else {
            format!("{:.1} GiB available", self.available_gib)
        }
    }
}

/// `MemAvailable` from `/proc/meminfo`, in GiB.
pub fn parse_mem_available(s: &str) -> Option<f64> {
    let line = s.lines().find(|l| l.starts_with("MemAvailable:"))?;
    let kb: f64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb / 1024.0 / 1024.0)
}

/// The ARC above its floor from `/proc/spl/kstat/zfs/arcstats`, in GiB:
/// `max(0, size - max(c_min, arc_meta_min))`.
pub fn parse_arc_reclaimable(s: &str) -> Option<f64> {
    let field = |k: &str| {
        s.lines().find_map(|l| {
            let mut w = l.split_whitespace();
            (w.next()? == k).then(|| w.nth(1)?.parse::<f64>().ok())?
        })
    };
    let size = field("size")?;
    let floor = field("c_min")
        .unwrap_or(0.0)
        .max(field("arc_meta_min").unwrap_or(0.0));
    Some(((size - floor).max(0.0)) / 1024.0 / 1024.0 / 1024.0)
}

/// [`FreeRam`] now. `count_arc`: None = when ZFS is there; `margin_gib` comes off the ARC part.
pub fn free_ram(count_arc: Option<bool>, margin_gib: f64) -> Option<FreeRam> {
    let available_gib = parse_mem_available(&std::fs::read_to_string("/proc/meminfo").ok()?)?;
    let arc_gib = if count_arc != Some(false) {
        std::fs::read_to_string("/proc/spl/kstat/zfs/arcstats")
            .ok()
            .and_then(|t| parse_arc_reclaimable(&t))
            .map(|a| (a - margin_gib).max(0.0))
            .unwrap_or(0.0)
    } else {
        0.0
    };
    Some(FreeRam {
        available_gib,
        arc_gib,
    })
}

/// [`free_ram`]'s total with the defaults (ARC counted when present, 0.5 GiB margin).
pub fn free_ram_gib() -> Option<f64> {
    free_ram(None, DEFAULT_ARC_MARGIN_GIB).map(|f| f.total())
}

pub const DEFAULT_ARC_MARGIN_GIB: f64 = 0.5;

impl Supervisor {
    pub fn new(cfg: BridgeConfig, gpus: Vec<Gpu>, log: Log) -> Arc<Supervisor> {
        Self::with_runners(
            cfg,
            gpus,
            log,
            Arc::new(DockerRunner::default()),
            Arc::new(ProcessRunner::default()),
        )
    }

    pub fn with_runners(
        cfg: BridgeConfig,
        gpus: Vec<Gpu>,
        log: Log,
        docker: Arc<dyn Runner>,
        process: Arc<dyn Runner>,
    ) -> Arc<Supervisor> {
        let package: Arc<dyn Runner> =
            Arc::new(crate::package::PackageRunner::new(cfg.state_dir()));
        Arc::new(Supervisor {
            cfg,
            gpus,
            docker,
            process,
            package,
            inst: Mutex::default(),
            locks: Mutex::default(),
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(3))
                .timeout(Duration::from_secs(10))
                .build()
                .expect("http client"),
            log,
            on_state: Mutex::new(None),
            prepared: Mutex::new(BTreeMap::new()),
            poll: Duration::from_secs(2),
        })
    }

    pub fn set_on_state(&self, f: OnState) {
        *self.on_state.lock().unwrap() = Some(f);
    }

    pub fn config(&self) -> &BridgeConfig {
        &self.cfg
    }

    fn spec(&self, engine: &str, card: &str) -> Result<RunSpec, String> {
        let e = self
            .cfg
            .engines
            .get(engine)
            .ok_or_else(|| format!("no engine `{engine}` on this bridge"))?;
        if !e.cards.iter().any(|c| c == card) {
            return Err(format!("engine `{engine}` does not run on card `{card}`"));
        }
        let port = e
            .port_for(card)
            .ok_or_else(|| format!("engine `{engine}` has no port for `{card}`"))?;
        let gpu = card
            .strip_prefix("GPU")
            .and_then(|n| n.parse::<u32>().ok())
            .and_then(|n| self.gpus.iter().find(|g| g.index == n).cloned());
        Ok(RunSpec {
            bridge: self.cfg.bridge.id.clone(),
            engine: engine.into(),
            card: card.into(),
            port,
            gpu,
            cfg: e.clone(),
            model_dir: self.cfg.bridge.model_dir.clone(),
            state_dir: self.cfg.state_dir(),
        })
    }

    fn runner(&self, spec: &RunSpec) -> Option<&Arc<dyn Runner>> {
        match spec.cfg.method() {
            "docker" => Some(&self.docker),
            "process" => Some(&self.process),
            "package" => Some(&self.package),
            _ => None,
        }
    }

    fn host_of(spec: &RunSpec) -> String {
        spec.cfg
            .docker
            .as_ref()
            .and_then(|d| d.reach_host.clone())
            .unwrap_or_else(|| "127.0.0.1".into())
    }

    fn lock_for(&self, key: &Key) -> Arc<tokio::sync::Mutex<()>> {
        self.locks
            .lock()
            .unwrap()
            .entry(key.clone())
            .or_default()
            .clone()
    }

    fn set(&self, key: &Key, f: impl FnOnce(&mut Inst)) {
        let snapshot = {
            let mut g = self.inst.lock().unwrap();
            let i = g.entry(key.clone()).or_default();
            f(i);
            json!({"engine": key.0, "card": key.1, "state": i.state, "error": i.error})
        };
        if let Some(cb) = self.on_state.lock().unwrap().clone() {
            cb(snapshot);
        }
    }

    async fn healthy(&self, spec: &RunSpec) -> bool {
        // `health = "tcp"`: a port that accepts a connection and keeps it open (wyoming engines).
        if spec.cfg.health == "tcp" {
            return tcp_ready(&format!("{}:{}", Self::host_of(spec), spec.port)).await;
        }
        let url = format!(
            "http://{}:{}{}",
            Self::host_of(spec),
            spec.port,
            spec.cfg.health
        );
        self.http
            .get(url)
            .header(crate::proto::NO_SWITCH, "1")
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
    }

    /// Adopt instances an earlier bridge process left running (docker only).
    pub async fn adopt_all(&self) {
        for (eid, e) in &self.cfg.engines {
            for card in &e.cards {
                let Ok(spec) = self.spec(eid, card) else {
                    continue;
                };
                let Some(r) = self.runner(&spec) else {
                    continue;
                };
                if let Some(h) = r.adopt(&spec).await {
                    (self.log)(&format!("adopted {eid} on {card} (already running)"));
                    self.set(&(eid.clone(), card.clone()), |i| {
                        i.state = State::Running;
                        i.handle = Some(h);
                        i.since = Some(Instant::now());
                        i.last_used = Instant::now();
                    });
                }
            }
        }
    }

    /// Make sure the instance runs (starting it if it is on demand), and hold it.
    pub async fn acquire(self: &Arc<Self>, engine: &str, card: &str) -> Result<Lease, String> {
        let spec = self.spec(engine, card)?;
        let key: Key = (engine.into(), card.into());
        let host = Self::host_of(&spec);
        if self.runner(&spec).is_some() {
            self.ensure(&spec, &key).await?;
        }
        let mut g = self.inst.lock().unwrap();
        let i = g.entry(key.clone()).or_default();
        i.active += 1;
        i.last_used = Instant::now();
        drop(g);
        Ok(Lease {
            sup: self.clone(),
            key,
            port: spec.port,
            host,
        })
    }

    async fn ensure(&self, spec: &RunSpec, key: &Key) -> Result<(), String> {
        let lock = self.lock_for(key);
        let _g = lock.lock().await;
        let runner = self.runner(spec).ok_or("not startable")?.clone();
        let (state, handle) = {
            let g = self.inst.lock().unwrap();
            g.get(key)
                .map(|i| (i.state.clone(), i.handle.clone()))
                .unwrap_or((State::Stopped, None))
        };
        if state == State::Running {
            if let Some(h) = &handle
                && runner.running(h).await
            {
                return Ok(());
            }
            (self.log)(&format!("{} on {}: it stopped by itself", key.0, key.1));
        }
        let need = spec
            .cfg
            .min_free_ram_gib
            .or(spec.cfg.mem_gib.filter(|_| spec.gpu.is_none()));
        let lim = &self.cfg.limits;
        if let Some(need) = need
            && let Some(free) = free_ram(
                lim.count_zfs_arc,
                lim.arc_margin_gib.unwrap_or(DEFAULT_ARC_MARGIN_GIB),
            )
            && free.total() < need
        {
            let e = format!(
                "not starting {} on {}: {} ({:.1} GiB), it needs {need:.1}",
                key.0,
                key.1,
                free.describe(),
                free.total()
            );
            self.set(key, |i| {
                i.state = State::Failed;
                i.error = Some(e.clone());
            });
            return Err(e);
        }
        (self.log)(&format!("starting {} on {}", key.0, key.1));
        self.set(key, |i| {
            i.state = State::Starting;
            i.error = None;
            i.starts += 1;
        });
        // the image or package first: a failed pull says so, instead of docker's run error
        if let Err(e) = runner.prepare(spec).await {
            self.prepared
                .lock()
                .unwrap()
                .insert(key.0.clone(), json!({"state": "failed", "error": e}));
            self.set(key, |i| {
                i.state = State::Failed;
                i.error = Some(e.clone());
            });
            return Err(e);
        }
        let h = match runner.start(spec).await {
            Ok(h) => h,
            Err(e) => {
                self.set(key, |i| {
                    i.state = State::Failed;
                    i.error = Some(e.clone());
                });
                return Err(e);
            }
        };
        let deadline = Instant::now() + Duration::from_secs(spec.cfg.start_timeout_seconds);
        loop {
            if self.healthy(spec).await {
                break;
            }
            if !runner.running(&h).await {
                let tail = runner.logs(&h, 20).await.unwrap_or_default();
                let _ = runner.stop(&h, Duration::from_secs(1)).await;
                let e = format!(
                    "{} on {} exited while starting: {}",
                    key.0,
                    key.1,
                    tail.trim()
                );
                self.set(key, |i| {
                    i.state = State::Failed;
                    i.error = Some(e.clone());
                    i.handle = None;
                });
                return Err(e);
            }
            if Instant::now() > deadline {
                let _ = runner.stop(&h, Duration::from_secs(5)).await;
                let e = format!(
                    "{} on {} was not healthy after {}s",
                    key.0, key.1, spec.cfg.start_timeout_seconds
                );
                self.set(key, |i| {
                    i.state = State::Failed;
                    i.error = Some(e.clone());
                    i.handle = None;
                });
                return Err(e);
            }
            tokio::time::sleep(self.poll).await;
        }
        (self.log)(&format!("{} on {} is ready", key.0, key.1));
        self.set(key, |i| {
            i.state = State::Running;
            i.handle = Some(h);
            i.since = Some(Instant::now());
            i.last_used = Instant::now();
        });
        Ok(())
    }

    /// Start without holding it (the `start` command; `always` engines at boot).
    pub async fn start(self: &Arc<Self>, engine: &str, card: &str) -> Result<(), String> {
        drop(self.acquire(engine, card).await?);
        Ok(())
    }

    pub async fn stop(&self, engine: &str, card: &str) -> Result<(), String> {
        let spec = self.spec(engine, card)?;
        let key: Key = (engine.into(), card.into());
        let lock = self.lock_for(&key);
        let _g = lock.lock().await;
        let h = self
            .inst
            .lock()
            .unwrap()
            .get(&key)
            .and_then(|i| i.handle.clone());
        if let (Some(h), Some(r)) = (h, self.runner(&spec)) {
            (self.log)(&format!("stopping {engine} on {card}"));
            r.stop(&h, Duration::from_secs(20)).await?;
        }
        self.set(&key, |i| {
            i.state = State::Stopped;
            i.handle = None;
            i.since = None;
        });
        Ok(())
    }

    /// Stop on-demand instances idle for longer than their `idle_stop_seconds`.
    pub async fn reap_idle(&self) {
        let idle: Vec<Key> = {
            let g = self.inst.lock().unwrap();
            g.iter()
                .filter(|(k, i)| {
                    i.state == State::Running
                        && i.active == 0
                        && self.cfg.engines.get(&k.0).is_some_and(|e| {
                            e.lifecycle == "on_demand"
                                && i.last_used.elapsed() >= Duration::from_secs(e.idle_stop_seconds)
                        })
                })
                .map(|(k, _)| k.clone())
                .collect()
        };
        for (e, c) in idle {
            (self.log)(&format!("{e} on {c} is idle: stopping it"));
            if let Err(err) = self.stop(&e, &c).await {
                (self.log)(&format!("stopping {e} on {c}: {err}"));
            }
        }
    }

    /// Start the `always` engines.
    pub async fn start_always(self: &Arc<Self>) {
        for (eid, e) in &self.cfg.engines {
            if e.lifecycle != "always" {
                continue;
            }
            for card in &e.cards {
                if let Err(err) = self.start(eid, card).await {
                    (self.log)(&format!("{eid} on {card}: {err}"));
                }
            }
        }
    }

    /// Stop everything this bridge started (shutdown of the desktop's bridge mode).
    pub async fn stop_all(&self) {
        let keys: Vec<Key> = self
            .inst
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, i)| i.handle.is_some())
            .map(|(k, _)| k.clone())
            .collect();
        for (e, c) in keys {
            let _ = self.stop(&e, &c).await;
        }
    }

    /// Fetch every startable engine's image or package now (the bridge calls this when it
    /// starts and after a config change), so the first request does not wait on a download and
    /// a failed pull is an error in the status, not a 503 later.
    pub async fn prepare_all(self: &Arc<Self>) {
        let mut todo = Vec::new();
        for (eid, e) in &self.cfg.engines {
            if matches!(e.method(), "external" | "process") {
                continue;
            }
            if let Some(card) = e.cards.first()
                && let Ok(spec) = self.spec(eid, card)
            {
                todo.push(spec);
            }
        }
        for spec in todo {
            let Some(r) = self.runner(&spec).cloned() else {
                continue;
            };
            self.prepared
                .lock()
                .unwrap()
                .insert(spec.engine.clone(), json!({"state": "fetching"}));
            let v = match r.prepare(&spec).await {
                Ok(what) => {
                    (self.log)(&format!("{}: {what}", spec.engine));
                    json!({"state": "ready", "detail": what})
                }
                Err(e) => {
                    (self.log)(&format!("{}: {e}", spec.engine));
                    json!({"state": "failed", "error": e})
                }
            };
            self.prepared
                .lock()
                .unwrap()
                .insert(spec.engine.clone(), v.clone());
            if let Some(f) = self.on_state.lock().unwrap().clone() {
                f(json!({"engine": spec.engine, "prepare": v}));
            }
        }
    }

    /// What [`Supervisor::prepare_all`] found so far, per engine.
    pub fn prepared(&self) -> BTreeMap<String, Value> {
        self.prepared.lock().unwrap().clone()
    }

    /// `status`: every engine on every card.
    pub fn status(&self) -> Value {
        let prepared = self.prepared.lock().unwrap().clone();
        let g = self.inst.lock().unwrap();
        let mut out = Vec::new();
        for (eid, e) in &self.cfg.engines {
            for card in &e.cards {
                let i = g.get(&(eid.clone(), card.clone()));
                out.push(json!({
                    "engine": eid,
                    "card": card,
                    "method": e.method(),
                    "lifecycle": e.lifecycle,
                    "port": e.port_for(card),
                    "state": i.map_or(State::Stopped, |i| i.state.clone()),
                    "error": i.and_then(|i| i.error.clone()),
                    "active": i.map_or(0, |i| i.active),
                    "idle_seconds": i.map(|i| i.last_used.elapsed().as_secs()),
                    "up_seconds": i.and_then(|i| i.since.map(|s| s.elapsed().as_secs())),
                    "starts": i.map_or(0, |i| i.starts),
                    "prepare": prepared.get(eid),
                }));
            }
        }
        json!({"engines": out})
    }

    pub async fn logs(&self, engine: &str, card: &str, lines: u32) -> Result<String, String> {
        let spec = self.spec(engine, card)?;
        let h = self
            .inst
            .lock()
            .unwrap()
            .get(&(engine.to_string(), card.to_string()))
            .and_then(|i| i.handle.clone());
        match (h, self.runner(&spec)) {
            (Some(h), Some(r)) => r.logs(&h, lines.min(2000)).await,
            _ => Ok(String::new()),
        }
    }

    /// `probe`: the engine's health path, read-only (`x-abhoth-no-switch`), never starts it.
    pub async fn probe(&self, engine: &str, card: &str) -> Result<Value, String> {
        let spec = self.spec(engine, card)?;
        let ok = self.healthy(&spec).await;
        Ok(json!({"engine": engine, "card": card, "healthy": ok}))
    }

    /// Ports per engine and card (for the descriptor).
    pub fn ports(&self) -> BTreeMap<String, BTreeMap<String, u16>> {
        self.cfg
            .engines
            .iter()
            .map(|(id, e)| {
                (
                    id.clone(),
                    e.cards
                        .iter()
                        .filter_map(|c| e.port_for(c).map(|p| (c.clone(), p)))
                        .collect(),
                )
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DockerRun;

    const ARCSTATS: &str = "13 1 0x01 123 33456 2345 6789
name                            type data
hits                            4    1234567
c                               4    6442450944
c_min                           4    1073741824
c_max                           4    8589934592
size                            4    6549825126
arc_meta_min                    4    16777216
";

    #[test]
    fn reclaimable_arc_is_the_arc_above_its_floor() {
        let a = parse_arc_reclaimable(ARCSTATS).unwrap();
        // 6549825126 - 1073741824 bytes = 5.1 GiB
        assert!((a - 5.1).abs() < 0.01, "{a}");
        // a floor above the size: nothing to reclaim
        let small = ARCSTATS.replace("6549825126", "900000000");
        assert_eq!(parse_arc_reclaimable(&small), Some(0.0));
        // an older ZFS whose arc_meta_min is the higher floor
        let meta = ARCSTATS.replace("16777216", "2147483648");
        assert!((parse_arc_reclaimable(&meta).unwrap() - 4.1).abs() < 0.01);
        assert_eq!(
            parse_arc_reclaimable(
                "name type data
"
            ),
            None
        );
        let mi = "MemTotal: 16000000 kB
MemAvailable:    1887436 kB
";
        assert!((parse_mem_available(mi).unwrap() - 1.8).abs() < 0.01);
        let f = FreeRam {
            available_gib: 1.8,
            arc_gib: 5.1,
        };
        assert_eq!(f.describe(), "1.8 GiB available + 5.1 GiB reclaimable ARC");
        assert!((f.total() - 6.9).abs() < 1e-9);
        assert_eq!(
            FreeRam {
                available_gib: 2.0,
                arc_gib: 0.0
            }
            .describe(),
            "2.0 GiB available"
        );
    }

    fn spec(gpu: bool) -> RunSpec {
        RunSpec {
            bridge: "nas".into(),
            engine: "kokoro".into(),
            card: if gpu { "GPU0".into() } else { "CPU".into() },
            port: 18880,
            gpu: gpu.then(|| Gpu {
                index: 0,
                uuid: "GPU-aaa".into(),
                name: "M4000".into(),
                vram_gib: 8.0,
            }),
            cfg: EngineConfig {
                docker: Some(DockerRun {
                    image: "img@sha256:1".into(),
                    container_port: 8880,
                    env: [("PORT".to_string(), "{port}".to_string())].into(),
                    mounts: vec!["{model_dir}/kokoro:/models:ro".into()],
                    cpus: Some("4".into()),
                    memory: Some("6g".into()),
                    ..DockerRun::default()
                }),
                ..EngineConfig::default()
            },
            model_dir: Some("/models".into()),
            state_dir: PathBuf::from("/state"),
        }
    }

    #[test]
    fn docker_run_binds_loopback_and_pins_the_gpu_by_uuid() {
        let a = docker_args(&spec(true)).unwrap().join(" ");
        assert!(a.contains("--name abhoth-bridge-nas-kokoro-gpu0"), "{a}");
        assert!(a.contains("-p 127.0.0.1:18880:8880"), "{a}");
        assert!(a.contains("--gpus device=GPU-aaa"), "{a}");
        assert!(a.contains("--cpus=4 --memory=6g"), "{a}");
        assert!(a.contains("-e PORT=18880"), "{a}");
        assert!(a.contains("-v /models/kokoro:/models:ro"), "{a}");
        assert!(a.ends_with("img@sha256:1"), "{a}");
        let c = docker_args(&spec(false)).unwrap().join(" ");
        assert!(
            c.contains("NVIDIA_VISIBLE_DEVICES=void") && !c.contains("--gpus"),
            "{c}"
        );
    }
}

#[cfg(test)]
mod tcp_tests {
    use super::tcp_ready;

    #[tokio::test]
    async fn a_port_that_closes_at_once_is_not_ready() {
        // like docker-proxy with nothing behind it: accept, then close
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = l.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            while let Ok((c, _)) = l.accept().await {
                drop(c);
            }
        });
        assert!(!tcp_ready(&a).await);
        // a server that keeps the connection open
        let l2 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a2 = l2.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            let mut keep = vec![];
            while let Ok((c, _)) = l2.accept().await {
                keep.push(c);
            }
        });
        assert!(tcp_ready(&a2).await);
    }
}
