//! What engines can run and what models are (ROUTER-INVENTORY.md 3, D152).
//!
//! An engine's capabilities (model types, file formats, architectures, quant types) come in
//! three layers: the **built-in catalog** entry for its kind (its id, or `caps.inherits`, which
//! may also name another engine: a fork inherits and adds or removes), then what a **live probe**
//! of the running engine reported (`/props`, `/v1/models`; cached per engine and build, never a
//! switch or a model load), then the **admin's overrides** (`[engines.X.caps]`).
//!
//! A model says what it is: its type, and per file the format (from the name: `.gguf`,
//! `.safetensors`, `.ninfer`…), architecture and quant (from Hugging Face or GGUF metadata). The
//! engines that can run it are computed: every engine whose capabilities match one of its files,
//! plus the model's `allow` exceptions, minus its `deny`. Adding an engine of a known kind (or a
//! fork that inherits one) makes it a candidate for every matching model with no model edits.

use std::collections::BTreeMap;
use std::sync::{OnceLock, RwLock};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::schema::{Artifact, ControlConfig, Engine, Model, ModelKind};

/// A built-in engine kind.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KindDef {
    pub id: &'static str,
    pub title: &'static str,
    pub kinds: &'static [ModelKind],
    pub formats: &'static [&'static str],
    /// Empty: any architecture.
    pub archs: &'static [&'static str],
    pub quants: &'static [&'static str],
    /// How a probe asks it: `props` (llama.cpp's `/props`), `models` (`/v1/models`), `none`.
    pub probe: &'static str,
    /// How it is usually launched (a hint for "Add engine").
    pub driver: &'static str,
    pub notes: &'static str,
}

use ModelKind::*;

/// The built-in catalog (b).
pub const CATALOG: &[KindDef] = &[
    KindDef {
        id: "llamacpp",
        title: "llama.cpp",
        kinds: &[Llm, Embedding],
        formats: &["gguf"],
        archs: &[],
        quants: &[],
        probe: "props",
        driver: "docker",
        notes: "Any GGUF architecture llama.cpp knows; images with an mmproj file.",
    },
    KindDef {
        id: "strata",
        title: "Strata",
        kinds: &[Llm],
        formats: &[],
        archs: &[],
        quants: &[],
        probe: "props",
        driver: "compose",
        notes: "Serves the one MoE build it is set up with: allow it on that model.",
    },
    KindDef {
        id: "ninfer",
        title: "ninfer",
        kinds: &[Llm],
        formats: &["ninfer"],
        archs: &[],
        quants: &[],
        probe: "models",
        driver: "docker",
        notes: "Runs .ninfer files (converted from safetensors).",
    },
    KindDef {
        id: "maya",
        title: "Maya",
        kinds: &[Tts],
        formats: &["safetensors"],
        archs: &[],
        quants: &[],
        probe: "models",
        driver: "docker",
        notes: "Speech from text.",
    },
    KindDef {
        id: "vllm",
        title: "vLLM",
        kinds: &[Llm, Embedding],
        formats: &["safetensors"],
        archs: &[],
        quants: &[],
        probe: "models",
        driver: "docker",
        notes: "Hugging Face safetensors repos (AWQ, GPTQ, FP8 quants).",
    },
    KindDef {
        id: "exllama",
        title: "ExLlamaV2 / TabbyAPI",
        kinds: &[Llm],
        formats: &["exl2"],
        archs: &[],
        quants: &[],
        probe: "models",
        driver: "docker",
        notes: "EXL2 quants.",
    },
    KindDef {
        id: "whisper",
        title: "whisper.cpp",
        kinds: &[Stt],
        formats: &["ggml", "gguf", "safetensors", "ct2"],
        archs: &["whisper"],
        quants: &[],
        probe: "none",
        driver: "docker",
        notes: "Speech to text (whisper.cpp, faster-whisper or transformers Whisper). Wyoming or HTTP.",
    },
    KindDef {
        id: "kokoro",
        title: "Kokoro",
        kinds: &[Tts],
        formats: &["onnx", "pytorch", "safetensors"],
        archs: &["kokoro"],
        quants: &[],
        probe: "none",
        driver: "docker",
        notes: "Small text-to-speech model.",
    },
    KindDef {
        id: "sa3",
        title: "Stable Audio",
        kinds: &[Music],
        formats: &["safetensors", "ckpt", "tflite"],
        archs: &["stable-audio"],
        quants: &[],
        probe: "sa3",
        driver: "docker",
        notes: "Instrumental music. The tflite build runs on the CPU (no card).",
    },
    KindDef {
        id: "comfyui",
        title: "ComfyUI",
        kinds: &[Image, Video],
        formats: &["safetensors", "gguf", "ckpt", "pytorch"],
        archs: &[],
        quants: &[],
        probe: "comfyui",
        driver: "docker",
        notes: "Image and video workflows. Its files are components (diffusion model, text encoder, VAE, LoRA…) in the ComfyUI model folders; a probe lists the files it has.",
    },
    KindDef {
        id: "chatterbox",
        title: "Chatterbox",
        kinds: &[Tts],
        formats: &["safetensors", "pytorch"],
        archs: &["chatterbox"],
        quants: &[],
        probe: "none",
        driver: "docker",
        notes: "Text to speech with cloned voices (Turbo, nano).",
    },
    KindDef {
        id: "qwen-image",
        title: "Qwen-Image",
        kinds: &[Image],
        formats: &["safetensors"],
        archs: &["qwen-image"],
        quants: &[],
        probe: "none",
        driver: "docker",
        notes: "Image generation.",
    },
];

pub fn kind_def(id: &str) -> Option<&'static KindDef> {
    CATALOG.iter().find(|k| k.id == id)
}

/// Effective capabilities and where each part came from.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Caps {
    pub kinds: Vec<ModelKind>,
    pub formats: Vec<String>,
    /// Empty: any.
    pub archs: Vec<String>,
    pub quants: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_context: Option<u64>,
    /// The catalog kind it starts from (after inheritance).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// `catalog:llamacpp`, `engine:llamacpp`, `probe`, `override`.
    pub sources: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// What a probe of a running engine found (cached per engine and build).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Probe {
    /// Image and build the engine ran when probed: a change means probe again.
    pub stamp: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Architectures it was seen serving (evidence it runs them).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub archs_seen: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kinds_seen: Vec<ModelKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ctx: Option<u64>,
    /// Media engines (D153): the model files it has, per folder (ComfyUI `/models/<folder>`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub files: BTreeMap<String, Vec<String>>,
    /// The device it reported (`cuda:0 Tesla V100-SXM2-16GB`, `cpu`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    pub at: i64,
}

fn probes() -> &'static RwLock<BTreeMap<String, Probe>> {
    static P: OnceLock<RwLock<BTreeMap<String, Probe>>> = OnceLock::new();
    P.get_or_init(Default::default)
}

/// The cached probe of `engine` if it is for the build the config names now.
pub fn probe_of(c: &ControlConfig, engine: &str) -> Option<Probe> {
    let e = c.engines.get(engine)?;
    probes()
        .read()
        .ok()?
        .get(engine)
        .filter(|p| p.stamp == stamp(e))
        .cloned()
}

/// Record a probe (and keep the cache file, when one is set).
pub fn record_probe(engine: &str, p: Probe) {
    if let Ok(mut m) = probes().write() {
        m.insert(engine.to_string(), p);
    }
    if let Some(path) = cache_file()
        && let Ok(m) = probes().read()
    {
        let _ = std::fs::write(path, serde_json::to_vec_pretty(&*m).unwrap_or_default());
    }
}

fn cache_file() -> Option<&'static std::path::PathBuf> {
    CACHE.get()
}
static CACHE: OnceLock<std::path::PathBuf> = OnceLock::new();

/// Load the probe cache from `<state_dir>/engine-probes.json` and keep it there.
pub fn load_cache(state_dir: &std::path::Path) {
    let path = state_dir.join("engine-probes.json");
    if let Ok(t) = std::fs::read_to_string(&path)
        && let Ok(m) = serde_json::from_str::<BTreeMap<String, Probe>>(&t)
        && let Ok(mut g) = probes().write()
    {
        *g = m;
    }
    let _ = CACHE.set(path);
}

/// What identifies the engine's build: a different image or build directory = probe again.
pub fn stamp(e: &Engine) -> String {
    let mut s = e.image.clone().unwrap_or_default();
    for (k, v) in &e.builds {
        s.push_str(&format!("|{k}={v}"));
    }
    if let Some(d) = &e.default_build {
        s.push_str(&format!("|default={d}"));
    }
    s
}

/// `qwen3_moe`, `Qwen3-MoE`, `qwen3moe` are the same architecture.
pub fn norm(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

fn add_all<T: PartialEq + Clone>(to: &mut Vec<T>, from: &[T]) {
    for x in from {
        if !to.contains(x) {
            to.push(x.clone());
        }
    }
}

/// An engine's effective capabilities: catalog, probe, overrides.
pub fn caps(c: &ControlConfig, engine: &str) -> Caps {
    let mut out = config_caps(c, engine, 0);
    if let Some(p) = probe_of(c, engine) {
        out.sources.push("probe".into());
        out.version = p.version.clone();
        if !out.archs.is_empty() {
            let seen: Vec<String> = p.archs_seen.iter().map(|a| norm(a)).collect();
            add_all(&mut out.archs, &seen);
        }
        add_all(&mut out.kinds, &p.kinds_seen);
        if out.max_context.is_none() {
            out.max_context = p.ctx;
        }
        // the overrides still win: re-apply them on top
        if let Some(o) = c.engines.get(engine).and_then(|e| e.caps.as_ref()) {
            apply_overrides(&mut out, o);
        }
    }
    out
}

fn apply_overrides(out: &mut Caps, o: &super::schema::EngineCaps) {
    if let Some(k) = &o.kinds {
        out.kinds = k.clone();
    }
    if let Some(f) = &o.formats {
        out.formats = f.iter().map(|x| norm(x)).collect();
    }
    if let Some(a) = &o.archs {
        out.archs = a.iter().map(|x| norm(x)).collect();
    }
    if let Some(q) = &o.quants {
        out.quants = q.iter().map(|x| norm(x)).collect();
    }
    add_all(&mut out.kinds, &o.add.kinds);
    add_all(
        &mut out.formats,
        &o.add.formats.iter().map(|x| norm(x)).collect::<Vec<_>>(),
    );
    if !out.archs.is_empty() {
        add_all(
            &mut out.archs,
            &o.add.archs.iter().map(|x| norm(x)).collect::<Vec<_>>(),
        );
    }
    if !out.quants.is_empty() {
        add_all(
            &mut out.quants,
            &o.add.quants.iter().map(|x| norm(x)).collect::<Vec<_>>(),
        );
    }
    out.kinds.retain(|k| !o.remove.kinds.contains(k));
    let rm = |v: &mut Vec<String>, r: &[String]| {
        let r: Vec<String> = r.iter().map(|x| norm(x)).collect();
        v.retain(|x| !r.contains(x));
    };
    rm(&mut out.formats, &o.remove.formats);
    rm(&mut out.archs, &o.remove.archs);
    rm(&mut out.quants, &o.remove.quants);
    if o.max_context.is_some() {
        out.max_context = o.max_context;
    }
}

/// Catalog + inheritance + overrides (no probe), following `inherits` up to 8 deep.
fn config_caps(c: &ControlConfig, engine: &str, depth: usize) -> Caps {
    let e = c.engines.get(engine);
    let o = e.and_then(|e| e.caps.as_ref());
    let parent = o.and_then(|o| o.inherits.clone());
    let mut out = match parent.as_deref() {
        Some(p) if depth < 8 && p != engine && c.engines.contains_key(p) => {
            let mut x = config_caps(c, p, depth + 1);
            x.sources = vec![format!("engine:{p}")];
            x
        }
        Some(p) => from_catalog(p),
        None => from_catalog(engine),
    };
    if let Some(o) = o {
        let before = out.clone();
        apply_overrides(&mut out, o);
        if out != before || o.kinds.is_some() || o.formats.is_some() {
            out.sources.push("override".into());
        }
    }
    if out.sources.is_empty() {
        out.sources.push("none".into());
    }
    out
}

fn from_catalog(id: &str) -> Caps {
    match kind_def(id) {
        Some(k) => Caps {
            kinds: k.kinds.to_vec(),
            formats: k.formats.iter().map(|x| norm(x)).collect(),
            archs: k.archs.iter().map(|x| norm(x)).collect(),
            quants: k.quants.iter().map(|x| norm(x)).collect(),
            max_context: None,
            base: Some(k.id.into()),
            sources: vec![format!("catalog:{}", k.id)],
            version: None,
        },
        None => Caps::default(),
    }
}

/// Does `engine` run models of `kind` at all?
pub fn runs_kind(c: &ControlConfig, engine: &str, kind: ModelKind) -> bool {
    caps(c, engine).kinds.contains(&kind)
}

// ── what a model's file is ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FileDesc {
    pub format: Option<String>,
    pub arch: Option<String>,
    pub quant: Option<String>,
}

/// The format a file name says (`.gguf`, `.safetensors`, `.ninfer`, …).
pub fn format_of_name(file: &str) -> Option<String> {
    let f = file.to_ascii_lowercase();
    let base = f.rsplit('/').next().unwrap_or(&f);
    if base.contains("exl2") {
        return Some("exl2".into());
    }
    let ext = base.rsplit_once('.').map(|(_, e)| e)?;
    Some(
        match ext {
            "gguf" => "gguf",
            "safetensors" => "safetensors",
            "ninfer" => "ninfer",
            "onnx" => "onnx",
            "tflite" => "tflite",
            "ckpt" => "ckpt",
            "pt" | "pth" => "pytorch",
            "bin" if base.starts_with("ggml") => "ggml",
            "bin" => "pytorch",
            _ => return None,
        }
        .into(),
    )
}

/// The quant a file name says (`Q4_K_M`, `IQ4_XS`, `UD-Q4_K_XL`, `nvfp4`, `F16`…).
pub fn quant_of_name(file: &str) -> Option<String> {
    let base = file.rsplit('/').next().unwrap_or(file);
    let up = base.to_ascii_uppercase();
    let parts: Vec<&str> = up.split(['-', '.']).collect();
    for p in parts.iter().rev() {
        let ok = (p.starts_with('Q') || p.starts_with("IQ"))
            && p.chars()
                .nth(if p.starts_with("IQ") { 2 } else { 1 })
                .is_some_and(|c| c.is_ascii_digit());
        if ok || matches!(*p, "F16" | "BF16" | "F32") {
            return Some(p.to_string());
        }
    }
    for q in ["NVFP4", "FP8", "AWQ", "GPTQ", "INT8", "INT4", "MXFP4"] {
        if up.contains(q) {
            return Some(q.to_ascii_lowercase());
        }
    }
    None
}

/// What one of the model's files is: the artifact's own fields, else the model's, else its name.
pub fn describe(m: &Model, a: &Artifact) -> FileDesc {
    FileDesc {
        format: a
            .format
            .clone()
            .or_else(|| format_of_name(&a.file))
            .or_else(|| m.format.clone())
            .map(|x| norm(&x)),
        arch: a.arch.clone().or_else(|| m.arch.clone()),
        quant: a
            .quant
            .clone()
            .or_else(|| quant_of_name(&a.file))
            .or_else(|| m.quant.clone()),
    }
}

/// Does an engine with `caps` run this file of a `kind` model? `Ok(reason)` or `Err(why not)`.
pub fn matches(caps: &Caps, kind: ModelKind, d: &FileDesc) -> Result<String, String> {
    if !caps.kinds.contains(&kind) {
        return Err(format!("does not run {}", kind.label()));
    }
    let Some(fmt) = &d.format else {
        return Err("the file's format is not known".into());
    };
    if !caps.formats.iter().any(|f| f == fmt) {
        return Err(format!("no {fmt} support"));
    }
    let mut why = vec![fmt.clone()];
    match &d.arch {
        Some(a) if !caps.archs.is_empty() && !caps.archs.contains(&norm(a)) => {
            return Err(format!("no {a} support"));
        }
        Some(a) => why.push(a.clone()),
        None => {}
    }
    if let Some(q) = &d.quant
        && !caps.quants.is_empty()
        && !caps.quants.contains(&norm(q))
    {
        return Err(format!("no {q} quant support"));
    }
    let mut s = format!("{} ✓", why.join(" + "));
    if d.arch.is_none() && !caps.archs.is_empty() {
        s.push_str(" (architecture not recorded)");
    }
    Ok(s)
}

/// Does what the engine is known to run rule this file out? (Unknown = no.)
fn contradicts(cp: &Caps, kind: ModelKind, d: &FileDesc) -> bool {
    (!cp.kinds.is_empty() && !cp.kinds.contains(&kind))
        || d.format
            .as_ref()
            .is_some_and(|f| !cp.formats.is_empty() && !cp.formats.contains(f))
        || d.arch
            .as_ref()
            .is_some_and(|a| !cp.archs.is_empty() && !cp.archs.contains(&norm(a)))
}

/// One engine's answer for a model.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct EngineMatch {
    pub engine: String,
    pub ok: bool,
    pub reason: String,
    /// `capability`, `allow`, `deny`, `none`.
    pub via: String,
    /// Whose file it would load (the artifact's key).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_of: Option<String>,
}

/// Every engine, answered for `model` (unordered: see [`engine_order`]).
pub fn supported(c: &ControlConfig, model: &Model) -> Vec<EngineMatch> {
    // D153: a model made of components (no files of its own) runs on an engine that runs
    // every component it needs
    if model.artifacts.is_empty() && !model.needs.is_empty() && model.role.is_none() {
        let parts: Vec<(&String, Vec<EngineMatch>)> = model
            .needs
            .iter()
            .filter_map(|n| c.models.get(n).map(|m| (n, m)))
            // only files (a role or files of their own): a recipe needing a recipe, or itself,
            // never recurses
            .filter(|(_, m)| m.role.is_some() || !m.artifacts.is_empty() || m.needs.is_empty())
            .map(|(n, m)| (n, supported(c, m)))
            .collect();
        return c
            .engines
            .keys()
            .map(|eid| {
                let bad = parts.iter().find_map(|(n, ms)| {
                    ms.iter()
                        .find(|m| &m.engine == eid)
                        .filter(|m| !m.ok)
                        .map(|m| format!("{n}: {}", m.reason))
                });
                let missing = model.needs.iter().find(|n| !c.models.contains_key(*n));
                let denied = model.deny.iter().any(|d| d == eid);
                let ok = bad.is_none() && missing.is_none() && !denied && !parts.is_empty();
                EngineMatch {
                    engine: eid.clone(),
                    ok,
                    reason: if denied {
                        "never used for this model (deny)".into()
                    } else if let Some(m) = missing {
                        format!("needs `{m}`, which is not set up")
                    } else if let Some(b) = bad {
                        b
                    } else {
                        format!("runs its {} components ✓", parts.len())
                    },
                    via: if denied {
                        "deny".into()
                    } else {
                        "components".into()
                    },
                    file_of: None,
                }
            })
            .collect();
    }
    let kind = model.kind();
    c.engines
        .keys()
        .map(|eid| {
            let cp = caps(c, eid);
            // its own launch settings first, then any other file
            let mut files: Vec<(&String, &Artifact)> = Vec::new();
            if let Some((k, a)) = model.artifacts.get_key_value(eid) {
                files.push((k, a));
            }
            files.extend(model.artifacts.iter().filter(|(k, _)| *k != eid));
            let mut first_err: Option<String> = None;
            let mut hit: Option<(String, String)> = None;
            for (k, a) in &files {
                match matches(&cp, kind, &describe(model, a)) {
                    Ok(r) => {
                        hit = Some(((*k).clone(), r));
                        break;
                    }
                    Err(e) => {
                        first_err.get_or_insert(e);
                    }
                }
            }
            if model.deny.iter().any(|d| d == eid) {
                return EngineMatch {
                    engine: eid.clone(),
                    ok: false,
                    reason: "never used for this model (deny)".into(),
                    via: "deny".into(),
                    file_of: None,
                };
            }
            match hit {
                Some((k, r)) => EngineMatch {
                    engine: eid.clone(),
                    ok: true,
                    reason: r,
                    via: "capability".into(),
                    file_of: Some(k),
                },
                // its own launch settings, and nothing it knows says no (Strata's one build,
                // engines without a catalog entry): runs as before the inventory
                None if model
                    .artifacts
                    .get(eid)
                    .is_some_and(|a| !contradicts(&cp, kind, &describe(model, a))) =>
                {
                    EngineMatch {
                        engine: eid.clone(),
                        ok: true,
                        reason: "its own launch settings".into(),
                        via: "settings".into(),
                        file_of: Some(eid.clone()),
                    }
                }
                None if model.allow.iter().any(|a| a == eid) => EngineMatch {
                    engine: eid.clone(),
                    ok: true,
                    reason: "allowed for this model".into(),
                    via: "allow".into(),
                    file_of: model
                        .artifacts
                        .contains_key(eid)
                        .then(|| eid.clone())
                        .or_else(|| model.artifacts.keys().next().cloned()),
                },
                None => EngineMatch {
                    engine: eid.clone(),
                    ok: false,
                    reason: first_err.unwrap_or_else(|| "the model lists no files".into()),
                    via: "none".into(),
                    file_of: None,
                },
            }
        })
        .collect()
}

/// The engines that can run `model`, in the order the router tries them: primary, then
/// `prefer`, then those with their own launch settings (file order), then the rest.
pub fn engine_order(c: &ControlConfig, model: &Model) -> Vec<String> {
    let ok: Vec<String> = supported(c, model)
        .into_iter()
        .filter(|m| m.ok)
        .map(|m| m.engine)
        .collect();
    let mut out: Vec<String> = Vec::new();
    let mut push = |e: &String| {
        if ok.contains(e) && !out.contains(e) {
            out.push(e.clone());
        }
    };
    if let Some(p) = &model.primary {
        push(p);
    }
    for p in &model.prefer {
        push(p);
    }
    for k in model.artifacts.keys() {
        push(k);
    }
    for e in &ok {
        push(e);
    }
    out
}

/// The artifact `engine` loads for `model`: its own, else the file of another engine it can run.
pub fn artifact_for<'a>(c: &ControlConfig, model: &'a Model, engine: &str) -> Option<&'a Artifact> {
    if let Some(a) = model.artifacts.get(engine) {
        return Some(a);
    }
    let m = supported(c, model)
        .into_iter()
        .find(|m| m.engine == engine && m.ok)?;
    model.artifacts.get(m.file_of.as_deref()?)
}

// ── live probes (a) ──────────────────────────────────────────────────────────────────────────

/// Turn an engine's `/props` and `/v1/models` answers into a [`Probe`].
pub fn parse_probe(
    stamp: String,
    props: Option<&Value>,
    models: Option<&Value>,
    now: i64,
) -> Probe {
    let mut p = Probe {
        stamp,
        at: now,
        ..Default::default()
    };
    if let Some(pr) = props {
        p.version = pr
            .get("build_info")
            .or_else(|| pr.get("version"))
            .and_then(Value::as_str)
            .map(str::to_string);
        if let Some(a) = pr
            .pointer("/model_meta/general.architecture")
            .or_else(|| pr.get("architecture"))
            .and_then(Value::as_str)
        {
            p.archs_seen.push(a.to_string());
        }
        p.ctx = pr
            .pointer("/default_generation_settings/n_ctx")
            .and_then(Value::as_u64);
        if pr.pointer("/modalities/audio").and_then(Value::as_bool) == Some(true) {
            p.kinds_seen.push(ModelKind::Stt);
        }
    }
    if let Some(m) = models
        && let Some(first) = m.get("data").and_then(|d| d.get(0))
    {
        if p.version.is_none() {
            p.version = first
                .get("owned_by")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty() && *s != "organization-owner")
                .map(str::to_string);
        }
        if let Some(a) = first
            .pointer("/meta/general.architecture")
            .or_else(|| first.pointer("/meta/arch"))
            .and_then(Value::as_str)
            && !p.archs_seen.iter().any(|x| x == a)
        {
            p.archs_seen.push(a.to_string());
        }
        if p.ctx.is_none() {
            p.ctx = first
                .pointer("/meta/n_ctx_train")
                .or_else(|| first.get("max_model_len"))
                .and_then(Value::as_u64);
        }
    }
    p
}

/// Ask a running engine at `url` what it is: static reads only (`/props`, `/v1/models`), sent
/// with `x-abhoth-no-switch`, never a completion or a load.
pub async fn probe_engine(
    http: &reqwest::Client,
    url: &str,
    key: Option<&str>,
    how: &str,
    stamp: String,
) -> Option<Probe> {
    let base = url.trim_end_matches('/').trim_end_matches("/v1");
    let get = |path: &str| {
        let mut r = http
            .get(format!("{base}{path}"))
            .header("x-abhoth-no-switch", "1")
            .timeout(std::time::Duration::from_secs(5));
        if let Some(k) = key {
            r = r.bearer_auth(k);
        }
        r
    };
    let props = if how == "props" {
        match get("/props").send().await {
            Ok(r) if r.status().is_success() => r.json::<Value>().await.ok(),
            _ => None,
        }
    } else {
        None
    };
    let models = match get("/v1/models").send().await {
        Ok(r) if r.status().is_success() => r.json::<Value>().await.ok(),
        _ => None,
    };
    if props.is_none() && models.is_none() {
        return None;
    }
    Some(parse_probe(
        stamp,
        props.as_ref(),
        models.as_ref(),
        chrono::Utc::now().timestamp(),
    ))
}

/// How a probe asks `engine` (its kind's way; `none` when probes are off for it).
pub fn probe_how(c: &ControlConfig, engine: &str) -> &'static str {
    let e = c.engines.get(engine);
    if e.and_then(|e| e.caps.as_ref()).and_then(|o| o.probe) == Some(false) {
        return "none";
    }
    caps(c, engine)
        .base
        .as_deref()
        .and_then(kind_def)
        .map_or("models", |k| k.probe)
}

/// Does `engine` need a (re-)probe? Not when the cache holds one for its current build that is
/// under `max_age` seconds old.
pub fn needs_probe(c: &ControlConfig, engine: &str, now: i64, max_age: i64) -> bool {
    probe_how(c, engine) != "none" && probe_of(c, engine).is_none_or(|p| now - p.at > max_age)
}

// ── media engine probes (D153) ──────────────────────────────────────────────────────────────

/// ComfyUI folders a probe lists (component folders, their old names, the GGUF loaders').
pub const COMFY_FOLDERS: &[&str] = &[
    "checkpoints",
    "diffusion_models",
    "unet",
    "unet_gguf",
    "text_encoders",
    "clip",
    "clip_gguf",
    "vae",
    "loras",
    "controlnet",
    "ipadapter",
    "clip_vision",
    "upscale_models",
    "background_removal",
    "embeddings",
    "audio_encoders",
];

/// A ComfyUI probe from `/system_stats` and the `/models/<folder>` lists.
pub fn parse_comfyui(
    stamp: String,
    stats: &Value,
    files: BTreeMap<String, Vec<String>>,
    now: i64,
) -> Probe {
    Probe {
        stamp,
        version: stats
            .pointer("/system/comfyui_version")
            .and_then(Value::as_str)
            .map(|v| format!("ComfyUI {v}")),
        kinds_seen: vec![ModelKind::Image],
        device: stats
            .pointer("/devices/0/name")
            .and_then(Value::as_str)
            .map(str::to_string),
        files,
        at: now,
        ..Default::default()
    }
}

/// A Stable Audio service probe from its `/health` (`{"ok", "model", …}`).
pub fn parse_sa3(stamp: String, health: &Value, now: i64) -> Probe {
    let model = health.get("model").and_then(Value::as_str);
    Probe {
        stamp,
        version: model.map(str::to_string),
        kinds_seen: vec![ModelKind::Music],
        device: model.map(|m| {
            if m.to_ascii_lowercase().contains("cpu") {
                "cpu".to_string()
            } else {
                "gpu".to_string()
            }
        }),
        at: now,
        ..Default::default()
    }
}

/// Ask a running media engine at `base` what it is and which files it has: static reads only
/// (`/system_stats`, `/models`, `/models/<folder>`, `/health`), with `x-abhoth-no-switch`.
/// Never a prompt, a load or a queue change.
pub async fn probe_media(
    http: &reqwest::Client,
    base: &str,
    how: &str,
    stamp: String,
) -> Option<Probe> {
    let base = base.trim_end_matches('/');
    let get = |path: String| async move {
        let r = http
            .get(format!("{base}{path}"))
            .header("x-abhoth-no-switch", "1")
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
            .ok()?;
        if !r.status().is_success() {
            return None;
        }
        r.json::<Value>().await.ok()
    };
    let now = chrono::Utc::now().timestamp();
    match how {
        "comfyui" => {
            // busy (a prompt running): ask another time
            let q = get("/queue".into()).await?;
            if q["queue_running"].as_array().is_some_and(|a| !a.is_empty()) {
                return None;
            }
            let stats = get("/system_stats".into()).await?;
            let listed: Vec<String> = get("/models".into())
                .await
                .and_then(|v| serde_json::from_value(v).ok())
                .unwrap_or_default();
            let mut files = BTreeMap::new();
            for f in COMFY_FOLDERS {
                if !listed.iter().any(|x| x == f) {
                    continue;
                }
                if let Some(v) = get(format!("/models/{f}")).await {
                    let names: Vec<String> = serde_json::from_value(v).unwrap_or_default();
                    if !names.is_empty() {
                        files.insert((*f).to_string(), names);
                    }
                }
            }
            Some(parse_comfyui(stamp, &stats, files, now))
        }
        "sa3" => Some(parse_sa3(stamp, &get("/health".into()).await?, now)),
        _ => None,
    }
}

/// Where each media engine (one whose probe is `comfyui` or `sa3`) listens: (engine, server,
/// base URL), from its ports on GPU and CPU cards.
pub fn media_targets(c: &ControlConfig) -> Vec<(String, String, String)> {
    let host_of = |server: &str| -> Option<String> {
        let s = c.servers.get(server)?;
        Some(if s.exec == super::schema::ExecKind::Local {
            "127.0.0.1".to_string()
        } else {
            s.host.clone()
        })
    };
    let mut out = Vec::new();
    for (eid, e) in &c.engines {
        if !matches!(probe_how(c, eid), "comfyui" | "sa3") {
            continue;
        }
        let mut ports: Vec<(String, u16)> = Vec::new();
        for (card, cd) in &c.cards {
            if let Some(p) = e.port.get(card) {
                ports.push((cd.server.clone(), *p));
            }
        }
        for (server, p) in ports {
            let Some(h) = host_of(&server) else { continue };
            let t = (eid.clone(), server, format!("http://{h}:{p}"));
            if !out.contains(&t) {
                out.push(t);
            }
        }
    }
    out
}

/// Record a media probe for `engine` on `server` (its files differ per server).
pub fn record_media_probe(engine: &str, server: &str, p: Probe) {
    record_probe(&format!("{engine}@{server}"), p.clone());
    record_probe(engine, p);
}

/// The files a media engine reported on `server` (its last probe), by folder.
pub fn files_on(engine: &str, server: &str) -> Option<BTreeMap<String, Vec<String>>> {
    probes()
        .read()
        .ok()?
        .get(&format!("{engine}@{server}"))
        .map(|p| p.files.clone())
}

/// Is a component's file on `server`, by any ComfyUI engine's last probe there? `None`: no
/// probe has listed that server's files.
pub fn component_present(
    c: &ControlConfig,
    role: &super::schema::ComponentRole,
    file: &str,
    server: &str,
) -> Option<bool> {
    let base = file.rsplit(['/', '\\']).next().unwrap_or(file);
    let mut seen = false;
    for eid in c.engines.keys() {
        if probe_how(c, eid) != "comfyui" {
            continue;
        }
        let Some(files) = files_on(eid, server) else {
            continue;
        };
        seen = true;
        let mut folders = vec![role.folder];
        folders.extend(role.legacy.iter().copied());
        if role.id == "diffusion" {
            folders.push("unet_gguf");
        }
        if role.id == "text_encoder" {
            folders.push("clip_gguf");
        }
        if folders.iter().any(|f| {
            files
                .get(*f)
                .is_some_and(|v| v.iter().any(|x| x == file || x == base))
        }) {
            return Some(true);
        }
    }
    seen.then_some(false)
}

/// The engine catalog as the admin pages show it.
pub fn catalog_view() -> Value {
    json!(CATALOG)
}

// ── GGUF metadata (local files) ─────────────────────────────────────────────────────────────

/// `general.architecture` (and the file type) from the start of a GGUF file. `None` when the
/// bytes are not GGUF or the key is not in them.
pub fn gguf_meta(bytes: &[u8]) -> Option<BTreeMap<String, String>> {
    let mut r = Reader { b: bytes, i: 0 };
    if r.take(4)? != b"GGUF" {
        return None;
    }
    let version = r.u32()?;
    if version < 2 {
        return None;
    }
    let _tensors = r.u64()?;
    let kvs = r.u64()?;
    let mut out = BTreeMap::new();
    for _ in 0..kvs.min(100_000) {
        let k = r.string()?;
        let t = r.u32()?;
        let v = r.value(t, 0)?;
        if matches!(
            k.as_str(),
            "general.architecture"
                | "general.name"
                | "general.file_type"
                | "general.quantization_version"
        ) || k.ends_with(".context_length")
        {
            out.insert(k, v);
        }
        if out.contains_key("general.architecture")
            && out.keys().any(|k| k.ends_with(".context_length"))
        {
            break;
        }
    }
    out.contains_key("general.architecture").then_some(out)
}

struct Reader<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.b.get(self.i..self.i.checked_add(n)?)?;
        self.i += n;
        Some(s)
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn string(&mut self) -> Option<String> {
        let n = self.u64()? as usize;
        if n > 1 << 20 {
            return None;
        }
        Some(String::from_utf8_lossy(self.take(n)?).into_owned())
    }
    /// A value as text (arrays are skipped: "[n]").
    fn value(&mut self, t: u32, depth: u32) -> Option<String> {
        Some(match t {
            0 => self.take(1)?[0].to_string(),
            1 => (self.take(1)?[0] as i8).to_string(),
            2 => u16::from_le_bytes(self.take(2)?.try_into().ok()?).to_string(),
            3 => i16::from_le_bytes(self.take(2)?.try_into().ok()?).to_string(),
            4 => self.u32()?.to_string(),
            5 => (self.u32()? as i32).to_string(),
            6 => f32::from_le_bytes(self.take(4)?.try_into().ok()?).to_string(),
            7 => (self.take(1)?[0] != 0).to_string(),
            8 => self.string()?,
            9 => {
                if depth > 2 {
                    return None;
                }
                let et = self.u32()?;
                let n = self.u64()?;
                for _ in 0..n {
                    self.value(et, depth + 1)?;
                }
                format!("[{n}]")
            }
            10 => self.u64()?.to_string(),
            11 => (self.u64()? as i64).to_string(),
            12 => f64::from_le_bytes(self.take(8)?.try_into().ok()?).to_string(),
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests;
