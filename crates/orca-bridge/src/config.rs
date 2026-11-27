//! `bridge.toml`: who the bridge is, where its router is, which cards it offers and which
//! engines it can start. Environment variables override the `[bridge]` keys
//! (`ABHOTH_BRIDGE_ID`, `ABHOTH_BRIDGE_ROUTER`, `ABHOTH_BRIDGE_TOKEN_FILE`,
//! `ABHOTH_BRIDGE_STATE_DIR`, `ABHOTH_BRIDGE_MODEL_DIR`, `ABHOTH_BRIDGE_TITLE`).
//!
//! ```toml
//! [bridge]
//! id = "truenas"
//! router = "http://deepthought:8010"      # ws:// or http:// (wss:// / https:// behind TLS)
//! token_file = "/state/bridge.token"      # mode 600; never printed
//! state_dir = "/state"
//! model_dir = "/models"
//!
//! [detect]
//! gpus = ["GPU-…"]                         # only these (default: every GPU nvidia-smi lists)
//!
//! [cards.GPU0]                             # overrides of a detected card (or a new CPU card)
//! engines = { tts = ["kokoro"], stt = ["whisper"] }
//! shared = true
//!
//! [engines.kokoro]
//! kinds = ["tts"]
//! api = "openai-speech"
//! cards = ["GPU0"]
//! port = 18880                             # or ports = { GPU0 = 18880 }
//! lifecycle = "on_demand"
//! idle_stop_seconds = 600
//! health = "/health"
//! [engines.kokoro.docker]
//! image = "ghcr.io/…@sha256:…"
//! container_port = 8880
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BridgeConfig {
    pub bridge: BridgeSection,
    #[serde(default)]
    pub detect: DetectSection,
    /// Overrides per card id (`GPU0`, `GPU1`, `CPU`).
    #[serde(default)]
    pub cards: BTreeMap<String, CardOverride>,
    #[serde(default)]
    pub engines: BTreeMap<String, EngineConfig>,
    #[serde(default)]
    pub limits: LimitsSection,
    /// The token itself, when the caller holds it (the desktop app's bridge mode); never read
    /// from or written to a file here.
    #[serde(skip)]
    pub token_value: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BridgeSection {
    /// The bridge's id (and its server's id in the router): `a-z 0-9 - _`.
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub title: String,
    /// The router: `http://host:8010`, `ws://host:8010`, or the full `/bridge/ws` URL.
    #[serde(default)]
    pub router: String,
    /// The bridge token's file (mode 600).
    #[serde(default)]
    pub token_file: String,
    /// Engine handles, logs and downloads' part files.
    #[serde(default)]
    pub state_dir: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comfy_root: Option<String>,
    /// Folders under `model_dir` whose files the descriptor lists (default: every top folder,
    /// two levels deep).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub file_folders: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct DetectSection {
    /// Ask `nvidia-smi` for GPUs.
    pub nvidia: bool,
    /// Only these GPU UUIDs (empty: all of them).
    pub gpus: Vec<String>,
    /// Offer the CPU as a card.
    pub cpu: bool,
}

impl Default for DetectSection {
    fn default() -> Self {
        Self {
            nvidia: true,
            gpus: Vec::new(),
            cpu: true,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CardOverride {
    /// Leave this card out of the descriptor.
    #[serde(default)]
    pub disable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vram_gib: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cores: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ram_gib: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reserve_gib: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shared: Option<bool>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub engines: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LimitsSection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrent: Option<u32>,
    /// Count the ZFS ARC above its floor as free RAM for `min_free_ram_gib` (TrueNAS: the ARC
    /// holds most of it and gives it back under pressure). Unset: yes when ZFS is there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count_zfs_arc: Option<bool>,
    /// Kept back from the reclaimable ARC (default 0.5 GiB).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arc_margin_gib: Option<f64>,
}

/// How an engine is run on this host.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EngineConfig {
    #[serde(default)]
    pub kinds: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caps: Option<Value>,
    /// An engine package (`kokoro` or `kokoro@0.9.4`) from the default store: the shared runner
    /// (orca-engines) installs it on first start and runs it. Without `docker`/`process` this is
    /// how the engine starts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
    /// The package's `{{param.<id>}}` values (its defaults otherwise).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<String, Value>,
    /// Let a package use a host-process method (pip, uv, binary, git): code that is not
    /// sandboxed. Off: only docker and compose methods.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub host_processes: bool,
    /// The cards it may run on.
    #[serde(default)]
    pub cards: Vec<String>,
    /// The host port, the same on every card…
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// …or one per card.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub ports: BTreeMap<String, u16>,
    /// `on_demand` (default), `always`, `external` (never started or stopped here).
    #[serde(default = "d_lifecycle")]
    pub lifecycle: String,
    #[serde(default = "d_idle")]
    pub idle_stop_seconds: u64,
    /// The health path (GET, 200 = ready). Default `/health`.
    #[serde(default = "d_health")]
    pub health: String,
    #[serde(default = "d_start_timeout")]
    pub start_timeout_seconds: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_gib: Option<f64>,
    /// Refuse to start while the host has less RAM available than this (Linux MemAvailable;
    /// default: `mem_gib` on a CPU card). The router then sends the request elsewhere.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_free_ram_gib: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docker: Option<DockerRun>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process: Option<ProcessRun>,
}

fn d_lifecycle() -> String {
    "on_demand".into()
}
fn d_idle() -> u64 {
    600
}
fn d_health() -> String {
    "/health".into()
}
fn d_start_timeout() -> u64 {
    600
}

impl EngineConfig {
    pub fn port_for(&self, card: &str) -> Option<u16> {
        self.ports.get(card).copied().or(self.port)
    }
    pub fn method(&self) -> &'static str {
        if self.lifecycle == "external" {
            "external"
        } else if self.docker.is_some() {
            "docker"
        } else if self.process.is_some() {
            "process"
        } else if self.package.is_some() {
            "package"
        } else {
            "external"
        }
    }
}

/// A container (through the `docker` CLI; on a host, a mounted docker socket).
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DockerRun {
    pub image: String,
    /// The port inside the container (published on the engine's host port, loopback only).
    pub container_port: u16,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// `host:container[:ro]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mounts: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpus: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<String>,
    /// Extra `docker run` flags (`--shm-size=1g`). Privileged, host network and host PID are
    /// refused.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub flags: Vec<String>,
    /// Publish on 0.0.0.0 instead of 127.0.0.1 (the bridge itself runs in a container that
    /// reaches engines through the host's gateway).
    #[serde(default)]
    pub publish_all_interfaces: bool,
    /// How the bridge reaches the published port (default 127.0.0.1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reach_host: Option<String>,
}

/// A child process.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessRun {
    /// argv; `{port}`, `{card}`, `{gpu_index}`, `{gpu_uuid}`, `{model_dir}`, `{state_dir}`.
    pub command: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}

impl BridgeConfig {
    /// Read the file, then apply the environment.
    pub fn load(path: &Path) -> Result<BridgeConfig, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut c: BridgeConfig =
            toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        c.apply_env(|k| std::env::var(k).ok());
        c.check()?;
        Ok(c)
    }

    /// Only the environment (no file).
    pub fn from_env() -> Result<BridgeConfig, String> {
        let mut c = BridgeConfig::default();
        c.apply_env(|k| std::env::var(k).ok());
        c.check()?;
        Ok(c)
    }

    pub fn apply_env(&mut self, get: impl Fn(&str) -> Option<String>) {
        let b = &mut self.bridge;
        for (k, slot) in [
            ("ABHOTH_BRIDGE_ID", &mut b.id),
            ("ABHOTH_BRIDGE_TITLE", &mut b.title),
            ("ABHOTH_BRIDGE_ROUTER", &mut b.router),
            ("ABHOTH_BRIDGE_TOKEN_FILE", &mut b.token_file),
            ("ABHOTH_BRIDGE_STATE_DIR", &mut b.state_dir),
        ] {
            if let Some(v) = get(k).filter(|v| !v.trim().is_empty()) {
                *slot = v.trim().to_string();
            }
        }
        if let Some(v) = get("ABHOTH_BRIDGE_MODEL_DIR").filter(|v| !v.trim().is_empty()) {
            b.model_dir = Some(v.trim().to_string());
        }
    }

    pub fn check(&self) -> Result<(), String> {
        let b = &self.bridge;
        let mut p = Vec::new();
        if !crate::proto::valid_id(&b.id) {
            p.push(format!("bridge.id `{}` must be 1-32 of a-z 0-9 - _", b.id));
        }
        if ws_url(&b.router).is_err() {
            p.push(format!(
                "bridge.router `{}` must be an http(s):// or ws(s):// URL",
                b.router
            ));
        }
        if b.token_file.is_empty() && self.token_value.is_none() {
            p.push("bridge.token_file is not set".into());
        }
        for (id, e) in &self.engines {
            if !crate::proto::valid_id(id) {
                p.push(format!("engines.{id}: ids are 1-32 of a-z 0-9 - _"));
            }
            if e.cards.is_empty() {
                p.push(format!("engines.{id}: no cards"));
            }
            for card in &e.cards {
                if e.port_for(card).is_none() {
                    p.push(format!("engines.{id}: no port for card {card}"));
                }
            }
            if !matches!(e.lifecycle.as_str(), "on_demand" | "always" | "external") {
                p.push(format!(
                    "engines.{id}.lifecycle must be on_demand, always or external"
                ));
            }
            if let Some(d) = &e.docker {
                for f in &d.flags {
                    let f = f.to_ascii_lowercase();
                    if f.starts_with("--privileged")
                        || f.starts_with("--network=host")
                        || f.starts_with("--net=host")
                        || f.starts_with("--pid=host")
                        || f.starts_with("--cap-add")
                    {
                        p.push(format!("engines.{id}.docker.flags: `{f}` is not allowed"));
                    }
                }
            }
        }
        if p.is_empty() {
            Ok(())
        } else {
            Err(p.join("\n"))
        }
    }

    pub fn state_dir(&self) -> PathBuf {
        if self.bridge.state_dir.is_empty() {
            PathBuf::from(".")
        } else {
            PathBuf::from(&self.bridge.state_dir)
        }
    }

    /// The token, read fresh from its file each time (rotation needs no restart).
    pub fn token(&self) -> Result<String, String> {
        if let Some(t) = &self.token_value {
            let t = t.trim();
            return if t.starts_with(crate::proto::TOKEN_PREFIX) {
                Ok(t.to_string())
            } else {
                Err("the bridge token setting does not hold a bridge token".into())
            };
        }
        let t = std::fs::read_to_string(&self.bridge.token_file)
            .map_err(|e| format!("the token file {}: {e}", self.bridge.token_file))?;
        let t = t.trim().to_string();
        if !t.starts_with(crate::proto::TOKEN_PREFIX) {
            return Err(format!(
                "the token file {} does not hold a bridge token",
                self.bridge.token_file
            ));
        }
        Ok(t)
    }
}

/// The router's `/bridge/ws` URL from what the config says.
pub fn ws_url(router: &str) -> Result<String, String> {
    let r = router.trim().trim_end_matches('/');
    let (scheme, rest) = r.split_once("://").ok_or("no scheme")?;
    let scheme = match scheme {
        "http" | "ws" => "ws",
        "https" | "wss" => "wss",
        _ => return Err("scheme".into()),
    };
    if rest.is_empty() {
        return Err("no host".into());
    }
    if rest.ends_with(crate::proto::WS_PATH) {
        Ok(format!("{scheme}://{rest}"))
    } else {
        Ok(format!("{scheme}://{rest}{}", crate::proto::WS_PATH))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_become_the_bridge_endpoint() {
        assert_eq!(ws_url("http://h:8010").unwrap(), "ws://h:8010/bridge/ws");
        assert_eq!(ws_url("https://h/").unwrap(), "wss://h/bridge/ws");
        assert_eq!(ws_url("ws://h:1/bridge/ws").unwrap(), "ws://h:1/bridge/ws");
        assert!(ws_url("h:8010").is_err());
        assert!(ws_url("ftp://h").is_err());
    }

    #[test]
    fn a_config_loads_and_the_environment_wins() {
        let mut c: BridgeConfig = toml::from_str(
            r#"
            [bridge]
            id = "nas"
            router = "http://r:8010"
            token_file = "/t"
            [cards.GPU0]
            engines = { tts = ["kokoro"] }
            shared = true
            [engines.kokoro]
            kinds = ["tts"]
            cards = ["GPU0", "CPU"]
            port = 18880
            [engines.kokoro.docker]
            image = "img@sha256:abc"
            container_port = 8880
            "#,
        )
        .unwrap();
        c.apply_env(|k| (k == "ABHOTH_BRIDGE_ROUTER").then(|| "http://other:8010".into()));
        c.check().unwrap();
        assert_eq!(c.bridge.router, "http://other:8010");
        let e = &c.engines["kokoro"];
        assert_eq!(e.lifecycle, "on_demand");
        assert_eq!(e.idle_stop_seconds, 600);
        assert_eq!(e.port_for("CPU"), Some(18880));
        assert_eq!(e.method(), "docker");
    }

    #[test]
    fn unsafe_docker_flags_and_bad_ids_are_refused() {
        let c: BridgeConfig = toml::from_str(
            r#"
            [bridge]
            id = "Bad"
            router = "r"
            token_file = ""
            [engines.x]
            cards = ["GPU0"]
            [engines.x.docker]
            image = "i"
            container_port = 1
            flags = ["--privileged"]
            "#,
        )
        .unwrap();
        let e = c.check().unwrap_err();
        for needle in [
            "bridge.id",
            "bridge.router",
            "token_file",
            "no port",
            "--privileged",
        ] {
            assert!(e.contains(needle), "{needle}: {e}");
        }
    }
}
