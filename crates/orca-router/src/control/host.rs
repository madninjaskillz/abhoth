//! The real GPU host for the switch orchestrator: [`crate::switch::HostOps`] over the
//! executors, the drivers and the engines' own HTTP endpoints (ROUTER-CONFIG.md 5).
//!
//! Every host-side step is a driver command or a builtin sent through the server's executor
//! (local, or SSH to `hostctl serve`), so a strict host only ever sees allowlisted commands.
//! Health and the verify chat go straight to the engine's port, never through routes, and the
//! verify chat carries `x-abhoth-no-switch` so it cannot cause an on-demand switch.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use serde_json::{Value, json};

use super::driver::{Cmd, EngineDriver, Observed as DriverObserved};
use super::exec::{Cancel, ExecError, ExecOutput, Executor, Scrubber};
use super::hostd::HostdClient;
use super::schema::{self, ControlConfig};
use super::telemetry::{self, TelemetryCache};
use super::template::{self, TplValue};
use super::{drivers, resolve};
use crate::switch::{
    CardSpec, Clocks, Health, HostError, HostOps, HostResult, Observed, Spec, wiring,
};

/// The header that marks a request that must never trigger an on-demand switch.
pub const NO_SWITCH_HEADER: &str = "x-abhoth-no-switch";

pub struct ExecHost {
    cfg: RwLock<Arc<ControlConfig>>,
    pub server: String,
    exec: Arc<dyn Executor>,
    hostd: Arc<dyn HostdClient>,
    http: reqwest::Client,
    /// The engines' API key (health and verify), when they have one.
    engine_key: Option<String>,
    scrubber: Scrubber,
    /// Lock owner id for this router process.
    owner: String,
    pub telemetry: TelemetryCache,
    /// The layout `router.db` says is active (to adopt unlabelled instances by it).
    pub active: Arc<dyn Fn() -> Option<String> + Send + Sync>,
}

fn herr(what: &str, e: impl std::fmt::Display) -> HostError {
    HostError(format!("{what}: {e}"))
}

impl ExecHost {
    pub fn new(
        cfg: Arc<ControlConfig>,
        server: &str,
        exec: Arc<dyn Executor>,
        hostd: Arc<dyn HostdClient>,
        engine_key: Option<String>,
        scrubber: Scrubber,
    ) -> Self {
        Self {
            cfg: RwLock::new(cfg),
            server: server.into(),
            exec,
            hostd,
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(3))
                .build()
                .expect("http client"),
            engine_key,
            scrubber,
            owner: format!("router-{}", std::process::id()),
            telemetry: TelemetryCache::default(),
            active: Arc::new(|| None),
        }
    }

    /// Swap in a reloaded config (switches in progress keep the one they started with).
    pub fn set_config(&self, c: Arc<ControlConfig>) {
        *self.cfg.write().unwrap_or_else(|e| e.into_inner()) = c;
    }

    fn cfg(&self) -> Arc<ControlConfig> {
        self.cfg.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn schema_spec(s: &CardSpec) -> schema::CardSpec {
        schema::CardSpec {
            engine: s.engine.clone(),
            profile: s.profile.clone(),
            model: s.model.clone(),
        }
    }

    async fn run(&self, cmd: &Cmd) -> HostResult<ExecOutput> {
        let out = self
            .exec
            .run(cmd.request(&self.server), &Cancel::new())
            .await
            .map_err(|e| herr(&cmd.id, e))?;
        if !out.ok() && !cmd.allow_fail {
            return Err(herr(&cmd.id, out.summary()));
        }
        Ok(out)
    }

    async fn builtin(
        &self,
        name: &str,
        args: Value,
        root: bool,
        timeout: Duration,
    ) -> HostResult<ExecOutput> {
        self.run(&Cmd::builtin(name, args, root, timeout)).await
    }

    fn driver(
        &self,
        c: &ControlConfig,
        engine: &str,
    ) -> HostResult<(Box<dyn EngineDriver>, schema::Engine)> {
        let e = c
            .engines
            .get(engine)
            .ok_or_else(|| HostError(format!("unknown engine {engine}")))?
            .clone();
        Ok((drivers::for_engine(&e), e))
    }

    fn card_uuid(&self, c: &ControlConfig, card: &str) -> HostResult<String> {
        c.cards
            .get(card)
            .map(|x| x.gpu_uuid.clone())
            .ok_or_else(|| HostError(format!("unknown card {card}")))
    }

    fn engine_base(
        &self,
        c: &ControlConfig,
        card: &str,
        spec: &CardSpec,
    ) -> HostResult<(String, schema::Engine, resolve_out::R)> {
        let r = resolve::resolve(c, card, &Self::schema_spec(spec)).map_err(|e| herr(card, e))?;
        let host = c
            .servers
            .get(&self.server)
            .map(|s| s.host.clone())
            .unwrap_or_else(|| "127.0.0.1".into());
        let host = if c
            .servers
            .get(&self.server)
            .is_some_and(|s| s.exec == schema::ExecKind::Local)
        {
            "127.0.0.1".to_string()
        } else {
            host
        };
        let e = c.engines[&spec.engine].clone();
        Ok((format!("http://{host}:{}", r.port), e, r))
    }

    /// One status read of `engine`'s instance on `card`.
    async fn status(
        &self,
        c: &ControlConfig,
        card: &str,
        engine: &str,
    ) -> HostResult<(DriverObserved, String)> {
        let (d, _) = self.driver(c, engine)?;
        let iref = resolve::instance_ref(c, card, engine).map_err(|e| herr(card, e))?;
        let cmd = d.status(&iref).map_err(|e| herr(card, e))?;
        let out = self
            .exec
            .run(cmd.request(&self.server), &Cancel::new())
            .await
            .map_err(|e| herr(&cmd.id, e))?;
        let o = d.parse_status(&iref, &out).map_err(|e| herr(card, e))?;
        Ok((o, iref.name))
    }

    /// GPU telemetry, refreshed when stale.
    pub async fn poll_telemetry(&self) -> HostResult<Vec<telemetry::GpuSample>> {
        if let Some(s) = self.telemetry.fresh(&self.server) {
            return Ok(s);
        }
        let out = self
            .builtin("telemetry", json!({}), false, Duration::from_secs(20))
            .await?;
        let s = telemetry::parse(&out.stdout);
        self.telemetry.put(&self.server, s.clone());
        Ok(s)
    }

    async fn sample(&self, card: &str, fresh: bool) -> HostResult<telemetry::GpuSample> {
        let c = self.cfg();
        let uuid = self.card_uuid(&c, card)?;
        let samples = if fresh {
            let out = self
                .builtin("telemetry", json!({}), false, Duration::from_secs(20))
                .await?;
            let s = telemetry::parse(&out.stdout);
            self.telemetry.put(&self.server, s.clone());
            s
        } else {
            self.poll_telemetry().await?
        };
        samples
            .into_iter()
            .find(|s| s.uuid.eq_ignore_ascii_case(&uuid))
            .ok_or_else(|| HostError(format!("card {card}: {uuid} not in nvidia-smi")))
    }

    /// Variables for hook parameters: the card's base variables plus `lanes`.
    fn hook_vars(
        &self,
        c: &ControlConfig,
        card: &str,
        spec: &CardSpec,
    ) -> HostResult<template::Vars> {
        let mut v = resolve::base_vars(c, card, &spec.engine).map_err(|e| herr(card, e))?;
        v.insert("lanes".into(), TplValue::Word(spec.lanes.to_string()));
        v.insert("profile".into(), TplValue::Word(spec.profile.clone()));
        v.insert("model".into(), TplValue::Word(spec.model.clone()));
        Ok(v)
    }

    fn render_params(
        params: &indexmap::IndexMap<String, Value>,
        vars: &template::Vars,
    ) -> HostResult<Value> {
        let mut m = serde_json::Map::new();
        for (k, v) in params {
            let v = match v {
                Value::String(s) => {
                    Value::String(template::render_word(s, vars).map_err(|e| herr(k, e))?)
                }
                other => other.clone(),
            };
            m.insert(k.clone(), v);
        }
        Ok(Value::Object(m))
    }

    async fn run_hook(
        &self,
        c: &ControlConfig,
        h: &schema::Hook,
        vars: &template::Vars,
    ) -> HostResult<String> {
        let timeout = Duration::from_secs(h.timeout.unwrap_or(120));
        if let Some(b) = &h.builtin {
            let args = Self::render_params(&h.params, vars)?;
            let root = super::builtins::WRITES.contains(&b.as_str());
            let out = self.builtin(b, args, root, timeout).await?;
            return Ok(format!("{b}: {}", out.stdout.trim()));
        }
        if let Some(name) = &h.command {
            // Server templates are exported as written (`hostctl export`): no variables.
            let text = c
                .servers
                .get(&self.server)
                .and_then(|s| s.templates.get(name))
                .ok_or_else(|| HostError(format!("no template {name} on {}", self.server)))?;
            let out = self
                .run(&Cmd::shell(
                    &format!("template:{name}"),
                    text.clone(),
                    false,
                    Duration::from_secs(60),
                ))
                .await?;
            return Ok(format!("{name}: {}", out.stdout.trim()));
        }
        Ok(String::new())
    }

    fn get(&self, url: &str, timeout: Duration) -> reqwest::RequestBuilder {
        let mut r = self.http.get(url).timeout(timeout);
        if let Some(k) = &self.engine_key {
            r = r.bearer_auth(k);
        }
        r
    }

    /// The last lines of an instance's log, secrets scrubbed.
    async fn log_tail(&self, c: &ControlConfig, card: &str, engine: &str) -> String {
        let Ok((d, _)) = self.driver(c, engine) else {
            return String::new();
        };
        let Ok(iref) = resolve::instance_ref(c, card, engine) else {
            return String::new();
        };
        let Ok(cmd) = d.logs(&iref, super::hostctl::LOG_LINES) else {
            return String::new();
        };
        match self
            .exec
            .run(cmd.request(&self.server), &Cancel::new())
            .await
        {
            Ok(o) => {
                let text = format!("{}{}", o.stdout, o.stderr);
                let tail: Vec<&str> = text.lines().rev().take(15).collect();
                self.scrubber
                    .scrub(&tail.into_iter().rev().collect::<Vec<_>>().join("\n"))
            }
            Err(e) => e.to_string(),
        }
    }
}

/// Local alias so the tuple type above stays readable.
mod resolve_out {
    pub type R = crate::control::driver::Resolved;
}

impl HostOps for ExecHost {
    async fn observe(&self) -> HostResult<Observed> {
        let c = self.cfg();
        let cards: Vec<String> = c
            .managed_cards(&self.server)
            .into_iter()
            .map(String::from)
            .collect();
        let active = (self.active)().and_then(|l| wiring::target_spec(&c, &self.server, &l, &[]));
        let mut obs = Observed::default();
        let mut ours: Vec<String> = Vec::new();
        for card in &cards {
            let mut found: Option<CardSpec> = None;
            for engine in c.engines.keys() {
                let (o, name) = self.status(&c, card, engine).await?;
                ours.push(name.clone());
                if !o.running {
                    continue;
                }
                let spec = o
                    .spec_raw
                    .as_ref()
                    .and_then(|raw| resolve::spec_from_label(&c, card, raw))
                    .map(|s| wiring::spec_of(&c, &s))
                    .or_else(|| {
                        // No (known) label: adopt the active layout's spec when the engine matches.
                        active
                            .as_ref()
                            .and_then(|a| a.get(card))
                            .filter(|s| s.engine == *engine)
                            .cloned()
                    })
                    .unwrap_or_else(|| {
                        let mut s = CardSpec::new(engine, "", "", 0);
                        s.detail.insert(
                            "unknown".into(),
                            o.spec_raw
                                .as_ref()
                                .map(|v| v.to_string())
                                .unwrap_or_else(|| "no label".into()),
                        );
                        s
                    });
                if found.is_some() {
                    obs.foreign
                        .entry(card.clone())
                        .or_default()
                        .push(format!("{name} (second engine on the card)"));
                } else {
                    found = Some(spec);
                }
            }
            obs.spec
                .insert(card.clone(), found.unwrap_or_else(CardSpec::off));
        }
        // Compute holders that are not our instances (best effort: an older host without
        // compute-apps just reports none).
        if let Ok(out) = self
            .builtin("compute-apps", json!({}), true, Duration::from_secs(30))
            .await
            && let Ok(apps) = serde_json::from_str::<Vec<super::builtins::ComputeApp>>(&out.stdout)
        {
            let ignore = c
                .servers
                .get(&self.server)
                .map(|s| s.ignore_holders.clone())
                .unwrap_or_default();
            for card in &cards {
                let uuid = &c.cards[card].gpu_uuid;
                for a in apps
                    .iter()
                    .filter(|a| a.gpu_uuid.eq_ignore_ascii_case(uuid))
                {
                    let who = a
                        .container
                        .clone()
                        .unwrap_or_else(|| format!("pid {} ({})", a.pid, a.process));
                    if ours.contains(&who) || ignore.contains(&who) {
                        continue;
                    }
                    obs.foreign.entry(card.clone()).or_default().push(who);
                }
            }
        }
        Ok(obs)
    }

    async fn preconditions(&self, target: &Spec) -> Result<(), String> {
        let c = self.cfg();
        for (card, spec) in target {
            if spec.is_off() {
                continue;
            }
            let s = Self::schema_spec(spec);
            let r = resolve::resolve(&c, card, &s).map_err(|e| format!("card {card}: {e}"))?;
            let e = &c.engines[&spec.engine];
            drivers::for_engine(e)
                .start(&r)
                .map_err(|e| format!("card {card}: {e}"))?;
            if let Some(path) = &r.model_file {
                let present = self
                    .builtin(
                        "file-exists",
                        json!({"path": path}),
                        false,
                        Duration::from_secs(20),
                    )
                    .await
                    .map_err(|e| e.to_string());
                let present = match present {
                    Ok(_) => true,
                    Err(e) if e.contains("does not exist") => false,
                    Err(e) => return Err(format!("card {card}: {e}")),
                };
                let restorable = c
                    .models
                    .get(&spec.model)
                    .and_then(|m| m.artifacts.get(&spec.engine))
                    .is_some_and(|a| a.restore.is_some());
                if !present && !restorable {
                    return Err(format!(
                        "card {card}: model file {path} is missing and has no archive copy"
                    ));
                }
            }
        }
        Ok(())
    }

    async fn restore_models(&self, card: &str, spec: &CardSpec) -> HostResult<Vec<String>> {
        let c = self.cfg();
        let r = resolve::resolve(&c, card, &Self::schema_spec(spec)).map_err(|e| herr(card, e))?;
        let Some(path) = r.model_file else {
            return Ok(vec![]);
        };
        let Some(restore) = c
            .models
            .get(&spec.model)
            .and_then(|m| m.artifacts.get(&spec.engine))
            .and_then(|a| a.restore.clone())
        else {
            return Ok(vec![]);
        };
        let manifest = template::render_word(&restore.from_manifest, &r.iref.vars)
            .map_err(|e| herr(card, e))?;
        let out = self
            .builtin(
                "restore-file",
                json!({"path": path, "manifest": manifest}),
                true,
                Duration::from_secs(4 * 3600),
            )
            .await?;
        Ok(match out.stdout.trim() {
            "present" => vec![],
            rec => vec![format!(
                "restored {} from the archive (sha256 verified): {rec}",
                spec.model
            )],
        })
    }

    async fn lock(&self) -> HostResult<()> {
        let c = self.cfg();
        let locks = c
            .servers
            .get(&self.server)
            .map(|s| s.locks.clone())
            .unwrap_or_default();
        for l in locks {
            self.builtin("flock", json!({"path": l, "op": "acquire", "owner": self.owner, "wait_seconds": 120, "ttl_seconds": 3600}), true, Duration::from_secs(150))
                .await?;
        }
        Ok(())
    }

    async fn unlock(&self) {
        let c = self.cfg();
        let locks = c
            .servers
            .get(&self.server)
            .map(|s| s.locks.clone())
            .unwrap_or_default();
        for l in locks.iter().rev() {
            let _ = self
                .builtin(
                    "flock",
                    json!({"path": l, "op": "release", "owner": self.owner}),
                    true,
                    Duration::from_secs(30),
                )
                .await;
        }
    }

    async fn stop_once(&self, card: &str, spec: &CardSpec) -> HostResult<bool> {
        let c = self.cfg();
        let (d, e) = self.driver(&c, &spec.engine)?;
        let iref = resolve::instance_ref(&c, card, &spec.engine).map_err(|x| herr(card, x))?;
        for cmd in d
            .stop(&iref, Duration::from_secs(e.stop.timeout_seconds))
            .map_err(|x| herr(card, x))?
        {
            // A stop that fails is judged by the status read below, not by its exit code.
            let _ = self
                .exec
                .run(cmd.request(&self.server), &Cancel::new())
                .await
                .map_err(|x| herr(&cmd.id, x))?;
        }
        let (o, _) = self.status(&c, card, &spec.engine).await?;
        Ok(match e.stop.mode {
            schema::StopMode::Remove => !o.exists,
            schema::StopMode::Stop => !o.running,
        })
    }

    async fn gpu_used_mib(&self, card: &str) -> HostResult<u64> {
        self.sample(card, true)
            .await?
            .mem_used_mib
            .ok_or_else(|| HostError(format!("card {card}: memory.used not reported")))
    }

    async fn before_start(&self, card: &str, spec: &CardSpec) -> HostResult<Vec<String>> {
        let c = self.cfg();
        let (_, e) = self.driver(&c, &spec.engine)?;
        let vars = self.hook_vars(&c, card, spec)?;
        let mut notes = Vec::new();
        for h in e.prepare.iter().chain(&e.before_start) {
            let n = self.run_hook(&c, h, &vars).await?;
            if !n.is_empty() {
                notes.push(n);
            }
        }
        Ok(notes)
    }

    async fn start(&self, card: &str, spec: &CardSpec) -> HostResult<()> {
        let c = self.cfg();
        let cmds = resolve::start_commands(&c, card, &Self::schema_spec(spec))
            .map_err(|e| herr(card, e))?;
        for cmd in &cmds {
            self.run(cmd).await?;
        }
        Ok(())
    }

    async fn health(&self, card: &str, spec: &CardSpec) -> Health {
        let c = self.cfg();
        let (base, e, _) = match self.engine_base(&c, card, spec) {
            Ok(x) => x,
            Err(err) => return Health::NotYet(err.0),
        };
        let url = format!("{base}{}", e.health.path);
        match self.get(&url, Duration::from_secs(5)).send().await {
            Ok(r) => {
                let status = r.status();
                let body = r.text().await.unwrap_or_default();
                let body_ok = e
                    .health
                    .expect_body
                    .as_ref()
                    .is_none_or(|want| body.contains(want.as_str()));
                if status.is_success() && body_ok {
                    Health::Ok
                } else {
                    Health::NotYet(format!(
                        "HTTP {}: {}",
                        status.as_u16(),
                        body.chars().take(120).collect::<String>()
                    ))
                }
            }
            Err(err) => {
                // Not answering: still starting, or gone (then fail fast with its log tail).
                match self.status(&c, card, &spec.engine).await {
                    Ok((o, _)) if !o.running => {
                        let mut why = self.log_tail(&c, card, &spec.engine).await;
                        if o.oom_killed {
                            why = format!("OOM-killed\n{why}");
                        }
                        Health::Exited(why)
                    }
                    _ => Health::NotYet(format!(
                        "not answering yet ({})",
                        if err.is_connect() {
                            "connect"
                        } else {
                            "timeout"
                        }
                    )),
                }
            }
        }
    }

    async fn verify_chat(&self, card: &str, spec: &CardSpec) -> Result<(), String> {
        let c = self.cfg();
        let (base, e, r) = self.engine_base(&c, card, spec).map_err(|x| x.0)?;
        let body = json!({
            "model": r.served_id,
            "messages": [{"role": "user", "content": e.verify.prompt}],
            "max_tokens": e.verify.max_tokens,
            "stream": false,
        });
        let mut req = self
            .http
            .post(format!("{base}/v1/chat/completions"))
            .timeout(Duration::from_secs(e.verify.timeout_seconds))
            .header(NO_SWITCH_HEADER, "1")
            .json(&body);
        if let Some(k) = &self.engine_key {
            req = req.bearer_auth(k);
        }
        let resp = req
            .send()
            .await
            .map_err(|x| format!("verify chat on {card}: {x}"))?;
        let status = resp.status();
        let v: Value = resp.json().await.unwrap_or(Value::Null);
        if !status.is_success() || v["choices"].as_array().is_none_or(|a| a.is_empty()) {
            return Err(format!(
                "verify chat on {card}: HTTP {} {}",
                status.as_u16(),
                self.scrubber
                    .scrub(&v.to_string())
                    .chars()
                    .take(200)
                    .collect::<String>()
            ));
        }
        // The engine is Ready: the clock controller re-asserts clocks and power now.
        if let Err(err) = self.hostd.engine_ready(&self.server, card).await {
            tracing::warn!("{err}");
        }
        Ok(())
    }

    async fn clocks(&self, card: &str) -> HostResult<Clocks> {
        let s = self.sample(card, true).await?;
        Ok(Clocks {
            max_sm_mhz: s
                .max_sm_mhz
                .ok_or_else(|| HostError(format!("card {card}: clocks.max.sm not reported")))?,
        })
    }

    async fn oom_kills(&self) -> HostResult<Vec<String>> {
        let out = self
            .builtin("oom-kills", json!({}), true, Duration::from_secs(30))
            .await?;
        serde_json::from_str(&out.stdout).map_err(|e| herr("oom-kills", e))
    }

    async fn gpu_power_hold(&self, secs: u64) -> HostResult<()> {
        let Some(gp) = self
            .cfg()
            .servers
            .get(&self.server)
            .and_then(|s| s.gpu_power.clone())
        else {
            return Ok(());
        };
        let url = format!("{}/hold?secs={secs}", gp.url.trim_end_matches('/'));
        self.http
            .get(&url)
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .map_err(|e| herr("gpu-power hold", e))?;
        Ok(())
    }

    async fn gpu_power_release(&self) {
        if let Some(gp) = self
            .cfg()
            .servers
            .get(&self.server)
            .and_then(|s| s.gpu_power.clone())
        {
            let url = format!("{}/hold?secs=0", gp.url.trim_end_matches('/'));
            let _ = self
                .http
                .get(&url)
                .timeout(Duration::from_secs(5))
                .send()
                .await;
        }
    }
}

/// Everything a server's control needs, built from the config: its executor (local or SSH),
/// the hostd client, the real host for the orchestrator, and the data plane's wake hook.
pub struct ServerControl {
    pub exec: Arc<dyn Executor>,
    pub hostd: Arc<dyn HostdClient>,
    pub host: Arc<ExecHost>,
    pub wake: Arc<dyn super::hostd::CardWake>,
}

/// Build [`ServerControl`] for `server`. Secrets come from `[secrets]` (files or env) and
/// feed the scrubber; `with_hostd` = the host runs `abhoth hostd`.
pub fn for_server(
    c: Arc<ControlConfig>,
    server: &str,
    state_dir: &std::path::Path,
    with_hostd: bool,
) -> Result<ServerControl, String> {
    use crate::control_api::resolve_secret;
    let values: Vec<String> = c
        .secrets
        .keys()
        .filter_map(|id| resolve_secret(&c, &format!("secret:{id}")))
        .filter_map(|b| String::from_utf8(b).ok())
        .collect();
    let scrubber = Scrubber::new(values);
    let execs = super::ssh::Executors::from_config(&c, state_dir, scrubber.clone(), |id| {
        c.secrets
            .get(id)
            .and_then(|s| s.file.clone())
            .map(std::path::PathBuf::from)
    })?;
    let exec: Arc<dyn Executor> = Arc::new(execs);
    let hostd: Arc<dyn HostdClient> = if with_hostd {
        Arc::new(super::hostd::ExecHostd::new(exec.clone()))
    } else {
        Arc::new(super::hostd::NoHostd)
    };
    // The engines' key (they all share one today); the first engine with a key decides.
    let engine_key = c
        .engines
        .values()
        .find_map(|e| e.key.as_deref())
        .and_then(|k| resolve_secret(&c, k))
        .and_then(|b| String::from_utf8(b).ok())
        .map(|s| s.trim().to_string());
    let server_of = c
        .cards
        .iter()
        .filter(|(_, x)| x.server == server)
        .map(|(id, _)| (id.clone(), server.to_string()))
        .collect();
    let host = Arc::new(ExecHost::new(
        c.clone(),
        server,
        exec.clone(),
        hostd.clone(),
        engine_key,
        scrubber,
    ));
    let wake = Arc::new(super::hostd::WakeByCard {
        hostd: hostd.clone(),
        server_of,
    });
    Ok(ServerControl {
        exec,
        hostd,
        host,
        wake,
    })
}

impl From<ExecError> for HostError {
    fn from(e: ExecError) -> Self {
        HostError(e.to_string())
    }
}

#[cfg(test)]
mod tests;
