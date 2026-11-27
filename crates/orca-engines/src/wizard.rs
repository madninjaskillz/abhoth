//! The "Create package" wizard's back half (ROUTER-MEDIA-PLAN.md 13.3): first drafts of a
//! manifest from a source (nothing, a docker image, a compose file, a pip package, a GitHub
//! release, a manual engine), image metadata read from the registry (no container runs), and
//! zip export. Drafts are JSON values, not [`crate::Manifest`]s: they are meant to be finished in
//! the form and may not validate yet.

use std::collections::BTreeMap;
use std::io::Write;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::pins;
use crate::validate::Files;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Draft {
    pub manifest: Value,
    pub files: Files,
    /// What the wizard could not work out (to fill in by hand).
    pub notes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe: Option<ImageProbe>,
}

/// What the registry says about an image (manifest and config blob only).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ImageProbe {
    /// `docker.io/library/nginx:1.27`
    pub reference: String,
    /// The digest a pull of `reference` gets (the index digest for multi-arch images).
    pub digest: String,
    /// `reference@digest`, ready for `image`.
    pub pinned: String,
    pub platforms: Vec<String>,
    /// Container ports from `EXPOSE` (`8188/tcp`).
    pub ports: Vec<String>,
    /// `K=V` from the image config; values of names that look secret are dropped.
    pub env: Vec<String>,
    pub labels: BTreeMap<String, String>,
    pub entrypoint: Vec<String>,
    pub cmd: Vec<String>,
    pub workdir: String,
    pub volumes: Vec<String>,
    pub licence: Option<String>,
    pub source: Option<String>,
    pub size_bytes: Option<u64>,
}

/// An id from a name: lower case, `[a-z0-9-]`, at most 40 characters.
pub fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let t: String = out.trim_matches('-').chars().take(40).collect();
    let t = t.trim_matches('-').to_string();
    if t.is_empty() { "engine".into() } else { t }
}

fn base(id: &str, name: &str, version: &str) -> Value {
    json!({
        "$schema": "https://raw.githubusercontent.com/madninjaskillz/abhoth/main/stores/schema/abhoth-engine.schema.json",
        "schema": 1,
        "id": slug(id),
        "name": name,
        "version": version,
        "description": "",
        "kinds": [],
        "hardware": { "cpu_ok": true },
        "api": "http",
        "probe": "http",
        "methods": [],
        "licence": "",
        "source": "",
        "private": true
    })
}

/// An empty package with one docker method to fill in.
pub fn draft_scratch() -> Draft {
    let mut m = base("my-engine", "My engine", "0.1.0");
    m["methods"] = json!([{
        "type": "docker", "label": "docker", "platforms": ["linux/amd64"], "gpu": ["cuda", "cpu"],
        "image": "", "ports": [{ "id": "http", "container": 8080 }], "health": { "http": "/health" }
    }]);
    Draft {
        manifest: m,
        notes: vec!["Fill in the image (pinned by digest), its port and health path.".into()],
        ..Default::default()
    }
}

/// Split `[registry/]repo[:tag][@digest]` into (registry host, repo, tag or digest).
pub fn parse_image(r: &str) -> Result<(String, String, String), String> {
    let r = r.trim();
    if r.is_empty() || r.contains(char::is_whitespace) {
        return Err("an image reference has no spaces".into());
    }
    let (rest, digest) = match r.split_once('@') {
        Some((a, d)) => (a, Some(d.to_string())),
        None => (r, None),
    };
    let (host, path) = match rest.split_once('/') {
        Some((h, p)) if h.contains('.') || h.contains(':') || h == "localhost" => {
            (h.to_string(), p.to_string())
        }
        _ => ("docker.io".to_string(), rest.to_string()),
    };
    let (path, tag) = match path.rsplit_once(':') {
        Some((p, t)) if !t.contains('/') => (p.to_string(), t.to_string()),
        _ => (path.clone(), "latest".to_string()),
    };
    let path = if host == "docker.io" && !path.contains('/') {
        format!("library/{path}")
    } else {
        path
    };
    if !path
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "._/-".contains(c))
    {
        return Err(format!("`{path}` is not a repository name"));
    }
    Ok((host, path, digest.unwrap_or(tag)))
}

fn secretish(name: &str) -> bool {
    let n = name.to_ascii_uppercase();
    ["KEY", "TOKEN", "SECRET", "PASSWORD", "PASS", "CREDENTIAL"]
        .iter()
        .any(|w| n.contains(w))
}

const ACCEPT: &str = "application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json";

/// The most any registry answer may be.
const MAX_REGISTRY_BYTES: usize = 4 * 1024 * 1024;

async fn get_limited(rsp: reqwest::Response) -> Result<Vec<u8>, String> {
    if let Some(n) = rsp.content_length()
        && n as usize > MAX_REGISTRY_BYTES
    {
        return Err("the registry answer is too large".into());
    }
    let b = rsp.bytes().await.map_err(|e| e.to_string())?;
    if b.len() > MAX_REGISTRY_BYTES {
        return Err("the registry answer is too large".into());
    }
    Ok(b.to_vec())
}

/// Bearer realm/service/scope from `WWW-Authenticate`.
fn auth_params(h: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let h = h
        .trim_start_matches("Bearer ")
        .trim_start_matches("bearer ");
    let re = regex::Regex::new(r#"(\w+)="([^"]*)""#).expect("regex");
    for c in re.captures_iter(h) {
        out.insert(c[1].to_string(), c[2].to_string());
    }
    out
}

struct Registry {
    client: reqwest::Client,
    host: String,
    repo: String,
    token: Option<String>,
}

impl Registry {
    fn api_host(&self) -> &str {
        if self.host == "docker.io" {
            "registry-1.docker.io"
        } else {
            &self.host
        }
    }
    async fn get(&mut self, path: &str, accept: &str) -> Result<reqwest::Response, String> {
        let url = format!("https://{}/v2/{}/{path}", self.api_host(), self.repo);
        for _ in 0..2 {
            let mut rq = self.client.get(&url).header("Accept", accept);
            if let Some(t) = &self.token {
                rq = rq.bearer_auth(t);
            }
            let rsp = rq.send().await.map_err(|e| e.to_string())?;
            if rsp.status() == reqwest::StatusCode::UNAUTHORIZED && self.token.is_none() {
                let h = rsp
                    .headers()
                    .get("www-authenticate")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                let p = auth_params(&h);
                let realm = p.get("realm").cloned().unwrap_or_default();
                if !realm.starts_with("https://") {
                    return Err("the registry wants a sign-in this wizard can't do".into());
                }
                let mut url = format!("{realm}?scope=repository:{}:pull", self.repo);
                if let Some(s) = p.get("service") {
                    let ok = s
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || ".-_".contains(c));
                    if ok {
                        url.push_str(&format!("&service={s}"));
                    }
                }
                let t = self
                    .client
                    .get(&url)
                    .send()
                    .await
                    .map_err(|e| e.to_string())?;
                let v: Value =
                    serde_json::from_slice(&get_limited(t).await?).map_err(|e| e.to_string())?;
                let tok = v["token"]
                    .as_str()
                    .or(v["access_token"].as_str())
                    .ok_or("the registry gave no token (a private image?)")?;
                self.token = Some(tok.to_string());
                continue;
            }
            if !rsp.status().is_success() {
                return Err(format!("the registry answered {} for {path}", rsp.status()));
            }
            return Ok(rsp);
        }
        Err("the registry refused the anonymous token (a private image?)".into())
    }
}

/// Read an image's manifest and config from its registry (anonymous pull access; nothing is
/// downloaded but JSON, nothing runs).
pub async fn probe_image(reference: &str) -> Result<ImageProbe, String> {
    let (host, repo, tag) = parse_image(reference)?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent("abhoth-engine-wizard")
        .build()
        .map_err(|e| e.to_string())?;
    let mut reg = Registry {
        client,
        host: host.clone(),
        repo: repo.clone(),
        token: None,
    };
    let rsp = reg.get(&format!("manifests/{tag}"), ACCEPT).await?;
    let digest = rsp
        .headers()
        .get("docker-content-digest")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let body = get_limited(rsp).await?;
    let digest = digest.unwrap_or_else(|| format!("sha256:{}", pins::sha256_bytes(&body)));
    let mut top: Value = serde_json::from_slice(&body).map_err(|e| e.to_string())?;
    let mut platforms = vec![];
    if let Some(list) = top["manifests"].as_array().cloned() {
        for m in &list {
            let os = m["platform"]["os"].as_str().unwrap_or("");
            let arch = m["platform"]["architecture"].as_str().unwrap_or("");
            if !os.is_empty() && os != "unknown" {
                platforms.push(format!("{os}/{arch}"));
            }
        }
        let pick = list
            .iter()
            .find(|m| m["platform"]["os"] == "linux" && m["platform"]["architecture"] == "amd64")
            .or_else(|| list.first())
            .ok_or("the image index lists no images")?;
        let d = pick["digest"]
            .as_str()
            .ok_or("an index entry has no digest")?;
        let r = reg.get(&format!("manifests/{d}"), ACCEPT).await?;
        top = serde_json::from_slice(&get_limited(r).await?).map_err(|e| e.to_string())?;
    }
    let size: u64 = top["layers"]
        .as_array()
        .map(|l| l.iter().filter_map(|x| x["size"].as_u64()).sum())
        .unwrap_or(0);
    let cfg_digest = top["config"]["digest"]
        .as_str()
        .ok_or("the image manifest has no config")?
        .to_string();
    let r = reg
        .get(&format!("blobs/{cfg_digest}"), "application/json")
        .await?;
    let cfg: Value = serde_json::from_slice(&get_limited(r).await?).map_err(|e| e.to_string())?;
    let c = &cfg["config"];
    if platforms.is_empty() {
        platforms.push(format!(
            "{}/{}",
            cfg["os"].as_str().unwrap_or("linux"),
            cfg["architecture"].as_str().unwrap_or("amd64")
        ));
    }
    let strs = |v: &Value| -> Vec<String> {
        v.as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    };
    let keys = |v: &Value| -> Vec<String> {
        v.as_object()
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default()
    };
    let env: Vec<String> = strs(&c["Env"])
        .into_iter()
        .map(|kv| match kv.split_once('=') {
            Some((k, _)) if secretish(k) => format!("{k}="),
            _ => kv,
        })
        .collect();
    let labels: BTreeMap<String, String> = c["Labels"]
        .as_object()
        .map(|o| {
            o.iter()
                .filter_map(|(k, v)| {
                    v.as_str()
                        .map(|s| (k.clone(), s.chars().take(500).collect()))
                })
                .collect()
        })
        .unwrap_or_default();
    let licence = labels.get("org.opencontainers.image.licenses").cloned();
    let source = labels
        .get("org.opencontainers.image.source")
        .or(labels.get("org.opencontainers.image.url"))
        .cloned();
    let reference = format!("{host}/{}:{}", repo.trim_start_matches("library/"), tag);
    let reference = if tag.starts_with("sha256:") {
        format!("{host}/{}", repo.trim_start_matches("library/"))
    } else {
        reference
    };
    let pinned = format!("{reference}@{digest}");
    Ok(ImageProbe {
        reference,
        digest,
        pinned,
        platforms,
        ports: keys(&c["ExposedPorts"]),
        env,
        labels,
        entrypoint: strs(&c["Entrypoint"]),
        cmd: strs(&c["Cmd"]),
        workdir: c["WorkingDir"].as_str().unwrap_or("").to_string(),
        volumes: keys(&c["Volumes"]),
        licence,
        source,
        size_bytes: (size > 0).then_some(size),
    })
}

fn ports_of(p: &ImageProbe) -> Vec<Value> {
    let mut out = vec![];
    for (k, x) in p.ports.iter().enumerate() {
        let (n, proto) = x.split_once('/').unwrap_or((x, "tcp"));
        if let Ok(n) = n.parse::<u16>() {
            let id = if k == 0 {
                "http".to_string()
            } else {
                format!("p{n}")
            };
            let mut v = json!({ "id": id, "container": n });
            if proto != "tcp" || k > 0 {
                v["protocol"] = json!(if proto == "udp" { "udp" } else { "tcp" });
            }
            out.push(v);
        }
    }
    out
}

/// A draft from a docker image and what its registry said (when it could be read).
pub fn draft_docker(image: &str, probe: Option<ImageProbe>, probe_error: Option<String>) -> Draft {
    let (_, repo, tag) = parse_image(image).unwrap_or_default();
    let name = repo.rsplit('/').next().unwrap_or("engine").to_string();
    let version = if tag.is_empty() || tag == "latest" || tag.starts_with("sha256:") {
        "0.1.0".to_string()
    } else {
        tag.trim_start_matches('v').to_string()
    };
    let mut m = base(&name, &name, &version);
    let mut notes = vec![];
    let mut meth = json!({
        "type": "docker", "label": "docker", "platforms": ["linux/amd64"], "gpu": ["cuda", "cpu"],
        "image": image, "health": { "http": "/health", "start_seconds": 300 }
    });
    match &probe {
        Some(p) => {
            meth["image"] = json!(p.pinned);
            let plats: Vec<&String> = p
                .platforms
                .iter()
                .filter(|x| x.starts_with("linux/"))
                .collect();
            if !plats.is_empty() {
                meth["platforms"] = json!(plats);
            }
            let ports = ports_of(p);
            if ports.is_empty() {
                notes.push("The image exposes no port: add the port it listens on.".into());
                meth["ports"] = json!([{ "id": "http", "container": 8080 }]);
            } else {
                meth["ports"] = json!(ports);
            }
            if let Some(l) = &p.licence {
                m["licence"] = json!(l);
            } else {
                notes.push("The image has no licence label: set the licence.".into());
            }
            if let Some(s) = &p.source {
                m["source"] = json!(s);
            }
            if let Some(d) = p.labels.get("org.opencontainers.image.description") {
                m["description"] = json!(d);
            }
            if !p.volumes.is_empty() {
                notes.push(format!(
                    "The image declares volumes ({}): mount the model folder or {{{{state}}}} there if it keeps data.",
                    p.volumes.join(", ")
                ));
            }
            notes.push(format!(
                "Pinned to {} as the registry serves it now.",
                p.digest
            ));
        }
        None => {
            notes.push(format!(
                "The registry could not be read ({}): pin the image by digest by hand.",
                probe_error.unwrap_or_else(|| "no answer".into())
            ));
            meth["ports"] = json!([{ "id": "http", "container": 8080 }]);
        }
    }
    notes.push(
        "Set the kinds, the API and the health path; a test run can guess the last two.".into(),
    );
    m["methods"] = json!([meth]);
    Draft {
        manifest: m,
        notes,
        probe,
        ..Default::default()
    }
}

/// A draft around a compose file (kept as `compose.yml` in the package).
pub fn draft_compose(text: &str) -> Draft {
    let images = pins::compose_images(text);
    let name = images
        .first()
        .and_then(|i| parse_image(i).ok())
        .map(|(_, r, _)| r.rsplit('/').next().unwrap_or("engine").to_string())
        .unwrap_or_else(|| "compose-engine".into());
    let mut m = base(&name, &name, "0.1.0");
    m["methods"] = json!([{
        "type": "compose", "label": "compose", "platforms": ["linux/amd64"], "gpu": ["cpu"],
        "compose_file": "compose.yml", "ports": [{ "id": "http", "container": 8080 }],
        "health": { "http": "/health" }
    }]);
    let mut notes = vec![
        "Use {{name}} as container_name, publish \"{{host}}:{{port}}:<container port>\" and mount {{model_dir}} where the engine reads models.".to_string(),
    ];
    for i in &images {
        if !pins::image_pinned(i) {
            notes.push(format!("{i} is not pinned by digest (image@sha256:…)."));
        }
    }
    let mut files = Files::new();
    files.insert("compose.yml".into(), text.to_string());
    Draft {
        manifest: m,
        files,
        notes,
        probe: None,
    }
}

/// A draft for a pip package (`name==version`, run on the host in a venv).
pub fn draft_pip(spec: &str) -> Draft {
    let spec = spec.trim();
    let (name, version) = spec
        .split_once("==")
        .map(|(a, b)| (a.trim().to_string(), b.trim().to_string()))
        .unwrap_or((spec.to_string(), String::new()));
    let mut m = base(
        &name,
        &name,
        if version.is_empty() {
            "0.1.0"
        } else {
            &version
        },
    );
    let mut notes = vec![
        "Set run: the command that starts the server, e.g. [\"{{python}}\", \"-m\", \"<module>\", \"--port\", \"{{port}}\"].".to_string(),
        "pip installs run on the host, not in a container: they need the admin's approval.".to_string(),
    ];
    if version.is_empty() {
        notes.push("Pin the version (name==1.2.3), or hash-pin a requirements file.".into());
    }
    m["methods"] = json!([{
        "type": "pip", "label": "pip (host)",
        "platforms": ["linux/amd64", "windows/amd64", "macos/arm64"], "gpu": ["cuda", "cpu"],
        "python": "3.12", "package": spec,
        "run": ["{{python}}", "-m", slug(&name).replace('-', "_"), "--host", "{{host}}", "--port", "{{port}}"],
        "ports": [{ "id": "http" }], "health": { "http": "/health", "start_seconds": 300 }
    }]);
    Draft {
        manifest: m,
        notes,
        ..Default::default()
    }
}

/// A GitHub release asset: name, URL and (when GitHub lists one) its sha256.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReleaseAsset {
    pub name: String,
    pub url: String,
    pub sha256: Option<String>,
    pub size: u64,
}

/// `https://github.com/<owner>/<repo>/releases/tag/<tag>` → (owner, repo, tag).
pub fn parse_release_url(u: &str) -> Result<(String, String, String), String> {
    let u = u.trim().trim_end_matches('/');
    let rest = u
        .strip_prefix("https://github.com/")
        .ok_or("a GitHub release URL starts with https://github.com/")?;
    let p: Vec<&str> = rest.split('/').collect();
    match p.as_slice() {
        [o, r, "releases", "tag", t] => Ok((o.to_string(), r.to_string(), t.to_string())),
        _ => Err("expected https://github.com/<owner>/<repo>/releases/tag/<tag>".into()),
    }
}

/// The assets of a GitHub release (public API, no token).
pub async fn release_assets(
    url: &str,
) -> Result<(String, String, String, Vec<ReleaseAsset>), String> {
    let (o, r, t) = parse_release_url(url)?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent("abhoth-engine-wizard")
        .build()
        .map_err(|e| e.to_string())?;
    let rsp = client
        .get(format!(
            "https://api.github.com/repos/{o}/{r}/releases/tags/{t}"
        ))
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !rsp.status().is_success() {
        return Err(format!("GitHub answered {}", rsp.status()));
    }
    let v: Value = serde_json::from_slice(&get_limited(rsp).await?).map_err(|e| e.to_string())?;
    let assets = v["assets"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|x| ReleaseAsset {
                    name: x["name"].as_str().unwrap_or("").to_string(),
                    url: x["browser_download_url"].as_str().unwrap_or("").to_string(),
                    sha256: x["digest"]
                        .as_str()
                        .and_then(|d| d.strip_prefix("sha256:"))
                        .filter(|d| pins::is_sha256(d))
                        .map(String::from),
                    size: x["size"].as_u64().unwrap_or(0),
                })
                .collect()
        })
        .unwrap_or_default();
    Ok((o, r, t, assets))
}

/// Guess `os/arch` from an asset name (`llama-b123-bin-win-cuda-x64.zip`).
pub fn guess_platform(name: &str) -> Option<String> {
    let n = name.to_ascii_lowercase();
    let os = if n.contains("win") {
        "windows"
    } else if n.contains("mac") || n.contains("darwin") || n.contains("osx") {
        "macos"
    } else if n.contains("linux") || n.contains("ubuntu") {
        "linux"
    } else {
        return None;
    };
    let arch = if n.contains("arm64") || n.contains("aarch64") {
        "arm64"
    } else {
        "amd64"
    };
    Some(format!("{os}/{arch}"))
}

/// A draft from a GitHub release's assets: one binary method with every archive that names a
/// platform (checksums from GitHub where it lists them).
pub fn draft_release(owner: &str, repo: &str, tag: &str, assets: &[ReleaseAsset]) -> Draft {
    let mut m = base(repo, repo, tag.trim_start_matches('v'));
    m["source"] = json!(format!("https://github.com/{owner}/{repo}"));
    let mut notes = vec![
        "Keep only the builds you want (one per platform and GPU stack) and set run, e.g. [\"{{install}}/server\", \"--port\", \"{{port}}\"].".to_string(),
        "Binary installs run on the host: they need the admin's approval.".to_string(),
    ];
    let mut list = vec![];
    let mut plats = vec![];
    for a in assets {
        let lower = a.name.to_ascii_lowercase();
        let archive = [".zip", ".tar.gz", ".tgz", ".tar.xz"]
            .iter()
            .any(|x| lower.ends_with(x));
        let Some(p) = guess_platform(&a.name).filter(|_| archive) else {
            continue;
        };
        if a.sha256.is_none() {
            notes.push(format!("{}: GitHub lists no sha256; fill it in.", a.name));
        }
        if !plats.contains(&p) {
            plats.push(p.clone());
        }
        let gpu = if lower.contains("cuda") {
            "cuda"
        } else if lower.contains("rocm") || lower.contains("hip") {
            "rocm"
        } else if lower.contains("metal") || p.starts_with("macos") {
            "metal"
        } else {
            "cpu"
        };
        let mut x = json!({ "platform": p, "gpu": gpu, "url": a.url, "sha256": a.sha256.clone().unwrap_or_default() });
        if a.size > 0 {
            x["size"] = json!(a.size);
        }
        list.push(x);
    }
    if list.is_empty() {
        notes.push("No archive in the release names a platform: add the assets by hand.".into());
    }
    m["methods"] = json!([{
        "type": "binary", "label": "release build", "platforms": plats,
        "gpu": ["cuda", "cpu"], "assets": list, "run": ["{{install}}/server", "--host", "{{host}}", "--port", "{{port}}"],
        "ports": [{ "id": "http" }], "health": { "http": "/health" }
    }]);
    Draft {
        manifest: m,
        notes,
        ..Default::default()
    }
}

/// A draft describing an engine already set up by hand (an image on the host, or a URL).
pub fn draft_existing(
    id: &str,
    title: &str,
    image: Option<&str>,
    port: Option<u16>,
    health: &str,
) -> Draft {
    let mut m = base(id, if title.is_empty() { id } else { title }, "1");
    let mut meth = json!({
        "type": "existing", "label": "already set up", "platforms": ["linux/amd64"], "gpu": ["cuda", "cpu"],
        "health": { "http": health }
    });
    match image {
        Some(i) => {
            meth["detect"] = json!({ "image": i });
            meth["ports"] = json!([{ "id": "http", "container": port.unwrap_or(8080) }]);
        }
        None => {
            meth["detect"] = json!({ "command": id });
            meth["ports"] = json!([{ "id": "http" }]);
        }
    }
    m["methods"] = json!([meth]);
    Draft {
        manifest: m,
        notes: vec![
            "This describes the engine as it runs today; nothing is fetched or verified.".into(),
            "Add the run words, env and mounts the engine's router.toml entry uses.".into(),
        ],
        ..Default::default()
    }
}

/// A package's files as a zip (`<id>/<path>`).
pub fn zip_files(id: &str, files: &Files) -> Result<Vec<u8>, String> {
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut z = zip::ZipWriter::new(&mut buf);
        let o = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (p, t) in files {
            if p.contains("..") || p.starts_with('/') {
                return Err(format!("{p}: a bad file name"));
            }
            z.start_file(format!("{}/{p}", slug(id)), o)
                .map_err(|e| e.to_string())?;
            z.write_all(t.as_bytes()).map_err(|e| e.to_string())?;
        }
        z.finish().map_err(|e| e.to_string())?;
    }
    Ok(buf.into_inner())
}
