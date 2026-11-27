//! The engine store behind `/admin/api/engine-store/…` (D153, ROUTER-MEDIA-PLAN.md 13.3): the
//! stores (the built-in default store, My packages, and store URLs an admin adds), package
//! details and validation, the fit of a package on a server's cards, install / update / remove
//! jobs, and the "Create package" wizard's drafts and test runs.
//!
//! The router installs **container methods only** (docker, compose, an existing image): the
//! orca-engines runner runs against the server's executor, so `docker pull` goes through the
//! same channel (and hostctl allowlist) as every other command, and the install record lands
//! under the router's state dir (see [`super::packages`]). An install adds an
//! `[engines.<id>]` with `driver = "package"`; manual engines are untouched.
//!
//! Files: `<state_dir>/engine-stores.json` (added stores), `<state_dir>/my-packages/<id>/`
//! (My packages), `<state_dir>/packages/<server>/…` (installs).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use indexmap::IndexMap;
use orca_engines::exec::{ExecOpts, ExecResult, Fetch, HostExec, HttpFetch};
use orca_engines::host::{CardSel, Cpu, Gpu, HostFacts, MethodFilter};
use orca_engines::manifest::{GpuKind, MANIFEST_FILE};
use orca_engines::store::{self, Index};
use orca_engines::validate::{self, Files, Mode};
use orca_engines::{Approvals, Installed, LocalRunner, Manifest, Package, Runner, wizard};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::downloads::Service;
use super::exec::{Cancel, ExecRequest};
use super::schema::{ControlConfig, DriverKind};
use super::template::argv_to_shell;

/// Ports new package engines get, from here up (the first one no engine on the server uses).
pub const FIRST_PORT: u16 = 18100;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoreEntry {
    pub id: String,
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub trusted: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub op: String,
    pub package: String,
    pub server: String,
    pub card: String,
    pub engine: String,
    pub state: String,
    pub log: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub started: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished: Option<i64>,
}

#[derive(Default)]
struct Cached {
    at: Option<Instant>,
    index: Option<Index>,
    error: Option<String>,
    packages: HashMap<String, Package>,
}

/// Per router (keyed by state dir): jobs and the store index cache.
#[derive(Default)]
pub struct Store {
    jobs: Mutex<IndexMap<String, Job>>,
    cache: Mutex<HashMap<String, Cached>>,
    /// Test hook: package fetches go through this instead of HTTPS.
    pub fetch: Mutex<Option<Arc<dyn Fetch>>>,
}

fn registry() -> &'static Mutex<HashMap<PathBuf, Arc<Store>>> {
    static R: OnceLock<Mutex<HashMap<PathBuf, Arc<Store>>>> = OnceLock::new();
    R.get_or_init(Mutex::default)
}

/// The store state for a router state dir.
pub fn for_state(state_dir: &Path) -> Arc<Store> {
    registry()
        .lock()
        .unwrap()
        .entry(state_dir.to_path_buf())
        .or_default()
        .clone()
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

const INDEX_TTL: Duration = Duration::from_secs(600);
const MAX_JOBS: usize = 50;

/// Commands through the server's executor, as root (docker), with their text in the job log.
struct RouterExec {
    svc: Arc<Service>,
    server: String,
    log: Arc<dyn Fn(String) + Send + Sync>,
}

#[async_trait]
impl HostExec for RouterExec {
    async fn run(&self, argv: &[String], o: &ExecOpts) -> Result<ExecResult, String> {
        let mut text = argv_to_shell(argv);
        if let Some(d) = &o.cwd {
            text = format!("cd {} && {text}", super::template::shell_quote(d));
        }
        (self.log)(format!("$ {text}"));
        let id = format!("package-install:{}", super::driver::short_hash(&text));
        let ex = self.svc.exec(&self.server)?;
        let timeout = o.timeout.unwrap_or(Duration::from_secs(1800));
        let out = ex
            .run(
                ExecRequest::command(&self.server, &id, text, timeout).as_root(),
                &Cancel::new(),
            )
            .await
            .map_err(|e| e.to_string())?;
        Ok(ExecResult {
            code: out.exit_code.unwrap_or(-1),
            stdout: out.stdout,
            stderr: out.stderr,
        })
    }
}

/// What the router knows of a server, as orca-engines host facts (from the config's cards:
/// docker is assumed, and only container methods are offered).
pub fn host_facts(c: &ControlConfig, server: &str) -> HostFacts {
    let mut gpus = vec![];
    let mut cpu = Cpu::default();
    for (k, (_, cd)) in c
        .cards
        .iter()
        .filter(|(_, cd)| cd.server == server)
        .enumerate()
    {
        if cd.is_cpu() {
            cpu.cores = cd.cores.unwrap_or(0);
            cpu.ram_gib = cd.ram_gib.unwrap_or(0.0);
        } else {
            gpus.push(Gpu {
                index: k as u32,
                uuid: cd.gpu_uuid.clone(),
                vendor: GpuKind::Cuda,
                name: cd.label.clone(),
                vram_gib: cd.vram_gib.unwrap_or(0.0),
            });
        }
    }
    HostFacts {
        os: "linux".into(),
        arch: "amd64".into(),
        docker: true,
        compose: true,
        pythons: vec![],
        gpus,
        cpu,
        path_tools: None,
    }
}

/// The card as orca-engines sees it.
pub fn card_sel(c: &ControlConfig, card: &str) -> Option<CardSel> {
    let cd = c.cards.get(card)?;
    Some(if cd.is_cpu() {
        CardSel::Cpu {
            card: card.into(),
            cores: cd.cores.unwrap_or(0),
            ram_gib: cd.ram_gib.unwrap_or(0.0),
        }
    } else {
        CardSel::Gpu {
            card: card.into(),
            index: 0,
            uuid: cd.gpu_uuid.clone(),
            vendor: GpuKind::Cuda,
            vram_gib: cd.vram_gib.unwrap_or(0.0),
        }
    })
}

/// A package's summary for the list.
pub fn summary(p: &Package, installed: &[(String, Installed)], newest: Option<&str>) -> Value {
    let m = &p.manifest;
    let inst: Vec<Value> = installed
        .iter()
        .filter(|(_, i)| i.id == m.id)
        .map(|(s, i)| json!({"server": s, "version": i.version}))
        .collect();
    let update = newest.filter(|n| {
        installed
            .iter()
            .any(|(_, i)| i.id == m.id && i.version != *n && i.store == p.store)
    });
    json!({
        "store": p.store, "trusted": p.trusted, "id": m.id, "name": m.name, "version": m.version,
        "kinds": m.kinds, "summary": m.description.lines().next().unwrap_or(""), "api": m.api,
        "licence": m.licence, "source": m.source, "private": m.private,
        "methods": m.methods.iter().map(|x| json!({
            "type": x.r#type.as_str(), "label": x.label(), "platforms": x.platforms, "gpu": x.gpu,
        })).collect::<Vec<_>>(),
        "needsApproval": needs_approval(p),
        "installed": inst,
        "update": update,
    })
}

/// What installing this package will need approved, whatever the method.
pub fn needs_approval(p: &Package) -> Vec<String> {
    let mut v = vec![];
    if !p.trusted {
        v.push("untrusted".to_string());
    }
    let s = &p.manifest.security;
    if s.privileged {
        v.push("privileged".into());
    }
    if s.host_network {
        v.push("host_network".into());
    }
    for h in &s.host_mounts {
        v.push(format!("host_mounts:{}", h.source));
    }
    for d in &s.devices {
        v.push(format!("devices:{d}"));
    }
    for c in &s.cap_add {
        v.push(format!("cap_add:{c}"));
    }
    v
}

/// `{{param.x}}` in a host-mount source, from the params given (else the defaults).
fn render_source(m: &Manifest, src: &str, params: &IndexMap<String, Value>) -> String {
    let mut s = src.to_string();
    for (k, p) in &m.params {
        let v = params
            .get(k)
            .or(p.default.as_ref())
            .map(|v| match v {
                Value::String(x) => x.clone(),
                other => other.to_string(),
            })
            .unwrap_or_default();
        s = s.replace(&format!("{{{{param.{k}}}}}"), &v);
    }
    s
}

fn job_log(store: &Arc<Store>, id: &str) -> Arc<dyn Fn(String) + Send + Sync> {
    let st = store.clone();
    let id = id.to_string();
    Arc::new(move |line: String| {
        if let Some(j) = st.jobs.lock().unwrap().get_mut(&id)
            && j.log.len() < 2000
        {
            j.log.push(orca_engines::exec::scrub(&line));
        }
    })
}

impl Store {
    fn new_job(&self, op: &str, package: &str, server: &str, card: &str, engine: &str) -> Job {
        let id = format!(
            "pkg-{}-{}",
            now(),
            super::driver::short_hash(&format!(
                "{op}{package}{server}{card}{engine}{:?}",
                Instant::now()
            ))
        );
        let j = Job {
            id: id.clone(),
            op: op.into(),
            package: package.into(),
            server: server.into(),
            card: card.into(),
            engine: engine.into(),
            state: "running".into(),
            log: vec![],
            error: None,
            started: now(),
            finished: None,
        };
        let mut jobs = self.jobs.lock().unwrap();
        jobs.insert(id, j.clone());
        while jobs.len() > MAX_JOBS {
            jobs.shift_remove_index(0);
        }
        j
    }

    fn finish(&self, id: &str, r: Result<String, String>) {
        if let Some(j) = self.jobs.lock().unwrap().get_mut(id) {
            j.finished = Some(now());
            match r {
                Ok(msg) => {
                    j.state = "done".into();
                    j.log.push(msg);
                }
                Err(e) => {
                    j.state = "failed".into();
                    j.error = Some(orca_engines::exec::scrub(&e));
                }
            }
        }
    }

    pub fn jobs(&self) -> Vec<Job> {
        self.jobs.lock().unwrap().values().rev().cloned().collect()
    }
}

fn stores_file(state_dir: &Path) -> PathBuf {
    state_dir.join("engine-stores.json")
}

pub fn added_stores(state_dir: &Path) -> Vec<StoreEntry> {
    std::fs::read_to_string(stores_file(state_dir))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save_stores(state_dir: &Path, s: &[StoreEntry]) -> Result<(), String> {
    std::fs::create_dir_all(state_dir).map_err(|e| e.to_string())?;
    let t = serde_json::to_string_pretty(s).map_err(|e| e.to_string())?;
    let tmp = stores_file(state_dir).with_extension("json.tmp");
    std::fs::write(&tmp, t).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, stores_file(state_dir)).map_err(|e| e.to_string())
}

pub fn my_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("my-packages")
}

/// My packages, as saved (each validated as private).
pub fn my_packages(state_dir: &Path) -> Vec<Package> {
    let mut out = vec![];
    let Ok(rd) = std::fs::read_dir(my_dir(state_dir)) else {
        return out;
    };
    for e in rd.flatten() {
        if let Ok(p) = store::load_dir(&e.path(), "my", true) {
            out.push(p);
        }
    }
    out.sort_by(|a, b| a.manifest.id.cmp(&b.manifest.id));
    out
}

/// Every install on every server (`(server, record)`).
pub fn installed_all(c: &ControlConfig, state_dir: &Path) -> Vec<(String, Installed)> {
    let mut v = vec![];
    for s in c.servers.keys() {
        for i in super::packages::layout(state_dir, s).list_installed() {
            v.push((s.clone(), i));
        }
    }
    v
}

fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// The engine-store API (`rest` is the path after `engine-store/`).
pub async fn api(
    svc: &Arc<Service>,
    method: &str,
    rest: &[&str],
    qs: &[(String, String)],
    body: &Value,
    who: &str,
) -> (u16, Value) {
    let st = for_state(&svc.state_dir);
    let q = |k: &str| {
        qs.iter()
            .find(|(a, _)| a == k)
            .map(|(_, b)| b.clone())
            .unwrap_or_default()
    };
    let s = |k: &str| body[k].as_str().unwrap_or("").to_string();
    let c = match svc.config() {
        Ok(c) => c,
        Err(e) => {
            return (
                409,
                json!({"error": format!("router.toml does not load: {e}")}),
            );
        }
    };
    let fail = |code: u16, e: String| (code, json!({"error": e}));
    match (method, rest) {
        ("GET", ["stores"]) => {
            let mut v = vec![
                json!({"id": "default", "name": "Abhoth default store", "url": null, "trusted": true,
                    "builtin": true, "packages": store::embedded::packages().len()}),
                json!({"id": "my", "name": "My packages", "url": null, "trusted": true,
                    "builtin": true, "packages": my_packages(&svc.state_dir).len()}),
            ];
            for e in added_stores(&svc.state_dir) {
                let (n, err) = match st.index(&e).await {
                    Ok(i) => (i.packages.len(), None),
                    Err(x) => (0, Some(x)),
                };
                v.push(
                    json!({"id": e.id, "name": e.name, "url": e.url, "trusted": e.trusted,
                    "builtin": false, "packages": n, "error": err}),
                );
            }
            (200, json!({"stores": v}))
        }
        ("POST", ["stores"]) => {
            let url = s("url");
            if let Err(e) = store::index_base(&url) {
                return fail(400, e);
            }
            let mut list = added_stores(&svc.state_dir);
            let name = if s("name").is_empty() {
                url.clone()
            } else {
                s("name")
            };
            let id = format!("s{}", super::driver::short_hash(&url));
            if list.iter().any(|x| x.id == id) {
                return fail(409, "that store is already added".into());
            }
            let e = StoreEntry {
                id,
                name,
                url,
                trusted: body["trusted"].as_bool().unwrap_or(false),
            };
            list.push(e.clone());
            if let Err(x) = save_stores(&svc.state_dir, &list) {
                return fail(500, x);
            }
            tracing::info!(store = %e.url, trusted = e.trusted, by = who, "engine store added");
            (200, serde_json::to_value(&e).unwrap_or_default())
        }
        ("DELETE", ["stores", id]) => {
            let mut list = added_stores(&svc.state_dir);
            let n = list.len();
            list.retain(|x| x.id != *id);
            if list.len() == n {
                return fail(
                    404,
                    "no such store (the built-in ones can't be removed)".into(),
                );
            }
            st.cache.lock().unwrap().remove(*id);
            match save_stores(&svc.state_dir, &list) {
                Ok(()) => (200, json!({"ok": true})),
                Err(x) => fail(500, x),
            }
        }
        ("GET", ["packages"]) => {
            let inst = installed_all(&c, &svc.state_dir);
            let mut out = vec![];
            for p in store::embedded::packages() {
                out.push(summary(&p, &inst, Some(&p.manifest.version.clone())));
            }
            for p in my_packages(&svc.state_dir) {
                out.push(summary(&p, &inst, Some(&p.manifest.version.clone())));
            }
            for e in added_stores(&svc.state_dir) {
                if let Ok(i) = st.index(&e).await {
                    for x in &i.packages {
                        // From the index alone (packages are fetched when opened).
                        out.push(json!({
                            "store": e.id, "trusted": e.trusted, "id": x.id, "name": x.name,
                            "version": x.version, "kinds": x.kinds, "summary": x.summary,
                            "api": "", "licence": "", "source": "", "private": false,
                            "methods": x.methods.iter().map(|t| json!({"type": t, "label": t, "platforms": [], "gpu": []})).collect::<Vec<_>>(),
                            "needsApproval": if e.trusted { vec![] } else { vec!["untrusted"] },
                            "installed": inst.iter().filter(|(_, i)| i.id == x.id).map(|(s, i)| json!({"server": s, "version": i.version})).collect::<Vec<_>>(),
                            "update": inst.iter().any(|(_, i)| i.id == x.id && i.store == e.id && i.version != x.version).then_some(x.version.clone()),
                        }));
                    }
                }
            }
            (200, json!({"packages": out}))
        }
        ("GET", ["package"]) => match st.package(&svc.state_dir, &q("store"), &q("id")).await {
            Ok(p) => {
                let r = p.validate();
                (200, json!({"package": p, "report": r}))
            }
            Err(e) => fail(404, e),
        },
        ("POST", ["fit"]) => {
            let p = match st.package(&svc.state_dir, &s("store"), &s("id")).await {
                Ok(p) => p,
                Err(e) => return fail(404, e),
            };
            let server = s("server");
            if !c.servers.contains_key(&server) {
                return fail(404, format!("no server `{server}`"));
            }
            let h = host_facts(&c, &server);
            let cards: Vec<Value> = c
                .cards
                .iter()
                .filter(|(_, cd)| cd.server == server)
                .filter_map(|(id, cd)| {
                    let sel = card_sel(&c, id)?;
                    let mut f =
                        orca_engines::fit(&p.manifest, &h, &sel, &MethodFilter::containers_only());
                    // Approval keys as the client ticks them (`host_run`, `host_mounts:<src>`…).
                    if let Some(t) = f.method_type {
                        let mut keys = needs_approval(&p);
                        if t.runs_on_host() {
                            keys.insert(0, "host_run".into());
                        }
                        f.approvals = if f.ok { keys } else { vec![] };
                    }
                    Some(
                        json!({"card": id, "device": if cd.is_cpu() {"cpu"} else {"gpu"},
                        "label": cd.label, "vramGib": cd.vram_gib, "fit": f}),
                    )
                })
                .collect();
            (200, json!({"server": server, "host": h, "cards": cards}))
        }
        ("POST", ["validate"]) => {
            let mode = if s("mode") == "store" {
                Mode::Store
            } else {
                Mode::Private
            };
            let files = files_of(&body["files"]);
            let text = s("manifest");
            (
                200,
                serde_json::to_value(validate::validate_text(&text, &files, mode))
                    .unwrap_or_default(),
            )
        }
        ("POST", ["install"]) => install(svc, &st, &c, body, who).await,
        ("POST", ["update"]) => update(svc, &st, &c, &s("engine"), who).await,
        ("POST", ["remove"]) => {
            remove(
                svc,
                &st,
                &c,
                &s("engine"),
                body["uninstall"].as_bool().unwrap_or(false),
                who,
            )
            .await
        }
        ("GET", ["jobs"]) => (200, json!({"jobs": st.jobs()})),
        ("GET", ["installed"]) => {
            let v: Vec<Value> = installed_all(&c, &svc.state_dir)
                .into_iter()
                .map(|(server, i)| {
                    let engines: Vec<&String> =
                        c.engines
                            .iter()
                            .filter(|(_, e)| {
                                e.driver == DriverKind::Package
                                    && e.package
                                        .as_ref()
                                        .is_some_and(|p| p.id == i.id && p.version == i.version)
                                    && e.port.cards().iter().any(|cd| {
                                        c.cards.get(*cd).is_some_and(|x| x.server == server)
                                    })
                            })
                            .map(|(k, _)| k)
                            .collect();
                    json!({"server": server, "id": i.id, "version": i.version, "method": i.method,
                        "method_type": i.method_type.as_str(), "pins": i.pins, "at": i.at,
                        "store": i.store, "engines": engines})
                })
                .collect();
            let manual: Vec<&String> = c
                .engines
                .iter()
                .filter(|(_, e)| e.driver != DriverKind::Package)
                .map(|(k, _)| k)
                .collect();
            (200, json!({"installed": v, "manual": manual}))
        }
        ("POST", ["wizard", "draft"]) => draft(&c, &s("source"), &s("value")).await,
        ("POST", ["wizard", "test-run"]) => test_run(svc, &c, body, who).await,
        ("GET", ["my"]) => {
            let inst = installed_all(&c, &svc.state_dir);
            let v: Vec<Value> = my_packages(&svc.state_dir)
                .iter()
                .map(|p| summary(p, &inst, None))
                .collect();
            (200, json!({"packages": v}))
        }
        ("PUT", ["my", id]) => {
            if !safe_id(id) {
                return fail(
                    400,
                    "a package id is lower case letters, digits and -".into(),
                );
            }
            let files = files_of(&body["files"]);
            let text = s("manifest");
            let r = validate::validate_text(&text, &files, Mode::Private);
            if !r.ok {
                return (
                    422,
                    json!({"error": "the package does not validate", "report": r}),
                );
            }
            if r.manifest.as_ref().is_some_and(|m| m.id != *id) {
                return (
                    422,
                    json!({"error": "the manifest's id differs from the URL's", "report": r}),
                );
            }
            let dir = my_dir(&svc.state_dir).join(id);
            let tmp = my_dir(&svc.state_dir).join(format!(".{id}.tmp"));
            let _ = std::fs::remove_dir_all(&tmp);
            let write = || -> std::io::Result<()> {
                std::fs::create_dir_all(&tmp)?;
                std::fs::write(tmp.join(MANIFEST_FILE), &text)?;
                for (p, t) in &files {
                    if p == MANIFEST_FILE {
                        continue;
                    }
                    let f = tmp.join(p);
                    if let Some(d) = f.parent() {
                        std::fs::create_dir_all(d)?;
                    }
                    std::fs::write(f, t)?;
                }
                let _ = std::fs::remove_dir_all(&dir);
                std::fs::rename(&tmp, &dir)
            };
            match write() {
                Ok(()) => {
                    tracing::info!(package = %id, by = who, "my package saved");
                    (
                        200,
                        json!({"report": validate::validate_path(&dir, Mode::Private)}),
                    )
                }
                Err(e) => fail(500, e.to_string()),
            }
        }
        ("DELETE", ["my", id]) => {
            if !safe_id(id) {
                return fail(400, "bad id".into());
            }
            match std::fs::remove_dir_all(my_dir(&svc.state_dir).join(id)) {
                Ok(()) => (200, json!({"ok": true})),
                Err(_) => fail(404, "no such package".into()),
            }
        }
        _ => fail(404, "no such engine-store call".into()),
    }
}

/// `GET engine-store/my/<id>/export`: the zip bytes.
pub fn export_my(state_dir: &Path, id: &str) -> Result<Vec<u8>, String> {
    if !safe_id(id) {
        return Err("bad id".into());
    }
    let p = store::load_dir(&my_dir(state_dir).join(id), "my", true).map_err(|r| r.text())?;
    wizard::zip_files(id, &p.files)
}

fn files_of(v: &Value) -> Files {
    v.as_object()
        .map(|o| {
            o.iter()
                .filter_map(|(k, x)| x.as_str().map(|t| (k.clone(), t.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

impl Store {
    fn fetcher(&self) -> Arc<dyn Fetch> {
        self.fetch
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| Arc::new(HttpFetch::default()))
    }

    async fn index(&self, e: &StoreEntry) -> Result<Index, String> {
        if let Some(x) = self.cache.lock().unwrap().get(&e.id)
            && x.at.is_some_and(|t| t.elapsed() < INDEX_TTL)
        {
            return match (&x.index, &x.error) {
                (Some(i), _) => Ok(i.clone()),
                (None, Some(er)) => Err(er.clone()),
                _ => Err("no index".into()),
            };
        }
        let base = store::index_base(&e.url)?;
        let r = async {
            let t = self.fetcher().text(&format!("{base}/index.json")).await?;
            if t.len() > 4 * 1024 * 1024 {
                return Err("the store's index is too large".to_string());
            }
            serde_json::from_str::<Index>(&t).map_err(|x| format!("index.json: {x}"))
        }
        .await;
        let mut cache = self.cache.lock().unwrap();
        let ent = cache.entry(e.id.clone()).or_default();
        ent.at = Some(Instant::now());
        match &r {
            Ok(i) => {
                ent.index = Some(i.clone());
                ent.error = None;
            }
            Err(x) => {
                ent.index = None;
                ent.error = Some(x.clone());
            }
        }
        r
    }

    /// One package from a store (`default`, `my` or an added store's id).
    pub async fn package(
        &self,
        state_dir: &Path,
        store_id: &str,
        id: &str,
    ) -> Result<Package, String> {
        match store_id {
            "default" | "" => store::embedded::packages()
                .into_iter()
                .find(|p| p.manifest.id == id)
                .ok_or_else(|| format!("no package `{id}` in the default store")),
            "my" => {
                if !safe_id(id) {
                    return Err("bad id".into());
                }
                store::load_dir(&my_dir(state_dir).join(id), "my", true).map_err(|r| r.text())
            }
            sid => {
                let e = added_stores(state_dir)
                    .into_iter()
                    .find(|x| x.id == sid)
                    .ok_or_else(|| format!("no store `{sid}`"))?;
                if let Some(p) = self
                    .cache
                    .lock()
                    .unwrap()
                    .get(sid)
                    .and_then(|x| x.packages.get(id).cloned())
                {
                    return Ok(p);
                }
                let idx = self.index(&e).await?;
                let ent = idx
                    .packages
                    .iter()
                    .find(|x| x.id == id)
                    .ok_or_else(|| format!("no package `{id}` in {}", e.name))?;
                let base = store::index_base(&e.url)?;
                let p = store::fetch_package(self.fetcher().as_ref(), &base, ent, sid, e.trusted)
                    .await?;
                self.cache
                    .lock()
                    .unwrap()
                    .entry(sid.to_string())
                    .or_default()
                    .packages
                    .insert(id.to_string(), p.clone());
                Ok(p)
            }
        }
    }
}

/// The approvals the admin ticked, with host mounts rendered from the params.
fn approvals_of(p: &Package, v: &Value, params: &IndexMap<String, Value>, who: &str) -> Approvals {
    let b = |k: &str| v[k].as_bool().unwrap_or(false);
    let list = |k: &str| -> Vec<String> {
        v[k].as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    };
    let asked = list("host_mounts");
    let mut mounts = vec![];
    for hm in &p.manifest.security.host_mounts {
        let r = render_source(&p.manifest, &hm.source, params);
        if asked.iter().any(|a| a == "*" || *a == hm.source || *a == r) {
            mounts.push(r);
        }
    }
    Approvals {
        host_run: b("host_run"),
        privileged: b("privileged"),
        host_network: b("host_network"),
        host_mounts: mounts,
        devices: list("devices"),
        cap_add: list("cap_add"),
        untrusted: b("untrusted"),
        by: who.to_string(),
    }
}

fn free_port(c: &ControlConfig, server: &str) -> u16 {
    let used: Vec<u16> = c
        .engines
        .values()
        .flat_map(|e| {
            c.cards
                .iter()
                .filter(|(_, cd)| cd.server == server)
                .filter_map(|(id, _)| e.port.get(id).copied())
                .collect::<Vec<_>>()
        })
        .collect();
    (FIRST_PORT..u16::MAX)
        .find(|p| !used.contains(p))
        .unwrap_or(FIRST_PORT)
}

/// The config edits that add `[engines.<engine>]` for an install on `card`.
pub fn engine_edits(
    c: &ControlConfig,
    engine: &str,
    m: &Manifest,
    inst: &Installed,
    card: &str,
    params: &IndexMap<String, Value>,
    store_id: &str,
) -> Vec<crate::control_api::form::Edit> {
    use crate::control_api::form::Edit;
    let server = c
        .cards
        .get(card)
        .map(|x| x.server.clone())
        .unwrap_or_default();
    let port = c
        .engines
        .get(engine)
        .and_then(|e| e.port.get(card).copied())
        .unwrap_or_else(|| free_port(c, &server));
    let meth = &m.methods[inst.method];
    let mut e = json!({
        "title": m.name,
        "driver": "package",
        "served_id": m.id,
        "port": { card: port },
        "container": format!("abhoth-{engine}-{{card_lc}}"),
        "package": { "id": m.id, "version": m.version, "store": store_id },
    });
    if !params.is_empty() {
        e["package"]["params"] = json!(params);
    }
    if let Some(h) = meth.health.http.clone() {
        e["health"] = json!({ "path": h });
    }
    let kinds: Vec<&str> = m
        .kinds
        .iter()
        .map(String::as_str)
        .filter(|k| {
            super::schema::ModelKind::ALL
                .iter()
                .any(|x| x.as_str() == *k)
        })
        .collect();
    let mut caps = json!({ "kinds": kinds });
    if let Some(i) = m
        .caps
        .inherits
        .as_ref()
        .filter(|i| super::caps::kind_def(i).is_some())
    {
        caps["inherits"] = json!(i);
    }
    e["caps"] = caps;
    let mut edits = vec![];
    // Keep a port an earlier install chose on other cards.
    if let Some(old) = c.engines.get(engine) {
        let mut ports = serde_json::Map::new();
        for cd in old.port.cards() {
            if let Some(p) = old.port.get(cd) {
                ports.insert(cd.to_string(), json!(p));
            }
        }
        ports.insert(card.to_string(), json!(port));
        e["port"] = Value::Object(ports);
    }
    edits.push(Edit {
        path: vec!["engines".into(), engine.into()],
        value: e,
    });
    // Let the card run it for each of its kinds.
    if let Some(cd) = c.cards.get(card) {
        for k in &kinds {
            let mut list: Vec<String> = cd
                .engines
                .as_ref()
                .and_then(|m| m.get(*k).cloned())
                .unwrap_or_default();
            if cd.engines.is_none() && *k == "llm" {
                continue; // unset llm = every llm engine with a port
            }
            if !list.iter().any(|x| x == engine) {
                list.push(engine.into());
                edits.push(Edit {
                    path: vec!["cards".into(), card.into(), "engines".into(), (*k).into()],
                    value: json!(list),
                });
            }
        }
    }
    edits
}

async fn save_edits(
    svc: &Service,
    edits: &[crate::control_api::form::Edit],
    reason: &str,
) -> Result<(), String> {
    let doc = svc.backend.config();
    let text = crate::control_api::form::apply_edits(&doc.text, edits)?;
    let r = svc
        .backend
        .save_config(&text, &doc.rev, reason, "router:engine-store")
        .await
        .map_err(|e| e.message)?;
    if !r.ok {
        return Err(format!(
            "router.toml would not validate: {}",
            r.problems
                .iter()
                .map(|i| format!("{i:?}"))
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }
    Ok(())
}

async fn install(
    svc: &Arc<Service>,
    st: &Arc<Store>,
    c: &ControlConfig,
    body: &Value,
    who: &str,
) -> (u16, Value) {
    let s = |k: &str| body[k].as_str().unwrap_or("").to_string();
    let (store_id, id, server, card) = (s("store"), s("id"), s("server"), s("card"));
    let p = match st.package(&svc.state_dir, &store_id, &id).await {
        Ok(p) => p,
        Err(e) => return (404, json!({"error": e})),
    };
    let r = p.validate();
    if !r.ok {
        return (
            422,
            json!({"error": "the package does not validate", "report": r}),
        );
    }
    let Some(sel) =
        card_sel(c, &card).filter(|_| c.cards.get(&card).is_some_and(|x| x.server == server))
    else {
        return (
            404,
            json!({"error": format!("no card `{card}` on `{server}`")}),
        );
    };
    let engine = if s("engine").is_empty() {
        p.manifest.id.clone()
    } else {
        s("engine")
    };
    if !safe_id(&engine) {
        return (
            400,
            json!({"error": "an engine id is lower case letters, digits, - and _"}),
        );
    }
    if c.engines
        .get(&engine)
        .is_some_and(|e| e.driver != DriverKind::Package)
    {
        return (
            409,
            json!({"error": format!("`{engine}` is a manual engine; pick another engine id")}),
        );
    }
    let h = host_facts(c, &server);
    let f = orca_engines::fit(&p.manifest, &h, &sel, &MethodFilter::containers_only());
    let Some(mi) = f.method else {
        return (
            422,
            json!({"error": "no method of this package fits that card", "fit": f}),
        );
    };
    let params: IndexMap<String, Value> = body["params"]
        .as_object()
        .map(|o| o.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default();
    let ok = approvals_of(&p, &body["approvals"], &params, who);
    let job = st.new_job("install", &p.manifest.key(), &server, &card, &engine);
    let (svc2, st2, jid, c2) = (svc.clone(), st.clone(), job.id.clone(), c.clone());
    tokio::spawn(async move {
        let log = job_log(&st2, &jid);
        log(format!(
            "install {} with {} on {server}/{card}",
            p.manifest.key(),
            p.manifest.methods[mi].label()
        ));
        let runner = LocalRunner::new(
            super::packages::layout(&svc2.state_dir, &server).root,
            Arc::new(RouterExec {
                svc: svc2.clone(),
                server: server.clone(),
                log: log.clone(),
            }),
            st2.fetcher(),
            false,
            h,
        );
        let r = async {
            let inst = runner.install(&p, mi, &sel, &ok).await.map_err(|e| e.0)?;
            for pin in &inst.pins {
                log(format!("pinned: {pin}"));
            }
            let edits = engine_edits(&c2, &engine, &p.manifest, &inst, &card, &params, &p.store);
            save_edits(
                &svc2,
                &edits,
                &format!(
                    "engine store: install {} as {engine} on {card}",
                    p.manifest.key()
                ),
            )
            .await?;
            Ok::<_, String>(format!(
                "installed; [engines.{engine}] is in router.toml (driver = \"package\")"
            ))
        }
        .await;
        st2.finish(&jid, r);
    });
    (202, json!({"job": job}))
}

async fn update(
    svc: &Arc<Service>,
    st: &Arc<Store>,
    c: &ControlConfig,
    engine: &str,
    who: &str,
) -> (u16, Value) {
    let Some(e) = c.engines.get(engine) else {
        return (404, json!({"error": format!("no engine `{engine}`")}));
    };
    let Some(pr) = e
        .package
        .clone()
        .filter(|_| e.driver == DriverKind::Package)
    else {
        return (409, json!({"error": "a manual engine is updated by hand"}));
    };
    let store_id = pr.store.clone().unwrap_or_else(|| "default".into());
    let p = match st.package(&svc.state_dir, &store_id, &pr.id).await {
        Ok(p) => p,
        Err(x) => return (404, json!({"error": x})),
    };
    if p.manifest.version == pr.version {
        return (
            409,
            json!({"error": format!("{} is already at {}", pr.id, pr.version)}),
        );
    }
    let cards: Vec<String> = e.port.cards().iter().map(|x| x.to_string()).collect();
    let Some(card) = cards.first().cloned() else {
        return (409, json!({"error": "the engine has no card"}));
    };
    let server = c
        .cards
        .get(&card)
        .map(|x| x.server.clone())
        .unwrap_or_default();
    let layout = super::packages::layout(&svc.state_dir, &server);
    let Some(old) = layout.find_installed(&pr.id, &pr.version) else {
        return (
            409,
            json!({"error": "the installed version's record is missing; install the new one"}),
        );
    };
    let sel = card_sel(c, &card).expect("card exists");
    let h = host_facts(c, &server);
    let f = orca_engines::fit(&p.manifest, &h, &sel, &MethodFilter::containers_only());
    let Some(mi) = f.method else {
        return (
            422,
            json!({"error": "no method of the new version fits", "fit": f}),
        );
    };
    // The same approvals as before; anything new the package asks for refuses the update.
    let mut ok = old.approvals.clone();
    ok.by = who.to_string();
    let job = st.new_job("update", &p.manifest.key(), &server, &card, engine);
    let (svc2, st2, jid, c2, engine) = (
        svc.clone(),
        st.clone(),
        job.id.clone(),
        c.clone(),
        engine.to_string(),
    );
    tokio::spawn(async move {
        let log = job_log(&st2, &jid);
        let runner = LocalRunner::new(
            layout.root.clone(),
            Arc::new(RouterExec {
                svc: svc2.clone(),
                server: server.clone(),
                log: log.clone(),
            }),
            st2.fetcher(),
            false,
            h,
        );
        let r = async {
            let inst = runner
                .update(&old, &p, mi, &sel, &ok)
                .await
                .map_err(|e| e.0)?;
            let mut edits = vec![];
            for cd in &cards {
                edits.extend(engine_edits(
                    &c2,
                    &engine,
                    &p.manifest,
                    &inst,
                    cd,
                    &pr.params,
                    &p.store,
                ));
            }
            save_edits(
                &svc2,
                &edits,
                &format!("engine store: update {engine} to {}", p.manifest.key()),
            )
            .await?;
            Ok::<_, String>(format!(
                "updated to {}; the old version stays until removed",
                p.manifest.version
            ))
        }
        .await;
        st2.finish(&jid, r);
    });
    (202, json!({"job": job}))
}

async fn remove(
    svc: &Arc<Service>,
    st: &Arc<Store>,
    c: &ControlConfig,
    engine: &str,
    uninstall: bool,
    _who: &str,
) -> (u16, Value) {
    let Some(e) = c.engines.get(engine) else {
        return (404, json!({"error": format!("no engine `{engine}`")}));
    };
    let Some(pr) = e
        .package
        .clone()
        .filter(|_| e.driver == DriverKind::Package)
    else {
        return (
            409,
            json!({"error": "a manual engine is removed in Inventory"}),
        );
    };
    let card = e
        .port
        .cards()
        .first()
        .map(|x| x.to_string())
        .unwrap_or_default();
    let server = c
        .cards
        .get(&card)
        .map(|x| x.server.clone())
        .unwrap_or_default();
    let others = c.engines.iter().any(|(k, x)| {
        k != engine
            && x.package
                .as_ref()
                .is_some_and(|p| p.id == pr.id && p.version == pr.version)
    });
    let job = st.new_job(
        "remove",
        &format!("{}@{}", pr.id, pr.version),
        &server,
        &card,
        engine,
    );
    let (svc2, st2, jid, c2, engine) = (
        svc.clone(),
        st.clone(),
        job.id.clone(),
        c.clone(),
        engine.to_string(),
    );
    tokio::spawn(async move {
        let log = job_log(&st2, &jid);
        let r = async {
            use crate::control_api::form::Edit;
            let mut edits = vec![Edit {
                path: vec!["engines".into(), engine.clone()],
                value: Value::Null,
            }];
            for (cid, cd) in &c2.cards {
                if let Some(m) = &cd.engines {
                    for (k, list) in m {
                        if list.contains(&engine) {
                            let l: Vec<&String> = list.iter().filter(|x| **x != engine).collect();
                            edits.push(Edit {
                                path: vec![
                                    "cards".into(),
                                    cid.clone(),
                                    "engines".into(),
                                    k.clone(),
                                ],
                                value: json!(l),
                            });
                        }
                    }
                }
            }
            save_edits(&svc2, &edits, &format!("engine store: remove {engine}")).await?;
            log(format!("[engines.{engine}] removed from router.toml"));
            if uninstall {
                if others {
                    log("another engine uses the same package: its files stay".into());
                } else {
                    let layout = super::packages::layout(&svc2.state_dir, &server);
                    if let Some(i) = layout.find_installed(&pr.id, &pr.version) {
                        let m = layout.installed_manifest(&i)?;
                        let runner = LocalRunner::new(
                            layout.root.clone(),
                            Arc::new(RouterExec {
                                svc: svc2.clone(),
                                server: server.clone(),
                                log: log.clone(),
                            }),
                            st2.fetcher(),
                            false,
                            host_facts(&c2, &server),
                        );
                        runner.uninstall(&m, &i).await.map_err(|e| e.0)?;
                        log("uninstalled (images are never removed)".into());
                    }
                }
            }
            Ok::<_, String>("removed".into())
        }
        .await;
        st2.finish(&jid, r);
    });
    (202, json!({"job": job}))
}

async fn draft(c: &ControlConfig, source: &str, value: &str) -> (u16, Value) {
    let d = match source {
        "scratch" => wizard::draft_scratch(),
        "docker" => match wizard::parse_image(value) {
            Err(e) => return (400, json!({"error": e})),
            Ok(_) => match wizard::probe_image(value).await {
                Ok(p) => wizard::draft_docker(value, Some(p), None),
                Err(e) => wizard::draft_docker(value, None, Some(e)),
            },
        },
        "compose" => {
            if value.len() > 256 * 1024 {
                return (400, json!({"error": "the compose file is too large"}));
            }
            wizard::draft_compose(value)
        }
        "pip" => wizard::draft_pip(value),
        "release" => match wizard::release_assets(value).await {
            Ok((o, r, t, a)) => wizard::draft_release(&o, &r, &t, &a),
            Err(e) => return (422, json!({"error": e})),
        },
        "engine" => {
            let Some(e) = c.engines.get(value) else {
                return (404, json!({"error": format!("no engine `{value}`")}));
            };
            let port = e
                .port
                .cards()
                .first()
                .and_then(|cd| e.port.get(cd).copied());
            let image = e.image.as_deref().filter(|i| !i.contains('{'));
            let mut d = wizard::draft_existing(value, &e.title, image, port, &e.health.path);
            if e.image.as_deref().is_some_and(|i| i.contains('{')) {
                d.notes.push(
                    "The engine's image is a template; fill in the image it renders to.".into(),
                );
            }
            d
        }
        _ => {
            return (
                400,
                json!({"error": "source is scratch, docker, compose, pip, release or engine"}),
            );
        }
    };
    (200, serde_json::to_value(d).unwrap_or_default())
}

/// The paths a test run tries, and the API each answer suggests.
const PROBE_PATHS: &[(&str, &str)] = &[
    ("/health", ""),
    ("/v1/models", "openai-chat"),
    ("/system_stats", "comfyui"),
    ("/v1/audio/voices", "openai-speech"),
    ("/docs", "http"),
    ("/", "http"),
];

async fn test_run(svc: &Arc<Service>, c: &ControlConfig, body: &Value, who: &str) -> (u16, Value) {
    if body["confirm"].as_bool() != Some(true) {
        return (
            400,
            json!({"error": "a test run starts a container: send confirm = true once the admin agreed"}),
        );
    }
    let s = |k: &str| body[k].as_str().unwrap_or("").to_string();
    let (image, server, card) = (s("image"), s("server"), s("card"));
    if wizard::parse_image(&image).is_err() {
        return (400, json!({"error": "not an image reference"}));
    }
    let Some(cd) = c.cards.get(&card).filter(|x| x.server == server) else {
        return (
            404,
            json!({"error": format!("no card `{card}` on `{server}`")}),
        );
    };
    let port: u16 = body["port"].as_u64().map(|p| p as u16).unwrap_or(8080);
    let host_port = free_port(c, &server).saturating_add(900);
    let ex = match svc.exec(&server) {
        Ok(e) => e,
        Err(e) => return (503, json!({"error": e})),
    };
    let name = format!(
        "abhoth-testrun-{}",
        super::driver::short_hash(&format!("{image}{}", now()))
    );
    let mut argv: Vec<String> = [
        "docker",
        "run",
        "-d",
        "--name",
        &name,
        "--cap-drop",
        "ALL",
        "--security-opt",
        "no-new-privileges",
        "--pids-limit",
        "512",
        "--memory",
        "8g",
        "--cpus",
        "4",
        "--label",
        "abhoth.testrun=1",
        "-p",
        &format!("127.0.0.1:{host_port}:{port}"),
    ]
    .iter()
    .map(|x| x.to_string())
    .collect();
    if cd.is_cpu() {
        argv.extend(["-e".into(), "NVIDIA_VISIBLE_DEVICES=void".into()]);
    } else {
        argv.extend(["--gpus".into(), format!("device={}", cd.gpu_uuid)]);
    }
    argv.push(image.clone());
    tracing::info!(image = %image, server = %server, card = %card, by = who, "engine wizard test run");
    let run = |id: &str, text: String, secs: u64| {
        let ex = ex.clone();
        let server = server.clone();
        let id = format!("{id}:{}", super::driver::short_hash(&text));
        async move {
            ex.run(
                ExecRequest::command(&server, &id, text, Duration::from_secs(secs)).as_root(),
                &Cancel::new(),
            )
            .await
        }
    };
    let started = run("testrun-start", argv_to_shell(&argv), 900).await;
    let cleanup = format!(
        "docker rm -f {} >/dev/null 2>&1; true",
        super::template::shell_quote(&name)
    );
    match &started {
        Ok(o) if o.ok() => {}
        Ok(o) => {
            let _ = run("testrun-rm", cleanup.clone(), 120).await;
            return (
                422,
                json!({"error": format!("docker run failed: {}", orca_engines::exec::scrub(&o.summary()))}),
            );
        }
        Err(e) => return (503, json!({"error": e.to_string()})),
    }
    // Wait up to 90 s for anything to answer.
    let mut probes = vec![];
    let curl = |p: &str| {
        format!(
            "curl -s -m 5 -o /dev/null -w '%{{http_code}}' http://127.0.0.1:{host_port}{p} || true"
        )
    };
    for _ in 0..18 {
        tokio::time::sleep(Duration::from_secs(5)).await;
        if let Ok(o) = run("testrun-probe", curl("/"), 30).await
            && o.stdout.trim().parse::<u16>().is_ok_and(|c| c > 0)
        {
            break;
        }
    }
    let mut api_guess: Option<String> = None;
    let mut health_guess: Option<String> = None;
    for (p, api) in PROBE_PATHS {
        let code = run("testrun-probe", curl(p), 30)
            .await
            .ok()
            .and_then(|o| o.stdout.trim().parse::<u16>().ok())
            .unwrap_or(0);
        if code == 200 {
            if health_guess.is_none() && *p != "/" {
                health_guess = Some(p.to_string());
            }
            if api_guess.is_none() && !api.is_empty() {
                api_guess = Some(api.to_string());
            }
        }
        probes.push(json!({"path": p, "status": code, "snippet": ""}));
    }
    let logs = run(
        "testrun-logs",
        format!(
            "docker logs --tail 80 {} 2>&1",
            super::template::shell_quote(&name)
        ),
        60,
    )
    .await
    .map(|o| orca_engines::exec::scrub(&o.stdout))
    .unwrap_or_default();
    let _ = run("testrun-rm", cleanup, 120).await;
    let ok = probes.iter().any(|p| p["status"] != 0);
    (
        200,
        json!({"ok": ok, "probes": probes, "api_guess": api_guess, "health_guess": health_guess, "logs": logs}),
    )
}
