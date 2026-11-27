//! How the runner runs a command on the host, and how it fetches artifacts.
//!
//! [`HostExec`] is local for the bridge (and a router with `exec = "local"`); the router on
//! DeepThought gives the runner its hostctl/ssh executor instead, so `docker` and `compose`
//! commands run on the host while the router stays in its container. Commands are argv, never
//! shell text.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Default)]
pub struct ExecOpts {
    pub cwd: Option<String>,
    pub env: Vec<(String, String)>,
    pub timeout: Option<Duration>,
}

impl ExecOpts {
    pub fn timeout(secs: u64) -> ExecOpts {
        ExecOpts {
            timeout: Some(Duration::from_secs(secs)),
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecResult {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl ExecResult {
    pub fn ok(&self) -> bool {
        self.code == 0
    }
    pub fn ok_with(stdout: &str) -> ExecResult {
        ExecResult {
            code: 0,
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }
    pub fn fail(code: i32, stderr: &str) -> ExecResult {
        ExecResult {
            code,
            stdout: String::new(),
            stderr: stderr.into(),
        }
    }
    /// The last lines of stderr (or stdout), scrubbed: for error messages.
    pub fn summary(&self) -> String {
        let t = if self.stderr.trim().is_empty() {
            &self.stdout
        } else {
            &self.stderr
        };
        let lines: Vec<&str> = t.lines().filter(|l| !l.trim().is_empty()).collect();
        let tail = lines[lines.len().saturating_sub(6)..].join("\n");
        scrub(&format!("exit {}: {}", self.code, tail))
    }
}

#[async_trait]
pub trait HostExec: Send + Sync {
    async fn run(&self, argv: &[String], o: &ExecOpts) -> Result<ExecResult, String>;
}

/// Runs commands on this machine.
pub struct LocalExec;

#[async_trait]
impl HostExec for LocalExec {
    async fn run(&self, argv: &[String], o: &ExecOpts) -> Result<ExecResult, String> {
        let Some((prog, args)) = argv.split_first() else {
            return Err("an empty command".into());
        };
        let mut c = tokio::process::Command::new(prog);
        c.args(args)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        if let Some(d) = &o.cwd {
            c.current_dir(d);
        }
        for (k, v) in &o.env {
            c.env(k, v);
        }
        let fut = c.output();
        let out = match o.timeout {
            Some(t) => tokio::time::timeout(t, fut)
                .await
                .map_err(|_| format!("{prog} timed out after {}s", t.as_secs()))?,
            None => fut.await,
        }
        .map_err(|e| format!("{prog}: {e}"))?;
        Ok(ExecResult {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).to_string(),
        })
    }
}

/// A scripted executor for tests: answers by argv prefix (joined with spaces), records calls.
/// A prefix and its answers.
type Rule = (String, VecDeque<ExecResult>);

#[derive(Default, Clone)]
pub struct FakeExec {
    rules: Arc<Mutex<Vec<Rule>>>,
    calls: Arc<Mutex<Vec<Vec<String>>>>,
}

impl FakeExec {
    pub fn new() -> FakeExec {
        FakeExec::default()
    }
    /// Answer commands starting with `prefix` (the last answer repeats).
    pub fn on(&self, prefix: &str, answers: Vec<ExecResult>) -> &Self {
        self.rules
            .lock()
            .expect("lock")
            .push((prefix.into(), answers.into()));
        self
    }
    pub fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().expect("lock").clone()
    }
    pub fn joined(&self) -> Vec<String> {
        self.calls().iter().map(|c| c.join(" ")).collect()
    }
}

#[async_trait]
impl HostExec for FakeExec {
    async fn run(&self, argv: &[String], _o: &ExecOpts) -> Result<ExecResult, String> {
        self.calls.lock().expect("lock").push(argv.to_vec());
        let line = argv.join(" ");
        let mut rules = self.rules.lock().expect("lock");
        // the longest matching prefix wins
        let best = rules
            .iter_mut()
            .filter(|(p, _)| line.starts_with(p.as_str()))
            .max_by_key(|(p, _)| p.len());
        match best {
            Some((_, q)) => {
                let r = if q.len() > 1 {
                    q.pop_front()
                } else {
                    q.front().cloned()
                };
                Ok(r.unwrap_or_default())
            }
            None => Ok(ExecResult::default()),
        }
    }
}

/// Fetches artifacts and store indexes.
#[async_trait]
pub trait Fetch: Send + Sync {
    /// Download `url` to `dest`, checking its sha256 when given. Returns the size.
    async fn download(&self, url: &str, dest: &Path, sha256: Option<&str>) -> Result<u64, String>;
    async fn text(&self, url: &str) -> Result<String, String>;
}

/// The most a single download may be (llama.cpp CUDA builds are a few hundred MiB).
pub const MAX_DOWNLOAD_BYTES: u64 = 4 * 1024 * 1024 * 1024;

pub struct HttpFetch {
    client: reqwest::Client,
}

impl Default for HttpFetch {
    fn default() -> Self {
        HttpFetch {
            client: reqwest::Client::builder()
                .user_agent(concat!("abhoth-engines/", env!("CARGO_PKG_VERSION")))
                .connect_timeout(Duration::from_secs(20))
                .build()
                .unwrap_or_default(),
        }
    }
}

#[async_trait]
impl Fetch for HttpFetch {
    async fn download(&self, url: &str, dest: &Path, sha256: Option<&str>) -> Result<u64, String> {
        use futures_util::StreamExt as _;
        use tokio::io::AsyncWriteExt;
        if !url.starts_with("https://") && !url.starts_with("http://") {
            return Err(format!("{url}: only http(s) downloads"));
        }
        let resp = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| format!("{url}: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("{url}: HTTP {}", resp.status().as_u16()));
        }
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        let part = dest.with_extension("part");
        let mut f = tokio::fs::File::create(&part)
            .await
            .map_err(|e| format!("{}: {e}", part.display()))?;
        let mut h = Sha256::new();
        let mut n = 0u64;
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| format!("{url}: {e}"))?;
            n += chunk.len() as u64;
            if n > MAX_DOWNLOAD_BYTES {
                let _ = tokio::fs::remove_file(&part).await;
                return Err(format!(
                    "{url}: larger than {} GiB",
                    MAX_DOWNLOAD_BYTES >> 30
                ));
            }
            h.update(&chunk);
            f.write_all(&chunk).await.map_err(|e| format!("{e}"))?;
        }
        f.flush().await.map_err(|e| format!("{e}"))?;
        drop(f);
        let got = crate::pins::hex(&h.finalize());
        if let Some(want) = sha256
            && !got.eq_ignore_ascii_case(want)
        {
            let _ = tokio::fs::remove_file(&part).await;
            return Err(format!("{url}: sha256 {got} does not match the pin {want}"));
        }
        tokio::fs::rename(&part, dest)
            .await
            .map_err(|e| format!("{}: {e}", dest.display()))?;
        Ok(n)
    }

    async fn text(&self, url: &str) -> Result<String, String> {
        let resp = self
            .client
            .get(url)
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| format!("{url}: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("{url}: HTTP {}", resp.status().as_u16()));
        }
        let b = resp.bytes().await.map_err(|e| format!("{url}: {e}"))?;
        if b.len() > 4 * 1024 * 1024 {
            return Err(format!("{url}: more than 4 MiB of text"));
        }
        String::from_utf8(b.to_vec()).map_err(|_| format!("{url}: not UTF-8"))
    }
}

/// A fake fetcher: serves bytes by URL.
#[derive(Default, Clone)]
pub struct FakeFetch {
    pub files: Arc<Mutex<std::collections::BTreeMap<String, Vec<u8>>>>,
}

impl FakeFetch {
    pub fn with(self, url: &str, bytes: &[u8]) -> Self {
        self.files
            .lock()
            .expect("lock")
            .insert(url.into(), bytes.to_vec());
        self
    }
}

#[async_trait]
impl Fetch for FakeFetch {
    async fn download(&self, url: &str, dest: &Path, sha256: Option<&str>) -> Result<u64, String> {
        let b = self
            .files
            .lock()
            .expect("lock")
            .get(url)
            .cloned()
            .ok_or_else(|| format!("{url}: HTTP 404"))?;
        let got = crate::pins::sha256_bytes(&b);
        if let Some(want) = sha256
            && !got.eq_ignore_ascii_case(want)
        {
            return Err(format!("{url}: sha256 {got} does not match the pin {want}"));
        }
        if let Some(p) = dest.parent() {
            std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
        }
        std::fs::write(dest, &b).map_err(|e| e.to_string())?;
        Ok(b.len() as u64)
    }
    async fn text(&self, url: &str) -> Result<String, String> {
        let b = self
            .files
            .lock()
            .expect("lock")
            .get(url)
            .cloned()
            .ok_or_else(|| format!("{url}: HTTP 404"))?;
        String::from_utf8(b).map_err(|_| "not UTF-8".into())
    }
}

/// Hide tokens that look like secrets (`hf_…`, `sk-…`, `abk_…`, `abb_…`, GitHub tokens) and
/// any `KEY=value` where the key says it is a secret.
pub fn scrub(t: &str) -> String {
    static RE: std::sync::OnceLock<(Regex, Regex)> = std::sync::OnceLock::new();
    let (tok, kv) = RE.get_or_init(|| {
        (
            Regex::new(
                r"(hf_|sk-ant-|sk-|abk_|abb_|ghp_|gho_|github_pat_|xox[bp]-)[A-Za-z0-9_\-]{8,}",
            )
            .expect("static"),
            Regex::new(
                r"(?i)\b([A-Z0-9_]*(TOKEN|SECRET|PASSWORD|API_KEY|APIKEY)[A-Z0-9_]*)=([^\s]+)",
            )
            .expect("static"),
        )
    });
    let a = tok.replace_all(t, "${1}***");
    kv.replace_all(&a, "${1}=***").to_string()
}

/// Scrub these exact values too (secret params).
pub fn scrub_values(t: &str, values: &[String]) -> String {
    let mut s = scrub(t);
    for v in values {
        if v.len() >= 6 {
            s = s.replace(v.as_str(), "***");
        }
    }
    s
}
