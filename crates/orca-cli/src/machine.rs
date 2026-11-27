//! This machine (docs/design/MACHINE.md, D155): the router's management for one server, the PC
//! the desktop app runs on.
//!
//! - `<home>/machine/router.toml` is the inventory in the router's own schema (one server, its
//!   cards, engines and models), edited by the router's own admin API, run **in-process** with no
//!   socket: the web host's `/api/admin/machine/*` calls the router's axum app directly.
//! - `<home>/machine/engines.toml` holds how manual engines run (the bridge's engine format);
//!   each one is mirrored into `router.toml` as an external engine so the inventory sees it.
//! - Engines run under the bridge's supervisor (`orca-bridge`), shared by the app's own requests
//!   (`/api/machine/v1/*`, placed with the router's `inventory::place`) and, in Bridge mode, the
//!   router's. `share.json` says what the router may use.
//! - With *Bridge: allow router to config* on, a router admin edits all this through the bridge
//!   (`config.get`, `config.apply`, `machine.api`); every change is checked, backed up, logged
//!   in `remote-changes.jsonl` and undoable here, and anything that needs approval waits in
//!   `pending.json` for the user.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use orca_bridge::client::{self, Options, Remote, Timing};
use orca_bridge::config::{BridgeConfig, CardOverride, EngineConfig};
use orca_bridge::proto::{Descriptor, ModelDesc};
use orca_bridge::runner::Supervisor;
use orca_core::config::DesktopConfig;
use orca_router::control::schema::{ControlConfig, DriverKind, PerCard};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tower_service::Service;

/// The label that marks a `router.toml` engine as the mirror of a manual engine.
pub const MANUAL_LABEL: &str = "abhoth.machine";

// ── files ────────────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Paths {
    pub dir: PathBuf,
    pub main: PathBuf,
    pub json: PathBuf,
    pub state: PathBuf,
    pub engines: PathBuf,
    pub share: PathBuf,
    pub remote_log: PathBuf,
    pub pending: PathBuf,
    pub backups: PathBuf,
    pub status: PathBuf,
}

impl Paths {
    pub fn of(home: &Path) -> Paths {
        let dir = home.join("machine");
        Paths {
            main: dir.join("router.toml"),
            json: dir.join("router.json"),
            state: dir.join("state"),
            engines: dir.join("engines.toml"),
            share: dir.join("share.json"),
            remote_log: dir.join("remote-changes.jsonl"),
            pending: dir.join("pending.json"),
            backups: dir.join("remote-backups"),
            status: dir.join("status.json"),
            dir,
        }
    }
}

/// This machine's name as typed (`COMPUTERNAME`, `HOSTNAME`), or `pc`.
pub fn hostname() -> String {
    ["COMPUTERNAME", "HOSTNAME"]
        .iter()
        .filter_map(|k| std::env::var(k).ok())
        .map(|s| s.trim().to_string())
        .find(|s| !s.is_empty())
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "pc".into())
}

/// The server id for a hostname: lower case, `[a-z0-9-]`, at most 32 characters.
pub fn server_id(host: &str) -> String {
    let mut s: String = host
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    while s.contains("--") {
        s = s.replace("--", "-");
    }
    let s = s.trim_matches('-');
    let s: String = s.chars().take(32).collect();
    let s = s.trim_matches('-').to_string();
    if s.is_empty() { "pc".into() } else { s }
}

fn q(s: &str) -> String {
    toml::Value::String(s.to_string()).to_string()
}

fn slash(p: &Path) -> String {
    p.display().to_string().replace('\\', "/")
}

/// The new `router.toml`: this machine, its detected cards (managed, on demand), no engines or
/// models yet. `old`: a Bridge engines file to take card overrides and the model folder from.
pub fn render_main(
    p: &Paths,
    host: &str,
    gpus: &[orca_bridge::detect::Gpu],
    cpu: &orca_bridge::detect::Cpu,
    model_dir: &Path,
    old: Option<&BridgeConfig>,
) -> String {
    let id = server_id(host);
    let model_dir = old
        .and_then(|o| o.bridge.model_dir.clone())
        .unwrap_or_else(|| slash(model_dir));
    let mut t = format!(
        "# This machine (D155): the desktop app's own inventory, in the router's schema.\n\
         # Edit it on Settings -> This machine; the app checks, backs up and reloads every save.\n\
         version = 1\n\n\
         [router]\nid = \"machine\"\nlisten = \"127.0.0.1:0\"\nstate_dir = {}\n\n\
         # Nothing in the router itself starts or switches engines: the app runs them.\n\
         [control]\nenabled = false\n\n[inventory]\nversion = 1\n\n\
         [servers.{id}]\ntitle = {}\nhost = \"127.0.0.1\"\nexec = \"local\"\nmodel_dir = {}\n",
        q(&slash(&p.state)),
        q(host),
        q(&model_dir),
    );
    let ov = |card: &str| old.and_then(|o| o.cards.get(card));
    let mut gpus = gpus.to_vec();
    gpus.sort_by_key(|g| g.index);
    for g in &gpus {
        let cid = format!("GPU{}", g.index);
        let o = ov(&cid);
        if o.is_some_and(|o| o.disable) {
            continue;
        }
        t.push_str(&format!(
            "\n[cards.{cid}]\nserver = {}\ngpu_uuid = {}\nlabel = {}\nvram_gib = {:?}\n",
            q(&id),
            q(&g.uuid),
            q(o.and_then(|o| o.label.as_deref()).unwrap_or(&g.name)),
            o.and_then(|o| o.vram_gib).unwrap_or(g.vram_gib),
        ));
        card_tail(&mut t, o, true);
    }
    let o = ov("CPU");
    if !o.is_some_and(|o| o.disable) {
        t.push_str(&format!(
            "\n[cards.CPU]\nserver = {}\ndevice = \"cpu\"\nlabel = {}\n",
            q(&id),
            q(o.and_then(|o| o.label.as_deref()).unwrap_or("CPU")),
        ));
        let cores = o.and_then(|o| o.cores).unwrap_or(cpu.cores);
        if cores > 0 {
            t.push_str(&format!("cores = {cores}\n"));
        }
        let ram = o.and_then(|o| o.ram_gib).unwrap_or(cpu.ram_gib);
        if ram > 0.0 {
            t.push_str(&format!("ram_gib = {:?}\n", (ram * 10.0).round() / 10.0));
        }
        card_tail(&mut t, o, false);
    }
    t
}

/// `on_demand`: GPU cards are open to on-demand starts; a CPU card never is in the router's
/// schema (no layout switch), and this machine places on it anyway (see [`Machine::place`]).
fn card_tail(t: &mut String, o: Option<&CardOverride>, on_demand: bool) {
    if let Some(r) = o.and_then(|o| o.reserve_gib) {
        t.push_str(&format!("reserve_gib = {r:?}\n"));
    }
    t.push_str(if on_demand {
        "managed = true\non_demand = true\n"
    } else {
        "managed = true\n"
    });
    if let Some(o) = o.filter(|o| !o.engines.is_empty()) {
        t.push_str("engines = { ");
        let parts: Vec<String> = o
            .engines
            .iter()
            .map(|(k, v)| {
                format!(
                    "{k} = [{}]",
                    v.iter().map(|e| q(e)).collect::<Vec<_>>().join(", ")
                )
            })
            .collect();
        t.push_str(&parts.join(", "));
        t.push_str(" }\n");
    }
}

/// Manual engines (process, docker, external), in the bridge's own format.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Manual {
    #[serde(default)]
    pub engines: BTreeMap<String, EngineConfig>,
}

impl Manual {
    pub fn load(p: &Path) -> Result<Manual, String> {
        match std::fs::read_to_string(p) {
            Ok(t) => toml::from_str(&t).map_err(|e| format!("{}: {e}", p.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Manual::default()),
            Err(e) => Err(format!("{}: {e}", p.display())),
        }
    }
    pub fn rev(&self) -> String {
        let t = toml::to_string(self).unwrap_or_default();
        orca_bridge::proto::token_hash(&t)[..12].to_string()
    }
}

/// What the router may use (`share.json`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Share {
    #[serde(default)]
    pub engines: BTreeMap<String, bool>,
    #[serde(default)]
    pub models: BTreeMap<String, bool>,
}

impl Share {
    pub fn load(p: &Path) -> Share {
        std::fs::read_to_string(p)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }
    /// New engines and models get the default for now: shared while Bridge mode is on (an
    /// engine added while bridged is meant for the router), not shared while it is off.
    /// Returns whether anything was added.
    pub fn reconcile(&mut self, c: &ControlConfig, bridged: bool) -> bool {
        let mut changed = false;
        for id in c.engines.keys() {
            if !self.engines.contains_key(id) {
                self.engines.insert(id.clone(), bridged);
                changed = true;
            }
        }
        for (id, m) in &c.models {
            if !m.is_component() && !self.models.contains_key(id) {
                self.models.insert(id.clone(), bridged);
                changed = true;
            }
        }
        changed
    }
    pub fn engine(&self, id: &str) -> bool {
        self.engines.get(id).copied().unwrap_or(false)
    }
    pub fn model(&self, id: &str) -> bool {
        self.models.get(id).copied().unwrap_or(false)
    }
}

fn write_atomic(p: &Path, text: &str) -> Result<(), String> {
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    let tmp = p.with_extension("tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, p).map_err(|e| format!("{}: {e}", p.display()))
}

/// Make `<home>/machine/` if it is not there: detection, then the old Bridge engines file (if
/// one is set and readable) migrated once. Returns whether it was made now.
pub async fn ensure(p: &Paths, home: &Path, old_file: &str) -> Result<bool, String> {
    if p.main.exists() {
        return Ok(false);
    }
    let old = if old_file.trim().is_empty() {
        None
    } else {
        let f = PathBuf::from(old_file.trim());
        std::fs::read_to_string(&f)
            .ok()
            .and_then(|t| toml::from_str::<BridgeConfig>(&t).ok())
    };
    let gpus = orca_bridge::detect::gpus().await;
    let cpu = orca_bridge::detect::cpu().await;
    std::fs::create_dir_all(&p.state).map_err(|e| format!("{}: {e}", p.state.display()))?;
    let host = hostname();
    let text = render_main(p, &host, &gpus, &cpu, &home.join("models"), old.as_ref());
    let manual = Manual {
        engines: old.as_ref().map(|o| o.engines.clone()).unwrap_or_default(),
    };
    let mut share = Share::default();
    for id in manual.engines.keys() {
        share.engines.insert(id.clone(), true); // shared before the migration: kept so
    }
    write_atomic(&p.json, &orca_router::init::render_json())?;
    write_atomic(
        &p.engines,
        &toml::to_string(&manual).map_err(|e| e.to_string())?,
    )?;
    write_atomic(
        &p.share,
        &serde_json::to_string_pretty(&share).unwrap_or_default(),
    )?;
    let text = mirror_text(&text, &server_id(&host), &manual)?;
    write_atomic(&p.main, &text)?;
    if old.is_some() {
        tracing::info!(
            "this machine: made from {} (its engines and card overrides)",
            old_file.trim()
        );
    }
    Ok(true)
}

/// `router.toml` with each manual engine mirrored as an external engine (ports, kinds) and
/// mirrors of removed manual engines dropped. Comments and order elsewhere are kept.
pub fn mirror_text(text: &str, server: &str, manual: &Manual) -> Result<String, String> {
    let mut doc: toml_edit::DocumentMut = text.parse().map_err(|e| format!("router.toml: {e}"))?;
    if !doc.contains_key("engines") {
        let mut t = toml_edit::Table::new();
        t.set_implicit(true);
        doc["engines"] = toml_edit::Item::Table(t);
    }
    let cards: Vec<String> = doc
        .get("cards")
        .and_then(|c| c.as_table())
        .map(|t| {
            t.iter()
                .filter(|(_, v)| v.get("server").and_then(|s| s.as_str()) == Some(server))
                .map(|(k, _)| k.to_string())
                .collect()
        })
        .unwrap_or_default();
    let engines = doc["engines"]
        .as_table_mut()
        .ok_or("engines is not a table")?;
    let stale: Vec<String> = engines
        .iter()
        .filter(|(k, v)| {
            v.get("labels")
                .and_then(|l| l.get(MANUAL_LABEL))
                .and_then(|x| x.as_str())
                == Some("manual")
                && !manual.engines.contains_key(*k)
        })
        .map(|(k, _)| k.to_string())
        .collect();
    for k in stale {
        engines.remove(&k);
    }
    for (id, e) in &manual.engines {
        let mut t = match engines.get(id).and_then(|x| x.as_table()) {
            Some(existing)
                if existing
                    .get("labels")
                    .and_then(|l| l.get(MANUAL_LABEL))
                    .and_then(|x| x.as_str())
                    != Some("manual") =>
            {
                return Err(format!(
                    "engines.{id}: there is already an engine with this id (not a manual one)"
                ));
            }
            Some(existing) => existing.clone(),
            None => toml_edit::Table::new(),
        };
        let title = t
            .get("title")
            .and_then(|x| x.as_str())
            .unwrap_or(id)
            .to_string();
        t["title"] = toml_edit::value(title);
        t["driver"] = toml_edit::value("external");
        let mut port = toml_edit::InlineTable::new();
        let on: Vec<&String> = if e.cards.is_empty() {
            cards.iter().collect()
        } else {
            e.cards.iter().collect()
        };
        for c in on {
            if let Some(p) = e.port_for(c) {
                port.insert(c, (p as i64).into());
            }
        }
        t["port"] = toml_edit::value(port);
        let mut caps = toml_edit::InlineTable::new();
        if let Some(inh) = e
            .caps
            .as_ref()
            .and_then(|c| c.get("inherits"))
            .and_then(Value::as_str)
        {
            caps.insert("inherits", inh.into());
        }
        if !e.kinds.is_empty() {
            let mut a = toml_edit::Array::new();
            for k in &e.kinds {
                a.push(k.as_str());
            }
            caps.insert("kinds", toml_edit::Value::Array(a));
        }
        if caps.is_empty() {
            t.remove("caps");
        } else {
            t["caps"] = toml_edit::value(caps);
        }
        if !e.health.is_empty() && e.health != "tcp" {
            let mut h = toml_edit::InlineTable::new();
            h.insert("path", e.health.as_str().into());
            t["health"] = toml_edit::value(h);
        }
        let mut labels = toml_edit::InlineTable::new();
        labels.insert(MANUAL_LABEL, "manual".into());
        t["labels"] = toml_edit::value(labels);
        engines.insert(id, toml_edit::Item::Table(t));
    }
    Ok(doc.to_string())
}

// ── translation: router.toml (+ manual engines) to the bridge's config ──────────────────────

/// The one server in the machine's config (the first; there is only one).
pub fn the_server(c: &ControlConfig) -> Option<(&String, &orca_router::control::schema::Server)> {
    c.servers.iter().next()
}

/// The bridge config the supervisor runs, and what could not be translated (MACHINE.md 3).
pub fn to_bridge(c: &ControlConfig, manual: &Manual, state: &Path) -> (BridgeConfig, Vec<String>) {
    let mut problems = Vec::new();
    let mut b = BridgeConfig::default();
    let Some((sid, s)) = the_server(c) else {
        problems.push("no server in router.toml".into());
        return (b, problems);
    };
    b.bridge.id = sid.clone();
    b.bridge.title = if s.title.is_empty() {
        sid.clone()
    } else {
        s.title.clone()
    };
    b.bridge.model_dir = s.model_dir.clone();
    b.bridge.comfy_root = s.comfy_root.clone();
    // packages installed from the Engine store live where the router's layout put them
    b.bridge.state_dir = slash(&state.join("packages").join(sid));
    let mine: Vec<(&String, &orca_router::control::schema::Card)> =
        c.cards.iter().filter(|(_, k)| &k.server == sid).collect();
    for (cid, k) in &mine {
        let o = CardOverride {
            label: (!k.label.is_empty()).then(|| k.label.clone()),
            vram_gib: k.vram_gib,
            cores: k.cores,
            ram_gib: k.ram_gib,
            reserve_gib: Some(k.reserve_gib),
            engines: k
                .engines
                .as_ref()
                .map(|m| m.iter().map(|(a, v)| (a.clone(), v.clone())).collect())
                .unwrap_or_default(),
            disable: !k.managed,
            ..CardOverride::default()
        };
        b.cards.insert((*cid).clone(), o);
    }
    let card_ids: Vec<String> = mine.iter().map(|(k, _)| (*k).clone()).collect();
    for (eid, e) in &c.engines {
        let ports: BTreeMap<String, u16> = match &e.port {
            PerCard::All(p) => card_ids.iter().map(|k| (k.clone(), *p)).collect(),
            PerCard::Each(m) => m
                .iter()
                .filter(|(k, _)| card_ids.contains(k))
                .map(|(k, v)| (k.clone(), *v))
                .collect(),
        };
        let manual_one = e.labels.get(MANUAL_LABEL).map(String::as_str) == Some("manual");
        let mut ec: EngineConfig = match (manual_one, e.driver) {
            (true, _) => match manual.engines.get(eid) {
                Some(m) => m.clone(),
                None => continue,
            },
            (false, DriverKind::Package) => {
                let Some(p) = &e.package else {
                    problems.push(format!(
                        "engines.{eid}: driver = \"package\" without package"
                    ));
                    continue;
                };
                let mut ec = base_engine();
                ec.package = Some(format!("{}@{}", p.id, p.version));
                ec.params = p
                    .params
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                ec
            }
            (false, DriverKind::External) => {
                let mut ec = base_engine();
                ec.lifecycle = "external".into();
                ec
            }
            (false, d) => {
                problems.push(format!(
                    "engines.{eid}: driver {d:?} runs only under a router; add it on This machine → Engines instead"
                ));
                continue;
            }
        };
        if ec.cards.is_empty() {
            ec.cards = ports.keys().cloned().collect();
        }
        if ec.ports.is_empty() && ec.port.is_none() {
            ec.ports = ports;
        }
        if ec.caps.is_none()
            && let Some(caps) = &e.caps
        {
            ec.caps = serde_json::to_value(caps).ok();
        }
        if ec.kinds.is_empty()
            && let Some(k) = e.caps.as_ref().and_then(|c| c.kinds.as_ref())
        {
            ec.kinds = k
                .iter()
                .filter_map(|x| serde_json::to_value(x).ok())
                .filter_map(|v| v.as_str().map(String::from))
                .collect();
        }
        if ec.kinds.is_empty() {
            // the kinds the cards allow it for
            for (_, k) in &mine {
                for (kind, list) in k.engines.iter().flatten() {
                    if list.contains(eid) && !ec.kinds.contains(kind) {
                        ec.kinds.push(kind.clone());
                    }
                }
            }
        }
        if e.health.path != "/health" && ec.health == "/health" && !manual_one {
            ec.health = e.health.path.clone();
        }
        b.engines.insert(eid.clone(), ec);
    }
    (b, problems)
}

fn base_engine() -> EngineConfig {
    toml::from_str("").unwrap_or_default()
}

/// Files of unshared models (their Hugging Face folder `<owner>/<name>`), left out of the
/// descriptor.
pub fn unshared_files(c: &ControlConfig, share: &Share) -> Vec<String> {
    c.models
        .iter()
        .filter(|(id, m)| !m.is_component() && !share.model(id))
        .filter_map(|(_, m)| m.source.as_ref().and_then(|s| s.hf.clone()))
        .collect()
}

/// The shared models as the descriptor lists them.
pub fn shared_models(c: &ControlConfig, share: &Share) -> Vec<ModelDesc> {
    c.models
        .iter()
        .filter(|(id, m)| !m.is_component() && share.model(id))
        .map(|(id, m)| {
            let mut engines: Vec<String> = Vec::new();
            if let Some(p) = &m.primary {
                engines.push(p.clone());
            }
            for e in m.prefer.iter().chain(m.artifacts.keys()) {
                if !engines.contains(e) {
                    engines.push(e.clone());
                }
            }
            engines.retain(|e| share.engine(e));
            ModelDesc {
                id: id.clone(),
                kind: serde_json::to_value(m.kind.unwrap_or_default())
                    .ok()
                    .and_then(|v| v.as_str().map(String::from))
                    .unwrap_or_else(|| "llm".into()),
                engines,
            }
        })
        .collect()
}

// ── approvals for remote changes (MACHINE.md 8) ──────────────────────────────────────────────

/// Why a remote change to `router.toml` is refused outright (`Err`), or what in it needs the
/// user's approval first (`Ok(non-empty)`).
pub fn remote_config_check(
    old: &ControlConfig,
    new: &ControlConfig,
) -> Result<Vec<String>, String> {
    let ser = |v: &dyn erased::Ser| v.json();
    if ser(&old.router) != ser(&new.router)
        || ser(&old.control) != ser(&new.control)
        || ser(&old.access) != ser(&new.access)
        || ser(&old.secrets) != ser(&new.secrets)
        || ser(&old.upstreams) != ser(&new.upstreams)
        || ser(&old.routes) != ser(&new.routes)
    {
        return Err(
            "a router may not change [router], [control], [access], [secrets], upstreams or routes here"
                .into(),
        );
    }
    if new.servers.len() != 1 || old.servers.keys().ne(new.servers.keys()) {
        return Err("a router may not add, remove or rename this machine's server".into());
    }
    let mut need = Vec::new();
    for (id, s) in &new.servers {
        let o = &old.servers[id];
        if o.model_dir != s.model_dir
            || o.comfy_root != s.comfy_root
            || o.browse_roots != s.browse_roots
        {
            need.push("the model folder (downloads and browsing stay inside it)".to_string());
        }
        if ser(&o.exec) != ser(&s.exec) || o.host != s.host || ser(&o.vars) != ser(&s.vars) {
            return Err("a router may not change how this machine runs commands".into());
        }
    }
    for (id, e) in &new.engines {
        if old.engines.get(id).is_some_and(|o| ser(o) == ser(e)) {
            continue;
        }
        match e.driver {
            DriverKind::External => {}
            DriverKind::Package => {
                if let Some(m) = e.package.as_ref().and_then(|p| p.method.as_deref())
                    && !matches!(m, "docker" | "compose")
                {
                    need.push(format!(
                        "engines.{id}: package method `{m}` runs on the host"
                    ));
                }
            }
            d => need.push(format!(
                "engines.{id}: driver {d:?} runs commands on this machine"
            )),
        }
    }
    Ok(need)
}

/// What in a manual engine needs the user's approval when a router adds or changes it.
pub fn manual_check(old: &Manual, new: &Manual, model_dir: Option<&str>) -> Vec<String> {
    let mut need = Vec::new();
    for (id, e) in &new.engines {
        if old.engines.get(id) == Some(e) {
            continue;
        }
        if e.process.is_some() {
            need.push(format!("engines.{id}: a process on this machine"));
        }
        if e.host_processes {
            need.push(format!("engines.{id}: host-process package methods"));
        }
        if let Some(d) = &e.docker {
            if !d.flags.is_empty() {
                need.push(format!("engines.{id}: docker flags {}", d.flags.join(" ")));
            }
            if d.publish_all_interfaces {
                need.push(format!("engines.{id}: published on every interface"));
            }
            for m in &d.mounts {
                let host = m.split(':').next().unwrap_or("");
                let host = if host.len() == 1 {
                    // `C:/x:/y`: a Windows drive letter
                    m.splitn(3, ':').take(2).collect::<Vec<_>>().join(":")
                } else {
                    host.to_string()
                };
                let inside = model_dir.is_some_and(|d| {
                    let (h, d) = (host.replace('\\', "/"), d.replace('\\', "/"));
                    h == d || h.starts_with(&format!("{}/", d.trim_end_matches('/')))
                });
                if !inside {
                    need.push(format!(
                        "engines.{id}: mounts {host} (outside the model folder)"
                    ));
                }
            }
        }
    }
    need
}

mod erased {
    pub trait Ser {
        fn json(&self) -> serde_json::Value;
    }
    impl<T: serde::Serialize> Ser for T {
        fn json(&self) -> serde_json::Value {
            serde_json::to_value(self).unwrap_or_default()
        }
    }
}

/// A path under the model folder (remote downloads and browsing stay inside it).
pub fn inside(path: &str, root: &str) -> bool {
    let n = |s: &str| {
        s.replace('\\', "/")
            .trim_end_matches('/')
            .to_ascii_lowercase()
    };
    let (p, r) = (n(path), n(root));
    if p.split('/').any(|x| x == "..") || r.is_empty() {
        return false;
    }
    p == r || p.starts_with(&format!("{r}/"))
}

// ── the machine ──────────────────────────────────────────────────────────────────────────────

/// A remote change waiting for the user (MACHINE.md 8).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Pending {
    pub id: String,
    pub at: i64,
    pub who: String,
    /// `config.apply` or `machine.api`.
    pub op: String,
    pub args: Value,
    pub reasons: Vec<String>,
}

/// One remote change, as logged.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RemoteChange {
    pub id: String,
    pub at: i64,
    pub who: String,
    pub what: String,
    /// The `router.toml` before it (undo puts it back).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engines_backup: Option<String>,
    #[serde(default)]
    pub undone: bool,
}

/// What the descriptor shape and the share check read (updated on every tick).
#[derive(Debug, Clone, Default)]
struct View {
    share: Share,
    unshared_files: Vec<String>,
    models: Vec<ModelDesc>,
    remote: bool,
    rev: String,
}

struct Running {
    sig: String,
    sup: Arc<Supervisor>,
    problems: Vec<String>,
}

struct BridgeRun {
    sig: String,
    handle: client::Handle,
    /// The view the router last got (a change sends the descriptor again).
    seen: String,
}

pub struct Machine {
    pub home: PathBuf,
    pub paths: Paths,
    app: axum::Router,
    view: Arc<RwLock<View>>,
    running: Mutex<Option<Running>>,
    bridge: Mutex<Option<BridgeRun>>,
    read: fn(&Path) -> DesktopConfig,
    http: reqwest::Client,
    gpus: Vec<orca_bridge::detect::Gpu>,
    last_error: Mutex<String>,
    /// Serialises saves (local and remote) so a revision check and the write are one step.
    save: tokio::sync::Mutex<()>,
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn new_id() -> String {
    let mut b = [0u8; 6];
    let _ = getrandom::fill(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn json_response(status: u16, v: &Value) -> Response {
    let mut r = (
        StatusCode::from_u16(status).unwrap_or(StatusCode::OK),
        v.to_string(),
    )
        .into_response();
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/json"),
    );
    r
}

impl Machine {
    /// Make (if needed) and open `<home>/machine/`, with its in-process router.
    pub async fn open(
        home: &Path,
        read: fn(&Path) -> DesktopConfig,
    ) -> Result<Arc<Machine>, String> {
        let paths = Paths::of(home);
        let d = read(home);
        ensure(&paths, home, &d.bridge_config).await?;
        let snap = orca_router::config::Snapshot::load(&paths.json)?;
        let st = orca_router::state::AppState::new(snap, Some(paths.json.clone()));
        let started = orca_router::control::startup::start(&st, &paths.main).await?;
        tracing::info!("this machine: {}", started.message);
        let app = orca_router::server::app(st, orca_router::server::Mode::Gateway);
        let gpus = orca_bridge::detect::gpus().await;
        let m = Arc::new(Machine {
            home: home.to_path_buf(),
            paths,
            app,
            view: Default::default(),
            running: Mutex::new(None),
            bridge: Mutex::new(None),
            read,
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .build()
                .map_err(|e| e.to_string())?,
            gpus,
            last_error: Mutex::new(String::new()),
            save: tokio::sync::Mutex::new(()),
        });
        m.tick().await;
        Ok(m)
    }

    /// Every few seconds: the config, the supervisor, Bridge mode, the idle reaper.
    pub fn spawn_ticker(self: &Arc<Self>) {
        let me = self.clone();
        tokio::spawn(async move {
            let mut n = 0u64;
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                me.tick().await;
                n += 1;
                if n.is_multiple_of(3)
                    && let Some(s) = me.sup()
                {
                    s.reap_idle().await;
                }
            }
        });
    }

    fn sup(&self) -> Option<Arc<Supervisor>> {
        self.running.lock().unwrap().as_ref().map(|r| r.sup.clone())
    }

    fn config(&self) -> Result<ControlConfig, String> {
        orca_router::control::load::load(&self.paths.main)
            .map(|l| l.config)
            .map_err(|i| {
                i.iter()
                    .map(|x| x.to_string())
                    .collect::<Vec<_>>()
                    .join("; ")
            })
    }

    /// Reload what changed and apply it.
    pub async fn tick(self: &Arc<Self>) {
        let d = (self.read)(&self.home);
        let bridged = d.bridge_mode;
        let c = match self.config() {
            Ok(c) => c,
            Err(e) => {
                *self.last_error.lock().unwrap() = e;
                self.write_status(&d);
                return;
            }
        };
        let manual = Manual::load(&self.paths.engines).unwrap_or_default();
        let mut share = Share::load(&self.paths.share);
        if share.reconcile(&c, bridged) {
            let _ = write_atomic(
                &self.paths.share,
                &serde_json::to_string_pretty(&share).unwrap_or_default(),
            );
        }
        let rev = orca_bridge::proto::token_hash(
            &(std::fs::read_to_string(&self.paths.main).unwrap_or_default() + &manual.rev()),
        )[..12]
            .to_string();
        {
            let mut v = self.view.write().unwrap();
            v.unshared_files = unshared_files(&c, &share);
            v.models = shared_models(&c, &share);
            v.share = share;
            v.remote = bridged && d.bridge_remote_config;
            v.rev = rev;
        }
        // the supervisor: rebuilt when what it runs changed
        let (mut bc, problems) = to_bridge(&c, &manual, &self.paths.state);
        if let Err(e) = orca_bridge::package::fill(&mut bc) {
            *self.last_error.lock().unwrap() = e;
        } else {
            self.last_error.lock().unwrap().clear();
        }
        let sig =
            serde_json::to_string(&(&bc.cards, &bc.engines, &bc.bridge.model_dir)).unwrap_or_default();
        let old = {
            let mut g = self.running.lock().unwrap();
            if g.as_ref().is_some_and(|r| r.sig == sig) {
                if let Some(r) = g.as_mut() {
                    r.problems = problems;
                }
                None
            } else {
                let log: orca_bridge::Log =
                    Arc::new(|line: &str| tracing::info!("this machine: {line}"));
                let sup = Supervisor::new(bc.clone(), self.gpus.clone(), log);
                let p = sup.clone();
                tokio::spawn(async move { p.prepare_all().await });
                g.replace(Running {
                    sig: sig.clone(),
                    sup,
                    problems,
                })
                .map(|r| r.sup)
            }
        };
        if let Some(o) = old {
            o.stop_all().await;
        }
        self.bridge_tick(&d, &bc).await;
        self.write_status(&d);
    }

    async fn bridge_tick(self: &Arc<Self>, d: &DesktopConfig, bc: &BridgeConfig) {
        // Advanced: an engines file of its own runs the old way (its own supervisor)
        let want: Option<Result<(BridgeConfig, bool), String>> = if !d.bridge_mode {
            None
        } else if !d.bridge_config.trim().is_empty() {
            crate::bridge_mode::wanted(d, &self.home).map(|r| r.map(|c| (c, true)))
        } else {
            let mut c = bc.clone();
            c.bridge.router = d.bridge_router.trim().to_string();
            if !d.bridge_id.trim().is_empty() {
                c.bridge.id = d.bridge_id.trim().to_string();
            }
            if !d.bridge_token.trim().is_empty() {
                c.token_value = Some(d.bridge_token.trim().to_string());
            }
            Some(c.check().map(|_| (c, false)))
        };
        let sup = self.sup();
        let mut g = self.bridge.lock().unwrap();
        match want {
            None => {
                if let Some(b) = g.take() {
                    tracing::info!("bridge mode: off; disconnecting");
                    b.handle.stop();
                }
            }
            Some(Err(e)) => {
                *self.last_error.lock().unwrap() = e;
                if let Some(b) = g.take() {
                    b.handle.stop();
                }
            }
            Some(Ok((c, legacy))) => {
                let sup_id = sup
                    .as_ref()
                    .map(|s| format!("{:p}", Arc::as_ptr(s)))
                    .unwrap_or_default();
                let sig = format!(
                    "{}|{}|{}|{}|{legacy}|{}",
                    c.bridge.router,
                    c.bridge.id,
                    c.token_value
                        .as_deref()
                        .map(orca_bridge::proto::token_hash)
                        .unwrap_or_default(),
                    if legacy { String::new() } else { sup_id },
                    if legacy {
                        serde_json::to_string(&c.engines).unwrap_or_default()
                    } else {
                        String::new()
                    },
                );
                let seen = {
                    let v = self.view.read().unwrap();
                    format!("{}|{}|{:?}", v.rev, v.remote, v.share)
                };
                if g.as_ref().is_some_and(|b| b.sig == sig) {
                    if let Some(b) = g.as_mut()
                        && b.seen != seen
                    {
                        b.seen = seen;
                        b.handle.refresh();
                    }
                    return;
                }
                if let Some(b) = g.take() {
                    b.handle.stop();
                }
                tracing::info!("bridge mode: on; dialling {}", c.bridge.router);
                let log: orca_bridge::Log = Arc::new(|line: &str| tracing::info!("bridge: {line}"));
                let opts = if legacy {
                    Options::default()
                } else {
                    self.bridge_options(sup)
                };
                let handle =
                    client::spawn_with(c, env!("CARGO_PKG_VERSION"), log, Timing::default(), opts);
                *g = Some(BridgeRun { sig, handle, seen });
            }
        }
    }

    fn bridge_options(self: &Arc<Self>, sup: Option<Arc<Supervisor>>) -> Options {
        let v1 = self.view.clone();
        let v2 = self.view.clone();
        Options {
            sup,
            shape: Some(Arc::new(move |d: &mut Descriptor| {
                let v = v1.read().unwrap();
                orca_bridge::detect::share_only(d, &|e| v.share.engine(e), &v.unshared_files);
                d.models = v.models.clone();
                d.remote_config = v.remote;
                d.config_rev = Some(v.rev.clone());
            })),
            allow: Some(Arc::new(move |e: &str| v2.read().unwrap().share.engine(e))),
            remote: Some(Arc::new(RemoteHandler {
                m: Arc::downgrade(self),
            })),
        }
    }

    fn write_status(&self, d: &DesktopConfig) {
        let _ = write_atomic(
            &self.paths.status,
            &serde_json::to_string_pretty(&self.status_with(d)).unwrap_or_default(),
        );
    }

    pub fn status(&self) -> Value {
        self.status_with(&(self.read)(&self.home))
    }

    fn status_with(&self, d: &DesktopConfig) -> Value {
        let c = self.config().ok();
        let (id, title) = c
            .as_ref()
            .and_then(|c| the_server(c).map(|(k, s)| (k.clone(), s.title.clone())))
            .unwrap_or_default();
        let r = self.running.lock().unwrap();
        let engines = r.as_ref().map(|r| r.sup.status()).unwrap_or(json!({}));
        let problems = r.as_ref().map(|r| r.problems.clone()).unwrap_or_default();
        drop(r);
        // one lock, taken and let go here (locking it twice in one expression deadlocks)
        let last_error = Some(self.last_error.lock().unwrap().clone()).filter(|e| !e.is_empty());
        let bridge = self.bridge.lock().unwrap().as_ref().map(|b| {
            let s = b.handle.status();
            json!({"connected": s.connected, "server": s.server, "enabled": s.enabled,
                   "router": s.router, "lastError": s.last_error, "connects": s.connects})
        });
        let v = self.view.read().unwrap();
        json!({
            "server": id, "title": title, "hostname": hostname(),
            "bridgeMode": d.bridge_mode, "remoteConfig": d.bridge_mode && d.bridge_remote_config,
            "bridgeRouter": d.bridge_router, "advancedFile": d.bridge_config,
            "bridge": bridge,
            "engines": engines,
            "problems": problems,
            "error": last_error,
            "share": v.share, "rev": v.rev,
            "pending": self.pending(),
            "remoteChanges": self.remote_changes(20),
            "at": now(),
        })
    }

    // ── the in-process admin API ──────────────────────────────────────────────────────────

    /// A call to the machine's router admin API (`rest` after `/admin/api/`).
    pub async fn admin(
        &self,
        method: Method,
        rest: &str,
        if_match: Option<&str>,
        body: Bytes,
    ) -> (u16, Value, HeaderMap) {
        let plain = rest.split('?').next().unwrap_or("").trim_matches('/');
        let rev_of_disk = method == Method::GET && matches!(plain, "config" | "config/json");
        let mut b = Request::builder()
            .method(method)
            .uri(format!("/admin/api/{}", rest.trim_start_matches('/')));
        if !body.is_empty() {
            b = b.header(header::CONTENT_TYPE, "application/json");
        }
        if let Some(r) = if_match {
            b = b.header(header::IF_MATCH, format!("\"{}\"", r.trim_matches('"')));
        }
        let Ok(mut req) = b.body(Body::from(body)) else {
            return (400, json!({"error": "bad request"}), HeaderMap::new());
        };
        // a loopback peer: the router's own "admin on this host" (no socket: nothing else can
        // reach this app)
        req.extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 0))));
        let mut app = self.app.clone();
        let resp = match app.call(req).await {
            Ok(r) => r,
            Err(e) => match e {},
        };
        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        let bytes = axum::body::to_bytes(resp.into_body(), 64 << 20)
            .await
            .unwrap_or_default();
        let mut v = serde_json::from_slice(&bytes).unwrap_or_else(|_| {
            if bytes.is_empty() {
                Value::Null
            } else {
                Value::String(String::from_utf8_lossy(&bytes).into_owned())
            }
        });
        let mut headers = headers;
        if rev_of_disk
            && status == 200
            && let Ok(text) = std::fs::read_to_string(&self.paths.main)
        {
            // what a save is checked against, also right after a save
            let rev = disk_rev(&self.paths.main, &text);
            v["rev"] = json!(rev);
            if let Ok(h) = header::HeaderValue::from_str(&format!("\"{rev}\"")) {
                headers.insert(header::ETAG, h);
            }
        }
        (status, v, headers)
    }

    /// `router.toml` as it is on disk, and its revision (the one a save is checked against;
    /// the router's own GET may still report the last load's for a moment after a save).
    async fn config_doc(&self) -> Result<(String, String), String> {
        let text = std::fs::read_to_string(&self.paths.main)
            .map_err(|e| format!("{}: {e}", self.paths.main.display()))?;
        let rev = disk_rev(&self.paths.main, &text);
        Ok((text, rev))
    }

    /// Save `router.toml` through the router (check, backup, atomic write). `(status, body)`.
    async fn save_config(&self, text: &str, rev: &str, reason: &str) -> (u16, Value) {
        let body = json!({"text": text, "reason": reason}).to_string();
        let (s, v, _) = self
            .admin(Method::PUT, "config", Some(rev), Bytes::from(body))
            .await;
        (s, v)
    }

    /// The manual engines, and their revision.
    pub fn engines_doc(&self) -> Value {
        let m = Manual::load(&self.paths.engines);
        match m {
            Ok(m) => json!({"rev": m.rev(), "engines": m.engines}),
            Err(e) => json!({"error": e}),
        }
    }

    /// Replace the manual engines (revision checked) and mirror them into `router.toml`.
    pub async fn save_engines(&self, engines: Value, rev: &str, who: &str) -> (u16, Value) {
        let _g = self.save.lock().await;
        let old = match Manual::load(&self.paths.engines) {
            Ok(m) => m,
            Err(e) => return (500, json!({"error": e})),
        };
        if old.rev() != rev {
            return (
                412,
                json!({"error": "the engines changed since you read them", "currentRev": old.rev()}),
            );
        }
        let new: Manual = match serde_json::from_value(json!({"engines": engines})) {
            Ok(m) => m,
            Err(e) => return (422, json!({"error": format!("engines: {e}")})),
        };
        for (id, e) in &new.engines {
            if id.is_empty()
                || !id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                return (
                    422,
                    json!({"error": format!("engine id {id:?}: a-z, 0-9, - and _")}),
                );
            }
            if e.port.is_none() && e.ports.is_empty() && e.lifecycle != "external" {
                return (422, json!({"error": format!("engines.{id}: a port")}));
            }
        }
        let (text, crev) = match self.config_doc().await {
            Ok(x) => x,
            Err(e) => return (500, json!({"error": e})),
        };
        let server = toml::from_str::<ControlConfig>(&text)
            .ok()
            .and_then(|c| the_server(&c).map(|(k, _)| k.clone()))
            .unwrap_or_default();
        let mirrored = match mirror_text(&text, &server, &new) {
            Ok(t) => t,
            Err(e) => return (422, json!({"error": e})),
        };
        let backup = std::fs::read_to_string(&self.paths.engines).unwrap_or_default();
        if let Err(e) = write_atomic(
            &self.paths.engines,
            &toml::to_string(&new).unwrap_or_default(),
        ) {
            return (500, json!({"error": e}));
        }
        if mirrored != text {
            let (s, v) = self
                .save_config(&mirrored, &crev, &format!("engines ({who})"))
                .await;
            if s != 200 {
                let _ = write_atomic(&self.paths.engines, &backup);
                return (s, v);
            }
        }
        (200, json!({"ok": true, "rev": new.rev()}))
    }

    pub fn save_share(&self, v: &Value) -> (u16, Value) {
        let mut s = Share::load(&self.paths.share);
        for (k, map) in [("engines", &mut s.engines), ("models", &mut s.models)] {
            if let Some(o) = v.get(k).and_then(Value::as_object) {
                for (id, on) in o {
                    if let Some(b) = on.as_bool() {
                        map.insert(id.clone(), b);
                    }
                }
            }
        }
        match write_atomic(
            &self.paths.share,
            &serde_json::to_string_pretty(&s).unwrap_or_default(),
        ) {
            Ok(()) => {
                self.view.write().unwrap().share = s.clone();
                (200, json!(s))
            }
            Err(e) => (500, json!({"error": e})),
        }
    }

    // ── pending approvals and the remote log ──────────────────────────────────────────────

    pub fn pending(&self) -> Vec<Pending> {
        std::fs::read_to_string(&self.paths.pending)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    fn set_pending(&self, p: &[Pending]) {
        let _ = write_atomic(
            &self.paths.pending,
            &serde_json::to_string_pretty(p).unwrap_or_default(),
        );
    }

    fn park(&self, who: &str, op: &str, args: &Value, reasons: Vec<String>) -> Value {
        let p = Pending {
            id: new_id(),
            at: now(),
            who: who.to_string(),
            op: op.to_string(),
            args: args.clone(),
            reasons: reasons.clone(),
        };
        let mut all = self.pending();
        all.push(p.clone());
        all.truncate(50);
        self.set_pending(&all);
        tracing::info!("this machine: a change from {who} waits for approval: {reasons:?}");
        json!({"status": 202, "body": {"pending": p.id, "reasons": reasons,
            "message": "This needs the user's approval on the machine itself; it waits there."}})
    }

    pub fn remote_changes(&self, n: usize) -> Vec<RemoteChange> {
        let text = std::fs::read_to_string(&self.paths.remote_log).unwrap_or_default();
        let mut v: Vec<RemoteChange> = text
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        // later lines (an undo) win over earlier ones with the same id
        let mut seen = BTreeSet::new();
        v.reverse();
        v.retain(|c| seen.insert(c.id.clone()));
        v.truncate(n);
        v
    }

    fn log_change(&self, c: &RemoteChange) {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.paths.remote_log)
        {
            let _ = writeln!(f, "{}", serde_json::to_string(c).unwrap_or_default());
        }
    }

    fn backup(&self, what: &str, text: &str) -> Option<String> {
        let name = format!("{}-{}-{what}", now(), new_id());
        let p = self.paths.backups.join(&name);
        write_atomic(&p, text).ok().map(|_| name)
    }

    /// Put a remote change back (the user, here).
    pub async fn undo(&self, id: &str) -> (u16, Value) {
        let Some(mut c) = self.remote_changes(200).into_iter().find(|c| c.id == id) else {
            return (404, json!({"error": "no such change"}));
        };
        if c.undone {
            return (409, json!({"error": "already undone"}));
        }
        let _g = self.save.lock().await;
        if let Some(b) = &c.backup {
            let Ok(text) = std::fs::read_to_string(self.paths.backups.join(b)) else {
                return (410, json!({"error": "its backup is gone"}));
            };
            let rev = match self.config_doc().await {
                Ok((_, r)) => r,
                Err(e) => return (500, json!({"error": e})),
            };
            let (s, v) = self
                .save_config(&text, &rev, &format!("undo the change by {}", c.who))
                .await;
            if s != 200 {
                return (s, v);
            }
        }
        if let Some(b) = &c.engines_backup
            && let Ok(text) = std::fs::read_to_string(self.paths.backups.join(b))
            && let Err(e) = write_atomic(&self.paths.engines, &text)
        {
            return (500, json!({"error": e}));
        }
        c.undone = true;
        self.log_change(&c);
        (200, json!({"ok": true}))
    }

    /// Approve (run as the user) or reject a pending remote change.
    pub async fn decide(self: &Arc<Self>, id: &str, approve: bool) -> (u16, Value) {
        let mut all = self.pending();
        let Some(i) = all.iter().position(|p| p.id == id) else {
            return (404, json!({"error": "no such pending change"}));
        };
        let p = all.remove(i);
        self.set_pending(&all);
        if !approve {
            return (200, json!({"ok": true, "rejected": true}));
        }
        let v = self.remote(&p.op, &p.args, true).await;
        let status = v["status"].as_u64().unwrap_or(200) as u16;
        (status, v["body"].clone())
    }

    // ── remote config (MACHINE.md 8) ──────────────────────────────────────────────────────

    /// A router's call through the bridge: `{status, body}`. `approved`: the user approved it.
    pub async fn remote(self: &Arc<Self>, op: &str, args: &Value, approved: bool) -> Value {
        let d = (self.read)(&self.home);
        if !approved && !(d.bridge_mode && d.bridge_remote_config) {
            return json!({"status": 403, "body": {"error": "Read-only: this machine doesn't allow remote config", "readOnly": true}});
        }
        let who = args["who"].as_str().unwrap_or("a router admin").to_string();
        match op {
            "config.get" => {
                let (s, mut v, _) = self.admin(Method::GET, "config", None, Bytes::new()).await;
                if s == 200
                    && let Ok((text, rev)) = self.config_doc().await
                {
                    v["text"] = json!(text);
                    v["rev"] = json!(rev);
                }
                json!({"status": s, "body": v})
            }
            "config.apply" => self.remote_apply(args, &who, approved).await,
            "machine.api" => self.remote_api(args, &who, approved).await,
            _ => json!({"status": 404, "body": {"error": format!("no `{op}` here")}}),
        }
    }

    async fn remote_apply(self: &Arc<Self>, args: &Value, who: &str, approved: bool) -> Value {
        let text = args["text"].as_str().unwrap_or("").to_string();
        let rev = args["rev"].as_str().unwrap_or("").to_string();
        let reason = args["reason"].as_str().unwrap_or("").to_string();
        let _g = self.save.lock().await;
        let (cur, cur_rev) = match self.config_doc().await {
            Ok(x) => x,
            Err(e) => return json!({"status": 500, "body": {"error": e}}),
        };
        if rev != cur_rev {
            return json!({"status": 412, "body": {"error": "router.toml changed since it was read", "currentRev": cur_rev}});
        }
        let (old, new) = match (
            toml::from_str::<ControlConfig>(&cur),
            toml::from_str::<ControlConfig>(&text),
        ) {
            (Ok(a), Ok(b)) => (a, b),
            (_, Err(e)) => {
                return json!({"status": 422, "body": {"error": format!("router.toml: {e}")}});
            }
            (Err(e), _) => return json!({"status": 500, "body": {"error": e.to_string()}}),
        };
        match remote_config_check(&old, &new) {
            Err(e) => return json!({"status": 403, "body": {"error": e}}),
            Ok(need) if !need.is_empty() && !approved => {
                drop(_g);
                return self.park(who, "config.apply", args, need);
            }
            Ok(_) => {}
        }
        let backup = self.backup("router.toml", &cur);
        let (s, v) = self
            .save_config(&text, &rev, &format!("{who}: {reason}"))
            .await;
        if s == 200 {
            self.log_change(&RemoteChange {
                id: new_id(),
                at: now(),
                who: who.to_string(),
                what: if reason.is_empty() {
                    "changed router.toml".into()
                } else {
                    reason
                },
                backup,
                engines_backup: None,
                undone: false,
            });
        }
        json!({"status": s, "body": v})
    }

    async fn remote_api(self: &Arc<Self>, args: &Value, who: &str, approved: bool) -> Value {
        let method = args["method"]
            .as_str()
            .unwrap_or("GET")
            .to_ascii_uppercase();
        let path = args["path"]
            .as_str()
            .unwrap_or("")
            .trim_matches('/')
            .to_string();
        let query = args["query"].as_str().unwrap_or("").to_string();
        let body = args.get("body").cloned().unwrap_or(Value::Null);
        let if_match = args["ifMatch"].as_str().map(String::from);
        if path.split('/').any(|x| x == ".." || x == ".") {
            return json!({"status": 400, "body": {"error": "bad path"}});
        }
        let model_dir = self
            .config()
            .ok()
            .and_then(|c| the_server(&c).and_then(|(_, s)| s.model_dir.clone()))
            .unwrap_or_default();
        let qv = |k: &str| {
            query.split('&').find_map(|kv| {
                let (a, b) = kv.split_once('=')?;
                (a == k).then(|| percent_decode(b))
            })
        };
        let refuse = |e: &str| json!({"status": 403, "body": {"error": e}});
        // the machine's own pieces
        match (method.as_str(), path.as_str()) {
            ("GET", "machine/status") => return json!({"status": 200, "body": self.status()}),
            ("GET", "machine/engines") => return json!({"status": 200, "body": self.engines_doc()}),
            ("PUT", "machine/engines") => {
                let old = Manual::load(&self.paths.engines).unwrap_or_default();
                let new: Manual =
                    serde_json::from_value(json!({"engines": body["engines"]})).unwrap_or_default();
                let need = manual_check(&old, &new, Some(&model_dir));
                if !need.is_empty() && !approved {
                    return self.park(who, "machine.api", args, need);
                }
                let eb = self.backup(
                    "engines.toml",
                    &std::fs::read_to_string(&self.paths.engines).unwrap_or_default(),
                );
                let cb = self
                    .config_doc()
                    .await
                    .ok()
                    .and_then(|(t, _)| self.backup("router.toml", &t));
                let (s, v) = self
                    .save_engines(
                        body["engines"].clone(),
                        body["rev"].as_str().unwrap_or(""),
                        who,
                    )
                    .await;
                if s == 200 {
                    self.log_change(&RemoteChange {
                        id: new_id(),
                        at: now(),
                        who: who.into(),
                        what: "changed the engines".into(),
                        backup: cb,
                        engines_backup: eb,
                        undone: false,
                    });
                }
                return json!({"status": s, "body": v});
            }
            ("PUT", "machine/share") => {
                let (s, v) = self.save_share(&body);
                if s == 200 {
                    self.log_change(&RemoteChange {
                        id: new_id(),
                        at: now(),
                        who: who.into(),
                        what: "changed what is shared".into(),
                        backup: None,
                        engines_backup: None,
                        undone: false,
                    });
                }
                return json!({"status": s, "body": v});
            }
            _ => {}
        }
        // the router admin API: what the forms need, nothing else
        let allowed = match method.as_str() {
            "GET" => {
                path == "config/json"
                    || path == "config"
                    || path == "catalog"
                    || path == "downloads"
                    || path.starts_with("hf/model")
                    || path.starts_with("hf/search")
                    || path == "inventory"
                    || path == "inventory/browse"
                    || path == "inventory/stat"
                    || path.starts_with("engine-store/")
            }
            "POST" => {
                path == "config/edit"
                    || path == "config/check"
                    || path == "downloads"
                    || path.starts_with("downloads/")
                    || path == "inventory/detect"
                    || path == "inventory/gguf"
                    || path == "inventory/mkdir"
                    || path == "inventory/place"
                    || path.starts_with("engine-store/")
            }
            "PUT" => path.starts_with("engine-store/my/"),
            "DELETE" => path.starts_with("downloads/") || path.starts_with("engine-store/my/"),
            _ => false,
        };
        if !allowed {
            return refuse(&format!(
                "a router may not call {method} {path} on this machine"
            ));
        }
        if path == "engine-store/my" || path.ends_with("/export") {
            // fine: My packages are metadata
        }
        // browsing and downloads stay inside the model folder
        if path.starts_with("inventory/browse") || path.starts_with("inventory/stat") {
            let p = qv("path").unwrap_or_default();
            if !p.is_empty() && !inside(&p, &model_dir) {
                return refuse("a router may browse only inside this machine's model folder");
            }
        }
        if path == "inventory/mkdir" || path == "inventory/gguf" {
            let p = body["path"].as_str().unwrap_or("");
            if !inside(p, &model_dir) {
                return refuse("outside this machine's model folder");
            }
        }
        if method == "POST"
            && path == "downloads"
            && let Some(dir) = body["dir"].as_str()
            && !inside(dir, &model_dir)
        {
            return refuse("downloads go inside this machine's model folder");
        }
        // what needs the user: approvals, new stores, test runs (a container started)
        if !approved && method == "POST" {
            let mut need = Vec::new();
            if (path == "engine-store/install" || path == "engine-store/update")
                && body["approvals"].as_object().is_some_and(|o| {
                    o.values().any(|v| {
                        v.as_bool() == Some(true)
                            || v.is_object()
                            || v.as_array().is_some_and(|a| !a.is_empty())
                    })
                })
            {
                need.push("an engine package that needs approval".to_string());
            }
            if path == "engine-store/stores" {
                need.push(format!(
                    "a new engine store {}",
                    body["url"].as_str().unwrap_or("")
                ));
            }
            if path == "engine-store/wizard/test-run" {
                need.push(format!(
                    "a test container of {}",
                    body["image"].as_str().unwrap_or("")
                ));
            }
            if !need.is_empty() {
                return self.park(who, "machine.api", args, need);
            }
        }
        let before = if method != "GET" {
            self.config_doc().await.ok()
        } else {
            None
        };
        let full = if query.is_empty() {
            path.clone()
        } else {
            format!("{path}?{query}")
        };
        let m = Method::from_bytes(method.as_bytes()).unwrap_or(Method::GET);
        let raw = if body.is_null() {
            Bytes::new()
        } else {
            Bytes::from(body.to_string())
        };
        let (s, v, _) = self.admin(m, &full, if_match.as_deref(), raw).await;
        if method != "GET"
            && path != "config/edit"
            && path != "config/check"
            && (200..300).contains(&s)
        {
            let changed = match (&before, self.config_doc().await.ok()) {
                (Some((a, _)), Some((b, _))) => *a != b,
                _ => false,
            };
            self.log_change(&RemoteChange {
                id: new_id(),
                at: now(),
                who: who.into(),
                what: format!("{method} {path}"),
                backup: if changed {
                    before.and_then(|(t, _)| self.backup("router.toml", &t))
                } else {
                    None
                },
                engines_backup: None,
                undone: false,
            });
        }
        json!({"status": s, "body": v})
    }

    // ── local serving (MACHINE.md 2.5) ────────────────────────────────────────────────────

    /// The models this machine can serve (OpenAI's list shape).
    pub fn models(&self) -> Value {
        let data: Vec<Value> = self
            .config()
            .map(|c| {
                c.models
                    .iter()
                    .filter(|(_, m)| !m.is_component())
                    .map(|(id, m)| {
                        json!({"id": id, "object": "model", "owned_by": "this-machine",
                               "kind": m.kind.unwrap_or_default(),
                               "title": (!m.title.is_empty()).then(|| m.title.clone())})
                    })
                    .collect()
            })
            .unwrap_or_default();
        json!({"object": "list", "data": data})
    }

    /// Where a request for `model` goes: the router's placement over this one machine.
    pub fn place(&self, model: &str) -> Result<(String, String), String> {
        let c = self.config()?;
        let id = if c.models.contains_key(model) {
            model.to_string()
        } else {
            c.models
                .iter()
                .find(|(_, m)| m.aliases.iter().any(|a| a == model))
                .map(|(k, _)| k.clone())
                .ok_or_else(|| format!("no model `{model}` on this machine"))?
        };
        // every managed card here takes on-demand starts (the router's CPU cards never switch
        // layouts, which is why its schema keeps them off)
        let mut c = c;
        for k in c.cards.values_mut() {
            if k.managed {
                k.on_demand = true;
            }
        }
        let opts = orca_router::control::inventory::place(
            &c,
            &id,
            &orca_router::control::inventory::Pin::default(),
        );
        opts.iter()
            .find(|o| o.ok && !o.engine.is_empty())
            .map(|o| (o.engine.clone(), o.card.clone()))
            .ok_or_else(|| {
                let why: Vec<String> = opts
                    .iter()
                    .map(|o| format!("{}: {}", o.card, o.why.join(", ")))
                    .collect();
                format!("no card here can run `{model}` ({})", why.join("; "))
            })
    }

    /// `/api/machine/v1/<rest>`: an OpenAI-compatible call placed on this machine.
    pub async fn v1(
        &self,
        method: Method,
        rest: &str,
        headers: &HeaderMap,
        body: Bytes,
    ) -> Response {
        if method == Method::GET && (rest == "models" || rest.is_empty()) {
            return json_response(200, &self.models());
        }
        let model = serde_json::from_slice::<Value>(&body)
            .ok()
            .and_then(|v| v["model"].as_str().map(String::from))
            .unwrap_or_default();
        let (engine, card) = match self.place(&model) {
            Ok(x) => x,
            Err(e) => {
                return json_response(
                    404,
                    &json!({"error": {"message": e, "type": "model_not_found"}}),
                );
            }
        };
        let Some(sup) = self.sup() else {
            return json_response(
                503,
                &json!({"error": {"message": "the engines are not ready"}}),
            );
        };
        let lease = match sup.acquire(&engine, &card).await {
            Ok(l) => l,
            Err(e) => {
                return json_response(
                    503,
                    &json!({"error": {"message": e, "type": "engine_unavailable"}}),
                );
            }
        };
        let Some(port) = sup.ports().get(&engine).and_then(|m| m.get(&card)).copied() else {
            return json_response(
                503,
                &json!({"error": {"message": format!("{engine} has no port on {card}")}}),
            );
        };
        let url = format!(
            "http://127.0.0.1:{port}/v1/{}",
            rest.trim_start_matches('/')
        );
        let mut rb = self
            .http
            .request(method, &url)
            .timeout(Duration::from_secs(1800))
            .body(body);
        for k in [header::CONTENT_TYPE, header::ACCEPT] {
            if let Some(v) = headers.get(&k) {
                rb = rb.header(k, v);
            }
        }
        match rb.send().await {
            Ok(r) => {
                let status = r.status().as_u16();
                let ct = r.headers().get(header::CONTENT_TYPE).cloned();
                let stream = futures_util::StreamExt::map(r.bytes_stream(), move |c| {
                    let _keep = &lease; // the engine counts as busy until the body is done
                    c.map_err(std::io::Error::other)
                });
                let mut resp = Response::new(Body::from_stream(stream));
                *resp.status_mut() =
                    StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
                if let Some(ct) = ct {
                    resp.headers_mut().insert(header::CONTENT_TYPE, ct);
                }
                resp.headers_mut().insert(
                    "x-abhoth-engine",
                    header::HeaderValue::from_str(&format!("{engine}@{card}"))
                        .unwrap_or(header::HeaderValue::from_static("engine")),
                );
                resp
            }
            Err(e) => json_response(
                502,
                &json!({"error": {"message": format!("{engine} on {card}: {e}")}}),
            ),
        }
    }

    /// The web host's handler for `/api/admin/machine/*` and `/api/machine/*` (admins only;
    /// the host checks that before calling).
    pub async fn handle(self: &Arc<Self>, req: Request<Body>) -> Response {
        let method = req.method().clone();
        let path_q = req
            .uri()
            .path_and_query()
            .map(|p| p.as_str().to_string())
            .unwrap_or_default();
        let headers = req.headers().clone();
        let body = axum::body::to_bytes(req.into_body(), 256 << 20)
            .await
            .unwrap_or_default();
        let if_match = headers
            .get(header::IF_MATCH)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.trim_matches('"').to_string());
        if let Some(rest) = path_q.strip_prefix("/api/admin/machine/") {
            let (s, v, h) = self.admin(method, rest, if_match.as_deref(), body).await;
            let mut r = json_response(s, &v);
            if let Some(e) = h.get(header::ETAG) {
                r.headers_mut().insert(header::ETAG, e.clone());
            }
            return r;
        }
        let rest = path_q
            .strip_prefix("/api/machine/")
            .unwrap_or("")
            .to_string();
        let path = rest.split('?').next().unwrap_or("").to_string();
        let jbody = || serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null);
        match (method.clone(), path.as_str()) {
            (Method::GET, "status") => json_response(200, &self.status()),
            (Method::GET, "engines") => json_response(200, &self.engines_doc()),
            (Method::PUT, "engines") => {
                let b = jbody();
                let (s, v) = self
                    .save_engines(
                        b["engines"].clone(),
                        b["rev"].as_str().unwrap_or(""),
                        "this machine",
                    )
                    .await;
                self.tick().await;
                json_response(s, &v)
            }
            (Method::PUT, "share") => {
                let (s, v) = self.save_share(&jbody());
                self.tick().await;
                json_response(s, &v)
            }
            (Method::POST, p) if p.starts_with("pending/") => {
                let id = p.trim_start_matches("pending/");
                let approve = jbody()["approve"].as_bool().unwrap_or(false);
                let (s, v) = self.decide(id, approve).await;
                self.tick().await;
                json_response(s, &v)
            }
            (Method::POST, p) if p.starts_with("changes/") && p.ends_with("/undo") => {
                let id = p.trim_start_matches("changes/").trim_end_matches("/undo");
                let (s, v) = self.undo(id).await;
                self.tick().await;
                json_response(s, &v)
            }
            (Method::POST, "engines/start" | "engines/stop") => {
                let b = jbody();
                let (e, c) = (
                    b["engine"].as_str().unwrap_or(""),
                    b["card"].as_str().unwrap_or(""),
                );
                let Some(sup) = self.sup() else {
                    return json_response(503, &json!({"error": "not ready"}));
                };
                let r = if path.ends_with("start") {
                    sup.start(e, c).await
                } else {
                    sup.stop(e, c).await
                };
                match r {
                    Ok(()) => json_response(200, &json!({"ok": true})),
                    Err(e) => json_response(502, &json!({"error": e})),
                }
            }
            (_, p) if p == "v1" || p.starts_with("v1/") => {
                let r = rest
                    .strip_prefix("v1")
                    .unwrap_or("")
                    .trim_start_matches('/');
                self.v1(method, r, &headers, body).await
            }
            _ => json_response(404, &json!({"error": "no such machine call"})),
        }
    }
}

/// `desktop` from `<home>/config.json` (defaults when missing or unreadable; never written):
/// the settings this machine follows when no desktop window is there (`--hostui-machine`).
pub fn desktop_config(home: &Path) -> DesktopConfig {
    let Ok(bytes) = std::fs::read(home.join("config.json")) else {
        return Default::default();
    };
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&bytes);
    serde_json::from_slice::<orca_core::config::OrcaConfig>(bytes)
        .map(|c| c.desktop)
        .unwrap_or_default()
}

/// The revision the router checks a save against (`If-Match`).
pub fn disk_rev(main: &Path, text: &str) -> String {
    orca_router::control::load::load_with(main, text.to_string())
        .map(|l| l.rev)
        .unwrap_or_default()
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                let h = std::str::from_utf8(&b[i + 1..i + 3])
                    .ok()
                    .and_then(|h| u8::from_str_radix(h, 16).ok());
                match h {
                    Some(x) => {
                        out.push(x);
                        i += 3;
                    }
                    None => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The bridge's [`Remote`]: the router's config calls, answered by this machine.
struct RemoteHandler {
    m: std::sync::Weak<Machine>,
}

#[async_trait::async_trait]
impl Remote for RemoteHandler {
    async fn call(&self, op: &str, args: &Value) -> Option<Result<Value, String>> {
        if !matches!(op, "config.get" | "config.apply" | "machine.api") {
            return None;
        }
        let m = self.m.upgrade()?;
        let v = m.remote(op, args, false).await;
        // Apply the change after the answer is on its way: a change to the cards or engines
        // rebuilds the supervisor and so reconnects this bridge, which would lose the reply.
        if op != "config.get" {
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(500)).await;
                m.tick().await;
            });
        }
        Some(Ok(v))
    }
}

// ── the desktop app's side ───────────────────────────────────────────────────────────────────

/// The machine on a runtime of its own (the window's host may not be up yet), and a handler
/// for the web host that runs each call there.
pub fn spawn(home: PathBuf, read: fn(&Path) -> DesktopConfig) -> orca_webui::server::MachineFn {
    let (tx, rx) = std::sync::mpsc::channel::<(tokio::runtime::Handle, Arc<Machine>)>();
    let cell: Arc<std::sync::OnceLock<(tokio::runtime::Handle, Arc<Machine>)>> = Default::default();
    let spawned = std::thread::Builder::new()
        .name("abhoth-machine".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    tracing::warn!("this machine: no runtime: {e}");
                    return;
                }
            };
            rt.block_on(async move {
                match Machine::open(&home, read).await {
                    Ok(m) => {
                        m.spawn_ticker();
                        let _ = tx.send((tokio::runtime::Handle::current(), m));
                        std::future::pending::<()>().await;
                    }
                    Err(e) => tracing::warn!("this machine: {e}"),
                }
            });
        });
    if let Err(e) = spawned {
        tracing::warn!("this machine: no thread: {e}");
    }
    let rx = Arc::new(Mutex::new(Some(rx)));
    Arc::new(move |req: Request<Body>| {
        let cell = cell.clone();
        let rx = rx.clone();
        Box::pin(async move {
            if cell.get().is_none() {
                let r = rx.lock().unwrap().take();
                if let Some(r) = r {
                    let got = tokio::task::spawn_blocking(move || {
                        r.recv_timeout(Duration::from_secs(60))
                    })
                    .await
                    .ok()
                    .and_then(Result::ok);
                    if let Some(g) = got {
                        let _ = cell.set(g);
                    }
                }
            }
            let Some((h, m)) = cell.get().cloned() else {
                return json_response(
                    503,
                    &json!({"error": "this machine's config could not be opened (see the log)"}),
                );
            };
            match h.spawn(async move { m.handle(req).await }).await {
                Ok(r) => r,
                Err(e) => json_response(500, &json!({"error": e.to_string()})),
            }
        })
    })
}

#[cfg(test)]
#[path = "machine_tests.rs"]
mod tests;
