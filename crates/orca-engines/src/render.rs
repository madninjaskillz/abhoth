//! Templates and start plans (ROUTER-MEDIA-PLAN.md 13.3).
//!
//! `{{name}}` is replaced by a variable. An unknown name is an error, never left in place. The
//! output is argv (no shell), so a value can never add shell syntax; compose files get values
//! that may not contain a newline or a quote, so they cannot add YAML keys either.
//!
//! [`plan_start`] turns an installed package, a card and the engine's settings into what to run:
//! a `docker run` argv, a compose file with its up/down commands, a host process, or nothing (an
//! endpoint that is already running).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::host::CardSel;
use crate::manifest::{GpuKind, Manifest, Method, MethodType};
use crate::state::{Approvals, Installed};

/// Variable names every template may use (besides `param.<id>` and `port.<id>`).
pub const KNOWN_VARS: &[&str] = &[
    "port",
    "host",
    "card",
    "gpu_index",
    "gpu_uuid",
    "cpus",
    "mem_limit",
    "model_dir",
    "models",
    "model_file",
    "model_id",
    "state",
    "install",
    "bin",
    "venv",
    "python",
    "package",
    "exe",
    "name",
];

pub type Vars = BTreeMap<String, String>;

/// The model an engine instance serves (LLM engines; media engines load per request).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelRef {
    pub id: String,
    /// Relative to the model dir (`qwen/qwen3-8b-q4.gguf`).
    #[serde(default)]
    pub file: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Limits {
    #[serde(default)]
    pub cpus: Option<f64>,
    #[serde(default)]
    pub mem_gib: Option<f64>,
}

/// One engine instance on one card.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunSpec {
    /// The router's engine id.
    pub engine: String,
    pub card: CardSel,
    /// The main port on the host.
    pub port: u16,
    /// Extra ports by `ports[].id`.
    #[serde(default)]
    pub ports: BTreeMap<String, u16>,
    /// The bind address (default `127.0.0.1`; `0.0.0.0` only when the router says so).
    #[serde(default = "loopback")]
    pub host: String,
    #[serde(default)]
    pub model: Option<ModelRef>,
    #[serde(default)]
    pub params: BTreeMap<String, Value>,
    #[serde(default)]
    pub limits: Limits,
    /// The host's model folder.
    #[serde(default)]
    pub model_dir: Option<String>,
    /// The instance's data folder **on the host** (the router passes a host path; the local
    /// runner uses `<state>/engines/data/<name>`).
    #[serde(default)]
    pub state: Option<String>,
    /// The installed package folder **on the host** (templates a container mounts read-only);
    /// default `<install dir>/package`.
    #[serde(default)]
    pub package_dir: Option<String>,
    /// Container or process name (`abhoth-comfyui-x2`).
    pub name: String,
}

fn loopback() -> String {
    "127.0.0.1".into()
}

impl RunSpec {
    pub fn new(engine: &str, card: CardSel, port: u16) -> RunSpec {
        let name = format!("abhoth-{}-{}", engine, card.id().to_ascii_lowercase());
        RunSpec {
            engine: engine.into(),
            card,
            port,
            ports: BTreeMap::new(),
            host: loopback(),
            model: None,
            params: BTreeMap::new(),
            limits: Limits::default(),
            model_dir: None,
            state: None,
            package_dir: None,
            name: sanitize_name(&name),
        }
    }
}

/// A docker/compose-safe name: `[a-z0-9_.-]`, starting with a letter or digit.
pub fn sanitize_name(s: &str) -> String {
    let mut o: String = s
        .chars()
        .map(|c| {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();
    while o.starts_with(['-', '_', '.']) {
        o.remove(0);
    }
    if o.is_empty() {
        "abhoth-engine".into()
    } else {
        o
    }
}

/// The `{{…}}` names in a template, in order.
pub fn names_in(t: &str) -> Result<Vec<String>, String> {
    let mut out = vec![];
    let mut rest = t;
    while let Some(i) = rest.find("{{") {
        let after = &rest[i + 2..];
        let Some(j) = after.find("}}") else {
            return Err(format!("an unclosed {{{{ in {t:?}"));
        };
        out.push(after[..j].trim().to_string());
        rest = &after[j + 2..];
    }
    Ok(out)
}

/// Replace every `{{name}}`.
pub fn render(t: &str, vars: &Vars) -> Result<String, String> {
    let mut out = String::with_capacity(t.len());
    let mut rest = t;
    while let Some(i) = rest.find("{{") {
        out.push_str(&rest[..i]);
        let after = &rest[i + 2..];
        let Some(j) = after.find("}}") else {
            return Err(format!("an unclosed {{{{ in {t:?}"));
        };
        let name = after[..j].trim();
        match vars.get(name) {
            Some(v) => out.push_str(v),
            None => return Err(format!("unknown template variable {{{{{name}}}}}")),
        }
        rest = &after[j + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

fn render_all(ts: &[String], vars: &Vars) -> Result<Vec<String>, String> {
    ts.iter().map(|t| render(t, vars)).collect()
}

fn value_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// The venv's interpreter.
pub fn venv_python(venv: &Path, os: &str) -> PathBuf {
    if os == "windows" {
        venv.join("Scripts").join("python.exe")
    } else {
        venv.join("bin").join("python")
    }
}

fn p(s: &Path) -> String {
    s.to_string_lossy().to_string()
}

/// Every variable for this instance.
pub fn vars(m: &Manifest, i: &Installed, s: &RunSpec) -> Result<Vars, String> {
    let meth = m
        .methods
        .get(i.method)
        .ok_or_else(|| format!("the package has no method {}", i.method))?;
    let mut v = Vars::new();
    v.insert("port".into(), s.port.to_string());
    v.insert("host".into(), s.host.clone());
    v.insert("card".into(), s.card.id().to_string());
    v.insert("name".into(), s.name.clone());
    match &s.card {
        CardSel::Gpu { index, uuid, .. } => {
            v.insert("gpu_index".into(), index.to_string());
            v.insert(
                "gpu_uuid".into(),
                if uuid.is_empty() {
                    index.to_string()
                } else {
                    uuid.clone()
                },
            );
        }
        CardSel::Cpu { .. } => {
            v.insert("gpu_index".into(), String::new());
            v.insert("gpu_uuid".into(), String::new());
        }
    }
    let cpus = s.limits.cpus.or(match &s.card {
        CardSel::Cpu { cores, .. } if *cores > 0 => Some(f64::from(*cores)),
        _ => None,
    });
    v.insert(
        "cpus".into(),
        cpus.map(trim_float).unwrap_or_default(),
    );
    v.insert(
        "mem_limit".into(),
        s.limits
            .mem_gib
            .map(|g| format!("{}m", (g * 1024.0).round() as u64))
            .unwrap_or_default(),
    );
    let model_dir = s.model_dir.clone().unwrap_or_default();
    v.insert("model_dir".into(), model_dir.clone());
    let in_engine_models = match (&m.models, meth.is_container()) {
        (Some(l), true) if !l.mount.is_empty() => l.mount.clone(),
        _ => model_dir.clone(),
    };
    v.insert("models".into(), in_engine_models.clone());
    let (mid, mfile) = match &s.model {
        Some(r) => (
            r.id.clone(),
            r.file
                .as_ref()
                .map(|f| join_path(&in_engine_models, f))
                .unwrap_or_default(),
        ),
        None => (String::new(), String::new()),
    };
    v.insert("model_id".into(), mid);
    v.insert("model_file".into(), mfile);
    let state = s
        .state
        .clone()
        .unwrap_or_else(|| p(&i.dir.join("data").join(&s.name)));
    v.insert("state".into(), state);
    v.insert(
        "install".into(),
        i.install.as_deref().map(p).unwrap_or_default(),
    );
    v.insert("bin".into(), i.bin.as_deref().map(p).unwrap_or_default());
    v.insert(
        "package".into(),
        s.package_dir
            .clone()
            .unwrap_or_else(|| p(&i.dir.join("package"))),
    );
    v.insert("venv".into(), i.venv.as_deref().map(p).unwrap_or_default());
    v.insert(
        "python".into(),
        i.venv
            .as_deref()
            .map(|x| p(&venv_python(x, &i.os)))
            .unwrap_or_default(),
    );
    v.insert(
        "exe".into(),
        if i.os == "windows" {
            ".exe".into()
        } else {
            String::new()
        },
    );
    for (k, decl) in &m.params {
        let val = s.params.get(k).or(decl.default.as_ref());
        match val {
            Some(x) => {
                v.insert(format!("param.{k}"), value_str(x));
            }
            None if decl.required => return Err(format!("the setting {k} is required")),
            None => {
                v.insert(format!("param.{k}"), String::new());
            }
        }
    }
    for (n, port) in meth.ports.iter().enumerate() {
        let host_port = if n == 0 {
            Some(s.port)
        } else {
            s.ports.get(&port.id).copied()
        };
        if let Some(hp) = host_port {
            v.insert(format!("port.{}", port.id), hp.to_string());
        }
    }
    Ok(v)
}

fn trim_float(f: f64) -> String {
    if f.fract() == 0.0 {
        format!("{}", f as i64)
    } else {
        format!("{f}")
    }
}

fn join_path(dir: &str, file: &str) -> String {
    if dir.is_empty() {
        return file.to_string();
    }
    let sep = if dir.contains('\\') && !dir.contains('/') {
        '\\'
    } else {
        '/'
    };
    format!(
        "{}{sep}{}",
        dir.trim_end_matches(['/', '\\']),
        file.trim_start_matches(['/', '\\'])
    )
}

/// What to run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum StartPlan {
    /// The full `docker run -d …` argv.
    Docker { name: String, argv: Vec<String> },
    /// Write `yaml` to a file, then run `up` (with `{{compose_file}}` replaced by its path).
    Compose {
        project: String,
        yaml: String,
        up: Vec<String>,
        down: Vec<String>,
    },
    Process {
        name: String,
        program: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
        cwd: Option<String>,
        limits: Limits,
    },
    /// Already running somewhere: nothing to start.
    External { name: String, url: String },
}

/// The placeholder for the compose file path in a compose plan's commands.
pub const COMPOSE_FILE: &str = "{{compose_file}}";

/// Is `source` under one of the allowed roots?
fn under(source: &str, roots: &[&str]) -> bool {
    let norm = |x: &str| x.replace('\\', "/").trim_end_matches('/').to_string();
    let s = norm(source);
    if s.split('/').any(|seg| seg == "..") {
        return false;
    }
    roots.iter().filter(|r| !r.is_empty()).any(|r| {
        let r = norm(r);
        s == r || s.starts_with(&format!("{r}/"))
    })
}

/// Docker options a manifest may add (`docker_args`).
pub const SAFE_DOCKER_ARGS: &[&str] = &[
    "--shm-size",
    "--init",
    "--ulimit",
    "--tmpfs",
    "--user",
    "--stop-timeout",
    "--stop-signal",
    "--read-only",
    "--security-opt=no-new-privileges",
];

pub fn docker_arg_ok(a: &str) -> bool {
    SAFE_DOCKER_ARGS
        .iter()
        .any(|s| a == *s || a.starts_with(&format!("{s}=")))
        || !a.starts_with('-') && !a.is_empty()
}

/// Turn an installed package into what to run for one instance.
pub fn plan_start(
    m: &Manifest,
    i: &Installed,
    s: &RunSpec,
    ok: &Approvals,
) -> Result<StartPlan, String> {
    let meth = m
        .methods
        .get(i.method)
        .ok_or_else(|| format!("the package has no method {}", i.method))?;
    let v = vars(m, i, s)?;
    if meth.r#type == MethodType::Existing
        && let Some(url) = meth.detect.as_ref().and_then(|d| d.url.clone())
    {
        return Ok(StartPlan::External {
            name: s.name.clone(),
            url: render(&url, &v)?,
        });
    }
    if meth.is_container() {
        return plan_docker(m, meth, i, s, ok, &v);
    }
    plan_process(m, meth, i, s, &v)
}

fn device_env(m: &Manifest, meth: &Method, s: &RunSpec, env: &mut Vec<(String, String)>) {
    let set = |env: &mut Vec<(String, String)>, k: &str, val: String| {
        if !meth.env.contains_key(k) && !env.iter().any(|(a, _)| a == k) {
            env.push((k.to_string(), val));
        }
    };
    match &s.card {
        CardSel::Gpu { index, vendor, .. } => match vendor {
            GpuKind::Cuda if m.method_gpus(meth).contains(&GpuKind::Cuda) => {
                set(env, "CUDA_VISIBLE_DEVICES", index.to_string())
            }
            GpuKind::Rocm => {
                set(env, "HIP_VISIBLE_DEVICES", index.to_string());
                set(env, "ROCR_VISIBLE_DEVICES", index.to_string());
            }
            _ => {}
        },
        CardSel::Cpu { .. } => {
            set(env, "CUDA_VISIBLE_DEVICES", String::new());
            set(env, "HIP_VISIBLE_DEVICES", String::new());
        }
    }
}

fn plan_process(
    m: &Manifest,
    meth: &Method,
    i: &Installed,
    s: &RunSpec,
    v: &Vars,
) -> Result<StartPlan, String> {
    if meth.run.is_empty() {
        return Err(format!("method {} has no run command", meth.label()));
    }
    let argv = render_all(&meth.run, v)?;
    let mut env: Vec<(String, String)> = vec![];
    for (k, t) in &meth.env {
        env.push((k.clone(), render(t, v)?));
    }
    device_env(m, meth, s, &mut env);
    if let Some(l) = &m.models
        && let Some(e) = &l.env
        && !env.iter().any(|(k, _)| k == e)
        && let Some(d) = &s.model_dir
    {
        env.push((e.clone(), d.clone()));
    }
    let cwd = match &meth.workdir {
        Some(w) => Some(render(w, v)?),
        None => i.install.as_deref().map(p),
    };
    Ok(StartPlan::Process {
        name: s.name.clone(),
        program: argv[0].clone(),
        args: argv[1..].to_vec(),
        env,
        cwd,
        limits: s.limits.clone(),
    })
}

fn plan_docker(
    m: &Manifest,
    meth: &Method,
    i: &Installed,
    s: &RunSpec,
    ok: &Approvals,
    v: &Vars,
) -> Result<StartPlan, String> {
    if meth.r#type == MethodType::Compose {
        return plan_compose(m, meth, i, s, ok, v);
    }
    let image = i
        .image
        .clone()
        .or_else(|| meth.image.clone())
        .or_else(|| meth.detect.as_ref().and_then(|d| d.image.clone()))
        .ok_or("the method names no image")?;
    let mut a: Vec<String> = vec![
        "docker".into(),
        "run".into(),
        "-d".into(),
        "--name".into(),
        s.name.clone(),
        "--restart".into(),
        "on-failure:5".into(),
        "--label".into(),
        format!("abhoth.engine={}", s.engine),
        "--label".into(),
        format!("abhoth.package={}", m.key()),
        "--label".into(),
        format!("abhoth.card={}", s.card.id()),
    ];
    let sec = &m.security;
    if sec.privileged {
        if !ok.privileged {
            return Err(
                "the package asks for a privileged container and it is not approved".into(),
            );
        }
        a.push("--privileged".into());
    }
    if sec.host_network {
        if !ok.host_network {
            return Err("the package asks for the host network and it is not approved".into());
        }
        a.push("--network".into());
        a.push("host".into());
    } else {
        for (n, port) in meth.ports.iter().enumerate() {
            let Some(cp) = port.container else { continue };
            let hp = if n == 0 {
                Some(s.port)
            } else {
                s.ports.get(&port.id).copied()
            };
            if let Some(hp) = hp {
                a.push("-p".into());
                a.push(format!("{}:{hp}:{cp}", s.host));
            }
        }
    }
    for d in &sec.devices {
        if !ok.device_ok(d) {
            return Err(format!("the device {d} is not approved"));
        }
        a.push("--device".into());
        a.push(d.clone());
    }
    for c in &sec.cap_add {
        if !ok.cap_ok(c) {
            return Err(format!("the capability {c} is not approved"));
        }
        a.push("--cap-add".into());
        a.push(c.clone());
    }
    let gpus = m.method_gpus(meth);
    let mut env: Vec<(String, String)> = vec![];
    match &s.card {
        CardSel::Gpu {
            index,
            uuid,
            vendor: GpuKind::Cuda,
            ..
        } if gpus.contains(&GpuKind::Cuda) => {
            let dev = if uuid.is_empty() {
                index.to_string()
            } else {
                uuid.clone()
            };
            a.push("--gpus".into());
            a.push(format!("device={dev}"));
        }
        CardSel::Gpu {
            index,
            vendor: GpuKind::Rocm,
            ..
        } if gpus.contains(&GpuKind::Rocm) => {
            for d in ["/dev/kfd", "/dev/dri"] {
                a.push("--device".into());
                a.push(d.into());
            }
            env.push(("HIP_VISIBLE_DEVICES".into(), index.to_string()));
        }
        _ => {
            // A CPU card, or a stack the method does not use: hide every GPU, also from a
            // default nvidia runtime.
            env.push(("NVIDIA_VISIBLE_DEVICES".into(), "void".into()));
            env.push(("CUDA_VISIBLE_DEVICES".into(), String::new()));
        }
    }
    if let Some(c) = v.get("cpus").filter(|c| !c.is_empty()) {
        a.push("--cpus".into());
        a.push(c.clone());
    }
    if let Some(mem) = v.get("mem_limit").filter(|c| !c.is_empty()) {
        a.push("--memory".into());
        a.push(mem.clone());
    }
    let model_dir = v.get("model_dir").cloned().unwrap_or_default();
    let state = v.get("state").cloned().unwrap_or_default();
    if let Some(l) = &m.models
        && !l.mount.is_empty()
        && !model_dir.is_empty()
    {
        a.push("-v".into());
        a.push(format!(
            "{model_dir}:{}{}",
            l.mount,
            if l.read_only { ":ro" } else { "" }
        ));
        if let Some(e) = &l.env {
            env.push((e.clone(), l.mount.clone()));
        }
    }
    let declared: Vec<String> = sec
        .host_mounts
        .iter()
        .map(|h| render(&h.source, v))
        .collect::<Result<_, _>>()?;
    for h in &sec.host_mounts {
        let src = render(&h.source, v)?;
        if src.is_empty() {
            continue;
        }
        if !ok.mount_ok(&src) {
            return Err(format!("the host mount {src} ({}) is not approved", h.why));
        }
        let tgt = render(&h.target, v)?;
        a.push("-v".into());
        a.push(format!(
            "{src}:{tgt}{}",
            if h.read_only { ":ro" } else { "" }
        ));
    }
    for mt in &meth.mounts {
        let src = render(&mt.source, v)?;
        let tgt = render(&mt.target, v)?;
        let pkg = v.get("package").cloned().unwrap_or_default();
        let pkg_ok = under(&src, &[&pkg]) && mt.read_only;
        if !under(&src, &[&model_dir, &state]) && !pkg_ok && !declared.contains(&src) {
            return Err(format!(
                "the mount {src} is outside the model and state folders and not declared"
            ));
        }
        a.push("-v".into());
        a.push(format!(
            "{src}:{tgt}{}",
            if mt.read_only { ":ro" } else { "" }
        ));
    }
    for (k, t) in &meth.env {
        env.retain(|(x, _)| x != k);
        env.push((k.clone(), render(t, v)?));
    }
    for (k, val) in env {
        a.push("-e".into());
        a.push(format!("{k}={val}"));
    }
    if let Some(w) = &meth.workdir {
        a.push("-w".into());
        a.push(render(w, v)?);
    }
    for x in &meth.docker_args {
        let r = render(x, v)?;
        if !docker_arg_ok(&r) {
            return Err(format!("the docker option {r} is not allowed"));
        }
        a.push(r);
    }
    if let Some(ep) = &meth.entrypoint {
        a.push("--entrypoint".into());
        a.push(render(ep, v)?);
    }
    a.push(image);
    a.extend(render_all(&meth.run, v)?);
    Ok(StartPlan::Docker {
        name: s.name.clone(),
        argv: a,
    })
}

/// What a compose file asks for beyond a plain container (a line scan of the template).
pub fn compose_findings(yaml: &str) -> Vec<String> {
    let mut out = vec![];
    for l in yaml.lines() {
        let t = l.trim();
        let low = t.to_ascii_lowercase().replace(' ', "");
        if low.starts_with("privileged:true") {
            out.push("privileged".into());
        } else if low.starts_with("network_mode:") && low.contains("host") {
            out.push("host_network".into());
        } else if (low.starts_with("pid:") || low.starts_with("ipc:")) && low.contains("host") {
            out.push(format!("host namespace ({t})"));
        } else if low.starts_with("cap_add:") {
            out.push("cap_add".into());
        } else if low.starts_with("devices:") {
            out.push("devices".into());
        }
        if low.contains("docker.sock") {
            out.push("the docker socket".into());
        }
    }
    out
}

/// Bind-mount sources in a compose template: the items of every `volumes:` list under a
/// service (`- /host:/ctr`, `- source: /host`). Top-level named volumes are not binds.
pub fn compose_bind_sources(yaml: &str) -> Vec<String> {
    let mut out = vec![];
    let mut in_vol: Option<usize> = None;
    for l in yaml.lines() {
        if l.trim().is_empty() || l.trim_start().starts_with('#') {
            continue;
        }
        let indent = l.len() - l.trim_start().len();
        let t = l.trim();
        if let Some(vi) = in_vol
            && indent <= vi
            && !t.starts_with('-')
        {
            in_vol = None;
        }
        if t == "volumes:" {
            in_vol = if indent > 0 { Some(indent) } else { None };
            continue;
        }
        if in_vol.is_none() {
            continue;
        }
        let item = t.strip_prefix("- ").map(|x| x.trim()).unwrap_or(t);
        if let Some(x) = item.strip_prefix("source:") {
            out.push(x.trim().trim_matches(QUOTES).to_string());
        } else if t.starts_with("- ") {
            let x = item.trim_matches(QUOTES);
            if x.contains(':') && !x.starts_with("type:") {
                out.push(split_mount_source(x));
            }
        }
    }
    out
}

const QUOTES: [char; 2] = ['"', '\''];

fn split_mount_source(x: &str) -> String {
    let mut depth = 0;
    let b = x.as_bytes();
    let mut k = 0;
    while k < b.len() {
        if b[k..].starts_with(b"{{") {
            depth += 1;
            k += 2;
            continue;
        }
        if b[k..].starts_with(b"}}") {
            depth -= 1;
            k += 2;
            continue;
        }
        if b[k] == b':' && depth == 0 {
            return x[..k].to_string();
        }
        k += 1;
    }
    x.to_string()
}

fn plan_compose(
    m: &Manifest,
    meth: &Method,
    i: &Installed,
    s: &RunSpec,
    ok: &Approvals,
    v: &Vars,
) -> Result<StartPlan, String> {
    let text = match (&meth.compose, &meth.compose_file) {
        (Some(t), _) => t.clone(),
        (None, Some(f)) => std::fs::read_to_string(i.dir.join("package").join(f))
            .map_err(|e| format!("the compose template {f}: {e}"))?,
        (None, None) => return Err("the compose method has no compose file".into()),
    };
    for (k, val) in v {
        if val.contains('\n') || val.contains('"') {
            return Err(format!(
                "the value of {k} may not contain a newline or a quote"
            ));
        }
    }
    let sec = &m.security;
    for f in compose_findings(&text) {
        let allowed = match f.as_str() {
            "privileged" => sec.privileged && ok.privileged,
            "host_network" => sec.host_network && ok.host_network,
            "devices" => !sec.devices.is_empty() && sec.devices.iter().all(|d| ok.device_ok(d)),
            "cap_add" => !sec.cap_add.is_empty() && sec.cap_add.iter().all(|c| ok.cap_ok(c)),
            _ => false,
        };
        if !allowed {
            return Err(format!(
                "the compose file asks for {f}, which is not declared and approved"
            ));
        }
    }
    let yaml = render(&text, v)?;
    let model_dir = v.get("model_dir").cloned().unwrap_or_default();
    let state = v.get("state").cloned().unwrap_or_default();
    let declared: Vec<String> = sec
        .host_mounts
        .iter()
        .map(|h| render(&h.source, v))
        .collect::<Result<_, _>>()?;
    let pkg = v.get("package").cloned().unwrap_or_default();
    for src in compose_bind_sources(&yaml) {
        if !under(&src, &[&model_dir, &state, &pkg]) {
            if !declared.contains(&src) {
                return Err(format!(
                    "the compose file mounts {src}, which is not declared"
                ));
            }
            if !ok.mount_ok(&src) {
                return Err(format!("the host mount {src} is not approved"));
            }
        }
    }
    let project = sanitize_name(&s.name);
    let base = |verb: &[&str]| -> Vec<String> {
        let mut a: Vec<String> = ["docker", "compose", "-p", &project, "-f", COMPOSE_FILE]
            .iter()
            .map(|x| x.to_string())
            .collect();
        a.extend(verb.iter().map(|x| x.to_string()));
        a
    };
    Ok(StartPlan::Compose {
        project: project.clone(),
        yaml,
        up: base(&["up", "-d"]),
        down: base(&["down"]),
    })
}
