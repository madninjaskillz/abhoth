//! `POST /v1/audio/speech` (OpenAI's text to speech): to Chatterbox or Kokoro on DeepThought,
//! then the bridges (truenas.local) when those do not answer.

use std::time::Duration;

use axum::body::Bytes;
use axum::http::{HeaderMap, HeaderValue};
use serde_json::{Map, Value, json};

use super::place::{self, Target, Want};
use super::send::{self, Patience, Prepared, Tried};
use crate::control::caps;
use crate::control::schema::{ControlConfig, MediaApi, ModelKind};

/// OpenAI's own voice names: they mean "the engine's default voice" here.
const OPENAI_VOICES: &[&str] = &[
    "alloy", "ash", "ballad", "coral", "echo", "fable", "nova", "onyx", "sage", "shimmer", "verse",
];

/// What a client asked for.
#[derive(Debug, Clone, PartialEq)]
pub struct Ask {
    pub input: String,
    pub model: Option<String>,
    pub server: Option<String>,
    pub voice: Option<String>,
    pub format: Option<String>,
    pub speed: Option<f64>,
}

pub fn parse(raw: &[u8]) -> Result<Ask, String> {
    let v: Value =
        serde_json::from_slice(raw).map_err(|e| format!("the body must be JSON: {e}"))?;
    let o = v.as_object().ok_or("the body must be a JSON object")?;
    let s = |k: &str| {
        o.get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let input = s("input").ok_or("input is empty")?;
    let (model, server) = match s("model") {
        Some(m) => {
            let (m, srv) = place::split_pin(&m);
            (Some(m.to_string()), srv.map(str::to_string))
        }
        None => (None, None),
    };
    Ok(Ask {
        input,
        model,
        server,
        voice: s("voice"),
        format: s("response_format"),
        speed: o.get("speed").and_then(Value::as_f64),
    })
}

fn kokoro_voice(v: &str) -> bool {
    let b = v.as_bytes();
    b.len() > 3 && b[2] == b'_' && b[..2].iter().all(u8::is_ascii_lowercase)
}

/// The voice to send `engine`: the client's, unless it means nothing there.
pub fn voice_for(c: &ControlConfig, engine: &str, asked: Option<&str>) -> Option<String> {
    let e = c.engines.get(engine);
    let default = e.and_then(|e| e.labels.get("default_voice")).cloned();
    let kokoro = caps::caps(c, engine).base.as_deref() == Some("kokoro") || engine == "kokoro";
    match asked {
        Some(v) if OPENAI_VOICES.contains(&v) => default.or(kokoro.then(|| "bm_fable".into())),
        Some(v) if kokoro && !kokoro_voice(v) => default.or(Some("bm_fable".into())),
        Some(v) => Some(v.to_string()),
        None => default.or(kokoro.then(|| "bm_fable".into())),
    }
}

pub fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The request `t` gets for `a`.
pub fn prepare(c: &ControlConfig, t: &Target, a: &Ask) -> Result<Prepared, String> {
    let voice = voice_for(c, &t.engine, a.voice.as_deref());
    match t.api {
        MediaApi::Synth => {
            let mut q = format!("/synth?mode=whole&text={}", encode(&a.input));
            if let Some(v) = voice {
                q.push_str(&format!("&voice={}", encode(&v)));
            }
            Ok(Prepared {
                method: "GET".into(),
                path_q: q,
                headers: HeaderMap::new(),
                body: Bytes::new(),
            })
        }
        MediaApi::OpenaiSpeech => {
            let served = c
                .engines
                .get(&t.engine)
                .map(|e| e.served_id.clone())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "tts-1".into());
            let mut b = Map::new();
            b.insert("model".into(), json!(served));
            b.insert("input".into(), json!(a.input));
            if let Some(v) = voice {
                b.insert("voice".into(), json!(v));
            }
            b.insert(
                "response_format".into(),
                json!(a.format.clone().unwrap_or_else(|| "wav".into())),
            );
            if let Some(s) = a.speed {
                b.insert("speed".into(), json!(s));
            }
            let mut h = HeaderMap::new();
            h.insert("content-type", HeaderValue::from_static("application/json"));
            Ok(Prepared {
                method: "POST".into(),
                path_q: "/v1/audio/speech".into(),
                headers: h,
                body: Bytes::from(Value::Object(b).to_string()),
            })
        }
        other => Err(format!("{} speaks {}, not speech", t.key(), other.as_str())),
    }
}

pub fn targets(
    c: &ControlConfig,
    a: &Ask,
    bridge_priority: &dyn Fn(&str) -> Option<i32>,
) -> Vec<Target> {
    place::candidates(
        c,
        &Want {
            kinds: &[ModelKind::Tts],
            apis: &[MediaApi::OpenaiSpeech, MediaApi::Synth],
            model: a.model.as_deref(),
            server: a.server.as_deref(),
        },
        bridge_priority,
    )
}

pub async fn run(cx: &super::Cx<'_>, raw: &[u8]) -> Tried {
    let a = match parse(raw) {
        Ok(a) => a,
        Err(e) => {
            return Tried {
                response: super::error(400, &e),
                target: None,
                failed: vec![],
            };
        }
    };
    let ts = targets(cx.c, &a, &cx.bridge_priority());
    send::try_all(
        cx.http,
        cx.hub,
        cx.c,
        &ts,
        "text to speech",
        // a bridge may start its engine first (its own limit is longer)
        Patience(Duration::from_secs(600)),
        |t| prepare(cx.c, t, &a),
    )
    .await
}
