//! The inventory service (D152): Hugging Face search and downloads, GPU detection and GGUF
//! reads on a server, for the admin API. Downloads run **on the target server** through its
//! executor (local, or SSH to hostctl) with the `download` builtin: resumable, cancellable,
//! progress from `download-status`. A finished download marks the model's files present on that
//! server (`models.<id>.servers`) through the audited config save. The Hugging Face token lives
//! in a secret file and only travels to the host on stdin.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::exec::{Cancel, ExecRequest, Executor, Scrubber};
use super::hf::{FileRef, Hf};
use super::schema::ControlConfig;
use crate::control_api::ControlBackend;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DlFile {
    pub path: String,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    pub dest: String,
    #[serde(default)]
    pub bytes: u64,
    #[serde(default)]
    pub done: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Download {
    pub id: String,
    pub server: String,
    pub repo: String,
    pub revision: String,
    /// The catalog model it is for (marked present on the server when done).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub dir: String,
    pub files: Vec<DlFile>,
    /// queued | running | done | failed | cancelled
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub total: u64,
    pub bytes: u64,
    pub started: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished: Option<i64>,
    pub by: String,
}

/// What `POST downloads` takes.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartReq {
    pub server: String,
    pub repo: String,
    #[serde(default)]
    pub revision: Option<String>,
    pub files: Vec<FileRef2>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub gated: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FileRef2 {
    pub path: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub sha256: Option<String>,
}

impl From<&FileRef> for FileRef2 {
    fn from(f: &FileRef) -> Self {
        FileRef2 {
            path: f.path.clone(),
            size: f.size,
            sha256: f.sha256.clone(),
        }
    }
}

/// Builds the executor for a server (tests hand in a fake or a local one).
pub type ExecFor =
    Arc<dyn Fn(&ControlConfig, &str) -> Result<Arc<dyn Executor>, String> + Send + Sync>;

pub struct Service {
    pub main: PathBuf,
    pub state_dir: PathBuf,
    pub backend: Arc<dyn ControlBackend>,
    exec_for: ExecFor,
    jobs: Mutex<IndexMap<String, Download>>,
    cancels: Mutex<HashMap<String, Cancel>>,
    /// The on-demand names and models the control plane started with (a new one needs a restart).
    started: Mutex<Option<(Vec<String>, Vec<String>)>>,
    /// Retries of one file (timeouts and interruptions resume).
    pub tries: u32,
    pub poll: Duration,
}

/// One service per main config file (tests run several routers in a process).
fn registry() -> &'static Mutex<HashMap<PathBuf, Arc<Service>>> {
    static R: OnceLock<Mutex<HashMap<PathBuf, Arc<Service>>>> = OnceLock::new();
    R.get_or_init(Default::default)
}

/// The service for the router whose main file is `main`.
pub fn installed_for(main: &Path) -> Option<Arc<Service>> {
    registry().lock().unwrap().get(main).cloned()
}

/// Install the service for the admin API (once per main file).
pub fn install(main: &Path, state_dir: &Path, backend: Arc<dyn ControlBackend>) -> Arc<Service> {
    let mut r = registry().lock().unwrap();
    if let Some(s) = r.get(main) {
        return s.clone();
    }
    let s = Arc::new(Service::new(main, state_dir, backend, default_exec_for()));
    s.load_jobs();
    super::caps::load_cache(state_dir);
    r.insert(main.to_path_buf(), s.clone());
    s
}

/// The server's executor from the config, as the control plane builds it (secrets scrubbed).
pub fn default_exec_for() -> ExecFor {
    Arc::new(|c: &ControlConfig, server: &str| {
        use crate::control_api::resolve_secret;
        let values: Vec<String> = c
            .secrets
            .keys()
            .filter_map(|id| resolve_secret(c, &format!("secret:{id}")))
            .filter_map(|b| String::from_utf8(b).ok())
            .collect();
        let state_dir = PathBuf::from(&c.router.state_dir);
        let ex = super::ssh::Executors::from_config(c, &state_dir, Scrubber::new(values), |id| {
            c.secrets
                .get(id)
                .and_then(|s| s.file.clone())
                .map(PathBuf::from)
        })?;
        let e = ex
            .by_server
            .get(server)
            .cloned()
            .ok_or_else(|| format!("no server `{server}`"))?;
        Ok(e)
    })
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

impl Service {
    pub fn new(
        main: &Path,
        state_dir: &Path,
        backend: Arc<dyn ControlBackend>,
        exec_for: ExecFor,
    ) -> Service {
        Service {
            main: main.to_path_buf(),
            state_dir: state_dir.to_path_buf(),
            backend,
            exec_for,
            jobs: Mutex::default(),
            cancels: Mutex::default(),
            started: Mutex::default(),
            tries: 20,
            poll: Duration::from_secs(2),
        }
    }

    fn names(c: &ControlConfig) -> (Vec<String>, Vec<String>) {
        (
            super::startup::on_demand_models(c)
                .keys()
                .cloned()
                .collect(),
            c.models.keys().cloned().collect(),
        )
    }

    /// Remember what the control plane started with.
    pub fn note_started(&self, c: &ControlConfig) {
        *self.started.lock().unwrap() = Some(Self::names(c));
    }

    /// New models or on-demand names since the start: switching to them needs a restart.
    pub fn restart_needed(&self, c: &ControlConfig) -> bool {
        self.started
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|s| *s != Self::names(c))
    }

    /// Per card: does a Hugging Face choice fit? (weights + 10% vs VRAM less the reserve)
    pub fn fit(&self, c: &ControlConfig, details: &Value) -> Value {
        let mut out = serde_json::Map::new();
        for ch in details["choices"].as_array().into_iter().flatten() {
            let need = ch["needGib"].as_f64().unwrap_or(0.0);
            let per: Vec<Value> = c
                .cards
                .iter()
                .filter(|(_, cd)| cd.managed)
                .map(|(id, cd)| {
                    let room = cd.vram_gib.map(|v| v - cd.reserve_gib);
                    json!({"card": id, "server": cd.server, "label": cd.label, "roomGib": room,
                        "fits": room.map(|r| need <= r)})
                })
                .collect();
            out.insert(ch["id"].as_str().unwrap_or("").to_string(), json!(per));
        }
        Value::Object(out)
    }

    /// The config as the file says now.
    pub fn config(&self) -> Result<ControlConfig, String> {
        let doc = self.backend.config();
        super::load::load_with(&self.main, doc.text)
            .map(|l| l.config)
            .map_err(|i| {
                i.iter()
                    .map(|x| x.to_string())
                    .collect::<Vec<_>>()
                    .join("; ")
            })
    }

    // ── the token ──────────────────────────────────────────────────────────────────────────

    fn token_file(&self) -> PathBuf {
        self.state_dir.join("secrets").join("hf-token")
    }

    /// The token: `[hf] token = "secret:…"`, else the file the admin pages write.
    fn token(&self, c: &ControlConfig) -> Option<String> {
        c.hf.token
            .as_deref()
            .and_then(|r| crate::control_api::resolve_secret(c, r))
            .or_else(|| std::fs::read(self.token_file()).ok())
            .and_then(|b| String::from_utf8(b).ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    pub fn hf(&self, c: &ControlConfig) -> Hf {
        Hf::new(c.hf.endpoint.as_deref(), self.token(c))
    }

    pub fn token_view(&self, c: &ControlConfig) -> Value {
        json!({
            "set": self.token(c).is_some(),
            "from": if c.hf.token.is_some() { "config" } else { "file" },
        })
    }

    /// Store the token (0600), never echoed. Empty = remove.
    pub fn set_token(&self, token: &str) -> Result<(), String> {
        let t = token.trim();
        let f = self.token_file();
        if t.is_empty() {
            let _ = std::fs::remove_file(&f);
            return Ok(());
        }
        if t.len() > 512 || t.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err("that does not look like a Hugging Face token".into());
        }
        let dir = f.parent().ok_or("no secrets folder")?;
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        super::builtins::write_atomic(&f, t.as_bytes(), Some(0o600))
    }

    // ── server tools ───────────────────────────────────────────────────────────────────────

    /// The GPUs a server reports (`nvidia-smi` through its executor), with the card each is
    /// already set up as.
    pub async fn detect(&self, server: &str) -> Result<Value, String> {
        let c = self.config()?;
        let ex = (self.exec_for)(&c, server)?;
        let o = ex
            .run(
                ExecRequest::builtin(server, "telemetry", json!({}), Duration::from_secs(30)),
                &Cancel::new(),
            )
            .await
            .map_err(|e| e.to_string())?;
        if !o.ok() {
            return Err(format!("nvidia-smi on {server}: {}", o.summary()));
        }
        let gpus: Vec<Value> = super::telemetry::parse(&o.stdout)
            .into_iter()
            .map(|g| {
                let known = c
                    .cards
                    .iter()
                    .find(|(_, cd)| cd.gpu_uuid.eq_ignore_ascii_case(&g.uuid))
                    .map(|(k, _)| k.clone());
                json!({
                    "index": g.index, "uuid": g.uuid, "name": g.name,
                    "vramGib": g.mem_total_mib.map(|m| ((m as f64) / 1024.0 * 10.0).round() / 10.0),
                    "card": known,
                })
            })
            .collect();
        Ok(json!({"server": server, "gpus": gpus}))
    }

    /// `general.architecture` and friends from a GGUF file on a server.
    pub async fn gguf(&self, server: &str, path: &str) -> Result<Value, String> {
        let c = self.config()?;
        let ex = (self.exec_for)(&c, server)?;
        let o = ex
            .run(
                ExecRequest::builtin(
                    server,
                    "gguf-meta",
                    json!({"path": path}),
                    Duration::from_secs(30),
                ),
                &Cancel::new(),
            )
            .await
            .map_err(|e| e.to_string())?;
        if !o.ok() {
            return Err(o.summary());
        }
        serde_json::from_str(&o.stdout).map_err(|e| e.to_string())
    }

    // ── downloads ──────────────────────────────────────────────────────────────────────────

    fn jobs_file(&self) -> PathBuf {
        self.state_dir.join("downloads.json")
    }

    fn load_jobs(&self) {
        if let Ok(t) = std::fs::read_to_string(self.jobs_file())
            && let Ok(mut v) = serde_json::from_str::<IndexMap<String, Download>>(&t)
        {
            // a restart stopped whatever ran: resumable, not lost
            for d in v.values_mut() {
                if d.state == "running" || d.state == "queued" {
                    d.state = "cancelled".into();
                    d.error = Some("the router restarted: resume to carry on".into());
                }
            }
            *self.jobs.lock().unwrap() = v;
        }
    }

    fn save_jobs(&self) {
        let v = self.jobs.lock().unwrap().clone();
        let _ = std::fs::create_dir_all(&self.state_dir);
        let _ = super::builtins::write_atomic(
            &self.jobs_file(),
            &serde_json::to_vec_pretty(&v).unwrap_or_default(),
            Some(0o600),
        );
    }

    pub fn list(&self) -> Vec<Download> {
        let mut v: Vec<Download> = self.jobs.lock().unwrap().values().cloned().collect();
        v.reverse();
        v
    }

    pub fn get(&self, id: &str) -> Option<Download> {
        self.jobs.lock().unwrap().get(id).cloned()
    }

    fn update(&self, id: &str, f: impl FnOnce(&mut Download)) {
        if let Some(d) = self.jobs.lock().unwrap().get_mut(id) {
            f(d);
            d.bytes = d.files.iter().map(|x| x.bytes).sum();
        }
    }

    /// Where a repo's files go on `server`: `<model_dir>/<owner>/<name>/`.
    pub fn dir_for(c: &ControlConfig, server: &str, repo: &str) -> Result<String, String> {
        let s = c
            .servers
            .get(server)
            .ok_or_else(|| format!("no server `{server}`"))?;
        let md = s.model_dir.as_deref().ok_or_else(|| {
            format!("set a model folder for {server} first (Servers: Model folder)")
        })?;
        let mut md = md.to_string();
        for (k, v) in &s.vars {
            md = md.replace(&format!("{{var:{k}}}"), v);
        }
        Ok(format!("{}/{repo}", md.trim_end_matches('/')))
    }

    /// Start a download job.
    pub fn start(self: &Arc<Self>, req: StartReq, who: &str) -> Result<Download, String> {
        let c = self.config()?;
        if !super::hf::valid_repo(&req.repo) {
            return Err("a Hugging Face model id is `owner/name`".into());
        }
        if req.files.is_empty() {
            return Err("pick the files to download".into());
        }
        if req.files.iter().any(|f| !super::hf::valid_file(&f.path)) {
            return Err("a file name is not a plain path inside the repo".into());
        }
        if req.gated && self.token(&c).is_none() {
            return Err("this model is gated: accept its licence on huggingface.co and set a Hugging Face token first".into());
        }
        let dir = Self::dir_for(&c, &req.server, &req.repo)?;
        if let Some(m) = &req.model
            && !c.models.contains_key(m)
        {
            return Err(format!("no model `{m}` (save the model first)"));
        }
        let busy = self.jobs.lock().unwrap().values().any(|d| {
            d.server == req.server
                && d.repo == req.repo
                && (d.state == "running" || d.state == "queued")
        });
        if busy {
            return Err("that model is already downloading to this server".into());
        }
        let id = format!("dl-{}-{}", now(), self.jobs.lock().unwrap().len() + 1);
        let d = Download {
            id: id.clone(),
            server: req.server.clone(),
            repo: req.repo.clone(),
            revision: req.revision.clone().unwrap_or_else(|| "main".into()),
            model: req.model.clone(),
            dir: dir.clone(),
            files: req
                .files
                .iter()
                .map(|f| DlFile {
                    path: f.path.clone(),
                    size: f.size,
                    sha256: f.sha256.clone(),
                    dest: format!("{dir}/{}", f.path),
                    bytes: 0,
                    done: false,
                })
                .collect(),
            state: "queued".into(),
            error: None,
            total: req.files.iter().map(|f| f.size).sum(),
            bytes: 0,
            started: now(),
            finished: None,
            by: who.to_string(),
        };
        self.jobs.lock().unwrap().insert(id.clone(), d.clone());
        self.save_jobs();
        self.backend.record(
            who,
            "download.start",
            &format!("{} -> {}", req.repo, req.server),
            "",
            "",
            "",
            "",
        );
        self.spawn(id);
        Ok(d)
    }

    /// Carry on with a stopped or failed download (the parts are kept).
    pub fn resume(self: &Arc<Self>, id: &str, who: &str) -> Result<Download, String> {
        let d = self.get(id).ok_or("no such download")?;
        if d.state == "running" || d.state == "queued" || d.state == "done" {
            return Err(format!("the download is {}", d.state));
        }
        self.update(id, |d| {
            d.state = "queued".into();
            d.error = None;
            d.finished = None;
        });
        self.save_jobs();
        self.backend
            .record(who, "download.resume", &d.repo, "", "", "", "");
        self.spawn(id.to_string());
        Ok(self.get(id).unwrap_or(d))
    }

    pub fn cancel(&self, id: &str, who: &str) -> Result<Download, String> {
        let d = self.get(id).ok_or("no such download")?;
        if let Some(c) = self.cancels.lock().unwrap().get(id) {
            c.cancel();
        }
        if d.state == "queued" {
            self.update(id, |d| d.state = "cancelled".into());
        }
        self.backend
            .record(who, "download.cancel", &d.repo, "", "", "", "");
        Ok(self.get(id).unwrap_or(d))
    }

    /// Forget a finished, failed or cancelled download (its files stay).
    pub fn remove(&self, id: &str) -> Result<(), String> {
        let d = self.get(id).ok_or("no such download")?;
        if d.state == "running" || d.state == "queued" {
            return Err("cancel it first".into());
        }
        self.jobs.lock().unwrap().shift_remove(id);
        self.save_jobs();
        Ok(())
    }

    fn spawn(self: &Arc<Self>, id: String) {
        let me = self.clone();
        let cancel = Cancel::new();
        self.cancels
            .lock()
            .unwrap()
            .insert(id.clone(), cancel.clone());
        tokio::spawn(async move {
            let r = me.run(&id, &cancel).await;
            me.cancels.lock().unwrap().remove(&id);
            match r {
                Ok(()) => {
                    me.update(&id, |d| {
                        d.state = "done".into();
                        d.finished = Some(now());
                    });
                    me.mark_present(&id).await;
                }
                Err(e) if cancel.is_cancelled() => me.update(&id, |d| {
                    d.state = "cancelled".into();
                    d.error = Some(e);
                    d.finished = Some(now());
                }),
                Err(e) => me.update(&id, |d| {
                    d.state = "failed".into();
                    d.error = Some(e);
                    d.finished = Some(now());
                }),
            }
            me.save_jobs();
        });
    }

    async fn run(self: &Arc<Self>, id: &str, cancel: &Cancel) -> Result<(), String> {
        let d = self.get(id).ok_or("gone")?;
        let c = self.config()?;
        let ex = (self.exec_for)(&c, &d.server)?;
        let hf = self.hf(&c);
        self.update(id, |d| d.state = "running".into());
        for (i, f) in d.files.iter().enumerate() {
            if f.done {
                continue;
            }
            let url = hf.file_url(&d.repo, &d.revision, &f.path);
            let mut tries = 0;
            loop {
                if cancel.is_cancelled() {
                    return Err("cancelled".into());
                }
                tries += 1;
                let mut req = ExecRequest::builtin(
                    &d.server,
                    "download",
                    json!({"url": url, "dest": f.dest, "size": (f.size > 0).then_some(f.size), "sha256": f.sha256}),
                    Duration::from_secs(3600),
                );
                req.stdin = hf.token_bytes();
                let run = ex.run(req, cancel);
                tokio::pin!(run);
                let out = loop {
                    tokio::select! {
                        o = &mut run => break o,
                        _ = tokio::time::sleep(self.poll) => {
                            if let Ok(o) = ex.run(ExecRequest::builtin(&d.server, "download-status", json!({"dest": f.dest}), Duration::from_secs(15)), &Cancel::new()).await
                                && let Ok(v) = serde_json::from_str::<Value>(&o.stdout)
                                && let Some(b) = v["bytes"].as_u64()
                            {
                                self.update(id, |d| d.files[i].bytes = b);
                            }
                        }
                    }
                };
                let out = out.map_err(|e| e.to_string())?;
                let v: Value = serde_json::from_str(&out.stdout).unwrap_or(Value::Null);
                if let Some(b) = v["bytes"].as_u64() {
                    self.update(id, |d| d.files[i].bytes = b);
                }
                if out.ok() {
                    self.update(id, |d| {
                        d.files[i].done = true;
                        if d.files[i].size == 0 {
                            d.files[i].size = d.files[i].bytes;
                        }
                    });
                    self.save_jobs();
                    break;
                }
                if out.cancelled || cancel.is_cancelled() {
                    return Err("cancelled (the part is kept: resume carries on)".into());
                }
                let resumable = out.timed_out || out.exit_code == Some(5);
                if !resumable || tries >= self.tries {
                    return Err(format!("{}: {}", f.path, out.summary()));
                }
                self.save_jobs();
            }
        }
        Ok(())
    }

    /// The model's files are on this server now: add it to `models.<id>.servers`.
    async fn mark_present(&self, id: &str) {
        let Some(d) = self.get(id) else { return };
        let Some(model) = &d.model else { return };
        let Ok(c) = self.config() else { return };
        let Some(m) = c.models.get(model) else { return };
        let mut servers = m.servers.clone().unwrap_or_default();
        if m.servers.is_some() && servers.contains(&d.server) {
            return;
        }
        if m.servers.is_none() {
            // unset meant "everywhere": nothing to add
            return;
        }
        servers.push(d.server.clone());
        let doc = self.backend.config();
        let edits = vec![crate::control_api::form::Edit {
            path: vec!["models".into(), model.clone(), "servers".into()],
            value: json!(servers),
        }];
        let Ok(text) = crate::control_api::form::apply_edits(&doc.text, &edits) else {
            return;
        };
        let reason = format!("download of {} finished on {}", d.repo, d.server);
        if let Err(e) = self
            .backend
            .save_config(&text, &doc.rev, &reason, "router:downloads")
            .await
        {
            self.update(id, |d| {
                d.error = Some(format!(
                    "downloaded, but the model could not be marked present: {}",
                    e.message
                ))
            });
        }
    }
}

#[cfg(test)]
mod tests;
