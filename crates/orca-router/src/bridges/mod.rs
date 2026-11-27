//! Bridges (ROUTER-MEDIA-PLAN.md 11, 13.1, 13.4): machines that dial the router's `/bridge/ws`
//! and become router-managed servers without opening a port.
//!
//! - [`tokens`]: minting, checking and revoking bridge tokens (only hashes are kept);
//! - [`Hub`]: the connected bridges, their descriptors and the admin's settings for each;
//! - [`ws`]: the endpoint (hello, welcome, keepalive, descriptor updates);
//! - [`effective`]: the config placement reads, `router.toml` plus every bridge's server, cards
//!   and engines (an overlay; never written to the file);
//! - [`upstream_for`] / [`forward`] / [`forward_ws`]: sending a request to an engine on a bridge;
//! - [`api`]: `/admin/api/bridges` for the admin pages.

pub mod api;
pub mod tokens;
pub mod ws;

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use indexmap::IndexMap;
use orca_bridge::mux::Mux;
use orca_bridge::proto::{Descriptor, Settings, hop_by_hop};
use serde_json::{Value, json};

use crate::control::schema::{self, ControlConfig, ExecKind, PerCard};

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// One bridge the router knows: connected now, or seen before (its last descriptor).
pub struct Entry {
    pub descriptor: Descriptor,
    pub version: String,
    pub online: bool,
    pub since: i64,
    pub last_seen: i64,
    pub peer: String,
    /// Why parts of its descriptor were left out of the overlay.
    pub issues: Vec<String>,
    pub events: VecDeque<Value>,
    /// Per engine, why it cannot start (a failed image pull or install), from its events;
    /// shown in the bridge's status so a refused request is not a bare 503.
    pub engine_problems: BTreeMap<String, String>,
    mux: Option<Arc<Mux>>,
    /// Which connection this is (a stale connection's end must not mark a newer one offline).
    conn_no: u64,
}

/// The router's bridges.
pub struct Hub {
    pub state_dir: PathBuf,
    base: RwLock<Arc<ControlConfig>>,
    entries: Mutex<BTreeMap<String, Entry>>,
    settings: Mutex<BTreeMap<String, Settings>>,
    conn_no: std::sync::atomic::AtomicU64,
    /// How long `forward` waits for an engine's response head (an on-demand start included).
    pub head_timeout: Duration,
}

static INSTALLED: OnceLock<Mutex<Option<Arc<Hub>>>> = OnceLock::new();

/// The hub of the router in this process (the media proxy and placement reach it here).
pub fn installed() -> Option<Arc<Hub>> {
    INSTALLED.get()?.lock().unwrap().clone()
}

/// Make `hub` the process's hub.
pub fn install(hub: Arc<Hub>) {
    *INSTALLED.get_or_init(Default::default).lock().unwrap() = Some(hub);
}

impl Hub {
    /// A hub keeping its files under `state_dir` (`bridges.json`, `bridges/`, `secrets/bridges/`).
    pub fn new(state_dir: &Path, base: Arc<ControlConfig>) -> Arc<Hub> {
        let hub = Hub {
            state_dir: state_dir.to_path_buf(),
            base: RwLock::new(base),
            entries: Mutex::default(),
            settings: Mutex::new(load_settings(state_dir)),
            conn_no: Default::default(),
            head_timeout: Duration::from_secs(900),
        };
        hub.load_seen();
        Arc::new(hub)
    }

    /// The config from `router.toml` changed (a save): clashes are checked against this one.
    pub fn set_base(&self, base: Arc<ControlConfig>) {
        *self.base.write().unwrap() = base;
    }

    /// `router.toml` as last loaded, with the bridges merged in ([`effective_with`]): what the
    /// media routes place on (D153).
    pub fn config(&self) -> ControlConfig {
        let base = self.base.read().unwrap().clone();
        effective_with(self, &base)
    }

    fn seen_dir(&self) -> PathBuf {
        self.state_dir.join("bridges")
    }

    /// Bridges seen before a restart are listed (offline) until they dial in again.
    fn load_seen(&self) {
        let Ok(rd) = std::fs::read_dir(self.seen_dir()) else {
            return;
        };
        let mut g = self.entries.lock().unwrap();
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("json") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&p) else {
                continue;
            };
            let Ok(v) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            let Ok(d) = serde_json::from_value::<Descriptor>(v["descriptor"].clone()) else {
                continue;
            };
            let id = d.server.id.clone();
            if !tokens::exists(&self.state_dir, &id) {
                continue;
            }
            g.insert(
                id,
                Entry {
                    descriptor: d,
                    version: v["version"].as_str().unwrap_or("").into(),
                    online: false,
                    since: 0,
                    last_seen: v["lastSeen"].as_i64().unwrap_or(0),
                    peer: String::new(),
                    issues: Vec::new(),
                    events: VecDeque::new(),
                    engine_problems: BTreeMap::new(),
                    mux: None,
                    conn_no: 0,
                },
            );
        }
    }

    fn save_seen(&self, id: &str, e: &Entry) {
        let dir = self.seen_dir();
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        let v = json!({"descriptor": e.descriptor, "version": e.version, "lastSeen": e.last_seen});
        let tmp = dir.join(format!("{id}.json.tmp"));
        if std::fs::write(&tmp, v.to_string()).is_ok() {
            let _ = std::fs::rename(&tmp, dir.join(format!("{id}.json")));
        }
    }

    pub fn settings(&self, id: &str) -> Settings {
        self.settings
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .unwrap_or_default()
    }

    pub fn set_settings(&self, id: &str, s: Settings) -> Result<(), String> {
        let mut g = self.settings.lock().unwrap();
        g.insert(id.into(), s);
        save_settings(&self.state_dir, &g)
    }

    /// Problems that stop a descriptor from joining the overlay at all.
    pub fn refuse(&self, id: &str, d: &Descriptor) -> Vec<String> {
        let mut p = d.problems();
        if d.server.id != id {
            p.push(format!(
                "the descriptor's server id `{}` is not the bridge's id `{id}`",
                d.server.id
            ));
        }
        let base = self.base.read().unwrap().clone();
        if base.servers.contains_key(id) {
            p.push(format!(
                "`{id}` is already a server in router.toml; give the bridge another id"
            ));
        }
        p
    }

    /// A bridge connected (or sent a new descriptor on the same connection): its descriptor
    /// replaces the old one. Returns the connection's generation.
    pub fn upsert(
        &self,
        id: &str,
        d: Descriptor,
        version: &str,
        peer: &str,
        mux: Option<Arc<Mux>>,
    ) -> u64 {
        let issues = self.overlay_issues(id, &d);
        let mut g = self.entries.lock().unwrap();
        let new_conn = mux.is_some();
        let conn_no = if new_conn {
            self.conn_no
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                + 1
        } else {
            g.get(id).map_or(0, |e| e.conn_no)
        };
        let entry = g.entry(id.to_string()).or_insert_with(|| Entry {
            descriptor: d.clone(),
            version: String::new(),
            online: false,
            since: 0,
            last_seen: 0,
            peer: String::new(),
            issues: Vec::new(),
            events: VecDeque::new(),
            engine_problems: BTreeMap::new(),
            mux: None,
            conn_no: 0,
        });
        if let Some(m) = mux {
            if let Some(old) = entry.mux.replace(m) {
                old.close(
                    orca_bridge::proto::CLOSE_REPLACED,
                    "another connection took over",
                );
            }
            entry.conn_no = conn_no;
            entry.since = now();
            entry.peer = peer.to_string();
        }
        entry.descriptor = d;
        if !version.is_empty() {
            entry.version = version.to_string();
        }
        entry.online = entry.mux.is_some();
        entry.last_seen = now();
        entry.issues = issues;
        self.save_seen(id, entry);
        conn_no
    }

    /// Its connection `conn_no` ended: the bridge's cards stay listed but are never placed on.
    pub fn offline(&self, id: &str, conn_no: u64) {
        let mut g = self.entries.lock().unwrap();
        if let Some(e) = g.get_mut(id)
            && e.conn_no == conn_no
        {
            e.online = false;
            e.mux = None;
            e.last_seen = now();
            self.save_seen(id, e);
        }
    }

    /// Forget a bridge entirely (its token was revoked): disconnects it.
    pub fn remove(&self, id: &str) {
        if let Some(e) = self.entries.lock().unwrap().remove(id)
            && let Some(m) = e.mux
        {
            m.close(orca_bridge::proto::CLOSE_BAD_TOKEN, "the token was revoked");
        }
        let _ = std::fs::remove_file(self.seen_dir().join(format!("{id}.json")));
    }

    pub fn touch(&self, id: &str) {
        if let Some(e) = self.entries.lock().unwrap().get_mut(id) {
            e.last_seen = now();
        }
    }

    pub fn event(&self, id: &str, kind: &str, data: Value) {
        if let Some(e) = self.entries.lock().unwrap().get_mut(id) {
            if kind == "engine-state"
                && let Some(engine) = data["engine"].as_str()
            {
                let failed = data["prepare"]["state"] == "failed";
                if failed {
                    let why = data["prepare"]["error"]
                        .as_str()
                        .unwrap_or("it cannot start");
                    e.engine_problems
                        .insert(engine.to_string(), why.to_string());
                } else if data["prepare"]["state"] == "ready" || data["state"] == "running" {
                    e.engine_problems.remove(engine);
                } else if data["state"] == "failed"
                    && let Some(why) = data["error"].as_str()
                {
                    e.engine_problems
                        .insert(engine.to_string(), why.to_string());
                }
            }
            e.events
                .push_back(json!({"at": now(), "kind": kind, "data": data}));
            while e.events.len() > 50 {
                e.events.pop_front();
            }
        }
    }

    pub fn mux(&self, id: &str) -> Option<Arc<Mux>> {
        self.entries.lock().unwrap().get(id)?.mux.clone()
    }

    pub fn online(&self, id: &str) -> bool {
        self.entries
            .lock()
            .unwrap()
            .get(id)
            .is_some_and(|e| e.online)
    }

    /// Is `server` a bridge's server?
    pub fn is_bridge(&self, server: &str) -> bool {
        self.entries.lock().unwrap().contains_key(server)
    }

    /// Send a command to a connected bridge.
    pub async fn call(
        &self,
        id: &str,
        op: &str,
        args: Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        let m = self
            .mux(id)
            .ok_or_else(|| format!("bridge {id} is not connected"))?;
        m.call(op, args, timeout).await
    }

    /// The parts of a descriptor the overlay will leave out, and why.
    fn overlay_issues(&self, id: &str, d: &Descriptor) -> Vec<String> {
        let base = self.base.read().unwrap().clone();
        let mut v = Vec::new();
        for c in &d.cards {
            let gid = global_card(id, &c.id);
            if base.cards.contains_key(&gid) {
                v.push(format!("card {gid} is already in router.toml: left out"));
            }
        }
        v
    }

    /// The admin's view.
    pub fn view(&self) -> Value {
        let g = self.entries.lock().unwrap();
        let mut ids: Vec<String> = tokens::list(&self.state_dir)
            .into_iter()
            .map(|t| t.id)
            .collect();
        for k in g.keys() {
            if !ids.contains(k) {
                ids.push(k.clone());
            }
        }
        ids.sort();
        let toks: BTreeMap<String, tokens::TokenInfo> = tokens::list(&self.state_dir)
            .into_iter()
            .map(|t| (t.id.clone(), t))
            .collect();
        let list: Vec<Value> = ids
            .iter()
            .map(|id| {
                let s = self.settings(id);
                let t = toks.get(id);
                let mut v = json!({
                    "id": id,
                    "enabled": s.enabled,
                    "priority": s.priority,
                    "enginesOff": s.engines_off,
                    "token": t.map(|t| json!({"created": t.created})),
                    "online": false,
                    "seen": false,
                });
                if let Some(e) = g.get(id) {
                    let d = &e.descriptor;
                    v["seen"] = json!(true);
                    v["online"] = json!(e.online);
                    v["version"] = json!(e.version);
                    v["since"] = json!(e.since);
                    v["lastSeen"] = json!(e.last_seen);
                    v["peer"] = json!(e.peer);
                    v["title"] = json!(d.server.title);
                    v["os"] = json!(d.server.os);
                    v["arch"] = json!(d.server.arch);
                    v["modelDir"] = json!(d.server.model_dir);
                    v["cards"] = json!(
                        d.cards
                            .iter()
                            .map(|c| {
                                let mut cv = serde_json::to_value(c).unwrap_or_default();
                                cv["global"] = json!(global_card(id, &c.id));
                                cv
                            })
                            .collect::<Vec<_>>()
                    );
                    v["engines"] = json!(d.engines);
                    v["models"] = json!(d.models);
                    v["remoteConfig"] = json!(d.remote_config);
                    v["configRev"] = json!(d.config_rev);
                    v["files"] = json!(d.files.values().map(Vec::len).sum::<usize>());
                    v["limits"] = json!(d.limits);
                    v["issues"] = json!(e.issues);
                    v["engineProblems"] = json!(e.engine_problems);
                    v["events"] = json!(e.events.iter().rev().take(20).collect::<Vec<_>>());
                    v["streams"] = json!(e.mux.as_ref().map_or(0, |m| m.open_streams()));
                }
                v
            })
            .collect();
        json!({ "bridges": list, "path": orca_bridge::proto::WS_PATH })
    }

    /// Why `engine` on bridge `id` cannot start, if its bridge said so (shown with a refusal).
    pub fn engine_problem(&self, id: &str, engine: &str) -> Option<String> {
        self.entries
            .lock()
            .unwrap()
            .get(id)?
            .engine_problems
            .get(engine)
            .cloned()
    }

    /// Whether a bridge said it takes remote config (MACHINE.md 8), as of its last descriptor.
    pub fn remote_config(&self, id: &str) -> bool {
        self.entries
            .lock()
            .unwrap()
            .get(id)
            .is_some_and(|e| e.descriptor.remote_config)
    }

    /// Bridges whose overlay is in use (seen, with a token), by id.
    fn overlays(&self) -> Vec<(String, Descriptor, bool)> {
        self.entries
            .lock()
            .unwrap()
            .iter()
            .map(|(id, e)| (id.clone(), e.descriptor.clone(), e.online))
            .collect()
    }
}

fn load_settings(dir: &Path) -> BTreeMap<String, Settings> {
    std::fs::read_to_string(dir.join("bridges.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save_settings(dir: &Path, s: &BTreeMap<String, Settings>) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let tmp = dir.join("bridges.json.tmp");
    let text = serde_json::to_string_pretty(s).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, dir.join("bridges.json")).map_err(|e| e.to_string())
}

/// A bridge card's id in the router: `<SERVER>-<card>` (`TRUENAS-GPU0`).
pub fn global_card(server: &str, card: &str) -> String {
    format!("{}-{card}", server.to_ascii_uppercase())
}

/// A card id as the bridge knows it, from the router's (`TRUENAS-GPU0` -> `GPU0`).
pub fn local_card<'a>(server: &str, card: &'a str) -> &'a str {
    let prefix = format!("{}-", server.to_ascii_uppercase());
    card.strip_prefix(prefix.as_str()).unwrap_or(card)
}

// ── the overlay (13.4) ───────────────────────────────────────────────────────────────────────

/// `base` (router.toml) with every known bridge's server, cards and engines merged in. Bridge
/// servers carry `origin = bridge:<id>` and `exec = "bridge"`; their cards are managed only while
/// the bridge is online and enabled. An engine already in `base` gets the bridge's cards in its
/// `port` map; a new one becomes a `driver = "external"` engine.
pub fn effective(base: &ControlConfig) -> ControlConfig {
    match installed() {
        Some(h) => effective_with(&h, base),
        None => base.clone(),
    }
}

pub fn effective_with(hub: &Hub, base: &ControlConfig) -> ControlConfig {
    let mut c = base.clone();
    for (id, d, online) in hub.overlays() {
        if !hub.refuse(&id, &d).is_empty() {
            continue;
        }
        let s = hub.settings(&id);
        let mut d = d;
        // engines the router's admin turned off for this bridge (MACHINE.md 2.8)
        if !s.engines_off.is_empty() {
            orca_bridge::detect::share_only(
                &mut d,
                &|e| !s.engines_off.iter().any(|x| x == e),
                &[],
            );
        }
        merge(&mut c, &id, &d, online && s.enabled, s.priority);
    }
    c
}

fn merge(c: &mut ControlConfig, id: &str, d: &Descriptor, usable: bool, _priority: i32) {
    let server: Option<schema::Server> = serde_json::from_value(json!({
        "title": if d.server.title.is_empty() { id } else { d.server.title.as_str() },
        "host": format!("bridge:{id}"),
        "exec": "bridge",
        "model_dir": d.server.model_dir,
        "comfy_root": d.server.comfy_root,
    }))
    .ok();
    let Some(mut server) = server else {
        return;
    };
    server.origin = Some(format!("bridge:{id}"));
    c.servers.insert(id.to_string(), server);
    let base_cards: Vec<String> = c.cards.keys().cloned().collect();
    for card in &d.cards {
        let gid = global_card(id, &card.id);
        if c.cards.contains_key(&gid) {
            continue;
        }
        let reserve = card
            .reserve_gib
            .unwrap_or(if card.shared { 2.0 } else { 1.5 });
        let v = json!({
            "server": id,
            "device": card.device,
            "gpu_uuid": card.gpu_uuid.clone().unwrap_or_default(),
            "cores": card.cores,
            "ram_gib": card.ram_gib,
            "label": card.label,
            "vram_gib": card.vram_gib,
            "reserve_gib": reserve,
            "managed": usable,
            "on_demand": true,
            "engines": card.engines,
        });
        if let Ok(k) = serde_json::from_value::<schema::Card>(strip_nulls(v)) {
            c.cards.insert(gid, k);
        }
    }
    for e in &d.engines {
        let ports: Vec<(String, u16)> = e
            .ports
            .iter()
            .map(|(card, p)| (global_card(id, card), *p))
            .filter(|(g, _)| c.cards.get(g).is_some_and(|k| k.server == id))
            .collect();
        if ports.is_empty() {
            continue;
        }
        match c.engines.get_mut(&e.id) {
            Some(existing) => {
                let mut map: IndexMap<String, u16> = match &existing.port {
                    PerCard::Each(m) => m.clone(),
                    // `All` meant every card of router.toml: keep that, then add the bridge's.
                    PerCard::All(p) => base_cards.iter().map(|k| (k.clone(), *p)).collect(),
                };
                for (g, p) in ports {
                    map.insert(g, p);
                }
                existing.port = PerCard::Each(map);
            }
            None => {
                let caps = engine_caps(e);
                let v = json!({
                    "title": format!("{} (bridge {id})", e.id),
                    "driver": "external",
                    "port": ports.iter().cloned().collect::<IndexMap<String, u16>>(),
                    "caps": caps,
                });
                if let Ok(eng) = serde_json::from_value::<schema::Engine>(strip_nulls(v)) {
                    c.engines.insert(e.id.clone(), eng);
                }
            }
        }
    }
}

/// The descriptor's caps as router caps: its own overrides, its kinds when it gives none.
fn engine_caps(e: &orca_bridge::proto::EngineDesc) -> Value {
    let mut caps = e.caps.clone().unwrap_or_else(|| json!({}));
    if !caps.is_object() {
        caps = json!({});
    }
    if caps.get("kinds").is_none() && !e.kinds.is_empty() && caps.get("inherits").is_none() {
        caps["kinds"] = json!(e.kinds);
    }
    // Only keys the router's schema knows (it denies unknown fields).
    if serde_json::from_value::<schema::EngineCaps>(caps.clone()).is_err() {
        return if e.kinds.is_empty() {
            Value::Null
        } else {
            json!({"kinds": e.kinds})
        };
    }
    caps
}

fn strip_nulls(v: Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.into_iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| (k, strip_nulls(v)))
                .collect(),
        ),
        o => o,
    }
}

// ── routing (13.4) ───────────────────────────────────────────────────────────────────────────

/// Where a request for `engine` on `card` of `server` goes.
#[derive(Debug, Clone, PartialEq)]
pub enum Upstream {
    /// A server the router reaches directly.
    Http(String),
    /// Through a bridge's connection (`card` as the bridge knows it).
    Bridge {
        bridge: String,
        engine: String,
        card: String,
    },
}

/// `c` should be [`effective`]'s config.
pub fn upstream_for(c: &ControlConfig, server: &str, engine: &str, card: &str) -> Option<Upstream> {
    let s = c.servers.get(server)?;
    if s.exec == ExecKind::Bridge {
        return Some(Upstream::Bridge {
            bridge: server.to_string(),
            engine: engine.to_string(),
            card: local_card(server, card).to_string(),
        });
    }
    let port = c.engines.get(engine)?.port.get(card).copied()?;
    let host = if s.exec == ExecKind::Local || s.host.is_empty() {
        "127.0.0.1"
    } else {
        s.host.as_str()
    };
    Some(Upstream::Http(format!("http://{host}:{port}")))
}

/// Request headers a bridge engine may see (no client credentials, no hop-by-hop).
pub fn forward_headers(h: &HeaderMap) -> Vec<(String, String)> {
    h.iter()
        .filter(|(k, _)| {
            let k = k.as_str();
            !hop_by_hop(k)
                && !matches!(
                    k,
                    "authorization" | "cookie" | "x-api-key" | "proxy-authorization"
                )
        })
        .filter_map(|(k, v)| Some((k.to_string(), v.to_str().ok()?.to_string())))
        .collect()
}

fn err(status: u16, msg: &str) -> Response {
    crate::proxy::error(status, msg, "bridge_error", None)
}

/// Send one HTTP request to `engine` on the bridge's `card` and stream its answer back.
#[allow(clippy::too_many_arguments)]
pub async fn forward(
    hub: &Hub,
    bridge: &str,
    engine: &str,
    card: &str,
    method: &str,
    path_q: &str,
    headers: &HeaderMap,
    body: Bytes,
) -> Response {
    if !hub.settings(bridge).enabled {
        return err(503, &format!("bridge {bridge} is disabled"));
    }
    let Some(mux) = hub.mux(bridge) else {
        return err(503, &format!("bridge {bridge} is not connected"));
    };
    let card = local_card(bridge, card);
    let (s, head) = match mux.open(
        engine,
        card,
        method,
        path_q,
        forward_headers(headers),
        false,
    ) {
        Ok(x) => x,
        Err(e) => return err(503, &e),
    };
    let (tx, rx) = s.split();
    if let Err(e) = tx.send(&body).await {
        return err(502, &e);
    }
    tx.end();
    let head = match tokio::time::timeout(hub.head_timeout, head).await {
        Ok(Ok(h)) => h,
        Ok(Err(_)) => return err(502, &format!("bridge {bridge} dropped the request")),
        Err(_) => {
            tx.reset("no answer");
            return err(504, &format!("{engine} on {bridge}/{card} did not answer"));
        }
    };
    let stream = futures_util::stream::unfold((rx, tx), |(mut rx, tx)| async move {
        rx.recv()
            .await
            .map(|r| (r.map_err(std::io::Error::other), (rx, tx)))
    });
    let mut r = Response::new(Body::from_stream(stream));
    *r.status_mut() = StatusCode::from_u16(head.status).unwrap_or(StatusCode::BAD_GATEWAY);
    for (k, v) in head.headers {
        if hop_by_hop(&k) {
            continue;
        }
        if let (Ok(n), Ok(v)) = (
            axum::http::HeaderName::from_bytes(k.as_bytes()),
            HeaderValue::from_str(&v),
        ) {
            r.headers_mut().append(n, v);
        }
    }
    r.headers_mut().insert(
        "x-abhoth-bridge",
        HeaderValue::from_str(bridge).unwrap_or(HeaderValue::from_static("bridge")),
    );
    r
}

/// Carry a raw TCP connection (Wyoming: whisper, piper) to `engine` on the bridge: `client`
/// is the accepted socket (or any byte stream). Returns when either side closes; `Err` when the
/// bridge could not reach the engine (nothing was sent, so the caller can fail over).
pub async fn forward_tcp<S>(
    hub: &Hub,
    bridge: &str,
    engine: &str,
    card: &str,
    client: S,
) -> Result<(), String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin,
{
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mux = hub
        .mux(bridge)
        .ok_or_else(|| format!("bridge {bridge} is offline"))?;
    let card = local_card(bridge, card);
    let (s, head) = mux.open(
        engine,
        card,
        orca_bridge::proto::TCP_METHOD,
        "",
        Vec::new(),
        false,
    )?;
    let (tx, mut rx) = s.split();
    match tokio::time::timeout(hub.head_timeout, head).await {
        Ok(Ok(h)) if h.status == 200 => {}
        Ok(Ok(h)) => {
            tx.reset("refused");
            return Err(format!("{engine} on {bridge}/{card}: {}", h.status));
        }
        _ => {
            tx.reset("no answer");
            return Err(format!("{engine} on {bridge}/{card}: no answer"));
        }
    }
    let (mut r, mut w) = tokio::io::split(client);
    let up = async {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match r.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(&buf[..n]).await.is_err() {
                        return;
                    }
                }
            }
        }
        tx.end();
    };
    let down = async {
        while let Some(Ok(b)) = rx.recv().await {
            if w.write_all(&b).await.is_err() {
                break;
            }
        }
        let _ = w.shutdown().await;
    };
    tokio::join!(up, down);
    Ok(())
}

/// Carry a client's WebSocket to `engine` on the bridge (ComfyUI's `/ws`).
pub async fn forward_ws(
    hub: &Hub,
    bridge: &str,
    engine: &str,
    card: &str,
    path_q: &str,
    headers: &HeaderMap,
    client: axum::extract::ws::WebSocket,
) {
    use axum::extract::ws::Message;
    use futures_util::{SinkExt, StreamExt};
    use orca_bridge::proto::{WS_BINARY, WS_TEXT};
    let Some(mux) = hub.mux(bridge) else {
        return;
    };
    let card = local_card(bridge, card);
    let Ok((s, head)) = mux.open(engine, card, "GET", path_q, forward_headers(headers), true)
    else {
        return;
    };
    let (tx, mut rx) = s.split();
    match tokio::time::timeout(hub.head_timeout, head).await {
        Ok(Ok(h)) if h.status == 101 => {}
        _ => {
            tx.reset("no websocket");
            return;
        }
    }
    let (mut sink, mut stream) = client.split();
    let up = async {
        while let Some(Ok(m)) = stream.next().await {
            let mut v;
            match m {
                Message::Text(t) => {
                    v = vec![WS_TEXT];
                    v.extend_from_slice(t.as_bytes());
                }
                Message::Binary(b) => {
                    v = vec![WS_BINARY];
                    v.extend_from_slice(&b);
                }
                Message::Close(_) => break,
                _ => continue,
            }
            if tx.send_message(&v).await.is_err() {
                return;
            }
        }
        tx.end();
    };
    let down = async {
        while let Some(Ok(b)) = rx.recv().await {
            if b.is_empty() {
                continue;
            }
            let m = if b[0] == WS_TEXT {
                Message::Text(String::from_utf8_lossy(&b[1..]).into_owned().into())
            } else {
                Message::Binary(b.slice(1..))
            };
            if sink.send(m).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    };
    tokio::select! {
        _ = up => {}
        _ = down => {}
    }
}

/// `exec = "bridge"` servers' executor: builtins named after bridge commands become `cmd`
/// frames; anything else is refused (a bridge runs no shell commands for the router).
pub struct BridgeExecutor {
    pub bridge: String,
}

pub const BRIDGE_OPS: &[&str] = &[
    "status", "start", "stop", "logs", "probe", "detect", "files", "download",
];

#[async_trait::async_trait]
impl crate::control::exec::Executor for BridgeExecutor {
    async fn run(
        &self,
        req: crate::control::exec::ExecRequest,
        _cancel: &crate::control::exec::Cancel,
    ) -> Result<crate::control::exec::ExecOutput, crate::control::exec::ExecError> {
        use crate::control::exec::{ExecError, ExecOutput, Op};
        let Op::Builtin { name, args } = &req.op else {
            return Err(ExecError::Refused(format!(
                "bridge {} runs bridge commands only ({})",
                self.bridge,
                BRIDGE_OPS.join(", ")
            )));
        };
        if !BRIDGE_OPS.contains(&name.as_str()) {
            return Err(ExecError::Refused(format!(
                "bridge {} has no `{name}` command",
                self.bridge
            )));
        }
        let hub = installed().ok_or_else(|| ExecError::Unreachable("no bridges here".into()))?;
        let started = std::time::Instant::now();
        let v = hub
            .call(&self.bridge, name, args.clone(), req.timeout)
            .await
            .map_err(ExecError::Unreachable)?;
        Ok(ExecOutput {
            exit_code: Some(0),
            stdout: v.to_string(),
            duration_ms: started.elapsed().as_millis() as u64,
            ..ExecOutput::default()
        })
    }
}

/// For tests and the admin's "try it": a JSON body as a response's text.
pub async fn body_text(r: Response, limit: usize) -> (u16, Vec<(String, String)>, String) {
    let status = r.status().as_u16();
    let headers = r
        .headers()
        .iter()
        .filter_map(|(k, v)| Some((k.to_string(), v.to_str().ok()?.to_string())))
        .collect();
    let b = axum::body::to_bytes(r.into_body(), limit)
        .await
        .unwrap_or_default();
    (status, headers, String::from_utf8_lossy(&b).into_owned())
}

#[cfg(test)]
mod tests;
