//! [`HostOps`] over 🟦A's executors and engine drivers (ROUTER-CONFIG.md 5.4, 6).
//!
//! Instances are started, stopped and inspected with the engine's driver commands, run by the
//! server's [`Executor`] (local, SSH, hostctl). Health and the verify chat go straight to the
//! engine over HTTP (never through routes; verify chats carry `x-abhoth-no-switch`). The
//! gpu-power hold is its HTTP API (`/hold?secs=N`, 0 ends it).
//!
//! The safety-critical host steps are 🟦A's hostctl builtins (`control::builtins`), all built
//! in [`calls`]:
//! - `flock` `{path, op: acquire|release, owner}` per server lock, held for the job;
//! - `telemetry` -> nvidia-smi CSV, parsed with `control::telemetry::parse` (memory used,
//!   `clocks.max.sm`);
//! - `compute-apps` -> JSON `[ComputeApp]` (pid, container): processes on a managed card that
//!   are not our instances are foreign holders;
//! - `oom-kills` -> JSON list of kernel lines;
//! - `file-exists` `{path}` -> exit 0 when present;
//! - hooks (`prepare` before a start, run by `restore_models`; `before_start`): the hook's own
//!   parameters plus `card`, `gpu_uuid` and `model_file`.
//!
//! Until hostctl implements a builtin, the executor refuses it, so observing (and with it any
//! switch) is refused rather than guessed.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use super::{CardSpec, Clocks, Health, HostError, HostOps, HostResult, Observed, Spec, wiring};
use crate::control::driver::{Cmd, InstanceRef, Resolved};
use crate::control::exec::{Cancel, ExecOutput, ExecRequest, Executor};
use crate::control::schema::{self, ControlConfig, Engine, Hook, StopMode};
use crate::control::{drivers, resolve};

/// Looks a secret up by id (engine keys for the verify chat). Never logged.
pub type Secrets = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

pub struct ExecHost<E: Executor> {
    cfg: Arc<ControlConfig>,
    server: String,
    exec: Arc<E>,
    http: reqwest::Client,
    secrets: Secrets,
    locked: Mutex<bool>,
}

fn herr(e: impl std::fmt::Display) -> HostError {
    HostError(e.to_string())
}

/// The hostctl builtin requests (one place to change their shapes).
pub mod calls {
    use super::*;

    /// The owner name the switch holds server locks under.
    pub const LOCK_OWNER: &str = "abhoth-switch";

    pub fn flock(server: &str, acquire: bool, path: &str) -> ExecRequest {
        ExecRequest::builtin(
            server,
            "flock",
            json!({"path": path, "op": if acquire { "acquire" } else { "release" },
                   "owner": LOCK_OWNER}),
            Duration::from_secs(150),
        )
    }

    pub fn telemetry(server: &str) -> ExecRequest {
        ExecRequest::builtin(server, "telemetry", json!({}), Duration::from_secs(20))
    }

    pub fn compute_apps(server: &str) -> ExecRequest {
        ExecRequest::builtin(server, "compute-apps", json!({}), Duration::from_secs(30))
    }

    pub fn oom_kills(server: &str) -> ExecRequest {
        ExecRequest::builtin(server, "oom-kills", json!({}), Duration::from_secs(20))
    }

    pub fn file_exists(server: &str, path: &str) -> ExecRequest {
        ExecRequest::builtin(
            server,
            "file-exists",
            json!({"path": path}),
            Duration::from_secs(20),
        )
    }

    /// A builtin hook with its string parameters rendered (`{var:ai_root}`, `{card_lc}`,
    /// `{lanes}`, ...). `None` for a command hook.
    pub fn hook(
        server: &str,
        h: &Hook,
        vars: &crate::control::template::Vars,
        extra: &[(&str, Value)],
    ) -> Option<Result<ExecRequest, String>> {
        let name = h.builtin.as_deref()?;
        let mut args = serde_json::Map::new();
        for (k, v) in &h.params {
            let v = match v {
                Value::String(t) => match crate::control::template::render_word(t, vars) {
                    Ok(r) => Value::String(r),
                    Err(e) => return Some(Err(format!("{name}.{k}: {e}"))),
                },
                other => other.clone(),
            };
            args.insert(k.clone(), v);
        }
        for (k, v) in extra {
            args.entry(k.to_string()).or_insert(v.clone());
        }
        Some(Ok(ExecRequest::builtin(
            server,
            name,
            Value::Object(args),
            Duration::from_secs(h.timeout.unwrap_or(600)),
        )))
    }
}

impl<E: Executor> ExecHost<E> {
    pub fn new(cfg: Arc<ControlConfig>, server: &str, exec: Arc<E>, secrets: Secrets) -> Self {
        ExecHost {
            cfg,
            server: server.to_string(),
            exec,
            http: reqwest::Client::new(),
            secrets,
            locked: Mutex::new(false),
        }
    }

    async fn run(&self, req: ExecRequest) -> HostResult<ExecOutput> {
        let label = req.op.label().to_string();
        self.exec
            .run(req, &Cancel::new())
            .await
            .map_err(|e| HostError(format!("{label}: {e}")))
    }

    async fn run_ok(&self, req: ExecRequest) -> HostResult<ExecOutput> {
        let label = req.op.label().to_string();
        let o = self.run(req).await?;
        if o.ok() {
            Ok(o)
        } else {
            Err(HostError(format!("{label}: {}", o.summary())))
        }
    }

    async fn run_cmds(&self, cmds: &[Cmd]) -> HostResult<()> {
        for c in cmds {
            let o = self.run(c.request(&self.server)).await?;
            if !o.ok() && !c.allow_fail {
                return Err(HostError(format!("{}: {}", c.id, o.summary())));
            }
        }
        Ok(())
    }

    fn engine(&self, id: &str) -> HostResult<&Engine> {
        self.cfg
            .engines
            .get(id)
            .ok_or_else(|| HostError(format!("unknown engine `{id}`")))
    }

    fn schema_spec(spec: &CardSpec) -> schema::CardSpec {
        schema::CardSpec {
            engine: spec.engine.clone(),
            profile: spec.profile.clone(),
            model: spec.model.clone(),
        }
    }

    fn resolved(&self, card: &str, spec: &CardSpec) -> HostResult<Resolved> {
        resolve::resolve(&self.cfg, card, &Self::schema_spec(spec)).map_err(HostError)
    }

    fn iref(&self, card: &str, engine: &str) -> HostResult<InstanceRef> {
        resolve::instance_ref(&self.cfg, card, engine).map_err(HostError)
    }

    fn uuid(&self, card: &str) -> HostResult<&str> {
        self.cfg
            .cards
            .get(card)
            .map(|c| c.gpu_uuid.as_str())
            .ok_or_else(|| HostError(format!("unknown card `{card}`")))
    }

    fn cards(&self) -> Vec<String> {
        self.cfg
            .managed_cards(&self.server)
            .into_iter()
            .map(String::from)
            .collect()
    }

    /// Engines that can run on this server's cards (those with profiles).
    fn engines(&self) -> Vec<String> {
        self.cfg
            .engines
            .iter()
            .filter(|(_, e)| !e.profiles.is_empty())
            .map(|(k, _)| k.clone())
            .collect()
    }

    async fn status(
        &self,
        card: &str,
        engine: &str,
    ) -> HostResult<crate::control::driver::Observed> {
        let e = self.engine(engine)?;
        let d = drivers::for_engine(e);
        let i = self.iref(card, engine)?;
        let cmd = d.status(&i).map_err(herr)?;
        let out = self.run(cmd.request(&self.server)).await?;
        d.parse_status(&i, &out).map_err(herr)
    }

    async fn telemetry(&self, card: &str) -> HostResult<crate::control::telemetry::GpuSample> {
        let uuid = self.uuid(card)?.to_string();
        let o = self.run_ok(calls::telemetry(&self.server)).await?;
        crate::control::telemetry::parse(&o.stdout)
            .into_iter()
            .find(|g| g.uuid == uuid)
            .ok_or_else(|| HostError(format!("telemetry: card {card} ({uuid}) not reported")))
    }

    async fn hooks(&self, card: &str, spec: &CardSpec, prepare: bool) -> HostResult<Vec<String>> {
        let e = self.engine(&spec.engine)?;
        let hooks = if prepare { &e.prepare } else { &e.before_start };
        if hooks.is_empty() {
            return Ok(Vec::new());
        }
        let r = self.resolved(card, spec)?;
        let extra = [
            ("card", json!(card)),
            ("gpu_uuid", json!(self.uuid(card)?)),
            ("model_file", json!(r.model_file)),
        ];
        let mut vars = r.iref.vars.clone();
        vars.insert("lanes".into(), r.lanes.to_string().into());
        vars.insert("port".into(), r.port.to_string().into());
        vars.insert("served_id".into(), r.served_id.clone().into());
        if let Some(f) = &r.model_file {
            vars.insert("model_file".into(), f.clone().into());
        }
        let mut msgs = Vec::new();
        for h in hooks {
            let Some(req) = calls::hook(&self.server, h, &vars, &extra) else {
                return Err(HostError(format!(
                    "{card}: command hooks are not supported by the switch yet ({:?})",
                    h.command
                )));
            };
            let name = h.builtin.clone().unwrap_or_default();
            let o = self.run_ok(req.map_err(HostError)?).await?;
            let s = o.stdout.trim();
            msgs.push(if s.is_empty() {
                format!("{card}: {name} ok")
            } else {
                format!("{card}: {name}: {}", s.lines().last().unwrap_or(""))
            });
        }
        Ok(msgs)
    }

    fn base_url(&self, r: &Resolved) -> HostResult<String> {
        let host = &self
            .cfg
            .servers
            .get(&self.server)
            .ok_or_else(|| HostError(format!("unknown server `{}`", self.server)))?
            .host;
        Ok(format!("http://{host}:{}", r.port))
    }

    fn engine_key(&self, e: &Engine) -> Option<String> {
        let id = ControlConfig::secret_id(e.key.as_deref()?)?;
        (self.secrets)(id)
    }

    async fn power(&self, secs: u64) -> HostResult<()> {
        let Some(gp) = self
            .cfg
            .servers
            .get(&self.server)
            .and_then(|s| s.gpu_power.as_ref())
        else {
            return Ok(());
        };
        let url = format!("{}/hold?secs={secs}", gp.url.trim_end_matches('/'));
        let r = self
            .http
            .get(url)
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|e| HostError(format!("gpu-power: {e}")))?;
        if r.status().is_success() {
            Ok(())
        } else {
            Err(HostError(format!("gpu-power: HTTP {}", r.status())))
        }
    }
}

impl<E: Executor> HostOps for ExecHost<E> {
    async fn observe(&self) -> HostResult<Observed> {
        let mut spec = Spec::new();
        let mut ours: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for card in self.cards() {
            let mut found = CardSpec::off();
            for eng in self.engines() {
                let o = self.status(&card, &eng).await?;
                if o.exists {
                    ours.entry(self.uuid(&card)?.to_string())
                        .or_default()
                        .push(self.iref(&card, &eng)?.name);
                }
                if o.running {
                    let known = o.spec.clone().or_else(|| {
                        o.spec_raw
                            .as_ref()
                            .and_then(|raw| resolve::spec_from_label(&self.cfg, &card, raw))
                    });
                    found = match &known {
                        Some(s) => wiring::spec_of(&self.cfg, s),
                        None => {
                            let mut c = CardSpec::new(&eng, "?", "?", 0);
                            c.detail.insert("raw".into(), format!("{:?}", o.spec_raw));
                            c
                        }
                    };
                    break;
                }
            }
            spec.insert(card, found);
        }
        // compute containers on our cards that are not our instances
        let o = self.run_ok(calls::compute_apps(&self.server)).await?;
        let apps: Vec<crate::control::builtins::ComputeApp> = serde_json::from_str(o.stdout.trim())
            .map_err(|e| HostError(format!("compute-apps: not JSON ({e})")))?;
        let mut foreign = BTreeMap::new();
        for card in self.cards() {
            let uuid = self.uuid(&card)?;
            let mine = ours.get(uuid).cloned().unwrap_or_default();
            let mut theirs: Vec<String> = apps
                .iter()
                .filter(|a| a.gpu_uuid == uuid)
                .map(|a| match &a.container {
                    Some(c) => c.trim_start_matches('/').to_string(),
                    None => format!("{} (pid {})", a.process, a.pid),
                })
                .filter(|n| !mine.contains(n))
                .collect();
            theirs.dedup();
            if !theirs.is_empty() {
                foreign.insert(card, theirs);
            }
        }
        Ok(Observed { spec, foreign })
    }

    async fn preconditions(&self, target: &Spec) -> Result<(), String> {
        let mut bad = Vec::new();
        for (card, s) in target {
            if s.is_off() {
                continue;
            }
            if let Err(e) = self.resolved(card, s) {
                bad.push(format!("{card}: {}", e.0));
            }
        }
        if bad.is_empty() {
            Ok(())
        } else {
            Err(bad.join("; "))
        }
    }

    async fn restore_models(&self, card: &str, spec: &CardSpec) -> HostResult<Vec<String>> {
        if spec.is_off() {
            return Ok(Vec::new());
        }
        let mut msgs = self.hooks(card, spec, true).await?;
        if let Some(f) = self.resolved(card, spec)?.model_file {
            let o = self.run(calls::file_exists(&self.server, &f)).await?;
            if !o.ok() {
                return Err(HostError(format!(
                    "{card}: model file {f} is missing after the prepare hooks"
                )));
            }
            msgs.push(format!("{card}: model file present"));
        }
        Ok(msgs)
    }

    async fn lock(&self) -> HostResult<()> {
        let locks = self
            .cfg
            .servers
            .get(&self.server)
            .map(|s| s.locks.clone())
            .unwrap_or_default();
        if locks.is_empty() {
            return Ok(());
        }
        for (i, path) in locks.iter().enumerate() {
            if let Err(e) = self.run_ok(calls::flock(&self.server, true, path)).await {
                for held in locks[..i].iter().rev() {
                    let _ = self.run(calls::flock(&self.server, false, held)).await;
                }
                return Err(e);
            }
        }
        *self.locked.lock().unwrap() = true;
        Ok(())
    }

    async fn unlock(&self) {
        if !std::mem::take(&mut *self.locked.lock().unwrap()) {
            return;
        }
        let locks = self
            .cfg
            .servers
            .get(&self.server)
            .map(|s| s.locks.clone())
            .unwrap_or_default();
        for path in locks.iter().rev() {
            if let Err(e) = self.run_ok(calls::flock(&self.server, false, path)).await {
                tracing::warn!("releasing the switch lock {path}: {}", e.0);
            }
        }
    }

    async fn stop_once(&self, card: &str, spec: &CardSpec) -> HostResult<bool> {
        if spec.is_off() {
            return Ok(true);
        }
        let e = self.engine(&spec.engine)?;
        let d = drivers::for_engine(e);
        let i = self.iref(card, &spec.engine)?;
        let cmds = d
            .stop(&i, Duration::from_secs(e.stop.timeout_seconds))
            .map_err(herr)?;
        self.run_cmds(&cmds).await?;
        let o = self.status(card, &spec.engine).await?;
        Ok(match e.stop.mode {
            StopMode::Remove => !o.exists,
            _ => !o.running,
        })
    }

    async fn gpu_used_mib(&self, card: &str) -> HostResult<u64> {
        self.telemetry(card)
            .await?
            .mem_used_mib
            .ok_or_else(|| HostError(format!("telemetry: no memory.used for card {card}")))
    }

    async fn before_start(&self, card: &str, spec: &CardSpec) -> HostResult<Vec<String>> {
        self.hooks(card, spec, false).await
    }

    async fn start(&self, card: &str, spec: &CardSpec) -> HostResult<()> {
        let e = self.engine(&spec.engine)?;
        let r = self.resolved(card, spec)?;
        let cmds = drivers::for_engine(e).start(&r).map_err(herr)?;
        self.run_cmds(&cmds).await
    }

    async fn health(&self, card: &str, spec: &CardSpec) -> Health {
        let (e, r) = match (self.engine(&spec.engine), self.resolved(card, spec)) {
            (Ok(e), Ok(r)) => (e, r),
            (Err(x), _) | (_, Err(x)) => return Health::NotYet(x.0),
        };
        let url = match self.base_url(&r) {
            Ok(u) => format!("{u}{}", e.health.path),
            Err(x) => return Health::NotYet(x.0),
        };
        let resp = self
            .http
            .get(&url)
            .timeout(Duration::from_secs(5))
            .send()
            .await;
        match resp {
            Ok(resp) if resp.status().is_success() => {
                let body = resp.text().await.unwrap_or_default();
                match &e.health.expect_body {
                    Some(want) if !body.contains(want.as_str()) => {
                        Health::NotYet(format!("health body lacks {want:?}"))
                    }
                    _ => Health::Ok,
                }
            }
            other => {
                // not answering: did it exit?
                match self.status(card, &spec.engine).await {
                    Ok(o) if o.exists && !o.running => Health::Exited(format!(
                        "exited{}{} {}",
                        o.exit_code
                            .map(|c| format!(" with {c}"))
                            .unwrap_or_default(),
                        if o.oom_killed { " (OOM-killed)" } else { "" },
                        o.detail
                    )),
                    Ok(o) if !o.exists => Health::Exited("the instance is gone".into()),
                    _ => Health::NotYet(match other {
                        Ok(r) => format!("HTTP {}", r.status()),
                        Err(x) => x.to_string(),
                    }),
                }
            }
        }
    }

    async fn verify_chat(&self, card: &str, spec: &CardSpec) -> Result<(), String> {
        let e = self.engine(&spec.engine).map_err(|x| x.0)?;
        let r = self.resolved(card, spec).map_err(|x| x.0)?;
        let url = format!(
            "{}/v1/chat/completions",
            self.base_url(&r).map_err(|x| x.0)?
        );
        let body = json!({"model": r.served_id, "max_tokens": e.verify.max_tokens,
            "messages": [{"role": "user", "content": e.verify.prompt}]});
        let mut rb = self
            .http
            .post(url)
            .header(super::gate::NO_SWITCH, "1")
            .json(&body)
            .timeout(Duration::from_secs(e.verify.timeout_seconds));
        if let Some(k) = self.engine_key(e) {
            rb = rb.bearer_auth(k);
        }
        let resp = rb.send().await.map_err(|x| format!("test chat: {x}"))?;
        if !resp.status().is_success() {
            return Err(format!("test chat: HTTP {}", resp.status()));
        }
        let v: Value = resp.json().await.map_err(|x| format!("test chat: {x}"))?;
        if v.pointer("/choices/0").is_some() {
            Ok(())
        } else {
            Err("test chat: no choices in the answer".into())
        }
    }

    async fn clocks(&self, card: &str) -> HostResult<Clocks> {
        let t = self.telemetry(card).await?;
        let mhz = t
            .max_sm_mhz
            .ok_or_else(|| HostError(format!("telemetry: no clocks.max.sm for card {card}")))?;
        Ok(Clocks { max_sm_mhz: mhz })
    }

    async fn oom_kills(&self) -> HostResult<Vec<String>> {
        let o = self.run_ok(calls::oom_kills(&self.server)).await?;
        serde_json::from_str::<Vec<String>>(o.stdout.trim())
            .map_err(|e| HostError(format!("oom-kills: not a JSON list ({e})")))
    }

    async fn gpu_power_hold(&self, secs: u64) -> HostResult<()> {
        self.power(secs).await
    }

    async fn gpu_power_release(&self) {
        if let Err(e) = self.power(0).await {
            tracing::warn!("{}", e.0);
        }
    }
}

#[cfg(test)]
mod host_tests {
    use super::*;
    use crate::control::exec::{ExecError, FakeExecutor, Op};

    fn sample() -> Arc<ControlConfig> {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/control/sample.toml");
        Arc::new(crate::control::load::load(&p).unwrap().config)
    }

    const UA: &str = "GPU-aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";

    /// One nvidia-smi CSV line for card A in `QUERY_FIELDS` order (memory.used 812 MiB,
    /// clocks.max.sm 1597 MHz).
    fn telemetry_csv() -> String {
        let cols: Vec<&str> = crate::control::telemetry::QUERY_FIELDS
            .iter()
            .map(|f| match *f {
                "uuid" => UA,
                "index" => "0",
                "name" => "Tesla V100-SXM3-32GB",
                "memory.used" => "812",
                "clocks.max.sm" => "1597",
                "pstate" => "P0",
                _ => "1",
            })
            .collect();
        cols.join(", ")
    }

    fn rig(cfg: &Arc<ControlConfig>) -> (ExecHost<FakeExecutor>, FakeExecutor) {
        let fx = FakeExecutor::new();
        let dense = wiring::target_spec(cfg, "gpu", "dense-x2", &[]).unwrap();
        let a = schema::CardSpec {
            engine: dense["A"].engine.clone(),
            profile: dense["A"].profile.clone(),
            model: dense["A"].model.clone(),
        };
        let label = resolve::py_json_compact(&resolve::flat_spec(cfg, &a).unwrap());
        let a_name = resolve::instance_ref(cfg, "A", &a.engine).unwrap().name;
        let a_inspect = format!("docker-inspect:{}:A", a.engine);
        fx.with_handler(move |req| {
            let lbl = req.op.label().to_string();
            let ok = |s: &str| Some(Ok(ExecOutput::ok_with(s)));
            match lbl.as_str() {
                l if l.starts_with(&a_inspect) => ok(&format!(
                    "{}|{label}",
                    json!({"Running": true, "Status": "running"})
                )),
                l if l.contains("inspect") || l.contains("ps:") || l.contains("status") => {
                    Some(Ok(ExecOutput::fail(1, "No such object")))
                }
                "compute-apps" => ok(&json!([
                    {"gpu_uuid": UA, "pid": 10, "used_mib": 30000, "container": a_name,
                     "process": "ninfer-serve"},
                    {"gpu_uuid": UA, "pid": 11, "used_mib": 900, "container": "comfyui",
                     "process": "python3"},
                ])
                .to_string()),
                "telemetry" => ok(&telemetry_csv()),
                "oom-kills" => ok(r#"["Out of memory: Killed process 42 (ninfer)"]"#),
                _ => None,
            }
        });
        let secrets: Secrets = Arc::new(|_| None);
        (
            ExecHost::new(cfg.clone(), "gpu", Arc::new(fx.clone()), secrets),
            fx,
        )
    }

    fn builtin_args(fx: &FakeExecutor, want: &str) -> Vec<Value> {
        fx.requests()
            .into_iter()
            .filter_map(|r| match r.op {
                Op::Builtin { name, args } if name == want => Some(args),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn observe_reads_labels_and_finds_foreign_holders() {
        let cfg = sample();
        let (h, _fx) = rig(&cfg);
        let o = h.observe().await.unwrap();
        let dense = wiring::target_spec(&cfg, "gpu", "dense-x2", &[]).unwrap();
        assert_eq!(o.spec["A"], dense["A"]);
        assert!(o.spec["B"].is_off());
        assert_eq!(o.foreign["A"], ["comfyui"]);
        assert!(!o.foreign.contains_key("B"));
    }

    #[tokio::test]
    async fn host_steps_go_through_the_executor() {
        let cfg = sample();
        let (h, fx) = rig(&cfg);
        assert_eq!(h.gpu_used_mib("A").await.unwrap(), 812);
        assert_eq!(h.clocks("A").await.unwrap().max_sm_mhz, 1597);
        assert_eq!(h.oom_kills().await.unwrap().len(), 1);
        h.lock().await.unwrap();
        h.unlock().await;
        h.unlock().await; // released once only
        let flocks = builtin_args(&fx, "flock");
        assert_eq!(flocks.len(), 2);
        assert_eq!(flocks[0]["op"], "acquire");
        assert_eq!(flocks[0]["path"], "/run/watchdog.lock");
        assert_eq!(flocks[0]["owner"], calls::LOCK_OWNER);
        assert_eq!(flocks[1]["op"], "release");
        // stop: the driver's stop, then gone
        let spec = wiring::target_spec(&cfg, "gpu", "dense-x2", &[]).unwrap();
        fx.clear();
        assert!(h.stop_once("B", &spec["B"]).await.unwrap());
        assert_eq!(fx.labels()[0], "docker-rm-robust");
        // start: the driver's commands, in order
        fx.clear();
        h.start("B", &spec["B"]).await.unwrap();
        let labels = fx.labels();
        assert_eq!(labels[0], "docker-rm-robust");
        assert!(labels[1].starts_with("docker-run:"), "{labels:?}");
        // preconditions: a spec that does not resolve is a refusal
        let mut bad = spec.clone();
        bad.get_mut("B").unwrap().profile = "nope".into();
        assert!(h.preconditions(&bad).await.unwrap_err().contains("B:"));
        assert!(h.preconditions(&spec).await.is_ok());
    }

    #[tokio::test]
    async fn before_start_hooks_render_their_parameters() {
        let cfg = sample();
        let (h, fx) = rig(&cfg);
        let flash = wiring::target_spec(&cfg, "gpu", "flash-x2", &[]).unwrap();
        fx.clear();
        let msgs = h.before_start("B", &flash["B"]).await.unwrap();
        assert!(!msgs.is_empty());
        let js = builtin_args(&fx, "json-set");
        assert_eq!(js.len(), 1, "{:?}", fx.labels());
        assert_eq!(js[0]["file"], "/srv/ai/strata-b.json");
        assert_eq!(js[0]["value"], "2");
        assert_eq!(js[0]["card"], "B");
    }

    #[tokio::test]
    async fn an_unimplemented_builtin_refuses_rather_than_guesses() {
        let cfg = sample();
        let fx = FakeExecutor::new();
        fx.with_handler(|req| {
            (req.op.label() == "compute-apps").then(|| {
                Err(ExecError::Refused(
                    "builtin `compute-apps` is not available yet".into(),
                ))
            })
        });
        let h = ExecHost::new(cfg, "gpu", Arc::new(fx), Arc::new(|_| None));
        let e = h.observe().await.unwrap_err();
        assert!(e.0.contains("compute-apps"), "{}", e.0);
    }
}
