//! The runner (ROUTER-MEDIA-PLAN.md 13.3): installs, starts, stops, updates and uninstalls
//! engine packages, shared by the router and the bridge.
//!
//! Docker and compose engines run through [`HostExec`] (local docker for a bridge, hostctl for
//! the router on DeepThought). Host-process engines (pip, uv, binary, git, existing programs)
//! are children of this process, supervised by [`supervisor::Supervisor`]. Every artifact is
//! fetched and checked against its pin at install. State lives under
//! `<state>/engines/` (see [`Layout`]), so a restarted router or bridge adopts what runs.

pub mod proc;
pub mod supervisor;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use serde_json::Value;

use crate::exec::{ExecOpts, Fetch, HostExec, scrub};
use crate::host::{self, CardSel, HostFacts};
use crate::manifest::{MANIFEST_FILE, Manifest, Method, MethodType, Step};
use crate::pins;
use crate::render::{self, COMPOSE_FILE, RunSpec, StartPlan, Vars};
use crate::state::{Approvals, Handle, Installed, Phase, State};
use crate::store::Package;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunError(pub String);

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RunError {}

impl From<String> for RunError {
    fn from(s: String) -> Self {
        RunError(s)
    }
}
impl From<&str> for RunError {
    fn from(s: &str) -> Self {
        RunError(s.into())
    }
}

type R<T> = Result<T, RunError>;

#[async_trait]
pub trait Runner: Send + Sync {
    /// Install a package's method for a card (the card picks a binary build).
    async fn install(
        &self,
        p: &Package,
        method: usize,
        card: &CardSel,
        ok: &Approvals,
    ) -> R<Installed>;
    /// Install the new version, then run its update steps. The old one stays until uninstalled.
    async fn update(
        &self,
        old: &Installed,
        p: &Package,
        method: usize,
        card: &CardSel,
        ok: &Approvals,
    ) -> R<Installed>;
    async fn uninstall(&self, m: &Manifest, i: &Installed) -> R<()>;
    async fn start(&self, m: &Manifest, i: &Installed, s: &RunSpec) -> R<Handle>;
    async fn stop(&self, h: &Handle, grace: Duration) -> R<()>;
    async fn status(&self, h: &Handle) -> R<State>;
    async fn logs(&self, h: &Handle, lines: u32) -> R<String>;
    /// The handles in the state dir that still run (after a restart).
    async fn adopt(&self) -> Vec<Handle>;
}

/// Where things live under the state dir.
#[derive(Debug, Clone)]
pub struct Layout {
    pub root: PathBuf,
}

impl Layout {
    pub fn engines(&self) -> PathBuf {
        self.root.join("engines")
    }
    pub fn version_dir(&self, id: &str, version: &str) -> PathBuf {
        self.engines().join(id).join(version)
    }
    pub fn installed_file(&self, id: &str, version: &str) -> PathBuf {
        self.version_dir(id, version).join("installed.json")
    }
    pub fn run_dir(&self) -> PathBuf {
        self.engines().join("run")
    }
    pub fn handle_file(&self, name: &str) -> PathBuf {
        self.run_dir().join(format!("{name}.json"))
    }
    pub fn log_file(&self, name: &str) -> PathBuf {
        self.engines().join("logs").join(format!("{name}.log"))
    }
    pub fn data_dir(&self, name: &str) -> PathBuf {
        self.engines().join("data").join(name)
    }
    pub fn packages(&self) -> PathBuf {
        self.root.join("packages")
    }

    /// Every installed record.
    pub fn list_installed(&self) -> Vec<Installed> {
        let mut out = vec![];
        let Ok(ids) = std::fs::read_dir(self.engines()) else {
            return out;
        };
        for id in ids.flatten() {
            let n = id.file_name().to_string_lossy().to_string();
            if ["run", "logs", "data"].contains(&n.as_str()) {
                continue;
            }
            if let Ok(vs) = std::fs::read_dir(id.path()) {
                for v in vs.flatten() {
                    let f = v.path().join("installed.json");
                    if let Ok(t) = std::fs::read_to_string(&f)
                        && let Ok(i) = serde_json::from_str::<Installed>(&t)
                    {
                        out.push(i);
                    }
                }
            }
        }
        out.sort_by(|a, b| (&a.id, &a.version).cmp(&(&b.id, &b.version)));
        out
    }

    /// The manifest saved with an install.
    pub fn installed_manifest(&self, i: &Installed) -> Result<Manifest, String> {
        let t = std::fs::read_to_string(i.dir.join("package").join(MANIFEST_FILE))
            .map_err(|e| format!("{}: {e}", i.dir.display()))?;
        Manifest::parse(&t).map_err(|e| {
            e.iter()
                .map(|x| x.to_string())
                .collect::<Vec<_>>()
                .join("; ")
        })
    }

    pub fn find_installed(&self, id: &str, version: &str) -> Option<Installed> {
        let t = std::fs::read_to_string(self.installed_file(id, version)).ok()?;
        serde_json::from_str(&t).ok()
    }
}

pub struct LocalRunner {
    pub layout: Layout,
    exec: Arc<dyn HostExec>,
    fetch: Arc<dyn Fetch>,
    /// pip/uv/binary/git/existing programs allowed (false in the router's container).
    host_processes: bool,
    facts: HostFacts,
    sup: supervisor::Supervisor,
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn s(p: &Path) -> String {
    p.to_string_lossy().to_string()
}

fn argv(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

impl LocalRunner {
    pub fn new(
        state_dir: PathBuf,
        exec: Arc<dyn HostExec>,
        fetch: Arc<dyn Fetch>,
        host_processes: bool,
        facts: HostFacts,
    ) -> LocalRunner {
        LocalRunner {
            layout: Layout { root: state_dir },
            exec,
            fetch,
            host_processes,
            facts,
            sup: supervisor::Supervisor::default(),
        }
    }

    pub fn with_policy(mut self, p: supervisor::Policy) -> Self {
        self.sup = supervisor::Supervisor::new(p);
        self
    }

    pub fn facts(&self) -> &HostFacts {
        &self.facts
    }

    async fn run(&self, a: Vec<String>, o: ExecOpts) -> R<String> {
        let r = self.exec.run(&a, &o).await.map_err(RunError)?;
        if r.ok() {
            Ok(r.stdout)
        } else {
            Err(RunError(format!(
                "{}: {}",
                scrub(&a.join(" ")),
                r.summary()
            )))
        }
    }

    fn check_approvals(&self, p: &Package, meth: &Method, ok: &Approvals) -> R<()> {
        if !p.trusted && !ok.untrusted {
            return Err("the package comes from an untrusted store; approve it to install".into());
        }
        if meth.r#type.runs_on_host() && !ok.host_run {
            return Err(format!(
                "the {} method runs on the host, not sandboxed; approve it to install",
                meth.label()
            )
            .into());
        }
        if meth.is_host_process() && !self.host_processes {
            return Err(
                "this host runs engines only in containers (docker or compose methods)".into(),
            );
        }
        let sec = &p.manifest.security;
        if sec.privileged && !ok.privileged {
            return Err(
                "the package asks for a privileged container; approve it to install".into(),
            );
        }
        if sec.host_network && !ok.host_network {
            return Err("the package asks for the host network; approve it to install".into());
        }
        for d in &sec.devices {
            if !ok.device_ok(d) {
                return Err(format!("the device {d} needs approval").into());
            }
        }
        for c in &sec.cap_add {
            if !ok.cap_ok(c) {
                return Err(format!("the capability {c} needs approval").into());
            }
        }
        Ok(())
    }

    /// Variables for install steps (no card or port yet).
    fn install_vars(&self, m: &Manifest, i: &Installed) -> Vars {
        let mut v = Vars::new();
        for k in render::KNOWN_VARS {
            v.insert(k.to_string(), String::new());
        }
        v.insert(
            "install".into(),
            i.install.as_deref().map(s).unwrap_or_default(),
        );
        v.insert("bin".into(), i.bin.as_deref().map(s).unwrap_or_default());
        v.insert("venv".into(), i.venv.as_deref().map(s).unwrap_or_default());
        v.insert(
            "python".into(),
            i.venv
                .as_deref()
                .map(|x| s(&render::venv_python(x, &i.os)))
                .unwrap_or_default(),
        );
        v.insert("state".into(), s(&i.dir.join("data")));
        v.insert(
            "exe".into(),
            if i.os == "windows" {
                ".exe".into()
            } else {
                String::new()
            },
        );
        for (k, p) in &m.params {
            let d = match &p.default {
                Some(Value::String(x)) => x.clone(),
                Some(x) => x.to_string(),
                None => String::new(),
            };
            v.insert(format!("param.{k}"), d);
        }
        v
    }

    async fn run_steps(
        &self,
        m: &Manifest,
        i: &Installed,
        steps: &[Step],
        default_cwd: Option<&Path>,
    ) -> R<()> {
        let v = self.install_vars(m, i);
        for st in steps {
            if !st.platforms.is_empty()
                && !st
                    .platforms
                    .iter()
                    .any(|p| host::platform_matches(p, &self.facts.os, &self.facts.arch))
            {
                continue;
            }
            let a: Vec<String> = st
                .run
                .iter()
                .map(|x| render::render(x, &v))
                .collect::<Result<_, _>>()?;
            let mut env = vec![];
            for (k, t) in &st.env {
                env.push((k.clone(), render::render(t, &v)?));
            }
            let cwd = match &st.cwd {
                Some(c) => Some(render::render(c, &v)?),
                None => default_cwd.map(s),
            };
            self.run(
                a,
                ExecOpts {
                    cwd,
                    env,
                    timeout: Some(Duration::from_secs(3600)),
                },
            )
            .await?;
        }
        Ok(())
    }

    fn write_package(&self, p: &Package, dir: &Path) -> R<()> {
        let pd = dir.join("package");
        for (rel, text) in &p.files {
            if rel.contains("..") || rel.starts_with('/') || rel.contains(':') {
                return Err(format!("{rel}: a bad file name").into());
            }
            let f = pd.join(rel);
            if let Some(parent) = f.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            std::fs::write(&f, text).map_err(|e| format!("{}: {e}", f.display()))?;
        }
        if !p.files.contains_key(MANIFEST_FILE) {
            std::fs::create_dir_all(&pd).map_err(|e| e.to_string())?;
            std::fs::write(pd.join(MANIFEST_FILE), p.manifest.to_json_pretty())
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// Find a Python of the wanted version for a venv.
    async fn find_python(&self, want: Option<&str>) -> R<Vec<String>> {
        let mut cands: Vec<Vec<String>> = vec![];
        if let Some(w) = want {
            if self.facts.os == "windows" {
                cands.push(argv(&["py", &format!("-{w}")]));
            }
            cands.push(vec![format!("python{w}")]);
        }
        cands.push(argv(&["python3"]));
        cands.push(argv(&["python"]));
        for c in cands {
            let mut a = c.clone();
            a.extend(argv(&[
                "-c",
                "import sys;print('%d.%d'%sys.version_info[:2])",
            ]));
            if let Ok(r) = self.exec.run(&a, &ExecOpts::timeout(30)).await
                && r.ok()
            {
                let got = r.stdout.trim().to_string();
                if want.is_none_or(|w| got == w || got.starts_with(&format!("{w}."))) {
                    return Ok(c);
                }
            }
        }
        Err(format!("no Python {} found on this host", want.unwrap_or("3")).into())
    }

    /// The requirements file for pip/uv, from `package`, `requirements` and `requirements_file`.
    fn requirements(&self, p: &Package, meth: &Method) -> (String, bool) {
        let mut lines: Vec<String> = vec![];
        lines.extend(meth.package.clone());
        lines.extend(meth.requirements.iter().cloned());
        if let Some(f) = &meth.requirements_file
            && let Some(t) = p.files.get(f)
        {
            lines.extend(
                t.lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty() && !l.starts_with('#'))
                    .map(str::to_string),
            );
        }
        let hashed = !lines.is_empty() && lines.iter().all(|l| pins::requirement_hashed(l));
        (lines.join("\n") + "\n", hashed)
    }

    async fn install_python(
        &self,
        p: &Package,
        meth: &Method,
        i: &mut Installed,
        uv: bool,
    ) -> R<()> {
        let venv = i.dir.join("venv");
        let (req, hashed) = self.requirements(p, meth);
        let req_file = i.dir.join("requirements.txt");
        std::fs::write(&req_file, &req).map_err(|e| e.to_string())?;
        let py = render::venv_python(&venv, &i.os);
        if uv {
            let mut a = argv(&["uv", "venv"]);
            if let Some(v) = &meth.python {
                a.extend(argv(&["--python", v]));
            }
            a.push(s(&venv));
            self.run(a, ExecOpts::timeout(900)).await?;
            let mut a = argv(&["uv", "pip", "install", "--python", &s(&py)]);
            if hashed {
                a.push("--require-hashes".into());
            }
            for u in &meth.extra_index_urls {
                a.extend(argv(&["--extra-index-url", u]));
            }
            a.extend(argv(&["-r", &s(&req_file)]));
            self.run(a, ExecOpts::timeout(3600)).await?;
        } else {
            let mut a = self.find_python(meth.python.as_deref()).await?;
            a.extend(argv(&["-m", "venv", &s(&venv)]));
            self.run(a, ExecOpts::timeout(900)).await?;
            let mut a = vec![s(&py)];
            a.extend(argv(&[
                "-m",
                "pip",
                "install",
                "--no-input",
                "--disable-pip-version-check",
            ]));
            if hashed {
                a.push("--require-hashes".into());
            }
            for u in &meth.extra_index_urls {
                a.extend(argv(&["--extra-index-url", u]));
            }
            a.extend(argv(&["-r", &s(&req_file)]));
            self.run(a, ExecOpts::timeout(3600)).await?;
        }
        for l in req.lines().filter(|l| !l.is_empty()) {
            i.pins.push(format!(
                "pip {}",
                l.split("--hash").next().unwrap_or(l).trim()
            ));
        }
        if hashed {
            i.pins.push("pip --require-hashes".into());
        }
        i.venv = Some(venv);
        Ok(())
    }

    fn copy_files(&self, p: &Package, meth: &Method, dest: &Path) -> R<()> {
        for f in &meth.files {
            let Some(t) = p.files.get(f) else {
                return Err(format!("{f} is not in the package").into());
            };
            let to = dest.join(f);
            if let Some(par) = to.parent() {
                std::fs::create_dir_all(par).map_err(|e| e.to_string())?;
            }
            std::fs::write(&to, t).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    async fn install_binary(
        &self,
        p: &Package,
        meth: &Method,
        card: &CardSel,
        i: &mut Installed,
    ) -> R<()> {
        let assets = host::pick_assets(&p.manifest, meth, &self.facts, card);
        if assets.is_empty() {
            return Err(format!("no download for {}/{}", self.facts.os, self.facts.arch).into());
        }
        let bin = i.dir.join("bin");
        std::fs::create_dir_all(&bin).map_err(|e| e.to_string())?;
        for a in assets {
            if !pins::is_sha256(&a.sha256) {
                return Err("the download has no sha256 pin".into());
            }
            let fname = a
                .url
                .rsplit('/')
                .next()
                .filter(|x| !x.is_empty() && !x.contains(".."))
                .unwrap_or("download");
            let dl = i.dir.join("download").join(fname);
            self.fetch.download(&a.url, &dl, Some(&a.sha256)).await?;
            pins::verify_file(&dl, &a.sha256)?;
            i.pins.push(format!("sha256:{} {}", a.sha256, a.url));
            let strip = a.strip.unwrap_or(0) as usize;
            match a.unpack_kind() {
                "zip" => unzip(&dl, &bin, strip)?,
                "tar.gz" | "tar.xz" => {
                    let mut t = argv(&["tar", "-xf", &s(&dl), "-C", &s(&bin)]);
                    if strip > 0 {
                        t.push(format!("--strip-components={strip}"));
                    }
                    self.run(t, ExecOpts::timeout(600)).await?;
                }
                _ => {
                    std::fs::copy(&dl, bin.join(fname)).map_err(|e| e.to_string())?;
                }
            }
            let _ = std::fs::remove_file(&dl);
        }
        #[cfg(unix)]
        make_executable(&bin);
        i.install = Some(bin);
        Ok(())
    }

    async fn install_git(&self, p: &Package, meth: &Method, i: &mut Installed) -> R<()> {
        let repo = meth.repo.as_deref().ok_or("no repo")?;
        let commit = meth.commit.as_deref().ok_or("no commit")?;
        if !pins::is_commit(commit) {
            return Err("the commit must be a full 40-hex hash".into());
        }
        let src = i.dir.join("src");
        self.run(
            argv(&["git", "clone", "--no-checkout", "--", repo, &s(&src)]),
            ExecOpts::timeout(1800),
        )
        .await?;
        self.run(
            argv(&["git", "-C", &s(&src), "checkout", "--detach", commit]),
            ExecOpts::timeout(600),
        )
        .await?;
        let head = self
            .run(
                argv(&["git", "-C", &s(&src), "rev-parse", "HEAD"]),
                ExecOpts::timeout(60),
            )
            .await?;
        if head.trim() != commit {
            return Err(format!(
                "the checkout is at {}, not the pinned {commit}",
                head.trim()
            )
            .into());
        }
        i.pins.push(format!("git {commit} {repo}"));
        i.install = Some(src.clone());
        if meth.python.is_some()
            || !meth.requirements.is_empty()
            || meth.requirements_file.is_some()
        {
            self.install_python(p, meth, i, false).await?;
        }
        Ok(())
    }

    async fn install_existing(&self, m: &Manifest, meth: &Method, i: &mut Installed) -> R<()> {
        let d = meth.detect.clone().unwrap_or_default();
        if let Some(img) = &d.image {
            self.run(
                argv(&["docker", "image", "inspect", "--format", "{{.Id}}", img]),
                ExecOpts::timeout(60),
            )
            .await
            .map_err(|_| RunError(format!("the image {img} is not on this host")))?;
            i.image = Some(img.clone());
            i.pins.push(format!("existing image {img}"));
            return Ok(());
        }
        if let Some(url) = &d.url {
            i.pins.push(format!("existing endpoint {url}"));
            return Ok(());
        }
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .unwrap_or_default();
        let v = self.install_vars(m, i);
        let mut found: Option<PathBuf> = None;
        for pth in &d.paths {
            let r = render::render(pth, &v)?;
            let r = if let Some(rest) = r.strip_prefix("~/") {
                format!("{home}/{rest}")
            } else {
                r
            };
            let pb = PathBuf::from(&r);
            if pb.is_file() {
                found = Some(pb);
                break;
            }
        }
        if found.is_none()
            && let Some(c) = &d.command
        {
            found = host::find_on_path(c);
        }
        let bin = found.ok_or_else(|| {
            format!(
                "not found: {}",
                d.command.clone().unwrap_or_else(|| d.paths.join(", "))
            )
        })?;
        let args = d
            .version_args
            .clone()
            .unwrap_or_else(|| vec!["--version".into()]);
        let mut a = vec![s(&bin)];
        a.extend(args);
        let r = self
            .exec
            .run(&a, &ExecOpts::timeout(30))
            .await
            .map_err(RunError)?;
        let text = format!("{}\n{}", r.stdout, r.stderr);
        if let Some(re) = &d.version_regex {
            let re = Regex::new(re).map_err(|e| e.to_string())?;
            let Some(c) = re.captures(&text) else {
                return Err(format!(
                    "{} reports a version that does not match {}",
                    bin.display(),
                    re.as_str()
                )
                .into());
            };
            i.found_version = Some(
                c.get(1)
                    .or_else(|| c.get(0))
                    .map(|x| x.as_str().to_string())
                    .unwrap_or_default(),
            );
        }
        i.pins.push(format!("existing {}", bin.display()));
        i.install = bin.parent().map(Path::to_path_buf);
        i.bin = Some(bin);
        Ok(())
    }

    fn save_handle(&self, h: &Handle) {
        let f = self.layout.handle_file(h.name());
        if let Some(p) = f.parent() {
            let _ = std::fs::create_dir_all(p);
        }
        let _ = std::fs::write(f, serde_json::to_string_pretty(h).unwrap_or_default());
    }

    async fn http_ok(url: &str) -> bool {
        let c = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap_or_default();
        c.get(url)
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
    }
}

fn unzip(file: &Path, dest: &Path, strip: usize) -> R<()> {
    let f = std::fs::File::open(file).map_err(|e| e.to_string())?;
    let mut z = zip::ZipArchive::new(f).map_err(|e| format!("{}: {e}", file.display()))?;
    for k in 0..z.len() {
        let mut e = z.by_index(k).map_err(|e| e.to_string())?;
        let Some(name) = e.enclosed_name() else {
            return Err(format!(
                "the archive has an unsafe path: {}",
                String::from_utf8_lossy(e.name_raw())
            )
            .into());
        };
        let parts: Vec<_> = name.components().skip(strip).collect();
        if parts.is_empty() {
            continue;
        }
        let out = parts.iter().fold(dest.to_path_buf(), |acc, c| acc.join(c));
        if e.is_dir() {
            std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
            continue;
        }
        if let Some(p) = out.parent() {
            std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
        }
        let mut w = std::fs::File::create(&out).map_err(|e| e.to_string())?;
        std::io::copy(&mut e, &mut w).map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(unix)]
fn make_executable(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            make_executable(&p);
        } else if let Ok(m) = std::fs::metadata(&p) {
            let mut perm = m.permissions();
            perm.set_mode(perm.mode() | 0o755);
            let _ = std::fs::set_permissions(&p, perm);
        }
    }
}

#[async_trait]
impl Runner for LocalRunner {
    async fn install(
        &self,
        p: &Package,
        method: usize,
        card: &CardSel,
        ok: &Approvals,
    ) -> R<Installed> {
        let m = &p.manifest;
        let meth = m
            .methods
            .get(method)
            .ok_or_else(|| format!("the package has no method {method}"))?;
        self.check_approvals(p, meth, ok)?;
        let dir = self.layout.version_dir(&m.id, &m.version);
        if dir.exists() {
            std::fs::remove_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        std::fs::create_dir_all(dir.join("data")).map_err(|e| format!("{}: {e}", dir.display()))?;
        self.write_package(p, &dir)?;
        let mut i = Installed {
            id: m.id.clone(),
            version: m.version.clone(),
            method,
            method_type: meth.r#type,
            dir: dir.clone(),
            os: self.facts.os.clone(),
            venv: None,
            install: None,
            bin: None,
            image: None,
            pins: vec![],
            found_version: None,
            at: now(),
            approvals: ok.clone(),
            store: p.store.clone(),
        };
        let result: R<()> = async {
            match meth.r#type {
                MethodType::Docker => {
                    let img = meth.image.clone().ok_or("no image")?;
                    self.run(argv(&["docker", "pull", &img]), ExecOpts::timeout(3600))
                        .await?;
                    i.pins.push(format!("image {img}"));
                    i.image = Some(img);
                }
                MethodType::Compose => {
                    let text = match (&meth.compose, &meth.compose_file) {
                        (Some(t), _) => t.clone(),
                        (None, Some(f)) => p
                            .files
                            .get(f)
                            .cloned()
                            .ok_or("the compose file is missing")?,
                        _ => return Err("no compose file".into()),
                    };
                    for img in pins::compose_images(&text) {
                        if img.contains("{{") {
                            continue;
                        }
                        self.run(argv(&["docker", "pull", &img]), ExecOpts::timeout(3600))
                            .await?;
                        i.pins.push(format!("image {img}"));
                    }
                }
                MethodType::Pip | MethodType::Uv => {
                    self.install_python(p, meth, &mut i, meth.r#type == MethodType::Uv)
                        .await?;
                    if !meth.files.is_empty() {
                        let app = dir.join("app");
                        self.copy_files(p, meth, &app)?;
                        i.install = Some(app);
                    } else {
                        i.install = Some(dir.clone());
                    }
                }
                MethodType::Conda => return Err("conda installs are not supported yet".into()),
                MethodType::Binary => self.install_binary(p, meth, card, &mut i).await?,
                MethodType::Git => {
                    self.install_git(p, meth, &mut i).await?;
                    if let Some(src) = i.install.clone() {
                        self.copy_files(p, meth, &src)?;
                    }
                }
                MethodType::Existing => self.install_existing(m, meth, &mut i).await?,
            }
            let cwd = i.install.clone();
            self.run_steps(m, &i, &meth.build, cwd.as_deref()).await?;
            Ok(())
        }
        .await;
        if let Err(e) = result {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(e);
        }
        std::fs::write(
            self.layout.installed_file(&m.id, &m.version),
            serde_json::to_string_pretty(&i).unwrap_or_default(),
        )
        .map_err(|e| e.to_string())?;
        Ok(i)
    }

    async fn update(
        &self,
        old: &Installed,
        p: &Package,
        method: usize,
        card: &CardSel,
        ok: &Approvals,
    ) -> R<Installed> {
        if old.id != p.manifest.id {
            return Err("an update must be the same package".into());
        }
        if old.version == p.manifest.version {
            return Err(format!("{} is already installed", p.key()).into());
        }
        let i = self.install(p, method, card, ok).await?;
        let meth = &p.manifest.methods[method];
        let cwd = i.install.clone();
        self.run_steps(&p.manifest, &i, &meth.update, cwd.as_deref())
            .await?;
        Ok(i)
    }

    async fn uninstall(&self, m: &Manifest, i: &Installed) -> R<()> {
        if let Some(meth) = m.methods.get(i.method) {
            let cwd = i.install.clone();
            self.run_steps(m, i, &meth.uninstall, cwd.as_deref())
                .await?;
        }
        // Only ever remove our own folder. Docker images are left: layers are shared, and the
        // admin removes images by hand.
        let root = self.layout.engines();
        let dir = i.dir.canonicalize().unwrap_or(i.dir.clone());
        let root = root.canonicalize().unwrap_or(root);
        if !dir.starts_with(&root) || dir == root {
            return Err(format!("{} is not under the engines folder", i.dir.display()).into());
        }
        std::fs::remove_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        Ok(())
    }

    async fn start(&self, m: &Manifest, i: &Installed, spec: &RunSpec) -> R<Handle> {
        let mut spec = spec.clone();
        if spec.state.is_none() {
            spec.state = Some(s(&self.layout.data_dir(&spec.name)));
        }
        if let Some(st) = &spec.state {
            let _ = std::fs::create_dir_all(st);
        }
        let plan = render::plan_start(m, i, &spec, &i.approvals)?;
        let h = match plan {
            StartPlan::Docker { name, argv: a } => {
                let _ = self
                    .exec
                    .run(
                        &argv(&["docker", "rm", "-f", &name]),
                        &ExecOpts::timeout(60),
                    )
                    .await;
                self.run(a, ExecOpts::timeout(300)).await?;
                Handle::Container { name }
            }
            StartPlan::Compose {
                project, yaml, up, ..
            } => {
                let file = self.layout.run_dir().join(format!("{project}.compose.yml"));
                std::fs::create_dir_all(self.layout.run_dir()).map_err(|e| e.to_string())?;
                std::fs::write(&file, yaml).map_err(|e| e.to_string())?;
                let up: Vec<String> = up
                    .into_iter()
                    .map(|x| if x == COMPOSE_FILE { s(&file) } else { x })
                    .collect();
                self.run(up, ExecOpts::timeout(1800)).await?;
                Handle::Compose { project, file }
            }
            StartPlan::Process {
                name,
                program,
                args,
                env,
                cwd,
                limits,
            } => {
                if !self.host_processes {
                    return Err("this host runs engines only in containers".into());
                }
                let limit_memory = matches!(spec.card, CardSel::Cpu { .. });
                let (pid, started) = self
                    .sup
                    .spawn(supervisor::ProcSpec {
                        name: name.clone(),
                        program,
                        args,
                        env,
                        cwd,
                        limits,
                        limit_memory,
                        log: self.layout.log_file(&name),
                    })
                    .await?;
                Handle::Process { pid, started, name }
            }
            StartPlan::External { name, url } => Handle::External { url, name },
        };
        self.save_handle(&h);
        Ok(h)
    }

    async fn stop(&self, h: &Handle, grace: Duration) -> R<()> {
        match h {
            Handle::Container { name } => {
                let g = grace.as_secs().max(1).to_string();
                let _ = self
                    .exec
                    .run(
                        &argv(&["docker", "stop", "-t", &g, name]),
                        &ExecOpts::timeout(grace.as_secs() + 60),
                    )
                    .await;
                self.run(argv(&["docker", "rm", "-f", name]), ExecOpts::timeout(60))
                    .await?;
            }
            Handle::Compose { project, file } => {
                self.run(
                    argv(&[
                        "docker",
                        "compose",
                        "-p",
                        project,
                        "-f",
                        &s(file),
                        "down",
                        "-t",
                        &grace.as_secs().max(1).to_string(),
                    ]),
                    ExecOpts::timeout(grace.as_secs() + 120),
                )
                .await?;
            }
            Handle::Process { pid, started, name } => {
                if self.sup.supervises(name) {
                    self.sup.stop(name, grace).await;
                } else {
                    proc::stop_exact(*pid, *started, grace).await?;
                }
            }
            Handle::External { .. } => {}
        }
        let _ = std::fs::remove_file(self.layout.handle_file(h.name()));
        Ok(())
    }

    async fn status(&self, h: &Handle) -> R<State> {
        match h {
            Handle::Container { name } => {
                let r = self
                    .exec
                    .run(
                        &argv(&["docker", "inspect", "--format", "{{json .State}}", name]),
                        &ExecOpts::timeout(30),
                    )
                    .await
                    .map_err(RunError)?;
                if !r.ok() {
                    return Ok(State::of(Phase::Stopped));
                }
                Ok(container_state(r.stdout.trim()))
            }
            Handle::Compose { project, file } => {
                let r = self
                    .exec
                    .run(
                        &argv(&[
                            "docker",
                            "compose",
                            "-p",
                            project,
                            "-f",
                            &s(file),
                            "ps",
                            "--format",
                            "json",
                        ]),
                        &ExecOpts::timeout(30),
                    )
                    .await
                    .map_err(RunError)?;
                let t = r.stdout.trim();
                if !r.ok() || t.is_empty() || t == "[]" {
                    return Ok(State::of(Phase::Stopped));
                }
                let running = t.contains("\"running\"");
                let healthy = t.contains("\"healthy\"");
                let unhealthy = t.contains("\"unhealthy\"");
                Ok(State::of(if unhealthy {
                    Phase::Unhealthy
                } else if healthy {
                    Phase::Healthy
                } else if running {
                    Phase::Running
                } else {
                    Phase::Exited
                }))
            }
            Handle::Process { pid, started, name } => {
                if let Some(st) = self.sup.state(name) {
                    return Ok(st);
                }
                let mut st = State::of(if proc::alive(*pid, *started).await {
                    Phase::Running
                } else {
                    Phase::Stopped
                });
                if st.phase == Phase::Running {
                    st.pid = Some(*pid);
                    st.notes
                        .push("adopted after a restart: not restarted if it exits".into());
                }
                Ok(st)
            }
            Handle::External { url, .. } => Ok(State::of(if Self::http_ok(url).await {
                Phase::Healthy
            } else {
                Phase::Unknown
            })),
        }
    }

    async fn logs(&self, h: &Handle, lines: u32) -> R<String> {
        let n = lines.clamp(1, 5000).to_string();
        let t = match h {
            Handle::Container { name } => {
                let r = self
                    .exec
                    .run(
                        &argv(&["docker", "logs", "--tail", &n, name]),
                        &ExecOpts::timeout(30),
                    )
                    .await
                    .map_err(RunError)?;
                format!("{}{}", r.stdout, r.stderr)
            }
            Handle::Compose { project, file } => {
                let r = self
                    .exec
                    .run(
                        &argv(&[
                            "docker",
                            "compose",
                            "-p",
                            project,
                            "-f",
                            &s(file),
                            "logs",
                            "--no-color",
                            "--tail",
                            &n,
                        ]),
                        &ExecOpts::timeout(30),
                    )
                    .await
                    .map_err(RunError)?;
                format!("{}{}", r.stdout, r.stderr)
            }
            Handle::Process { name, .. } => supervisor::tail(&self.layout.log_file(name), lines),
            Handle::External { url, .. } => format!("{url} runs outside abhoth: no logs here"),
        };
        Ok(scrub(&t))
    }

    async fn adopt(&self) -> Vec<Handle> {
        let mut out = vec![];
        let Ok(rd) = std::fs::read_dir(self.layout.run_dir()) else {
            return out;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("json") {
                continue;
            }
            let Ok(t) = std::fs::read_to_string(&p) else {
                continue;
            };
            let Ok(h) = serde_json::from_str::<Handle>(&t) else {
                continue;
            };
            let keep = match &h {
                Handle::Process { pid, started, .. } => proc::alive(*pid, *started).await,
                _ => true,
            };
            if keep {
                out.push(h);
            } else {
                let _ = std::fs::remove_file(&p);
            }
        }
        out
    }
}

/// `docker inspect --format '{{json .State}}'` → a state.
pub fn container_state(json: &str) -> State {
    let v: Value = serde_json::from_str(json).unwrap_or(Value::Null);
    let status = v["Status"].as_str().unwrap_or("");
    let health = v["Health"]["Status"].as_str().unwrap_or("");
    let phase = match (status, health) {
        ("running", "healthy") => Phase::Healthy,
        ("running", "unhealthy") => Phase::Unhealthy,
        ("running", "starting") => Phase::Starting,
        ("running", _) => Phase::Running,
        ("restarting", _) => Phase::Backoff,
        ("exited", _) | ("dead", _) => Phase::Exited,
        ("created", _) => Phase::Starting,
        _ => Phase::Unknown,
    };
    let mut st = State::of(phase);
    st.pid = v["Pid"].as_u64().filter(|p| *p > 0).map(|p| p as u32);
    st.last_exit = v["ExitCode"].as_i64().map(|c| c as i32);
    st
}
