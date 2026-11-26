//! Hugging Face, from the router (D152): search, a model's card and files turned into what the
//! Add-model screen needs (type, size, context, vision, licence, gated, the choices of files:
//! a GGUF quant, a safetensors set), and the download URLs. The token is read from a secret file
//! and only ever sent to Hugging Face; it is never logged, returned or put in a command line.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::Serialize;
use serde_json::{Value, json};

use super::schema::ModelKind;

pub const DEFAULT_ENDPOINT: &str = "https://huggingface.co";

/// The client: endpoint and optional token. `Debug` never shows the token.
#[derive(Clone)]
pub struct Hf {
    pub endpoint: String,
    token: Option<String>,
    http: reqwest::Client,
}

impl std::fmt::Debug for Hf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hf")
            .field("endpoint", &self.endpoint)
            .field("token", &self.token.as_ref().map(|_| "<set>"))
            .finish()
    }
}

impl Hf {
    pub fn new(endpoint: Option<&str>, token: Option<String>) -> Hf {
        Hf {
            endpoint: endpoint
                .unwrap_or(DEFAULT_ENDPOINT)
                .trim_end_matches('/')
                .to_string(),
            token: token
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty()),
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(30))
                .user_agent("abhoth-router")
                .build()
                .unwrap_or_default(),
        }
    }

    pub fn has_token(&self) -> bool {
        self.token.is_some()
    }

    /// The token, for the download step only (it goes to the host on stdin).
    pub fn token_bytes(&self) -> Option<Vec<u8>> {
        self.token.as_ref().map(|t| t.as_bytes().to_vec())
    }

    async fn get(&self, path: &str) -> Result<Value, String> {
        let mut r = self.http.get(format!("{}{path}", self.endpoint));
        if let Some(t) = &self.token {
            r = r.bearer_auth(t);
        }
        let resp = r.send().await.map_err(|e| {
            // never echo the request (it may carry the token in headers; reqwest does not
            // print headers, but keep the message to the cause)
            format!("Hugging Face did not answer: {}", e.without_url())
        })?;
        let code = resp.status().as_u16();
        match code {
            200 => resp.json().await.map_err(|e| format!("Hugging Face: {e}")),
            401 => Err("Hugging Face refused the token (check it under Hugging Face token)".into()),
            403 => Err(
                "this model is gated: accept its licence on huggingface.co, then set a token"
                    .into(),
            ),
            404 => Err("not found on Hugging Face".into()),
            _ => Err(format!("Hugging Face answered HTTP {code}")),
        }
    }

    /// Search models (`kind` narrows by pipeline).
    pub async fn search(&self, q: &str, kind: Option<ModelKind>) -> Result<Value, String> {
        let mut path = format!(
            "/api/models?search={}&limit=24&sort=downloads&direction=-1",
            urlenc(q)
        );
        if let Some(k) = kind
            && let Some(p) = pipelines(k).first()
        {
            path.push_str(&format!("&pipeline_tag={p}"));
        }
        let v = self.get(&path).await?;
        Ok(json!(
            v.as_array()
                .map(|a| a.iter().map(search_row).collect::<Vec<_>>())
                .unwrap_or_default()
        ))
    }

    /// A model's details and the file choices.
    pub async fn model(&self, repo: &str) -> Result<Value, String> {
        if !valid_repo(repo) {
            return Err("a Hugging Face model id is `owner/name`".into());
        }
        let info = self.get(&format!("/api/models/{repo}?blobs=true")).await?;
        let config = if info
            .get("siblings")
            .and_then(Value::as_array)
            .is_some_and(|s| s.iter().any(|f| f["rfilename"] == "config.json"))
        {
            self.get(&format!("/{repo}/resolve/main/config.json"))
                .await
                .ok()
        } else {
            None
        };
        Ok(details(repo, &info, config.as_ref()))
    }

    /// The URL a file downloads from (a pinned revision when given).
    pub fn file_url(&self, repo: &str, revision: &str, file: &str) -> String {
        format!(
            "{}/{repo}/resolve/{}/{}",
            self.endpoint,
            urlenc(revision),
            file.split('/').map(urlenc).collect::<Vec<_>>().join("/")
        )
    }
}

pub fn valid_repo(repo: &str) -> bool {
    let mut parts = repo.split('/');
    let ok = |s: Option<&str>| {
        s.is_some_and(|s| {
            !s.is_empty()
                && s != "."
                && s != ".."
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        })
    };
    ok(parts.next()) && ok(parts.next()) && parts.next().is_none()
}

/// A safe relative file path inside a repo.
pub fn valid_file(f: &str) -> bool {
    !f.is_empty()
        && !f.starts_with('/')
        && f.split('/').all(|p| !p.is_empty() && p != "." && p != "..")
        && !f.contains('\\')
        && !f.chars().any(|c| c.is_control())
}

fn urlenc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Pipeline tags for a model type (the first is the search filter).
pub fn pipelines(k: ModelKind) -> &'static [&'static str] {
    match k {
        ModelKind::Llm => &[
            "text-generation",
            "image-text-to-text",
            "text2text-generation",
        ],
        ModelKind::Tts => &["text-to-speech"],
        ModelKind::Stt => &["automatic-speech-recognition"],
        ModelKind::Image => &["text-to-image", "image-to-image"],
        ModelKind::Music => &["text-to-audio"],
        ModelKind::Embedding => &["feature-extraction", "sentence-similarity"],
    }
}

/// The model type a pipeline tag (and tags) suggests.
pub fn kind_of(pipeline: Option<&str>, tags: &[String]) -> Option<ModelKind> {
    let p = pipeline?;
    if p == "text-to-audio" {
        return Some(if tags.iter().any(|t| t.contains("music")) {
            ModelKind::Music
        } else {
            ModelKind::Tts
        });
    }
    ModelKind::ALL
        .into_iter()
        .find(|k| pipelines(*k).contains(&p))
}

fn tags_of(v: &Value) -> Vec<String> {
    v.get("tags")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn licence_of(v: &Value, tags: &[String]) -> Option<String> {
    v.pointer("/cardData/license")
        .and_then(|l| match l {
            Value::String(s) => Some(s.clone()),
            Value::Array(a) => a.first().and_then(Value::as_str).map(str::to_string),
            _ => None,
        })
        .or_else(|| {
            tags.iter()
                .find_map(|t| t.strip_prefix("license:").map(str::to_string))
        })
}

fn gated_of(v: &Value) -> bool {
    match v.get("gated") {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => !s.is_empty() && s != "false",
        _ => false,
    }
}

fn search_row(v: &Value) -> Value {
    let tags = tags_of(v);
    let pipeline = v.get("pipeline_tag").and_then(Value::as_str);
    json!({
        "repo": v.get("id").or_else(|| v.get("modelId")).and_then(Value::as_str).unwrap_or(""),
        "kind": kind_of(pipeline, &tags).map(|k| k.as_str()),
        "pipeline": pipeline,
        "downloads": v.get("downloads").and_then(Value::as_u64).unwrap_or(0),
        "likes": v.get("likes").and_then(Value::as_u64).unwrap_or(0),
        "gated": gated_of(v),
        "licence": licence_of(v, &tags),
        "gguf": tags.iter().any(|t| t == "gguf"),
        "updated": v.get("lastModified").and_then(Value::as_str),
    })
}

/// One way to take a model: a set of files.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Choice {
    pub id: String,
    pub label: String,
    /// `gguf`, `safetensors`, …
    pub format: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quant: Option<String>,
    pub files: Vec<FileRef>,
    pub bytes: u64,
    pub size_gib: f64,
    /// Rough VRAM to load it: the weights plus 10% (KV cache and lanes come from the profile).
    pub need_gib: f64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FileRef {
    pub path: String,
    pub size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

fn gib(b: u64) -> f64 {
    (b as f64 / (1u64 << 30) as f64 * 10.0).round() / 10.0
}

/// `model-00001-of-00003.gguf` -> (`model`, true); a plain name -> (stem, false).
fn shard_group(name: &str) -> String {
    let stem = name.trim_end_matches(".gguf");
    if let Some(i) = stem.rfind("-of-") {
        let before = &stem[..i];
        if let Some(j) = before.rfind('-')
            && before[j + 1..].chars().all(|c| c.is_ascii_digit())
            && stem[i + 4..].chars().all(|c| c.is_ascii_digit())
        {
            return before[..j].to_string();
        }
    }
    stem.to_string()
}

/// The file choices of a repo: one per GGUF quant (shards together; an `mmproj` file is added to
/// each, for images), one safetensors set (weights + config + tokenizer), else everything.
pub fn choices(siblings: &[Value]) -> Vec<Choice> {
    let files: Vec<FileRef> = siblings
        .iter()
        .filter_map(|s| {
            let path = s.get("rfilename")?.as_str()?.to_string();
            let size = s
                .get("size")
                .and_then(Value::as_u64)
                .or_else(|| s.pointer("/lfs/size").and_then(Value::as_u64))
                .unwrap_or(0);
            let sha256 = s
                .pointer("/lfs/sha256")
                .and_then(Value::as_str)
                .map(str::to_string);
            Some(FileRef { path, size, sha256 })
        })
        .filter(|f| valid_file(&f.path))
        .collect();
    let mut out = Vec::new();
    let mmproj: Vec<&FileRef> = files
        .iter()
        .filter(|f| f.path.ends_with(".gguf") && f.path.to_lowercase().contains("mmproj"))
        .collect();
    let mut groups: BTreeMap<String, Vec<FileRef>> = BTreeMap::new();
    for f in files
        .iter()
        .filter(|f| f.path.ends_with(".gguf") && !f.path.to_lowercase().contains("mmproj"))
    {
        groups
            .entry(shard_group(&f.path))
            .or_default()
            .push(f.clone());
    }
    for (g, mut fs) in groups {
        let quant = super::caps::quant_of_name(&fs[0].path);
        // the smallest mmproj (F16 before F32)
        if let Some(m) = mmproj.iter().min_by_key(|m| m.size) {
            fs.push((*m).clone());
        }
        let bytes: u64 = fs.iter().map(|f| f.size).sum();
        let label = match &quant {
            Some(q) => format!(
                "{q} (GGUF{})",
                if fs.len() > 1 && mmproj.is_empty() {
                    format!(", {} parts", fs.len())
                } else {
                    String::new()
                }
            ),
            None => format!("{g} (GGUF)"),
        };
        out.push(Choice {
            id: format!("gguf:{g}"),
            label,
            format: "gguf".into(),
            quant,
            bytes,
            size_gib: gib(bytes),
            need_gib: need(bytes),
            files: fs,
        });
    }
    let st: Vec<&FileRef> = files
        .iter()
        .filter(|f| f.path.ends_with(".safetensors"))
        .collect();
    if !st.is_empty() {
        let side: Vec<&FileRef> = files
            .iter()
            .filter(|f| {
                let p = f.path.as_str();
                !p.contains('/')
                    && (p.ends_with(".json")
                        || p.ends_with(".txt")
                        || p.ends_with(".model")
                        || p.ends_with(".jinja"))
            })
            .collect();
        let mut fs: Vec<FileRef> = st.iter().map(|f| (*f).clone()).collect();
        fs.extend(side.into_iter().cloned());
        let weights: u64 = st.iter().map(|f| f.size).sum();
        let bytes: u64 = fs.iter().map(|f| f.size).sum();
        out.push(Choice {
            id: "safetensors".into(),
            label: format!(
                "safetensors ({} weight file{})",
                st.len(),
                if st.len() == 1 { "" } else { "s" }
            ),
            format: "safetensors".into(),
            quant: None,
            bytes,
            size_gib: gib(bytes),
            need_gib: need(weights),
            files: fs,
        });
    }
    if out.is_empty() && !files.is_empty() {
        let bytes: u64 = files.iter().map(|f| f.size).sum();
        let format = files
            .iter()
            .max_by_key(|f| f.size)
            .and_then(|f| super::caps::format_of_name(&f.path))
            .unwrap_or_else(|| "files".into());
        out.push(Choice {
            id: "all".into(),
            label: format!("all files ({format})"),
            format,
            quant: None,
            bytes,
            size_gib: gib(bytes),
            need_gib: need(bytes),
            files,
        });
    }
    out.sort_by(|a, b| a.format.cmp(&b.format).then(a.bytes.cmp(&b.bytes)));
    out
}

fn need(bytes: u64) -> f64 {
    ((bytes as f64 / (1u64 << 30) as f64) * 1.1 * 10.0).round() / 10.0
}

/// A model's details for the Add-model screen.
pub fn details(repo: &str, info: &Value, config: Option<&Value>) -> Value {
    let tags = tags_of(info);
    let pipeline = info.get("pipeline_tag").and_then(Value::as_str);
    let siblings = info
        .get("siblings")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let ch = choices(&siblings);
    let gguf = info.get("gguf");
    let arch = gguf
        .and_then(|g| g.get("architecture"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            config
                .and_then(|c| {
                    c.get("model_type")
                        .or_else(|| c.pointer("/text_config/model_type"))
                })
                .and_then(Value::as_str)
                .map(str::to_string)
        });
    let context = gguf
        .and_then(|g| g.get("context_length"))
        .and_then(Value::as_u64)
        .or_else(|| {
            config.and_then(|c| {
                c.get("max_position_embeddings")
                    .or_else(|| c.pointer("/text_config/max_position_embeddings"))
                    .and_then(Value::as_u64)
            })
        });
    let vision = pipeline == Some("image-text-to-text")
        || config.is_some_and(|c| c.get("vision_config").is_some())
        || siblings.iter().any(|s| {
            s["rfilename"]
                .as_str()
                .is_some_and(|f| f.to_lowercase().contains("mmproj"))
        });
    let gated = gated_of(info);
    json!({
        "repo": repo,
        "revision": info.get("sha").and_then(Value::as_str),
        "kind": kind_of(pipeline, &tags).map(|k| k.as_str()),
        "pipeline": pipeline,
        "arch": arch,
        "context": context,
        "vision": vision,
        "licence": licence_of(info, &tags),
        "gated": gated,
        "gatedHow": info.get("gated").and_then(Value::as_str),
        "downloads": info.get("downloads").and_then(Value::as_u64),
        "likes": info.get("likes").and_then(Value::as_u64),
        "title": repo.rsplit('/').next().unwrap_or(repo),
        "choices": ch,
    })
}

#[cfg(test)]
pub(crate) mod tests;
