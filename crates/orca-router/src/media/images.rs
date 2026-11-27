//! `/v1/images/generations` and `/v1/images/edits` in the router (D153 phase 4,
//! ROUTER-MEDIA-PLAN.md 4.3): the image API shim's logic over the ComfyUI engines.
//!
//! - A model name (or alias) is a recipe: an image model with `needs` (its components by
//!   role) and an optional `recipe` table; an unknown name gets the default recipe.
//! - Sizes snap to the nearest ~1 MP bucket (2K is off, as in the shim).
//! - A request waits (FIFO, up to an hour) for an idle ComfyUI that has the recipe's files:
//!   none of this router's renders on it and an empty ComfyUI queue. A render that fails there
//!   (connection lost, a timeout, an execution error) is tried once on each other one.
//! - Edits upload the picture(s) to the chosen ComfyUI (`/upload/image`) and run the recipe's
//!   edit workflow (the reference images through the Qwen-Image 2.1 text encoder).

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::http::{HeaderMap, HeaderValue};
use serde_json::{Value, json};

use super::place::{self, Target, Want};
use super::send::{self, Patience, Prepared, Tried};
use crate::control::caps;
use crate::control::schema::{ControlConfig, MediaApi, Model, ModelKind};

/// Qwen-Image 2.1 Turbo's schedule (the ComfyUI template): 8 steps, euler, cfg 1.
pub const QWEN21_SIGMAS: &str =
    "1.0, 0.978453, 0.95418, 0.926626, 0.89508, 0.845148, 0.704534, 0.414568, 0";
/// ~1 MP buckets, multiples of 32.
pub const BUCKETS: [(u32, u32); 7] = [
    (1024, 1024),
    (1184, 896),
    (896, 1184),
    (1248, 832),
    (832, 1248),
    (1376, 768),
    (768, 1376),
];
const JOB_TIMEOUT: Duration = Duration::from_secs(900);
const WAIT_TIMEOUT: Duration = Duration::from_secs(3600);
const POLL: Duration = Duration::from_secs(2);
const POLL_FAILS: u32 = 4;
pub const EDIT_MAX_IMAGES: usize = 4;

/// A recipe resolved to its files.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub model: String,
    pub unet: String,
    pub clip: String,
    pub vae: String,
    pub lora: Option<String>,
    pub sigmas: String,
    pub cfg: f64,
    pub edits: bool,
    /// Every file the workflow loads.
    pub files: Vec<String>,
    /// The component models (for "which server has them").
    pub needs: Vec<String>,
}

fn model_named<'c>(c: &'c ControlConfig, name: &str) -> Option<(&'c String, &'c Model)> {
    let lower = name.to_ascii_lowercase();
    c.models.iter().find(|(id, m)| {
        m.kind == Some(ModelKind::Image)
            && m.role.is_none()
            && (id.eq_ignore_ascii_case(&lower)
                || m.aliases.iter().any(|a| a.eq_ignore_ascii_case(&lower)))
    })
}

fn is_recipe(m: &Model) -> bool {
    m.kind == Some(ModelKind::Image) && m.role.is_none() && !m.needs.is_empty()
}

/// The default recipe: `recipe.default = true`, else `qwen-image`, else the first recipe.
pub fn default_recipe(c: &ControlConfig) -> Option<&String> {
    c.models
        .iter()
        .find(|(_, m)| is_recipe(m) && m.recipe.as_ref().is_some_and(|r| r.default))
        .or_else(|| {
            c.models
                .get_key_value("qwen-image")
                .filter(|(_, m)| is_recipe(m))
        })
        .or_else(|| c.models.iter().find(|(_, m)| is_recipe(m)))
        .map(|(k, _)| k)
}

/// The recipe a client's name means, and the name it was redirected from (unknown names).
pub fn resolve(c: &ControlConfig, name: Option<&str>) -> Result<(Plan, Option<String>), String> {
    let asked = name.map(str::trim).filter(|s| !s.is_empty());
    let (id, redirected) = match asked.and_then(|n| model_named(c, n)) {
        Some((id, m)) if is_recipe(m) => (id.clone(), None),
        Some((id, _)) => return Err(format!("{id} is not an image recipe (it has no needs)")),
        None => (
            default_recipe(c)
                .ok_or("no image recipe is configured")?
                .clone(),
            Some(asked.unwrap_or("(default)").to_string()),
        ),
    };
    plan(c, &id).map(|p| (p, redirected))
}

pub fn plan(c: &ControlConfig, id: &str) -> Result<Plan, String> {
    let m = c.models.get(id).ok_or_else(|| format!("no model {id}"))?;
    let r = m.recipe.clone().unwrap_or_default();
    let family = r.workflow.clone().unwrap_or_else(|| {
        match m.arch.as_deref() {
            Some("qwen-image") | None => "qwen21",
            Some(other) => other,
        }
        .to_string()
    });
    if family != "qwen21" {
        return Err(format!(
            "{id}: workflow {family} is not one the router builds"
        ));
    }
    let by_role = |role: &str| -> Option<String> {
        m.needs.iter().find_map(|n| {
            let cm = c.models.get(n)?;
            (cm.role.as_deref() == Some(role)).then(|| file_of(cm))?
        })
    };
    let unet = by_role("diffusion").ok_or_else(|| format!("{id} needs a diffusion model"))?;
    let clip = by_role("text_encoder").ok_or_else(|| format!("{id} needs a text encoder"))?;
    let vae = by_role("vae").ok_or_else(|| format!("{id} needs a VAE"))?;
    let lora = by_role("lora");
    let mut files = vec![unet.clone(), clip.clone(), vae.clone()];
    files.extend(lora.clone());
    Ok(Plan {
        model: id.to_string(),
        unet,
        clip,
        vae,
        lora,
        sigmas: r.sigmas.unwrap_or_else(|| QWEN21_SIGMAS.into()),
        cfg: r.cfg.unwrap_or(1.0),
        edits: r.edits.unwrap_or(true),
        files,
        needs: m.needs.clone(),
    })
}

/// The file a component loads: its ComfyUI artifact's (any artifact's when none says comfyui).
fn file_of(m: &Model) -> Option<String> {
    m.artifacts
        .get("comfyui")
        .or_else(|| m.artifacts.values().next())
        .map(|a| a.file.clone())
        .filter(|f| !f.is_empty())
}

/// The 1 MP bucket with the nearest aspect ratio to `WxH` (square when unreadable).
pub fn bucket(size: Option<&str>) -> (u32, u32) {
    let ar = size
        .and_then(|s| {
            let (w, h) = s
                .to_ascii_lowercase()
                .split_once('x')
                .map(|(a, b)| (a.trim().parse::<f64>().ok(), b.trim().parse::<f64>().ok()))?;
            Some(w? / h?)
        })
        .filter(|a| a.is_finite() && *a > 0.0)
        .unwrap_or(1.0);
    *BUCKETS
        .iter()
        .min_by(|a, b| {
            let d = |x: &(u32, u32)| ((x.0 as f64 / x.1 as f64) / ar).ln().abs();
            d(a).total_cmp(&d(b))
        })
        .expect("buckets")
}

fn unet_loader(file: &str) -> Value {
    if file.ends_with(".gguf") {
        json!({"class_type": "UnetLoaderGGUF", "inputs": {"unet_name": file}})
    } else {
        json!({"class_type": "UNETLoader", "inputs": {"unet_name": file, "weight_dtype": "default"}})
    }
}

fn common(p: &Plan, enc: Value, latent: Value, seed: u64, prefix: &str) -> Value {
    let mut wf = json!({
        "1": unet_loader(&p.unet),
        "2": {"class_type": "CLIPLoader",
              "inputs": {"clip_name": p.clip, "type": "qwen_image", "device": "default"}},
        "3": {"class_type": "VAELoader", "inputs": {"vae_name": p.vae}},
        "4": {"class_type": "TextEncodeQwenImage21", "inputs": enc},
        "6": {"class_type": "CFGGuider",
              "inputs": {"model": ["1", 0], "positive": ["4", 0], "negative": ["4", 1], "cfg": p.cfg}},
        "7": {"class_type": "KSamplerSelect", "inputs": {"sampler_name": "euler"}},
        "8": {"class_type": "ManualSigmas", "inputs": {"sigmas": p.sigmas}},
        "9": {"class_type": "RandomNoise", "inputs": {"noise_seed": seed}},
        "10": {"class_type": "SamplerCustomAdvanced",
               "inputs": {"noise": ["9", 0], "guider": ["6", 0], "sampler": ["7", 0],
                          "sigmas": ["8", 0], "latent_image": latent}},
        "11": {"class_type": "VAEDecode", "inputs": {"samples": ["10", 0], "vae": ["3", 0]}},
        "12": {"class_type": "SaveImage", "inputs": {"images": ["11", 0], "filename_prefix": prefix}},
    });
    if let Some(l) = &p.lora {
        wf["13"] = json!({"class_type": "LoraLoaderModelOnly",
                          "inputs": {"model": ["1", 0], "lora_name": l, "strength_model": 1.0}});
        wf["6"]["inputs"]["model"] = json!(["13", 0]);
    }
    wf
}

/// Text to image (the shim's workflow, node for node).
pub fn workflow(p: &Plan, prompt: &str, w: u32, h: u32, seed: u64) -> Value {
    let enc =
        json!({"clip": ["2", 0], "prompt": prompt, "negative_prompt": "", "resolution": 1024});
    let mut wf = common(p, enc, json!(["5", 0]), seed, "abhoth-image");
    wf["5"] = json!({"class_type": "EmptyLatentImage",
                     "inputs": {"width": w, "height": h, "batch_size": 1}});
    wf
}

/// An edit: the uploaded images as references; the latent comes from the encoder at the first
/// image's ~1 MP size.
pub fn edit_workflow(p: &Plan, prompt: &str, names: &[String], seed: u64) -> Value {
    let mut enc = json!({"clip": ["2", 0], "prompt": prompt, "negative_prompt": "",
                         "vae": ["3", 0], "resolution": 1024});
    for i in 1..=names.len() {
        enc[format!("images.image_{i}")] = json!([format!("2{i}"), 0]);
    }
    let mut wf = common(p, enc, json!(["4", 2]), seed, "abhoth-edit");
    for (i, n) in names.iter().enumerate() {
        wf[format!("2{}", i + 1)] = json!({"class_type": "LoadImage", "inputs": {"image": n}});
    }
    wf
}

fn seed() -> u64 {
    let mut b = [0u8; 4];
    let _ = getrandom::fill(&mut b);
    (u32::from_le_bytes(b) >> 1) as u64
}

// ── placement: who has the files, who is idle ────────────────────────────────────────────────

/// The shim's backend names: `@truenas` was DeepThought's ComfyUI.
fn pin_server(c: &ControlConfig, pin: &str) -> String {
    if c.servers.contains_key(pin) {
        return pin.to_string();
    }
    match pin {
        "truenas" => c
            .servers
            .iter()
            .find(|(_, s)| s.exec != crate::control::schema::ExecKind::Bridge)
            .map(|(k, _)| k.clone())
            .unwrap_or_else(|| pin.to_string()),
        other => other.to_string(),
    }
}

/// Does `t` have every file `p` loads? Its last probe says; with none, the components' `servers`.
pub fn has_files(c: &ControlConfig, t: &Target, p: &Plan) -> bool {
    match caps::files_on(&t.engine, &t.server) {
        Some(folders) => p.files.iter().all(|f| {
            folders
                .values()
                .flatten()
                .any(|x| x == f || x.rsplit(['/', '\\']).next() == Some(f.as_str()))
        }),
        None => p
            .needs
            .iter()
            .all(|n| c.models.get(n).is_none_or(|m| m.on_server(&t.server))),
    }
}

pub fn targets(
    c: &ControlConfig,
    p: &Plan,
    pin: Option<&str>,
    bridge_priority: &dyn Fn(&str) -> Option<i32>,
) -> Vec<Target> {
    let server = pin.map(|s| pin_server(c, s));
    place::candidates(
        c,
        &Want {
            kinds: &[ModelKind::Image],
            apis: &[MediaApi::Comfyui],
            model: Some(&p.model),
            server: server.as_deref(),
        },
        bridge_priority,
    )
    .into_iter()
    .filter(|t| has_files(c, t, p))
    .collect()
}

fn claimed() -> &'static Mutex<HashSet<String>> {
    static C: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    C.get_or_init(Default::default)
}

/// One render's hold on a ComfyUI.
pub struct Claim(String);
impl Drop for Claim {
    fn drop(&mut self) {
        claimed().lock().unwrap().remove(&self.0);
    }
}

fn turn() -> &'static tokio::sync::Mutex<()> {
    static T: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    T.get_or_init(Default::default)
}

/// `(running, pending)` from ComfyUI's `/queue`.
async fn queue_len(cx: &super::Cx<'_>, t: &Target) -> Result<usize, String> {
    let (status, b) = call(
        cx,
        t,
        "GET",
        "/queue",
        None,
        Bytes::new(),
        Duration::from_secs(5),
    )
    .await?;
    if status != 200 {
        return Err(format!("{} /queue answered {status}", t.key()));
    }
    let v: Value = serde_json::from_slice(&b).map_err(|e| format!("{} /queue: {e}", t.key()))?;
    let n = |k: &str| v.get(k).and_then(Value::as_array).map_or(0, Vec::len);
    Ok(n("queue_running") + n("queue_pending"))
}

/// Wait (in arrival order) for an idle target not yet tried; claim it.
async fn acquire(
    cx: &super::Cx<'_>,
    ts: &[Target],
    tried: &[String],
    deadline: Instant,
) -> Result<(Target, Claim), String> {
    let _turn = turn().lock().await;
    loop {
        let left: Vec<&Target> = ts.iter().filter(|t| !tried.contains(&t.key())).collect();
        if left.is_empty() {
            return Err("no ComfyUI left to try".into());
        }
        // cooled ones only when nothing else is left
        let warm: Vec<&Target> = left
            .iter()
            .copied()
            .filter(|t| !place::is_cooled(t))
            .collect();
        let pool = if warm.is_empty() { left } else { warm };
        let mut reachable = 0;
        for t in pool {
            let k = t.key();
            if claimed().lock().unwrap().contains(&k) {
                reachable += 1;
                continue;
            }
            match queue_len(cx, t).await {
                Ok(0) => {
                    claimed().lock().unwrap().insert(k.clone());
                    return Ok((t.clone(), Claim(k)));
                }
                Ok(_) => reachable += 1,
                Err(e) => {
                    tracing::info!("media: images: {e}");
                    place::cool(t);
                }
            }
        }
        if reachable == 0 && ts.iter().all(place::is_cooled) {
            return Err("no ComfyUI is answering".into());
        }
        if Instant::now() > deadline {
            return Err("timed out waiting for a free ComfyUI".into());
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// One call to a ComfyUI: `(status, body)`, or why it could not be reached.
async fn call(
    cx: &super::Cx<'_>,
    t: &Target,
    method: &str,
    path_q: &str,
    content_type: Option<&str>,
    body: Bytes,
    patience: Duration,
) -> Result<(u16, Bytes), String> {
    let mut h = HeaderMap::new();
    if let Some(ct) = content_type
        && let Ok(v) = HeaderValue::from_str(ct)
    {
        h.insert("content-type", v);
    }
    let p = Prepared {
        method: method.into(),
        path_q: path_q.into(),
        headers: h,
        body,
    };
    let r = match send::send(cx.http, cx.hub, cx.c, t, p, Patience(patience)).await {
        send::Sent::Answer(r) => r,
        send::Sent::Retry(_, Some(r)) if r.status().as_u16() < 500 => r,
        send::Sent::Retry(why, _) => return Err(why),
    };
    let status = r.status().as_u16();
    let b = tokio::time::timeout(patience, axum::body::to_bytes(r.into_body(), 256 << 20))
        .await
        .map_err(|_| format!("{} {path_q}: the answer stalled", t.key()))?
        .map_err(|e| format!("{} {path_q}: {e}", t.key()))?;
    Ok((status, b))
}

#[derive(Debug)]
enum Fail {
    /// Not reachable, hung, lost: cool it.
    Down(String),
    /// It answered, but the job failed (an execution error, a refused workflow).
    Job(String),
}

fn upload_body(data: &[u8], filename: &str, boundary: &str) -> Vec<u8> {
    let mut b = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"image\"; filename=\"{filename}\"\r\nContent-Type: image/png\r\n\r\n"
    )
    .into_bytes();
    b.extend_from_slice(data);
    b.extend_from_slice(
        format!("\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"overwrite\"\r\n\r\ntrue\r\n--{boundary}--\r\n").as_bytes(),
    );
    b
}

async fn render(
    cx: &super::Cx<'_>,
    t: &Target,
    p: &Plan,
    prompt: &str,
    size: (u32, u32),
    edit: &[Bytes],
) -> Result<Bytes, Fail> {
    let s = seed();
    let wf = if edit.is_empty() {
        workflow(p, prompt, size.0, size.1, s)
    } else {
        let tag = seed();
        let boundary = format!("----abhoth{tag}");
        let mut names = Vec::new();
        for (i, d) in edit.iter().enumerate() {
            let body = upload_body(d, &format!("abhoth-edit-{tag}-{}.png", i + 1), &boundary);
            let (status, b) = call(
                cx,
                t,
                "POST",
                "/upload/image",
                Some(&format!("multipart/form-data; boundary={boundary}")),
                Bytes::from(body),
                Duration::from_secs(60),
            )
            .await
            .map_err(Fail::Down)?;
            if status != 200 {
                return Err(Fail::Job(format!("image upload refused ({status})")));
            }
            let v: Value = serde_json::from_slice(&b).unwrap_or_default();
            let name = v["name"]
                .as_str()
                .ok_or_else(|| Fail::Job("the upload named no file".into()))?;
            names.push(match v["subfolder"].as_str().filter(|s| !s.is_empty()) {
                Some(sub) => format!("{sub}/{name}"),
                None => name.to_string(),
            });
        }
        edit_workflow(p, prompt, &names, s)
    };
    let body = json!({"prompt": wf, "client_id": "abhoth-router"}).to_string();
    let (status, b) = call(
        cx,
        t,
        "POST",
        "/prompt",
        Some("application/json"),
        Bytes::from(body),
        Duration::from_secs(30),
    )
    .await
    .map_err(Fail::Down)?;
    let detail = || String::from_utf8_lossy(&b[..b.len().min(400)]).to_string();
    if status >= 500 {
        return Err(Fail::Down(format!(
            "submit answered {status}: {}",
            detail()
        )));
    }
    if status != 200 {
        return Err(Fail::Job(format!(
            "workflow refused ({status}): {}",
            detail()
        )));
    }
    let v: Value = serde_json::from_slice(&b).unwrap_or_default();
    let Some(pid) = v["prompt_id"].as_str().map(str::to_string) else {
        return Err(Fail::Job("no prompt_id in ComfyUI's answer".into()));
    };
    super::comfy::remember_prompt(&pid, t);
    let end = Instant::now() + JOB_TIMEOUT;
    let mut fails = 0;
    while Instant::now() < end {
        tokio::time::sleep(POLL).await;
        let (status, b) = match call(
            cx,
            t,
            "GET",
            &format!("/history/{pid}"),
            None,
            Bytes::new(),
            Duration::from_secs(10),
        )
        .await
        {
            Ok(x) if x.0 == 200 => {
                fails = 0;
                x
            }
            Ok(x) => x,
            Err(e) => {
                fails += 1;
                if fails >= POLL_FAILS {
                    return Err(Fail::Down(format!(
                        "lost the connection while rendering: {e}"
                    )));
                }
                continue;
            }
        };
        if status != 200 {
            continue;
        }
        let h: Value = serde_json::from_slice(&b).unwrap_or_default();
        let Some(job) = h.get(&pid) else { continue };
        if job["status"]["status_str"] == "error" {
            let mut msg = "execution error".to_string();
            for m in job["status"]["messages"].as_array().into_iter().flatten() {
                if m[0] == "execution_error" {
                    msg = format!(
                        "{}: {}",
                        m[1]["exception_type"].as_str().unwrap_or("error"),
                        m[1]["exception_message"]
                            .as_str()
                            .unwrap_or_default()
                            .chars()
                            .take(300)
                            .collect::<String>()
                    );
                }
            }
            return Err(Fail::Job(msg));
        }
        let img = job["outputs"]
            .as_object()
            .into_iter()
            .flat_map(|o| o.values())
            .find_map(|o| o["images"].as_array().and_then(|a| a.first()).cloned());
        if let Some(img) = img {
            let q = format!(
                "/view?filename={}&subfolder={}&type={}",
                super::speech::encode(img["filename"].as_str().unwrap_or_default()),
                super::speech::encode(img["subfolder"].as_str().unwrap_or_default()),
                super::speech::encode(img["type"].as_str().unwrap_or("output")),
            );
            let (status, png) = call(
                cx,
                t,
                "GET",
                &q,
                None,
                Bytes::new(),
                Duration::from_secs(60),
            )
            .await
            .map_err(|e| Fail::Down(format!("could not fetch the image: {e}")))?;
            if status != 200 {
                return Err(Fail::Down(format!("/view answered {status}")));
            }
            return Ok(png);
        }
    }
    // give up: drop it so it does not hold the ComfyUI
    let _ = call(
        cx,
        t,
        "POST",
        "/queue",
        Some("application/json"),
        Bytes::from(json!({"delete": [pid]}).to_string()),
        Duration::from_secs(3),
    )
    .await;
    let _ = call(
        cx,
        t,
        "POST",
        "/interrupt",
        Some("application/json"),
        Bytes::from(json!({"prompt_id": pid}).to_string()),
        Duration::from_secs(3),
    )
    .await;
    Err(Fail::Down(format!(
        "the render took longer than {} s",
        JOB_TIMEOUT.as_secs()
    )))
}

/// What a client asked for.
#[derive(Debug, Default)]
pub struct Ask {
    pub prompt: String,
    pub model: Option<String>,
    pub size: Option<String>,
    pub images: Vec<Bytes>,
}

fn fail(status: u16, msg: &str) -> Tried {
    Tried {
        response: super::error(status, msg),
        target: None,
        failed: vec![],
    }
}

/// Parse a generation's JSON body.
pub fn parse_generation(raw: &[u8]) -> Result<Ask, String> {
    let v: Value =
        serde_json::from_slice(raw).map_err(|e| format!("the body must be JSON: {e}"))?;
    let s = |k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    Ok(Ask {
        prompt: s("prompt").ok_or("prompt required")?,
        model: s("model"),
        size: s("size"),
        images: vec![],
    })
}

/// Parse an edit: multipart (`image`, `image[]`; `prompt`, `model`) or JSON with base64 or
/// `data:` images (no URLs: the router fetches nothing for a client).
pub fn parse_edit(content_type: &str, raw: &Bytes) -> Result<Ask, String> {
    let mut a = Ask::default();
    if content_type.to_ascii_lowercase().starts_with("multipart/") {
        for p in super::multipart::parse(content_type, raw)? {
            match p.name.as_str() {
                "image" | "image[]" | "images" | "images[]" if !p.data.is_empty() => {
                    a.images.push(p.data)
                }
                "prompt" if p.filename.is_none() => a.prompt = p.text(),
                "model" if p.filename.is_none() => {
                    a.model = Some(p.text()).filter(|s| !s.is_empty())
                }
                "size" if p.filename.is_none() => a.size = Some(p.text()),
                _ => {}
            }
        }
    } else {
        let v: Value = serde_json::from_slice(raw)
            .map_err(|e| format!("the body must be multipart or JSON: {e}"))?;
        let refs = match v.get("image").or_else(|| v.get("images")) {
            Some(Value::Array(a)) => a.clone(),
            Some(x) => vec![x.clone()],
            None => vec![],
        };
        for r in refs {
            let s = match &r {
                Value::String(s) => s.clone(),
                Value::Object(o) => o
                    .get("b64_json")
                    .or_else(|| o.get("image_url"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                _ => String::new(),
            };
            if s.starts_with("http") {
                return Err(
                    "image URLs are not fetched: send the image as base64 or multipart".into(),
                );
            }
            let b64 = s.split_once("base64,").map_or(s.as_str(), |(_, b)| b);
            let d = crate::control::ssh::b64_decode(b64.trim())
                .ok_or("an image is not valid base64")?;
            a.images.push(Bytes::from(d));
        }
        a.prompt = v["prompt"].as_str().unwrap_or_default().trim().to_string();
        a.model = v["model"].as_str().map(str::to_string);
    }
    if a.prompt.is_empty() {
        return Err("prompt required".into());
    }
    if a.images.is_empty() {
        return Err("image required (multipart field 'image')".into());
    }
    a.images.truncate(EDIT_MAX_IMAGES);
    Ok(a)
}

/// Does this router render `/v1/images` itself (a recipe and a ComfyUI to run it)?
pub fn handles(c: &ControlConfig) -> bool {
    default_recipe(c).is_some_and(|id| plan(c, id).is_ok())
        && c.engines
            .keys()
            .any(|e| place::api_of(c, e) == Some(MediaApi::Comfyui))
}

pub async fn run(cx: &super::Cx<'_>, path: &str, content_type: &str, raw: &Bytes) -> Tried {
    let edit = path.ends_with("/edits");
    let a = match if edit {
        parse_edit(content_type, raw)
    } else {
        parse_generation(raw)
    } {
        Ok(a) => a,
        Err(e) => return fail(400, &e),
    };
    let (name, pin) = match a.model.as_deref() {
        Some(m) => {
            let (n, p) = place::split_pin(m);
            (Some(n.to_string()), p.map(str::to_string))
        }
        None => (None, None),
    };
    let (p, redirected) = match resolve(cx.c, name.as_deref()) {
        Ok(x) => x,
        Err(e) => return fail(400, &e),
    };
    if let Some(r) = &redirected {
        tracing::info!("media: images: model {r} -> {}", p.model);
    }
    if edit && !p.edits {
        return fail(400, &format!("{} does not do edits", p.model));
    }
    if let Some(pin) = &pin
        && !cx.c.servers.contains_key(&pin_server(cx.c, pin))
    {
        return fail(400, &format!("unknown server '{pin}'"));
    }
    let ts = targets(cx.c, &p, pin.as_deref(), &cx.bridge_priority());
    if ts.is_empty() {
        return fail(
            503,
            &format!(
                "no ComfyUI here has every file {} needs ({})",
                p.model,
                p.files.join(", ")
            ),
        );
    }
    let size = if edit {
        (0, 0)
    } else {
        bucket(a.size.as_deref())
    };
    let deadline = Instant::now() + WAIT_TIMEOUT;
    let mut tried: Vec<String> = Vec::new();
    let mut failed: Vec<String> = Vec::new();
    let mut all_job = true;
    loop {
        let (t, claim) = match acquire(cx, &ts, &tried, deadline).await {
            Ok(x) => x,
            Err(e) => {
                let msg = if failed.is_empty() {
                    format!("no image backend available: {e}")
                } else {
                    format!("image generation failed ({})", failed.join("; "))
                };
                let status = if !failed.is_empty() && all_job {
                    500
                } else {
                    503
                };
                return Tried {
                    response: super::error(status, &msg),
                    target: None,
                    failed,
                };
            }
        };
        let started = Instant::now();
        let out = render(cx, &t, &p, &a.prompt, size, &a.images).await;
        drop(claim);
        match out {
            Ok(png) => {
                tracing::info!(
                    "media: images: {} {}x{} on {} in {:.1} s",
                    p.model,
                    size.0,
                    size.1,
                    t.key(),
                    started.elapsed().as_secs_f64()
                );
                let mut r = crate::proxy::json_response(
                    200,
                    &json!({"created": chrono::Utc::now().timestamp(), "backend": t.server,
                            "model": p.model,
                            "data": [{"b64_json": crate::control::ssh::b64_encode(&png)}]}),
                );
                send::mark(&mut r, &t);
                return Tried {
                    response: r,
                    target: Some(t),
                    failed,
                };
            }
            Err(Fail::Down(e)) => {
                place::cool(&t);
                all_job = false;
                failed.push(format!("{}: down: {e}", t.key()));
            }
            Err(Fail::Job(e)) => failed.push(format!("{}: job failed: {e}", t.key())),
        }
        tracing::info!("media: images: {}; trying another", failed.last().unwrap());
        tried.push(t.key());
    }
}

/// `GET /compat/image-api/health` in the shim's shape (the pipeline's `images.py` waits on it).
pub async fn health(cx: &super::Cx<'_>) -> Value {
    let mut backends = Vec::new();
    let all = place::candidates(
        cx.c,
        &Want {
            kinds: &[ModelKind::Image],
            apis: &[MediaApi::Comfyui],
            ..Default::default()
        },
        &cx.bridge_priority(),
    );
    for t in &all {
        let q = queue_len(cx, t).await;
        let busy = claimed().lock().unwrap().contains(&t.key());
        backends.push(json!({"name": t.server, "card": t.card, "engine": t.engine,
                             "healthy": q.is_ok(), "inflight": u8::from(busy),
                             "queue": q.as_ref().ok(), "error": q.err()}));
    }
    let ok = backends.iter().any(|b| b["healthy"] == true);
    json!({"status": if ok { "ok" } else { "down" }, "backends": backends})
}
