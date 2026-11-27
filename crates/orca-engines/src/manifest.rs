//! The engine package manifest, `abhoth-engine.json` (ROUTER-MEDIA-PLAN.md 10 and 13.3).
//!
//! A manifest says what an engine is (kinds, capabilities, hardware, params, API, probe, model
//! folder layout, security needs, licence, source) and lists **methods** in order of preference:
//! the ways it can be installed and run. A host takes the first method that fits it
//! ([`crate::host::choose_method`]).
//!
//! Unknown keys are errors at every level, so a typo fails loudly instead of being ignored.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The manifest file name inside a package folder.
pub const MANIFEST_FILE: &str = "abhoth-engine.json";
/// The manifest schema version this crate reads and writes.
pub const SCHEMA_VERSION: u32 = 1;

/// The model kinds the router knows (`ModelKind` in orca-router).
pub const KINDS: &[&str] = &["llm", "tts", "stt", "image", "music", "embedding", "video"];

/// API types a package may declare (`api`, `also_speaks`). Other strings are a warning, not an
/// error: a new API type should not need a new abhoth to describe.
pub const APIS: &[&str] = &[
    "openai-chat",
    "openai-responses",
    "anthropic-messages",
    "openai-speech",
    "openai-transcriptions",
    "openai-images",
    "openai-embeddings",
    "comfyui",
    "wyoming",
    "sa3",
    "http",
];

/// Probe types (how the router asks a running engine what it is).
pub const PROBES: &[&str] = &[
    "props", "models", "comfyui", "sa3", "wyoming", "http", "none",
];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// An editor hint (`"$schema": "../../schema/abhoth-engine.schema.json"`); ignored.
    #[serde(rename = "$schema", default, skip_serializing_if = "Option::is_none")]
    pub json_schema: Option<String>,
    pub schema: u32,
    /// Lower-case id: `[a-z0-9][a-z0-9._-]*`. The engine id the router suggests.
    pub id: String,
    pub name: String,
    /// The package version (usually the engine's upstream version).
    pub version: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Model kinds it serves (`llm`, `tts`, `stt`, `image`, `music`, `embedding`, `video`).
    pub kinds: Vec<String>,
    #[serde(default, skip_serializing_if = "Caps::is_empty")]
    pub caps: Caps,
    #[serde(default)]
    pub hardware: Hardware,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub params: IndexMap<String, Param>,
    /// The API it speaks on its main port (`openai-chat`, `comfyui`, `wyoming`, `sa3`, …).
    pub api: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub also_speaks: Vec<String>,
    /// How the router probes it (`props`, `models`, `comfyui`, `sa3`, `wyoming`, `http`, `none`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe: Option<String>,
    /// Extra probe endpoints (`{"voices": "/voices"}`): informational, the probe type decides.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub endpoints: IndexMap<String, String>,
    /// Where model files go inside the engine and how the folder is laid out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models: Option<ModelsLayout>,
    /// Install and run methods, most preferred first.
    pub methods: Vec<Method>,
    #[serde(default, skip_serializing_if = "Security::is_default")]
    pub security: Security,
    /// SPDX id (`GPL-3.0`, `Apache-2.0`, `MIT`) or `"other"`/`"proprietary"`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub licence: String,
    /// The upstream project (a URL).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,
    /// Notes shown to the admin (install hints, known limits).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// A private package: may reference local files and URLs with no upstream pin. It can be
    /// saved in My packages and installed here, never published to a store.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub private: bool,
}

/// Capabilities, the same shape as the router's `[engines.X.caps]` (ROUTER-INVENTORY.md 3).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Caps {
    /// A built-in engine kind (`llamacpp`, `comfyui`, `whisper`…) to start from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inherits: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kinds: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub formats: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archs: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quants: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "CapsDelta::is_empty")]
    pub add: CapsDelta,
    #[serde(default, skip_serializing_if = "CapsDelta::is_empty")]
    pub remove: CapsDelta,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_context: Option<u64>,
}

impl Caps {
    pub fn is_empty(&self) -> bool {
        *self == Caps::default()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapsDelta {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kinds: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub formats: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub archs: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub quants: Vec<String>,
}

impl CapsDelta {
    pub fn is_empty(&self) -> bool {
        *self == CapsDelta::default()
    }
}

/// A GPU vendor stack, or `cpu`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GpuKind {
    Cuda,
    Rocm,
    Metal,
    Cpu,
}

impl GpuKind {
    pub fn as_str(self) -> &'static str {
        match self {
            GpuKind::Cuda => "cuda",
            GpuKind::Rocm => "rocm",
            GpuKind::Metal => "metal",
            GpuKind::Cpu => "cpu",
        }
    }
}

/// What the engine needs from the hardware, whatever the method.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hardware {
    /// GPU stacks it can use. Empty: none needed (CPU only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gpu: Vec<GpuKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_vram_gib: Option<f64>,
    /// It also runs on a CPU card (slowly, perhaps).
    #[serde(default)]
    pub cpu_ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_ram_gib: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_cores: Option<u32>,
    /// CPU instruction sets it needs (`avx`, `avx2`, `avx512f`, `neon`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub simd: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ParamType {
    String,
    Int,
    Float,
    Bool,
    Enum,
    /// A path on the host (a host mount source when a mount uses it).
    Path,
}

/// A setting the admin (or a model entry, when `per_model`) gives the engine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Param {
    pub r#type: ParamType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Set per model (`[models.X.params]`) rather than once per engine.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub per_model: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub required: bool,
    /// The value is a secret reference (`secret:<id>`); scrubbed from logs.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub secret: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
}

/// The model folder inside the engine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelsLayout {
    /// The path inside a container (or the env var a host process reads it from, see `env`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub mount: String,
    /// `flat` (files by name), `hf` (a Hugging Face cache), `comfyui` (role subfolders), `repo`
    /// (one folder per `owner/name`).
    pub layout: String,
    /// Subfolders the engine expects (`comfyui`: `diffusion_models`, `text_encoders`, `vae`…).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub folders: Vec<String>,
    /// An env var that points the engine at the folder (`HF_HOME`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<String>,
    /// Mount read-only (default true; ComfyUI writes nothing there either).
    #[serde(default = "yes")]
    pub read_only: bool,
}

fn yes() -> bool {
    true
}

/// Host access beyond the defaults. Each item needs the admin's approval at install.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Security {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub privileged: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub host_network: bool,
    /// Bind mounts outside `{{model_dir}}` and `{{state}}`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub host_mounts: Vec<HostMount>,
    /// Devices (`/dev/kfd`) beyond what the `gpu` stack implies.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub devices: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cap_add: Vec<String>,
}

impl Security {
    pub fn is_default(&self) -> bool {
        *self == Security::default()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostMount {
    /// The host path, may be a template (`{{param.sa3_dir}}`).
    pub source: String,
    pub target: String,
    #[serde(default)]
    pub read_only: bool,
    /// Why it is needed (shown to the admin).
    pub why: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MethodType {
    Compose,
    Docker,
    Pip,
    Uv,
    Conda,
    Binary,
    Git,
    Existing,
}

impl MethodType {
    pub fn as_str(self) -> &'static str {
        match self {
            MethodType::Compose => "compose",
            MethodType::Docker => "docker",
            MethodType::Pip => "pip",
            MethodType::Uv => "uv",
            MethodType::Conda => "conda",
            MethodType::Binary => "binary",
            MethodType::Git => "git",
            MethodType::Existing => "existing",
        }
    }
    /// Runs code on the host itself, outside a container (needs explicit approval).
    pub fn runs_on_host(self) -> bool {
        matches!(
            self,
            MethodType::Pip
                | MethodType::Uv
                | MethodType::Conda
                | MethodType::Binary
                | MethodType::Git
        )
    }
    /// Uses docker on the host.
    pub fn uses_docker(self) -> bool {
        matches!(self, MethodType::Compose | MethodType::Docker)
    }
}

/// One way to install and run the engine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Method {
    pub r#type: MethodType,
    /// A label for this method (`docker-cuda`), shown in the admin; defaults to the type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// `os/arch` entries (`linux/amd64`, `windows/*`, `*`). Empty: any.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub platforms: Vec<String>,
    /// GPU stacks this method uses (`cuda`, `rocm`, `metal`, `cpu`). Empty: the package's
    /// `hardware.gpu`, plus `cpu` when `cpu_ok`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gpu: Vec<GpuKind>,
    #[serde(default, skip_serializing_if = "Requires::is_empty")]
    pub requires: Requires,
    /// The command (argv, templated). For docker: the container's command (after the image).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub run: Vec<String>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub env: IndexMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workdir: Option<String>,
    /// The first port is the main one (the router's `port` for the card).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<PortDecl>,
    #[serde(default, skip_serializing_if = "Health::is_empty")]
    pub health: Health,
    /// Extra bind mounts; sources must be under `{{model_dir}}` or `{{state}}` unless listed in
    /// `security.host_mounts`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mounts: Vec<Mount>,
    /// Steps after an update is installed (argv, templated, no shell).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub update: Vec<Step>,
    /// Steps before the files are removed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub uninstall: Vec<Step>,

    // docker
    /// `registry/name@sha256:<digest>` (a tag may precede the digest).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entrypoint: Option<String>,
    /// Extra `docker run` options from a small safe set (`--shm-size=8g`, `--init`, `--ulimit …`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub docker_args: Vec<String>,

    // compose
    /// An inline compose file (templated).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compose: Option<String>,
    /// A compose template in the package folder (`compose.yml`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compose_file: Option<String>,

    // pip / uv / conda
    /// The Python version (`3.12`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub python: Option<String>,
    /// One pinned package (`kokoro==0.9.4`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
    /// Requirement lines (`name==version --hash=sha256:…`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requirements: Vec<String>,
    /// A requirements file in the package folder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requirements_file: Option<String>,
    /// Extra package indexes (`https://download.pytorch.org/whl/cu128`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_index_urls: Vec<String>,
    /// conda: an environment file in the package folder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment_file: Option<String>,
    /// Text files from the package folder copied into the install dir (a small server shim).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,

    // binary
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assets: Vec<Asset>,

    // git
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// The full 40-hex commit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub build: Vec<Step>,

    // existing
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detect: Option<Detect>,
}

impl Method {
    pub fn label(&self) -> String {
        self.label
            .clone()
            .unwrap_or_else(|| self.r#type.as_str().to_string())
    }

    /// A blank method of a type (tests and the wizard).
    pub fn of(t: MethodType) -> Method {
        Method {
            r#type: t,
            label: None,
            platforms: vec![],
            gpu: vec![],
            requires: Requires::default(),
            run: vec![],
            env: IndexMap::new(),
            workdir: None,
            ports: vec![],
            health: Health::default(),
            mounts: vec![],
            update: vec![],
            uninstall: vec![],
            image: None,
            entrypoint: None,
            docker_args: vec![],
            compose: None,
            compose_file: None,
            python: None,
            package: None,
            requirements: vec![],
            requirements_file: None,
            extra_index_urls: vec![],
            environment_file: None,
            files: vec![],
            assets: vec![],
            repo: None,
            commit: None,
            build: vec![],
            detect: None,
        }
    }

    /// Does this method run in docker on this host (docker, compose, or an existing image)?
    pub fn is_container(&self) -> bool {
        self.r#type.uses_docker()
            || (self.r#type == MethodType::Existing
                && self.detect.as_ref().is_some_and(|d| d.image.is_some()))
    }

    /// Does it start a host process the runner supervises?
    pub fn is_host_process(&self) -> bool {
        !self.is_container()
            && !(self.r#type == MethodType::Existing
                && self.detect.as_ref().is_some_and(|d| d.url.is_some()))
    }
}

/// Host-side tools a method needs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Requires {
    /// Programs that must be on PATH (`git`, `python3`, `nvidia-smi`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commands: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub simd: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_vram_gib: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_ram_gib: Option<f64>,
}

impl Requires {
    pub fn is_empty(&self) -> bool {
        *self == Requires::default()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortDecl {
    /// `http`, `wyoming`, `ws`…
    pub id: String,
    /// The port inside the container (docker and compose).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<u16>,
    /// `http` (default) or `tcp`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Health {
    /// An HTTP path on the main port that answers 2xx when ready.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<String>,
    /// Ready when the main port accepts a TCP connection (Wyoming).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub tcp: bool,
    /// How long a cold start may take before the engine counts as failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_seconds: Option<u64>,
}

impl Health {
    pub fn is_empty(&self) -> bool {
        *self == Health::default()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mount {
    pub source: String,
    pub target: String,
    #[serde(default)]
    pub read_only: bool,
}

/// A command run during install, update or uninstall: argv, no shell.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub run: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub env: IndexMap<String, String>,
    /// Only on these platforms (`windows/*`). Empty: all.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub platforms: Vec<String>,
}

/// A download for one platform.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Asset {
    /// `os/arch` (`windows/amd64`).
    pub platform: String,
    /// GPU stacks this build is for. Empty: any.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gpu: Vec<GpuKind>,
    pub url: String,
    pub sha256: String,
    /// `zip`, `tar.gz`, `tar.xz` or `none` (a single file). Default: from the URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unpack: Option<String>,
    /// Leading path parts to drop when unpacking (`llama-b123/llama-server` → `llama-server`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strip: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

impl Asset {
    pub fn unpack_kind(&self) -> &str {
        if let Some(u) = &self.unpack {
            return u;
        }
        let u = self.url.to_ascii_lowercase();
        if u.ends_with(".zip") {
            "zip"
        } else if u.ends_with(".tar.gz") || u.ends_with(".tgz") {
            "tar.gz"
        } else if u.ends_with(".tar.xz") {
            "tar.xz"
        } else {
            "none"
        }
    }
}

/// How an `existing` engine is found.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Detect {
    /// A program name looked up on PATH (`llama-server`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Paths to try, in order (templated; `~` is the home dir).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    /// Args that print the version (default `["--version"]`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version_args: Option<Vec<String>>,
    /// A regex the version output must match; group 1 (if any) is the version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version_regex: Option<String>,
    /// A docker image already on the host (`local/sa3:1`): run like the `docker` method.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// An endpoint that is already running: health-checked, never started or stopped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// A problem found in a manifest or package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Issue {
    pub level: Level,
    /// Where (`methods[1].image`, `files/compose.yml`, or empty for the whole package).
    pub at: String,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Error,
    Warning,
}

impl Issue {
    pub fn error(at: impl Into<String>, message: impl Into<String>) -> Issue {
        Issue {
            level: Level::Error,
            at: at.into(),
            message: message.into(),
        }
    }
    pub fn warning(at: impl Into<String>, message: impl Into<String>) -> Issue {
        Issue {
            level: Level::Warning,
            at: at.into(),
            message: message.into(),
        }
    }
    pub fn is_error(&self) -> bool {
        self.level == Level::Error
    }
}

impl std::fmt::Display for Issue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let l = match self.level {
            Level::Error => "error",
            Level::Warning => "warning",
        };
        if self.at.is_empty() {
            write!(f, "{l}: {}", self.message)
        } else {
            write!(f, "{l}: {}: {}", self.at, self.message)
        }
    }
}

impl Manifest {
    /// Parse a manifest's JSON. Syntax and shape errors only; [`crate::validate`] checks the rest.
    pub fn parse(text: &str) -> Result<Manifest, Vec<Issue>> {
        let v: Value = serde_json::from_str(text).map_err(|e| {
            vec![Issue::error(
                format!("line {}", e.line()),
                format!("not valid JSON: {e}"),
            )]
        })?;
        Self::from_value(v)
    }

    pub fn from_value(v: Value) -> Result<Manifest, Vec<Issue>> {
        if let Some(s) = v.get("schema").and_then(Value::as_u64)
            && s != u64::from(SCHEMA_VERSION)
        {
            return Err(vec![Issue::error(
                "schema",
                format!("schema {s} is not one this abhoth reads (it reads {SCHEMA_VERSION})"),
            )]);
        }
        serde_json::from_value::<Manifest>(v).map_err(|e| vec![Issue::error("", format!("{e}"))])
    }

    pub fn to_json_pretty(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }

    /// `id@version`.
    pub fn key(&self) -> String {
        format!("{}@{}", self.id, self.version)
    }

    /// The GPU stacks a method can use (its own list, else the package's, plus cpu if allowed).
    pub fn method_gpus(&self, m: &Method) -> Vec<GpuKind> {
        if !m.gpu.is_empty() {
            return m.gpu.clone();
        }
        let mut g = self.hardware.gpu.clone();
        if (self.hardware.cpu_ok || g.is_empty()) && !g.contains(&GpuKind::Cpu) {
            g.push(GpuKind::Cpu);
        }
        g
    }

    /// Does any method need approval to run on the host, or does security ask for anything?
    pub fn needs_approval(&self) -> Vec<String> {
        let mut out = vec![];
        for m in &self.methods {
            if m.r#type.runs_on_host() {
                out.push(format!("{}: runs on the host, not sandboxed", m.label()));
            }
        }
        let s = &self.security;
        if s.privileged {
            out.push("privileged container".into());
        }
        if s.host_network {
            out.push("host network".into());
        }
        for h in &s.host_mounts {
            out.push(format!(
                "host mount {} -> {} ({})",
                h.source, h.target, h.why
            ));
        }
        for d in &s.devices {
            out.push(format!("device {d}"));
        }
        for c in &s.cap_add {
            out.push(format!("capability {c}"));
        }
        out
    }
}
