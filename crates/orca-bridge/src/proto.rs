//! The bridge protocol, v1 (ROUTER-MEDIA-PLAN.md 13.1) and the descriptor (13.2).
//!
//! Control messages are JSON text frames `{"t": "<type>", ...}`; stream bodies are binary frames
//! (a 4-byte big-endian stream id, then the payload).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The protocol major this build speaks.
pub const PROTOCOL: u32 = 1;
/// Where the router accepts bridges (on its main port).
pub const WS_PATH: &str = "/bridge/ws";
/// The same endpoint under Migo's name (docs/NAMES.md). Routers accept both; bridges dial
/// [`WS_PATH`] unless their `router` URL names this one, so they reach old routers too.
pub const MIGO_WS_PATH: &str = "/migo/ws";

/// Whether `path` is the bridges' endpoint ([`WS_PATH`] or [`MIGO_WS_PATH`]).
pub fn is_ws_path(path: &str) -> bool {
    path == WS_PATH || path == MIGO_WS_PATH
}
/// Close codes.
pub const CLOSE_BAD_VERSION: u16 = 4400;
pub const CLOSE_BAD_TOKEN: u16 = 4401;
/// Another connection for the same bridge took over.
pub const CLOSE_REPLACED: u16 = 4409;
/// The bridge's id clashes with the router's config, or its descriptor was refused outright.
pub const CLOSE_REFUSED: u16 = 4422;
/// A keepalive every 20 s; 45 s of silence is a dead connection.
pub const PING_SECONDS: u64 = 20;
pub const SILENCE_SECONDS: u64 = 45;
/// Unacknowledged body bytes one side may have in flight per stream.
pub const WINDOW: usize = 1 << 20;
/// Body bytes per binary frame.
pub const CHUNK: usize = 64 * 1024;
/// Bridge tokens start with this.
pub const TOKEN_PREFIX: &str = "abb_";
/// Probes and proxied health checks carry this header: never load a model or switch a layout.
pub const NO_SWITCH: &str = "x-abhoth-no-switch";
/// `open` with this method: a raw TCP stream to the engine's port (Wyoming), no HTTP. The head
/// is 200 once the bridge has connected; binary frames are the bytes each way.
pub const TCP_METHOD: &str = "TCP";

/// One control message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "kebab-case")]
pub enum Frame {
    /// bridge → router, first on every connection.
    Hello {
        v: u32,
        bridge: String,
        version: String,
        descriptor: Descriptor,
    },
    /// router → bridge: accepted.
    Welcome {
        v: u32,
        server: String,
        #[serde(default)]
        settings: Settings,
    },
    /// bridge → router: the whole descriptor again after a local change.
    Descriptor {
        descriptor: Descriptor,
    },
    /// router → bridge: a command; answered by a [`Frame::Result`] with the same id.
    Cmd {
        id: u64,
        op: String,
        #[serde(default)]
        args: Value,
    },
    Result {
        id: u64,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// bridge → router, unprompted: `engine-state`, `download`, `sync`, `log`.
    Event {
        kind: String,
        data: Value,
    },
    /// router → bridge: open a proxied request (or WebSocket) to a local engine.
    Open {
        sid: u32,
        engine: String,
        card: String,
        method: String,
        path: String,
        #[serde(default)]
        headers: Vec<(String, String)>,
        #[serde(default)]
        ws: bool,
    },
    /// bridge → router: the engine's response head (101 for an accepted WebSocket).
    Head {
        sid: u32,
        status: u16,
        #[serde(default)]
        headers: Vec<(String, String)>,
    },
    Credit {
        sid: u32,
        bytes: u64,
    },
    End {
        sid: u32,
    },
    Reset {
        sid: u32,
        #[serde(default)]
        error: String,
    },
    Ping {
        at: u64,
    },
    Pong {
        at: u64,
    },
}

impl Frame {
    pub fn to_text(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".into())
    }
    pub fn parse(text: &str) -> Result<Frame, String> {
        serde_json::from_str(text).map_err(|e| format!("bad frame: {e}"))
    }
}

/// The router's settings for a bridge (`<state_dir>/bridges.json`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub enabled: bool,
    pub priority: i32,
    /// Shared engines the router's admin does not use (left out of the overlay).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub engines_off: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            priority: 0,
            engines_off: Vec::new(),
        }
    }
}

/// What a bridge is and has (13.2). Sent in `hello` and again whenever it changes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Descriptor {
    pub v: u32,
    pub server: ServerDesc,
    #[serde(default)]
    pub cards: Vec<CardDesc>,
    #[serde(default)]
    pub engines: Vec<EngineDesc>,
    /// Model files per folder (`comfyui/diffusion_models`, `hf`), relative to `model_dir`.
    #[serde(default)]
    pub files: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub limits: DescLimits,
    /// The bridge lets a router admin edit its inventory (`config.get` / `config.apply`,
    /// MACHINE.md 8). Absent: false (a headless bridge, or the desktop with the setting off).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub remote_config: bool,
    /// The revision of the bridge's editable config (changes on every save there).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_rev: Option<String>,
    /// The models the bridge shares (MACHINE.md 5): id, kind and the engines that serve it.
    /// Absent for a headless bridge.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<ModelDesc>,
}

/// A model a bridge has and shares.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelDesc {
    pub id: String,
    #[serde(default)]
    pub kind: String,
    /// Engines that can serve it, primary first.
    #[serde(default)]
    pub engines: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ServerDesc {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub os: String,
    #[serde(default)]
    pub arch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comfy_root: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CardDesc {
    pub id: String,
    /// `gpu` or `cpu`.
    pub device: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_uuid: Option<String>,
    #[serde(default)]
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vram_gib: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cores: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ram_gib: Option<f64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub simd: Vec<String>,
    /// Per kind, the engines allowed on the card (the shape of `cards.x.engines`).
    #[serde(default)]
    pub engines: BTreeMap<String, Vec<String>>,
    /// Something outside the bridge also uses this card: never assume all of its memory.
    #[serde(default)]
    pub shared: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reserve_gib: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EngineDesc {
    pub id: String,
    /// `<id>@<version>` of an engine package; null for a manual engine.
    #[serde(default)]
    pub package: Option<String>,
    /// How it runs here: `docker`, `process`, `external`, or a package method.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(default)]
    pub kinds: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caps: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe: Option<String>,
    /// The port the engine listens on, per card (local to the bridge's host).
    #[serde(default)]
    pub ports: BTreeMap<String, u16>,
    /// `on_demand` (started by the first request, stopped when idle), `always`, `external`.
    #[serde(default)]
    pub lifecycle: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_stop_seconds: Option<u64>,
    /// Memory it needs on a card, when known (placement fits against it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_gib: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DescLimits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrent: Option<u32>,
}

impl Descriptor {
    /// Problems that make a descriptor unusable (the router refuses it with these).
    pub fn problems(&self) -> Vec<String> {
        let mut p = Vec::new();
        if self.v != PROTOCOL {
            p.push(format!(
                "descriptor v{} (this side speaks v{PROTOCOL})",
                self.v
            ));
        }
        if !valid_id(&self.server.id) {
            p.push(format!(
                "server id `{}` must be 1-32 of a-z 0-9 - _",
                self.server.id
            ));
        }
        let mut seen = std::collections::BTreeSet::new();
        for c in &self.cards {
            if !valid_card_id(&c.id) {
                p.push(format!(
                    "card id `{}` must be 1-16 of A-Z a-z 0-9 - _",
                    c.id
                ));
            }
            if !seen.insert(c.id.clone()) {
                p.push(format!("card `{}` is listed twice", c.id));
            }
            if c.device != "gpu" && c.device != "cpu" {
                p.push(format!("card `{}`: device must be gpu or cpu", c.id));
            }
        }
        let mut engines = std::collections::BTreeSet::new();
        for e in &self.engines {
            if !valid_id(&e.id) {
                p.push(format!("engine id `{}` must be 1-32 of a-z 0-9 - _", e.id));
            }
            if !engines.insert(e.id.clone()) {
                p.push(format!("engine `{}` is listed twice", e.id));
            }
            for card in e.ports.keys() {
                if !seen.contains(card) {
                    p.push(format!("engine `{}`: port for unknown card `{card}`", e.id));
                }
            }
        }
        p
    }
}

/// A bridge, server or engine id: lower case, digits, `-` and `_`, 1-32 long.
pub fn valid_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 32
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

fn valid_card_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 16
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// A binary frame: the stream id, then the payload.
pub fn encode_data(sid: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&sid.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

pub fn decode_data(frame: &[u8]) -> Option<(u32, &[u8])> {
    if frame.len() < 4 {
        return None;
    }
    let sid = u32::from_be_bytes([frame[0], frame[1], frame[2], frame[3]]);
    Some((sid, &frame[4..]))
}

/// WebSocket streams: one message per binary frame, its first byte the opcode.
pub const WS_TEXT: u8 = 1;
pub const WS_BINARY: u8 = 2;

/// Hop-by-hop headers (and ones the tunnel sets itself), never forwarded either way.
pub fn hop_by_hop(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "host"
            | "content-length"
            | "sec-websocket-key"
            | "sec-websocket-version"
            | "sec-websocket-accept"
            | "sec-websocket-extensions"
    )
}

/// SHA-256 of a token, hex (what the router keeps).
pub fn token_hash(token: &str) -> String {
    use sha2::Digest;
    let d = sha2::Sha256::digest(token.trim().as_bytes());
    d.iter().map(|b| format!("{b:02x}")).collect()
}

/// A new bridge token (`abb_` + 43 url-safe characters, 256 bits).
pub fn new_token() -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut raw = [0u8; 43];
    getrandom::fill(&mut raw).expect("the OS random source");
    let mut s = String::from(TOKEN_PREFIX);
    for b in raw {
        s.push(A[(b & 63) as usize] as char);
    }
    s
}

/// Equal in constant time (token hashes).
pub fn same(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn frames_round_trip_with_their_tag() {
        let f = Frame::Cmd {
            id: 7,
            op: "status".into(),
            args: json!({}),
        };
        let t = f.to_text();
        assert!(t.starts_with(r#"{"t":"cmd""#), "{t}");
        assert_eq!(Frame::parse(&t).unwrap(), f);
        let r = Frame::parse(r#"{"t":"result","id":7,"ok":false,"error":"no"}"#).unwrap();
        assert!(matches!(r, Frame::Result { ok: false, .. }));
        assert!(Frame::parse(r#"{"t":"nope"}"#).is_err());
    }

    #[test]
    fn the_plan_docs_descriptor_parses_and_is_sound() {
        let d: Descriptor = serde_json::from_value(json!({ "v": 1,
          "server": { "id": "truenas", "title": "truenas.local", "os": "linux", "arch": "amd64",
                      "model_dir": "/models", "comfy_root": "/models/comfyui" },
          "cards": [
            { "id": "GPU0", "device": "gpu", "gpu_uuid": "GPU-x", "label": "Quadro M4000", "vram_gib": 8.0,
              "engines": { "tts": ["chatterbox", "kokoro"], "stt": ["whisper"] }, "shared": true },
            { "id": "CPU", "device": "cpu", "cores": 4, "ram_gib": 6.0, "simd": ["avx2"],
              "engines": { "tts": ["kokoro"] } } ],
          "engines": [
            { "id": "chatterbox", "package": "chatterbox@1.0.0", "method": "docker", "kinds": ["tts"],
              "caps": { "inherits": "chatterbox" }, "api": "openai-speech", "probe": "none",
              "ports": { "GPU0": 10204 }, "lifecycle": "on_demand", "idle_stop_seconds": 600 } ],
          "files": { "comfyui/diffusion_models": ["a.safetensors"], "hf": ["owner/name"] },
          "limits": { "max_concurrent": 2 } }))
        .unwrap();
        assert!(d.problems().is_empty(), "{:?}", d.problems());
        let mut bad = d.clone();
        bad.engines[0].ports.insert("GPU9".into(), 1);
        bad.server.id = "Bad Id".into();
        assert_eq!(bad.problems().len(), 2, "{:?}", bad.problems());
    }

    #[test]
    fn data_frames_carry_the_stream_id() {
        let b = encode_data(0x0102_0304, b"hi");
        assert_eq!(&b[..4], &[1, 2, 3, 4]);
        assert_eq!(decode_data(&b), Some((0x0102_0304, &b"hi"[..])));
        assert_eq!(decode_data(&[1, 2]), None);
    }

    #[test]
    fn tokens_are_long_random_and_hashed() {
        let (a, b) = (new_token(), new_token());
        assert!(a.starts_with(TOKEN_PREFIX) && a.len() == 47);
        assert_ne!(a, b);
        assert_eq!(token_hash(&a).len(), 64);
        assert!(same(&token_hash(&a), &token_hash(&format!("{a}\n"))));
        assert!(!same(&token_hash(&a), &token_hash(&b)));
    }
}
