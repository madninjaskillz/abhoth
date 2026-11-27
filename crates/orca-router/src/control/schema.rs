//! `router.toml`: the control plane's configuration (ROUTER-CONFIG.md section 4).
//!
//! These types are the schema: serde with `deny_unknown_fields`, so a typo is an error with the
//! TOML path rather than a silently ignored key. They describe what *can* exist; what was chosen
//! and what happened (active layout, offline cards, jobs, leases, audit) lives in `router.db`
//! ([`super::store`]).
//!
//! Secrets are references only (`"secret:<id>"` pointing at a `[secrets]` entry that names a
//! file or an env var). No field holds a key, a password or a token.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The schema version this build reads (`version = 1`). A newer major version is refused.
pub const SCHEMA_VERSION: u32 = 1;

/// The whole configuration after includes are merged.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ControlConfig {
    pub version: u32,
    /// Glob patterns of further TOML files, relative to the main file (`catalog/*.toml`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<String>,
    #[serde(default)]
    pub router: RouterSection,
    #[serde(default)]
    pub control: ControlSection,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub secrets: IndexMap<String, SecretRef>,
    /// Who may do what (11.1): the accounts link, anonymous LAN, loopback (owned by
    /// [`crate::access`]).
    #[serde(default)]
    pub access: crate::access::AccessSettings,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub servers: IndexMap<String, Server>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub cards: IndexMap<String, Card>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub engines: IndexMap<String, Engine>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub models: IndexMap<String, Model>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub layouts: IndexMap<String, Layout>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub client_profiles: IndexMap<String, ClientProfile>,
    #[serde(default)]
    pub route_defaults: RouteDefaults,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub routes: IndexMap<String, Route>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub upstreams: IndexMap<String, UpstreamDef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub budgets: Vec<Budget>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub guards: IndexMap<String, Guard>,
    #[serde(default)]
    pub slots: SlotsSection,
    #[serde(default)]
    pub placement: PlacementSection,
    #[serde(default)]
    pub on_demand: OnDemandSection,
    /// The inventory model (ROUTER-INVENTORY.md, D152): present once a file has been migrated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inventory: Option<InventorySection>,
    /// Hugging Face search and downloads (D152).
    #[serde(default, skip_serializing_if = "HfSection::is_default")]
    pub hf: HfSection,
    // Reserved for the wider supervisor (SUPERVISOR.md, being written): sensors, fans, alerts,
    // MQTT and calendars. Accepted and kept as raw values so files written for a newer build
    // still load and round-trip; their owners give them typed schemas and validation.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub sensors: IndexMap<String, Value>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub fans: IndexMap<String, Value>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub alerts: IndexMap<String, Value>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub mqtt: IndexMap<String, Value>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub calendar: IndexMap<String, Value>,
}

// ── a value per card, or one for all ─────────────────────────────────────────────────────────

/// `port = 8080` (every card) or `port = { A = 8094, B = 8096 }`.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(untagged)]
pub enum PerCard<T> {
    All(T),
    Each(IndexMap<String, T>),
}

impl<T> Default for PerCard<T> {
    fn default() -> Self {
        PerCard::Each(IndexMap::new())
    }
}

impl<T> PerCard<T> {
    pub fn get(&self, card: &str) -> Option<&T> {
        match self {
            PerCard::All(v) => Some(v),
            PerCard::Each(m) => m.get(card),
        }
    }
    /// Card ids named explicitly (empty for `All`).
    pub fn cards(&self) -> Vec<&str> {
        match self {
            PerCard::All(_) => Vec::new(),
            PerCard::Each(m) => m.keys().map(String::as_str).collect(),
        }
    }
}

// ── router / control ─────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct RouterSection {
    /// This router's id, sent as `x-abhoth-via` when chaining (9.4).
    pub id: String,
    pub listen: String,
    pub state_dir: String,
    pub compat: CompatListeners,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_log: Option<String>,
    pub timezone: String,
    pub limits: Limits,
    /// layoutctl's state directory: after each switch the router writes `live.json` and
    /// `offline.json` there in layoutctl's shape, so a rollback to layoutctl (and older tools)
    /// sees what runs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub legacy_state_dir: Option<String>,
}

impl Default for RouterSection {
    fn default() -> Self {
        Self {
            id: "abhoth".into(),
            listen: "0.0.0.0:8010".into(),
            state_dir: "state".into(),
            compat: CompatListeners::default(),
            request_log: None,
            timezone: "Europe/London".into(),
            limits: Limits::default(),
            legacy_state_dir: None,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct CompatListeners {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lb: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slots: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profiles: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Limits {
    pub timeout_seconds: u64,
    pub connect_timeout_seconds: u64,
    pub max_body_bytes: u64,
    pub failovers: u32,
    pub cooldown_seconds: u64,
    pub strip_fields: Vec<String>,
    pub max_hops: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            timeout_seconds: 3600,
            connect_timeout_seconds: 3,
            max_body_bytes: 64 << 20,
            failovers: 1,
            cooldown_seconds: 10,
            strip_fields: vec!["metadata".into(), "user".into()],
            max_hops: 2,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ControlMode {
    /// Decide and log; touch nothing.
    #[default]
    Observe,
    Act,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct ControlSection {
    /// The build's feature flag: false = data plane only (ROUTER.md behaviour). Off by default.
    pub enabled: bool,
    pub mode: ControlMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub legacy_state_mirror: Option<LegacyMirror>,
    pub reconcile: ReconcileSection,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LegacyMirror {
    pub dir: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pig_slots: Option<String>,
    /// `YYYY-MM-DD`; the mirror stops writing after this day.
    pub until: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct ReconcileSection {
    pub interval_seconds: u64,
    /// Start tries per card per boot before the card is left Failed (layoutctl `ensure`: 3).
    pub tries_per_boot: u32,
    /// Backoff between tries.
    pub backoff_seconds: Vec<u64>,
    /// At boot, a managed card running something other than the active layout is switched
    /// (layoutctl `reconcile --boot`, which ignores the quiet window).
    pub switch_on_boot: bool,
}

impl Default for ReconcileSection {
    fn default() -> Self {
        Self {
            interval_seconds: 60,
            tries_per_boot: 3,
            backoff_seconds: vec![30, 120, 600],
            switch_on_boot: true,
        }
    }
}

// ── secrets ──────────────────────────────────────────────────────────────────────────────────

/// `{ file = "/run/secrets/x" }` or `{ env = "NAME" }`. Never a value.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SecretRef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<String>,
}

// ── servers ──────────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecKind {
    #[default]
    Local,
    Ssh,
    /// A bridge's server (ROUTER-MEDIA-PLAN.md 13.4): set by the bridges overlay, refused in
    /// `router.toml`. Commands travel over the bridge's own connection.
    Bridge,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HostctlMode {
    /// The host runs only commands whose id and exact text are in its root-owned allowlist
    /// (`hostctl export`), builtins by name with path rules, and allowed hostd methods.
    #[default]
    Strict,
    /// The router sends rendered command text (development hosts only; flagged loudly).
    Trusted,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Server {
    #[serde(default)]
    pub title: String,
    pub host: String,
    #[serde(default)]
    pub exec: ExecKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh: Option<SshSettings>,
    #[serde(default)]
    pub hostctl: HostctlMode,
    /// `{var:name}` in any template on this server.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub vars: IndexMap<String, String>,
    /// Host-wide lock files held (hostctl `flock`) for a switch's stop/start/verify steps.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub locks: Vec<String>,
    /// Boot readiness checks; reconcile waits until all pass.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ready: Vec<Hook>,
    #[serde(default = "d_ready_timeout")]
    pub ready_timeout_seconds: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_check: Option<PostCheck>,
    /// Server-level named templates (`{ command = "name" }` hooks).
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub templates: IndexMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<ServerState>,
    /// gpu-power's hold endpoint (`GET {url}/hold?secs=`), held during switches (6.6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_power: Option<GpuPower>,
    /// Containers whose `NVIDIA_VISIBLE_DEVICES` names a card but that are not foreign holders
    /// (utility-only containers such as clockctl, gpu-power).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignore_holders: Vec<String>,
    /// Where models downloaded for this server go (D152: Hugging Face downloads).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_dir: Option<String>,
    /// Folders the admin pages may browse on this server (default: everything but /proc, /sys,
    /// /dev, /run and /boot). On a strict host the allowlist's read paths apply as well.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub browse_roots: Vec<String>,
    /// The ComfyUI model root on this server (D153): component files go to
    /// `<comfy_root>/<role folder>/`. Unset: `<model_dir>/comfyui`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comfy_root: Option<String>,
    /// The Docker API the router runs `standing` engines through (D153 phase 5), e.g. a
    /// loopback docker socket proxy `http://127.0.0.1:2375`. Unset: no standing engines here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docker_api: Option<String>,
    /// Host folders a standing engine's container may mount (each mount must be inside one),
    /// besides `model_dir`, `comfy_root` and the `engine_state` var.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub docker_mount_roots: Vec<String>,
    /// `bridge:<id>` for a server a bridge describes (the overlay sets it; never read from or
    /// written to `router.toml`).
    #[serde(skip)]
    pub origin: Option<String>,
}

fn d_ready_timeout() -> u64 {
    1500
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SshSettings {
    pub user: String,
    #[serde(default = "d_ssh_port")]
    pub port: u16,
    /// `"secret:<id>"`: the private key file.
    pub key: String,
    /// Pinned host key: `SHA256:<base64>` fingerprint, or a full `ssh-ed25519 AAAA…` line.
    pub host_key: String,
    #[serde(default = "d_connect_timeout")]
    pub connect_timeout: u64,
    #[serde(default = "d_keepalive")]
    pub keepalive_seconds: u64,
}

fn d_ssh_port() -> u16 {
    22
}
fn d_connect_timeout() -> u64 {
    5
}
fn d_keepalive() -> u64 {
    15
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PostCheck {
    pub route: String,
    #[serde(default = "d_post_tokens")]
    pub max_tokens: u32,
}

fn d_post_tokens() -> u32 {
    64
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ServerState {
    /// Only used while `router.db` has no active layout for the server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_layout: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct GpuPower {
    pub url: String,
    #[serde(default = "d_hold_extra")]
    pub hold_extra_seconds: u64,
}

fn d_hold_extra() -> u64 {
    120
}

/// A hook step: a hostctl builtin with its parameters, or a command (a named template of the
/// server or the engine, or literal template text).
///
/// `{ builtin = "numa-room", node = 0, need_gib = 64, drop_caches = true }`,
/// `{ command = "midclt-apps-settled", timeout = 60 }`.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct Hook {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub builtin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
    /// Builtin parameters (checked per builtin by validation).
    #[serde(flatten)]
    pub params: IndexMap<String, Value>,
}

// ── cards ────────────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Card {
    pub server: String,
    /// A GPU (the default) or the server's CPU (D153: engines that run on the CPU, such as
    /// Stable Audio's tflite build, are placed on a CPU card like any other).
    #[serde(default, skip_serializing_if = "DeviceKind::is_gpu")]
    pub device: DeviceKind,
    /// Pinned by UUID, never by index (index order is not stable). Empty for a CPU card.
    #[serde(default)]
    pub gpu_uuid: String,
    /// CPU cards: the cores engines on it may use (informational; `cpus` pins them).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cores: Option<u32>,
    /// CPU cards: the RAM engines on it may use, in GiB (placement fits against it, less
    /// `reserve_gib`, the way a GPU card fits against its VRAM).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ram_gib: Option<f64>,
    #[serde(default)]
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vram_gib: Option<f64>,
    #[serde(default = "d_reserve")]
    pub reserve_gib: f64,
    /// `docker --cpuset-cpus` for engines on this card.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpus: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clocks: Option<ClockExpect>,
    /// `false`: discovered and shown, never touched (ComfyUI's card).
    #[serde(default = "d_true")]
    pub managed: bool,
    /// May be switched by an on-demand request.
    #[serde(default)]
    pub on_demand: bool,
    /// Which engines may run here, per model type (D152): `llm = ["strata", "ninfer"]`,
    /// `tts = ["maya", "ninfer"]`. Unset: any engine with a port for this card may run LLMs
    /// (the behaviour before the inventory model).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engines: Option<IndexMap<String, Vec<String>>>,
}

/// What a card is (D153).
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeviceKind {
    #[default]
    Gpu,
    Cpu,
}

impl DeviceKind {
    pub fn is_gpu(&self) -> bool {
        *self == DeviceKind::Gpu
    }
}

impl Card {
    pub fn is_cpu(&self) -> bool {
        self.device == DeviceKind::Cpu
    }
    /// The memory engines on it fit against: VRAM for a GPU, the RAM budget for a CPU card.
    pub fn memory_gib(&self) -> Option<f64> {
        if self.is_cpu() {
            self.ram_gib
        } else {
            self.vram_gib
        }
    }
}

fn d_reserve() -> f64 {
    1.5
}
fn d_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ClockExpect {
    /// `"unlocked"` (the only expectation today).
    #[serde(default = "d_unlocked")]
    pub expect: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_sm_mhz: Option<u32>,
    /// Application clocks `[memory, graphics]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_clocks: Option<[u32; 2]>,
    #[serde(default)]
    pub on_mismatch: ClockMismatch,
}

fn d_unlocked() -> String {
    "unlocked".into()
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ClockMismatch {
    #[default]
    Warn,
    Fix,
    Fail,
}

// ── engines ──────────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum ApiKind {
    #[default]
    OpenaiChat,
    OpenaiResponses,
    AnthropicMessages,
}

/// A media engine's HTTP (or Wyoming) surface, for `/v1/audio/*`, `/v1/images/*` and
/// `/comfyui/` (D153, ROUTER-MEDIA-PLAN.md 4.2).
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum MediaApi {
    /// `POST /v1/audio/speech` (the store's Kokoro and Chatterbox servers, Kokoro-FastAPI).
    OpenaiSpeech,
    /// `GET /synth?text=&voice=&mode=whole` (the tts-nano Chatterbox service).
    Synth,
    /// Wyoming over TCP (whisper): the router speaks the protocol itself.
    Wyoming,
    /// `POST /v1/audio/transcriptions` (multipart, OpenAI's shape).
    OpenaiStt,
    /// Stable Audio 3's service: `POST /generate`, `GET /jobs/<id>`, `GET /audio/<id>.mp3`.
    Sa3,
    /// ComfyUI's own API and UI (`/prompt`, `/ws`, `/view`...).
    Comfyui,
    /// `POST /v1/images/generations` and `/edits` (the image API shim until phase 4).
    OpenaiImages,
}

impl MediaApi {
    pub fn as_str(self) -> &'static str {
        match self {
            MediaApi::OpenaiSpeech => "openai-speech",
            MediaApi::Synth => "synth",
            MediaApi::Wyoming => "wyoming",
            MediaApi::OpenaiStt => "openai-stt",
            MediaApi::Sa3 => "sa3",
            MediaApi::Comfyui => "comfyui",
            MediaApi::OpenaiImages => "openai-images",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum DriverKind {
    #[default]
    Docker,
    Compose,
    Systemd,
    Process,
    Template,
    /// Started outside the router (an endpoint that is already running, D153): the router
    /// probes and (later) routes to it, never starts, stops or observes it.
    External,
    /// An installed engine package (D153, ROUTER-MEDIA-PLAN.md 13.3): `package = { id, version }`
    /// names it; its docker and compose methods run as the same commands as those drivers.
    Package,
}

/// How an engine is kept (ROUTER-MEDIA-PLAN.md 3.1).
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Lifecycle {
    /// Kept up on its cards by the standing supervisor; never stopped by the router.
    Standing,
}

impl Engine {
    pub fn is_standing(&self) -> bool {
        self.lifecycle == Some(Lifecycle::Standing)
    }
}

impl ControlConfig {
    /// The memory the standing engines hold on `card`, `except` one engine.
    pub fn standing_gib(&self, card: &str, except: Option<&str>) -> f64 {
        self.engines
            .iter()
            .filter(|(id, e)| {
                e.is_standing() && e.port.get(card).is_some() && Some(id.as_str()) != except
            })
            .map(|(_, e)| e.memory_gib.unwrap_or(0.0))
            .sum()
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StartOrder {
    #[default]
    Parallel,
    /// One card at a time, each healthy before the next, before any parallel engine.
    Sequential,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Engine {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub api: ApiKind,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub also_speaks: Vec<ApiKind>,
    /// How the media routes talk to it (D153); unset: from its catalog kind (`comfyui`,
    /// `sa3`, `whisper` = Wyoming, `kokoro` and `chatterbox` = OpenAI speech).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_api: Option<MediaApi>,
    /// `standing` (D153 phase 5): the router keeps it running on each card of `port` (through
    /// the server's `docker_api`), restarts it with backoff and never stops it. Only for
    /// `driver = "package"` engines with a container method.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifecycle: Option<Lifecycle>,
    /// What a standing engine holds on its card (VRAM on a GPU card, RAM on a CPU card), taken
    /// off the card's room for everything else placed there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_gib: Option<f64>,
    #[serde(default)]
    pub driver: DriverKind,
    /// Docker image (may use `{build_image}`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Container name template (`ninfer-{card_lc}`); layoutctl's names keep rollback working.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    pub port: PerCard<u16>,
    /// `"secret:<id>"`: the engine's API key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default)]
    pub key_delivery: KeyDelivery,
    #[serde(default)]
    pub served_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_context: Option<u64>,
    #[serde(default)]
    pub health: HealthSpec,
    #[serde(default)]
    pub verify: VerifySpec,
    #[serde(default)]
    pub stop: StopSpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_estimate_seconds: Option<PerCard<u64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_probe: Option<IdleProbe>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_log: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_parser: Option<String>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub labels: IndexMap<String, String>,
    /// The command line (docker, process and systemd drivers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<RunSpec>,
    /// Directory of model files relative artifact paths are under (`{model_dir}`, `{model_path}`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_dir: Option<String>,
    #[serde(default)]
    pub spec_label: SpecLabel,
    /// Option defaults (layoutctl `NINFER_DEFAULTS`); `{lanes}` etc. allowed in strings.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub defaults: IndexMap<String, Value>,
    /// Build name -> directory (`{build_dir}`) or image (`{build_image}`).
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub builds: IndexMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_build: Option<String>,
    #[serde(default)]
    pub params: RequestParams,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub profiles: IndexMap<String, EngineProfile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compose: Option<ComposeSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub systemd: Option<SystemdSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process: Option<ProcessSpec>,
    /// `driver = "template"`: the admin writes start/stop/status/logs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub templates: Option<DriverTemplates>,
    #[serde(default)]
    pub start_order: StartOrder,
    /// Coupling group (5.5): members on one server restart together.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub share: Option<String>,
    /// Engines whose (re)start on the same server forces a restart of this one after them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub restart_if_peer_starts: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prepare: Vec<Hook>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub before_start: Vec<Hook>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub adapters: Vec<String>,
    /// What it can run (D152): the built-in catalog's entry for its kind (its id, or
    /// `caps.inherits`), refined by a live probe, then these overrides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caps: Option<EngineCaps>,
    /// `driver = "package"`: the installed package it runs (D153).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<PackageRef>,
}

/// An installed engine package (`<state_dir>/engines/<server>/<id>/<version>/`).
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PackageRef {
    pub id: String,
    pub version: String,
    /// Pin a method type (`docker`); default: the one it was installed with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    /// The manifest's params (`preview = "auto"`).
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub params: IndexMap<String, Value>,
    /// The store it came from (`default`, `my`, a store id), for updates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store: Option<String>,
    /// Host ports for the package's extra ports, by `ports[].id` (`wyoming = 11203`); the first
    /// port is the engine's `port`. An extra port left out is not published.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub ports: IndexMap<String, u16>,
    /// Extra environment for the engine's container, over the package's own (standing engines).
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub env: IndexMap<String, String>,
}

/// An engine's capabilities, as overrides of its kind's (ROUTER-INVENTORY.md 3).
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EngineCaps {
    /// A built-in engine kind (`llamacpp`) or another engine's id to start from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inherits: Option<String>,
    /// Replace the inherited lists (None: keep them). An empty `archs`/`quants` = any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kinds: Option<Vec<ModelKind>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub formats: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archs: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quants: Option<Vec<String>>,
    /// Add to / take from the inherited lists.
    #[serde(default, skip_serializing_if = "CapsDelta::is_empty")]
    pub add: CapsDelta,
    #[serde(default, skip_serializing_if = "CapsDelta::is_empty")]
    pub remove: CapsDelta,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_context: Option<u64>,
    /// Ask the running engine what it is (`/props`, `/v1/models`); default true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CapsDelta {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kinds: Vec<ModelKind>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub formats: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub archs: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub quants: Vec<String>,
}

impl CapsDelta {
    pub fn is_empty(&self) -> bool {
        self.kinds.is_empty()
            && self.formats.is_empty()
            && self.archs.is_empty()
            && self.quants.is_empty()
    }
}

/// How the engine key reaches the engine (6.4). Designed to work whether or not the engine has a
/// key-file flag: `file_flag` is the target; `wrapper` reads the file inside the container (the
/// key stays out of `docker inspect` but is in the engine's argv); `argv` is today's behaviour
/// (layoutctl parity only; validation warns).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct KeyDelivery {
    #[serde(default)]
    pub mode: KeyMode,
    /// The engine's flag: `--api-key-file` for `file_flag`, `--api-key` for `wrapper`/`argv`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flag: Option<String>,
    /// Where the key file is mounted inside the engine.
    #[serde(default = "d_key_path")]
    pub path: String,
    /// Where hostctl `secret-put` writes it on the host (tmpfs, 0600).
    #[serde(default = "d_key_host_dir")]
    pub host_dir: String,
    /// Builds known to read `--api-key-file` (our ninfer fork from 2026-11-26): with `wrapper`
    /// mode, instances of these builds get `file_flag` instead; older builds keep the wrapper.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub file_flag_builds: Vec<String>,
}

impl Default for KeyDelivery {
    fn default() -> Self {
        Self {
            mode: KeyMode::default(),
            flag: None,
            path: d_key_path(),
            host_dir: d_key_host_dir(),
            file_flag_builds: Vec::new(),
        }
    }
}

fn d_key_path() -> String {
    "/run/secrets/engine-key".into()
}
fn d_key_host_dir() -> String {
    "/run/abhoth/secrets".into()
}

impl KeyDelivery {
    /// The mode for an instance of `build`: `wrapper` becomes `file_flag` on builds listed in
    /// `file_flag_builds`.
    pub fn mode_for(&self, build: Option<&str>) -> KeyMode {
        if self.mode == KeyMode::Wrapper
            && build.is_some_and(|b| self.file_flag_builds.iter().any(|x| x == b))
        {
            KeyMode::FileFlag
        } else {
            self.mode
        }
    }

    /// The engine flag for an instance of `build`.
    pub fn flag_for(&self, build: Option<&str>) -> &str {
        if self.mode_for(build) != self.mode {
            "--api-key-file"
        } else {
            self.flag_or_default()
        }
    }

    pub fn flag_or_default(&self) -> &str {
        match (&self.flag, self.mode) {
            (Some(f), _) => f,
            (None, KeyMode::FileFlag) => "--api-key-file",
            (None, _) => "--api-key",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum KeyMode {
    #[default]
    FileFlag,
    Wrapper,
    Argv,
    /// No key (an engine on a private network only).
    None,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct HealthSpec {
    pub path: String,
    /// Substring the body must contain (`ok`, `"loaded": true`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expect_body: Option<String>,
    pub every_ms: u64,
    pub timeout_seconds: u64,
    /// While Ready: poll interval and consecutive failures before unhealthy (5.2).
    pub ready_every_seconds: u64,
    pub ready_failures: u32,
}

impl Default for HealthSpec {
    fn default() -> Self {
        Self {
            path: "/health".into(),
            expect_body: None,
            every_ms: 3000,
            timeout_seconds: 900,
            ready_every_seconds: 15,
            ready_failures: 3,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct VerifySpec {
    pub max_tokens: u32,
    pub prompt: String,
    pub timeout_seconds: u64,
}

impl Default for VerifySpec {
    fn default() -> Self {
        Self {
            max_tokens: 16,
            prompt: "Reply with the single word OK.".into(),
            timeout_seconds: 180,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StopMode {
    /// `docker rm -f` loop until gone (survives a zombie exit while pinned memory unpins).
    #[default]
    Remove,
    /// `docker stop -t T`; the container is kept (Strata's compose services).
    Stop,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct StopSpec {
    pub mode: StopMode,
    pub timeout_seconds: u64,
    /// The card counts as free when its used memory is under this (layoutctl: 1 GiB).
    pub gpu_free_below_mib: u64,
    pub gpu_free_timeout_seconds: u64,
}

impl Default for StopSpec {
    fn default() -> Self {
        Self {
            mode: StopMode::Remove,
            timeout_seconds: 120,
            gpu_free_below_mib: 1024,
            gpu_free_timeout_seconds: 90,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct IdleProbe {
    pub path: String,
    /// `any(.is_processing)` is the only form today.
    #[serde(default = "d_busy_when")]
    pub busy_when: String,
}

fn d_busy_when() -> String {
    "any(.is_processing)".into()
}

/// How the engine's command line is built (layoutctl `start_ninfer` / `start_llamacpp`).
///
/// `docker` is every word after `docker run -d --name {container}`, in order, image and engine
/// command included. A word that is exactly a list variable expands to its words:
///
/// | list variable | words |
/// |---|---|
/// | `{options}` | defaults overridden by the artifact's then the profile's options (sorted), minus `fixed` and `pop` keys, rendered by `options_style` |
/// | `{opt_args:<name>}` | `option_args[name]` rendered with `{value}` / `{value_x_lanes}` when the option is set, else nothing |
/// | `{env_args}` | `-e K=V` per profile `env` key (sorted) |
/// | `{mems_args}` | `--cpuset-mems=<mems>` when the profile sets `mems` |
/// | `{label_args}` | `--label k=v` per engine label |
/// | `{key_args}` | the key flag per `key_delivery` (`--api-key-file <path>`, or `--api-key <key>` in argv mode) |
/// | `{key_mount_args}` | `-v <host secret file>:<path>:ro` (file_flag and wrapper modes) |
///
/// `command` is the same for the process and systemd drivers (no docker words).
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RunSpec {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub docker: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    #[serde(default)]
    pub options_style: OptionsStyle,
    /// Options a profile or artifact may not set (the command line sets them itself).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fixed: Vec<String>,
    /// Options taken out of `{options}` (used through `{opt_args:…}` or `{served_id}`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pop: Vec<String>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub option_args: IndexMap<String, Vec<String>>,
}

/// Which keys the `{spec_json}` label carries (layoutctl's `norm_card`, so rollback and
/// layoutctl's `detect()` recognise containers the router started).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SpecLabel {
    /// Of `engine`, `slots`, `model`, `build`, `options`, `env`, `mems`.
    pub keys: Vec<String>,
    /// A build left out of the label (ninfer's `stock`: older specs compare equal).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub omit_build: Option<String>,
}

impl Default for SpecLabel {
    fn default() -> Self {
        Self {
            keys: [
                "engine", "slots", "model", "build", "options", "env", "mems",
            ]
            .map(String::from)
            .to_vec(),
            omit_build: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OptionsStyle {
    /// `{k = v}` -> `--k v`; `true` -> `--k`; `false`/null -> left out.
    #[default]
    Gnu,
    /// `--k=v`.
    GnuEquals,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EngineProfile {
    #[serde(default)]
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub lanes: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ctx: Option<u64>,
    #[serde(default)]
    pub vram_gib: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<String>,
    /// `docker --cpuset-mems` (NUMA node for pinned host memory).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mems: Option<String>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub options: IndexMap<String, Value>,
    /// Options set to "nothing" (left off the command line, and `null` in the spec label:
    /// TOML has no null). They override a default the way `false` does.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unset: Vec<String>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub env: IndexMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ComposeSpec {
    pub project: String,
    /// The compose file (may be `{var:…}` or `{latest_rendered_compose:<app>}`).
    pub file: String,
    pub service: PerCard<String>,
    pub container: PerCard<String>,
    /// File patched before a start (Strata's `parallel`); usually a `json-set` hook instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_file: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SystemdSpec {
    /// `abhoth-ninfer@{card}.service`.
    pub unit: String,
    /// Sidecar file the start writes with the spec JSON (reconcile reads it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec_file: Option<String>,
    /// Environment file written before a start (args, model, key path; never the key).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_file: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ProcessSpec {
    /// The executable; options are appended as for docker (`{args}`).
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Pid and spec sidecar files go here (`{state_dir}/proc` by default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_file: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DriverTemplates {
    pub start: String,
    pub stop: String,
    /// Prints JSON `{"running": bool, "spec": {...}}`.
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logs: Option<String>,
}

/// Request rules for traffic that lands on an engine, an artifact or an upstream (10.5).
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RequestParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort_levels: Option<Vec<String>>,
    /// What "thinking off" maps to: `drop`, `none`, or an effort level.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_off: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub drop: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub strict_fields: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow_fields: Vec<String>,
    /// Merged into the body when absent (LiteLLM `litellmParams` / client `extraBody`).
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub defaults: IndexMap<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_input_tokens: Option<i64>,
}

// ── models ───────────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Model {
    #[serde(default)]
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub vision: bool,
    /// What kind of model it is (D152). Unset: an LLM.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<ModelKind>,
    /// The engine tried first when the router places it (D152). The engines that can run it
    /// are worked out from what it is (format, architecture, quant) and what each engine can
    /// run; this is only a preference. Unset: the first in `prefer`, else the first artifact's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary: Option<String>,
    /// Preferred order after the primary (the rest follow by capability).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prefer: Vec<String>,
    /// Exceptions: engines that run it although their capabilities don't say so, and engines
    /// that never run it although they match.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny: Vec<String>,
    /// What it is (filled from Hugging Face or the file's GGUF metadata).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quant: Option<String>,
    /// Servers that have the files (D152). Unset: assumed on every server (as before).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub servers: Option<Vec<String>>,
    /// Context length the model supports (informational; from its card or config).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<u64>,
    /// Size of its files in GiB (informational; VRAM hints before an artifact says).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_gib: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub licence: Option<String>,
    /// Where it came from (a Hugging Face repo and the files picked).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<ModelSource>,
    /// What part of a workflow this file is (D153), for image and video models that are
    /// components rather than something to ask for: `diffusion`, `text_encoder`, `vae`,
    /// `lora`… It decides the ComfyUI folder the file lives in ([`ComponentRole`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// The component models (by id) this model needs on a server to run there (D153).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub needs: Vec<String>,
    /// Other names clients may ask for it by (D153).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    /// Engine id -> the artifact that engine loads.
    #[serde(default)]
    pub artifacts: IndexMap<String, Artifact>,
    /// How `/v1/images` renders this recipe (D153 phase 4); unset: the defaults of its `arch`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipe: Option<Recipe>,
}

/// An image recipe's workflow settings (ROUTER-MEDIA-PLAN.md 4.3). Its files are its `needs`, by
/// role (`diffusion`, `text_encoder`, `vae`, `lora`).
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Recipe {
    /// The workflow family (`qwen21`); unset: from the model's `arch` (`qwen-image` = `qwen21`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    /// The sampler's sigma schedule (ComfyUI `ManualSigmas`); unset: the family's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sigmas: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cfg: Option<f64>,
    /// `/v1/images/edits` with this recipe (default true).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edits: Option<bool>,
    /// Asked for without a model (or by a name no model has): the default recipe.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub default: bool,
}

impl Model {
    /// A part of a workflow (it has a role), not something clients ask for.
    pub fn is_component(&self) -> bool {
        self.role.is_some()
    }
    pub fn kind(&self) -> ModelKind {
        self.kind.unwrap_or_default()
    }
    /// Are the files on `server` (unset list: assumed everywhere)?
    pub fn on_server(&self, server: &str) -> bool {
        self.servers
            .as_ref()
            .is_none_or(|s| s.iter().any(|x| x == server))
    }
}

/// The model types the inventory knows (D152).
#[derive(
    Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq, Hash, PartialOrd, Ord,
)]
#[serde(rename_all = "snake_case")]
pub enum ModelKind {
    #[default]
    Llm,
    Tts,
    Stt,
    Image,
    Music,
    Embedding,
    /// Video generation (ComfyUI workflows such as Wan or LTXV; D153).
    Video,
}

impl ModelKind {
    pub const ALL: [ModelKind; 7] = [
        ModelKind::Llm,
        ModelKind::Tts,
        ModelKind::Stt,
        ModelKind::Image,
        ModelKind::Music,
        ModelKind::Embedding,
        ModelKind::Video,
    ];
    pub fn as_str(self) -> &'static str {
        match self {
            ModelKind::Llm => "llm",
            ModelKind::Tts => "tts",
            ModelKind::Stt => "stt",
            ModelKind::Image => "image",
            ModelKind::Music => "music",
            ModelKind::Embedding => "embedding",
            ModelKind::Video => "video",
        }
    }
    pub fn parse(s: &str) -> Option<ModelKind> {
        ModelKind::ALL.into_iter().find(|k| k.as_str() == s)
    }
}

/// A component's role and the ComfyUI folder its files live in (D153).
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ComponentRole {
    pub id: &'static str,
    pub title: &'static str,
    /// The folder under the ComfyUI model root (the modern name).
    pub folder: &'static str,
    /// Older folder names ComfyUI also reads for it (`unet`, `clip`).
    pub legacy: &'static [&'static str],
}

pub const COMPONENT_ROLES: &[ComponentRole] = &[
    ComponentRole {
        id: "checkpoint",
        title: "Checkpoint (all in one)",
        folder: "checkpoints",
        legacy: &[],
    },
    ComponentRole {
        id: "diffusion",
        title: "Diffusion model",
        folder: "diffusion_models",
        legacy: &["unet"],
    },
    ComponentRole {
        id: "text_encoder",
        title: "Text encoder",
        folder: "text_encoders",
        legacy: &["clip"],
    },
    ComponentRole {
        id: "vae",
        title: "VAE",
        folder: "vae",
        legacy: &[],
    },
    ComponentRole {
        id: "lora",
        title: "LoRA",
        folder: "loras",
        legacy: &[],
    },
    ComponentRole {
        id: "controlnet",
        title: "ControlNet",
        folder: "controlnet",
        legacy: &[],
    },
    ComponentRole {
        id: "ipadapter",
        title: "IP-Adapter",
        folder: "ipadapter",
        legacy: &[],
    },
    ComponentRole {
        id: "clip_vision",
        title: "CLIP vision",
        folder: "clip_vision",
        legacy: &[],
    },
    ComponentRole {
        id: "upscale",
        title: "Upscaler",
        folder: "upscale_models",
        legacy: &[],
    },
    ComponentRole {
        id: "background_removal",
        title: "Background removal",
        folder: "background_removal",
        legacy: &[],
    },
    ComponentRole {
        id: "embedding",
        title: "Textual inversion",
        folder: "embeddings",
        legacy: &[],
    },
    ComponentRole {
        id: "audio_encoder",
        title: "Audio encoder",
        folder: "audio_encoders",
        legacy: &[],
    },
];

pub fn component_role(id: &str) -> Option<&'static ComponentRole> {
    COMPONENT_ROLES.iter().find(|r| r.id == id)
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ModelSource {
    /// `org/name` on Hugging Face.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hf: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    /// The files picked (a GGUF quant, a safetensors set).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    /// Gated on Hugging Face (accept the licence there; downloads need a token).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub gated: bool,
}

/// `[inventory]`: the migration marker.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct InventorySection {
    pub version: u32,
}

/// `[hf]`: Hugging Face settings. The token is a secret reference (or the file
/// `<state_dir>/secrets/hf-token` the admin pages write); never a value.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HfSection {
    /// `"secret:<id>"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// The API's base (tests and mirrors); default `https://huggingface.co`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
}

impl HfSection {
    pub fn is_default(&self) -> bool {
        self == &HfSection::default()
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    /// Empty for engines that load no file of their own (Strata).
    #[serde(default)]
    pub file: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub served_id: Option<String>,
    #[serde(default)]
    pub vram_gib: f64,
    /// Engine profiles this artifact fits (a mismatch is a warning, as today).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub profiles: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restore: Option<Restore>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub options: IndexMap<String, Value>,
    /// As [`EngineProfile::unset`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unset: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<RequestParams>,
    /// Engine profile used when a pick names none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_profile: Option<String>,
    /// What the file is (default: from its name and the model's own fields).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quant: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Restore {
    /// The archive manifest (TSV: sha256, size, archive path, live path).
    pub from_manifest: String,
}

// ── layouts ──────────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Layout {
    #[serde(default)]
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// OpenOrca agent mode (auto/hands-on/orchestrator/peer), carried for the profiles shim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    pub cards: IndexMap<String, LayoutCard>,
    /// Client tuning keys (`agent.reasoning.lead`, `slots.agentsPerCopy`).
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub client: IndexMap<String, Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    /// `file` (default), `user`, `composed`, `on_demand`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// VRAM over budget is a warning instead of an error (`compose --force`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub force: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hidden: bool,
    /// Anything else the catalog carried (summaries are generated; kept for the shim).
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub extra: IndexMap<String, Value>,
}

/// `"same"` (as the previous card), `"off"`, `"held"` (used by an earlier card's instance that
/// spans it: that card's spec lists it in `devices`), or a spec.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(untagged)]
pub enum LayoutCard {
    Word(String),
    Spec(CardSpec),
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct CardSpec {
    pub engine: String,
    pub profile: String,
    pub model: String,
    /// One instance over several cards (tensor parallel): every card it uses, its own card
    /// first (`["A", "B"]`). The others are `"held"` in the layout and run nothing of their
    /// own. Empty = just its own card.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub devices: Vec<String>,
}

impl CardSpec {
    /// The cards an instance of this spec on `own` uses: its `devices`, or just `own`.
    pub fn cards_on(&self, own: &str) -> Vec<String> {
        if self.devices.is_empty() {
            vec![own.to_string()]
        } else {
            self.devices.clone()
        }
    }

    /// The other cards it holds (`devices` without its own card).
    pub fn held(&self, own: &str) -> Vec<String> {
        self.devices.iter().filter(|d| *d != own).cloned().collect()
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ClientProfile {
    #[serde(default)]
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub client: IndexMap<String, Value>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub extra: IndexMap<String, Value>,
}

// ── routes ───────────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ColdPolicy {
    #[default]
    Reject,
    Fallback,
    OnDemand,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct RouteDefaults {
    pub cold: ColdPolicy,
    /// `users` | `admins` | `keys:<list>` | `anonymous`.
    pub access: String,
    pub adapters: Vec<String>,
}

impl Default for RouteDefaults {
    fn default() -> Self {
        Self {
            cold: ColdPolicy::Reject,
            access: "users".into(),
            adapters: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Route {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub targets: Vec<Target>,
    /// Same targets and policy as another route (aliases).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub same_as: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cold: Option<ColdPolicy>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fallback: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_demand: Option<RouteOnDemand>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adapters: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hidden: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unsupported: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub allow_user_switch: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<RequestParams>,
}

/// One of `pool`, `card`, `model` or `upstream` (with `model`).
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Target {
    /// `"live"`: every ready local engine on a managed card.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool: Option<String>,
    /// With `pool`: only engines of this kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub card: Option<String>,
    /// A catalog model id, or with `upstream`, the model id the upstream expects.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
    /// Only when every earlier target has no ready upstream.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fallback: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weight: Option<u32>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RouteOnDemand {
    /// `"pick"`: a one-card change of the active layout.
    #[serde(default = "d_pick")]
    pub layout_from: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pick: Option<Pick>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cards: Vec<String>,
    /// Who may cause a switch: `admins` (default) or `keys:<list>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access: Option<String>,
    #[serde(default)]
    pub fallback_while_waiting: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_stay_seconds: Option<u64>,
}

fn d_pick() -> String {
    "pick".into()
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Pick {
    pub engine: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
}

// ── upstreams (non-engine) ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UpstreamKind {
    #[default]
    Openai,
    Anthropic,
    Router,
    /// Reserved (BRIDGE.md 10.3): only valid with `enabled = false`.
    Bridge,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct UpstreamDef {
    pub kind: UpstreamKind,
    #[serde(default)]
    pub base_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<String>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub headers: IndexMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<ApiKind>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub also_speaks: Vec<ApiKind>,
    /// `litellm` | `abhoth` for `kind = "router"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flavour: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health_path: Option<String>,
    #[serde(default = "d_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub prices: IndexMap<String, Price>,
    #[serde(default)]
    pub limits: UpstreamLimits,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connect_timeout: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<RequestParams>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub adapters: Vec<String>,
}

/// $ per million tokens.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Price {
    #[serde(default)]
    pub input: f64,
    #[serde(default)]
    pub output: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct UpstreamLimits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concurrency: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests_per_minute: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_per_minute: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    /// `user:<name>`, `user:*`, `key:<id>`, `route:<name>`, `global`.
    pub scope: String,
    /// An upstream id, a glob, or `*cloud`.
    #[serde(default = "d_star")]
    pub upstream: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub period: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests_per_minute: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_per_minute: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concurrency: Option<u32>,
}

fn d_star() -> String {
    "*".into()
}

// ── guards ───────────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum GuardKind {
    Schedule,
    Process,
    JsonFile,
    FileExists,
    Http,
    Lease,
    AdminHold,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum GuardAction {
    Switch,
    OnDemand,
    Ensure,
    Offline,
    Online,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Guard {
    pub kind: GuardKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    /// `schedule`: `HH:MM-HH:MM` in `router.timezone`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub windows: Vec<String>,
    pub blocks: Vec<GuardAction>,
    #[serde(default = "d_retry")]
    pub retry_seconds: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// `json_file`: keys that make it active when truthy.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub active_when: Vec<String>,
    #[serde(default, rename = "match", skip_serializing_if = "Option::is_none")]
    pub matcher: Option<ProcessMatch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub json_path: Option<String>,
    /// `lease`: the guard-lease reason (`bench`, `video`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// A source not reporting for this long counts as active for switch/on_demand.
    #[serde(default = "d_stale")]
    pub stale_seconds: u64,
}

fn d_retry() -> u64 {
    300
}
fn d_stale() -> u64 {
    45
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ProcessMatch {
    /// Any argv word ending with this (`run_daily.sh`).
    pub argv_any: String,
    /// Executable names (`comm`) accepted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exe: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label_arg: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label_default: Option<String>,
}

// ── slots, placement, on-demand ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct SlotsSection {
    pub ttl_seconds: u64,
    pub lead_reserve: u32,
    /// `{card}{n}`: A1..A4, B1..B4 from the active layout's lanes.
    pub naming: String,
    /// Wait after withdrawing slots before checking leases (layoutctl: 1.5 s).
    pub settle_ms: u64,
}

impl Default for SlotsSection {
    fn default() -> Self {
        Self {
            ttl_seconds: 90,
            lead_reserve: 1,
            naming: "{card}{n}".into(),
            settle_ms: 1500,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct PlacementSection {
    pub lease_aware: bool,
    pub admission: bool,
    pub affinity: bool,
    pub affinity_ttl_seconds: u64,
    pub queue_seconds: u64,
    pub min_move_gain_seconds: f64,
}

impl Default for PlacementSection {
    fn default() -> Self {
        Self {
            lease_aware: true,
            admission: true,
            affinity: true,
            affinity_ttl_seconds: 900,
            queue_seconds: 300,
            min_move_gain_seconds: 1.0,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct OnDemandSection {
    pub queue_seconds: u64,
    pub min_stay_seconds: u64,
    pub idle_grace_seconds: u64,
    pub switch_timeout_seconds: u64,
    pub failure_hold_seconds: u64,
    pub max_switches_per_hour: u32,
    pub max_switches_per_principal_per_hour: u32,
    pub allow_both_cards: bool,
    pub stream_note: String,
}

impl Default for OnDemandSection {
    fn default() -> Self {
        Self {
            queue_seconds: 300,
            min_stay_seconds: 300,
            idle_grace_seconds: 10,
            switch_timeout_seconds: 900,
            failure_hold_seconds: 60,
            max_switches_per_hour: 4,
            max_switches_per_principal_per_hour: 2,
            allow_both_cards: false,
            stream_note: "reasoning".into(),
        }
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────────────────────

impl ControlConfig {
    /// The layout's cards with `"same"` resolved (to the previous card's spec) and `"off"` and
    /// `"held"` as `None` (a held card runs nothing of its own), in card order.
    pub fn resolve_layout(&self, layout: &str) -> Option<Vec<(String, Option<CardSpec>)>> {
        let l = self.layouts.get(layout)?;
        Some(resolve_cards(l))
    }

    /// Managed cards of a server, in config order.
    pub fn managed_cards(&self, server: &str) -> Vec<&str> {
        self.cards
            .iter()
            .filter(|(_, c)| c.server == server && c.managed && !c.is_cpu())
            .map(|(k, _)| k.as_str())
            .collect()
    }

    /// `"secret:<id>"` -> `<id>`.
    pub fn secret_id(reference: &str) -> Option<&str> {
        reference.strip_prefix("secret:")
    }
}

/// See [`ControlConfig::resolve_layout`].
pub fn resolve_cards(l: &Layout) -> Vec<(String, Option<CardSpec>)> {
    let mut out: Vec<(String, Option<CardSpec>)> = Vec::new();
    for (card, lc) in &l.cards {
        let spec = match lc {
            LayoutCard::Spec(s) => Some(s.clone()),
            LayoutCard::Word(w) if w == "same" => out.last().and_then(|(_, s)| s.clone()),
            LayoutCard::Word(_) => None,
        };
        out.push((card.clone(), spec));
    }
    out
}
