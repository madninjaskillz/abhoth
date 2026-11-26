//! What the engines say about the models they run (D149 model metadata): image input and the
//! context window, read from each online card's own `GET /v1/models` (ninfer and OpenRouter-style
//! `architecture.input_modalities`; llama.cpp's `capabilities: ["multimodal"]`) and, when that
//! does not say, `GET /props` (`modalities.vision`, llama.cpp and ninfer).
//!
//! Read-only and cheap: only cards the config has online are asked (a cold card is never
//! touched, nothing is woken: no request lease, no clock wake), each answer is kept for
//! [`TTL`] ([`TTL_FAILED`] when the card did not answer), and a card that is slow to answer
//! costs at most [`TIMEOUT`] once per TTL. The engine's key is the one the config holds.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::config::Snapshot;

/// How long an answer is kept.
pub const TTL: Duration = Duration::from_secs(300);
/// How long "did not answer" is kept.
pub const TTL_FAILED: Duration = Duration::from_secs(60);
/// Per request to an engine.
pub const TIMEOUT: Duration = Duration::from_secs(2);

/// One card's engine, as it describes itself.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EngineCaps {
    /// Takes images (a vision projector is loaded); `None` = it does not say.
    pub vision: Option<bool>,
    /// The context window it serves.
    pub context: Option<i64>,
}

/// When it was asked, the engine URL, and the answer (`None`: it did not answer).
type Entry = (Instant, String, Option<EngineCaps>);

#[derive(Default)]
pub struct CapsCache {
    m: Mutex<HashMap<String, Entry>>,
}

impl CapsCache {
    /// The cached answers for cards that are online now with the same URL.
    pub fn current(&self, snap: &Snapshot) -> HashMap<String, EngineCaps> {
        let m = self.m.lock().unwrap();
        snap.config
            .upstreams
            .iter()
            .filter(|(_, u)| u.online && !u.is_virtual())
            .filter_map(|(id, u)| {
                m.get(id)
                    .filter(|(_, url, _)| *url == u.url)
                    .and_then(|(_, _, c)| c.clone())
                    .map(|c| (id.clone(), c))
            })
            .collect()
    }

    /// Set an answer (tests, and [`refresh`]).
    pub fn put(&self, card: &str, url: &str, caps: Option<EngineCaps>) {
        self.m
            .lock()
            .unwrap()
            .insert(card.into(), (Instant::now(), url.into(), caps));
    }

    fn stale(&self, card: &str, url: &str) -> bool {
        self.m.lock().unwrap().get(card).is_none_or(|(at, u, c)| {
            u != url || at.elapsed() >= if c.is_some() { TTL } else { TTL_FAILED }
        })
    }
}

/// Ask the online cards whose answer is stale, all at once (at most [`TIMEOUT`] x 2).
pub async fn refresh(cache: &CapsCache, http: &reqwest::Client, snap: &Snapshot) {
    let mut set = tokio::task::JoinSet::new();
    for (id, u) in &snap.config.upstreams {
        if !u.online || u.is_virtual() || u.url.is_empty() || !cache.stale(id, &u.url) {
            continue;
        }
        let (id, url, served) = (id.clone(), u.url.clone(), u.served_id.clone());
        let key = snap.key_for(&id).map(str::to_string);
        let http = http.clone();
        set.spawn(async move {
            let caps = probe(&http, &url, key.as_deref(), &served).await;
            (id, url, caps)
        });
    }
    while let Some(r) = set.join_next().await {
        if let Ok((id, url, caps)) = r {
            cache.put(&id, &url, caps);
        }
    }
}

async fn get_json(http: &reqwest::Client, url: &str, key: Option<&str>) -> Option<Value> {
    let mut rb = http
        .get(url)
        .timeout(TIMEOUT)
        .header("accept", "application/json")
        .header(orca_core::router_mode::NO_SWITCH_HEADER, "1");
    if let Some(k) = key {
        rb = rb.bearer_auth(k);
    }
    let r = rb.send().await.ok()?;
    if !r.status().is_success() {
        return None;
    }
    r.json().await.ok()
}

/// One engine's answer; `None` when it did not answer at all.
pub async fn probe(
    http: &reqwest::Client,
    root: &str,
    key: Option<&str>,
    served_id: &str,
) -> Option<EngineCaps> {
    let root = root.trim_end_matches('/');
    let models = get_json(http, &format!("{root}/v1/models"), key).await;
    let mut caps = models
        .as_ref()
        .map(|v| from_models(v, served_id))
        .unwrap_or_default();
    if caps.vision.is_none()
        && let Some(p) = get_json(http, &format!("{root}/props"), key).await
    {
        let pc = from_props(&p);
        caps.vision = pc.vision;
        caps.context = caps.context.or(pc.context);
    }
    if models.is_none() && caps == EngineCaps::default() {
        return None;
    }
    Some(caps)
}

/// From an OpenAI-style `/v1/models` list: the entries for `served_id` (llama.cpp lists the
/// model twice, `data` and `models`, with `capabilities` only in the latter), else the first of
/// each list.
pub fn from_models(v: &Value, served_id: &str) -> EngineCaps {
    let lists: Vec<&Vec<Value>> = ["data", "models"]
        .iter()
        .filter_map(|k| v.get(*k).and_then(Value::as_array))
        .collect();
    let matches: Vec<&Value> = lists
        .iter()
        .flat_map(|l| l.iter())
        .filter(|m| {
            !served_id.is_empty()
                && ["id", "model", "name"]
                    .iter()
                    .any(|k| m.get(*k).and_then(Value::as_str) == Some(served_id))
        })
        .collect();
    let entries: Vec<&Value> = if matches.is_empty() {
        lists.iter().filter_map(|l| l.first()).collect()
    } else {
        matches
    };
    let strs = |v: Option<&Value>| -> Option<Vec<String>> {
        v.and_then(Value::as_array).map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_ascii_lowercase)
                .collect()
        })
    };
    let vision_of = |m: &Value| {
        strs(m.pointer("/architecture/input_modalities"))
            .or_else(|| strs(m.get("input_modalities")))
            .map(|mods| mods.iter().any(|x| x == "image"))
            .or_else(|| {
                strs(m.get("capabilities"))
                    .filter(|c| !c.is_empty())
                    .map(|c| c.iter().any(|x| x == "multimodal" || x == "vision"))
            })
    };
    let context_of = |m: &Value| {
        ["context_window", "context_length", "max_model_len"]
            .iter()
            .find_map(|k| m.get(*k).and_then(Value::as_i64))
            .or_else(|| m.pointer("/meta/n_ctx").and_then(Value::as_i64))
            .filter(|n| *n > 0)
    };
    EngineCaps {
        vision: entries.iter().find_map(|m| vision_of(m)),
        context: entries.iter().find_map(|m| context_of(m)),
    }
}

/// From llama.cpp's (and ninfer's) `/props`.
pub fn from_props(v: &Value) -> EngineCaps {
    EngineCaps {
        vision: v.pointer("/modalities/vision").and_then(Value::as_bool),
        context: v
            .pointer("/default_generation_settings/n_ctx")
            .and_then(Value::as_i64)
            .filter(|n| *n > 0),
    }
}

/// What the online cards in `cards` say together: images when any of them takes them, none when
/// every one that says says none; the smallest context window.
pub fn merged<'a>(
    caps: &HashMap<String, EngineCaps>,
    cards: impl IntoIterator<Item = &'a String>,
) -> EngineCaps {
    let got: Vec<&EngineCaps> = cards.into_iter().filter_map(|c| caps.get(c)).collect();
    let said: Vec<bool> = got.iter().filter_map(|c| c.vision).collect();
    EngineCaps {
        vision: if said.iter().any(|v| *v) {
            Some(true)
        } else if said.is_empty() {
            None
        } else {
            Some(false)
        },
        context: got.iter().filter_map(|c| c.context).min(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ninfer_models_list_says_images() {
        let v = json!({"data": [{"architecture": {"input_modalities": ["text", "image"]},
            "context_length": 65536, "id": "m", "object": "model"}], "object": "list"});
        assert_eq!(
            from_models(&v, "m"),
            EngineCaps {
                vision: Some(true),
                context: Some(65536)
            }
        );
        let text = json!({"data": [{"architecture": {"input_modalities": ["text"]}, "id": "m"}]});
        assert_eq!(from_models(&text, "m").vision, Some(false));
    }

    #[test]
    fn llamacpp_capabilities_and_props() {
        let v = json!({"models": [{"model": "x", "capabilities": ["completion", "multimodal"]}],
            "data": [{"id": "x", "meta": {"n_ctx_train": 1}}]});
        assert_eq!(from_models(&v, "x").vision, Some(true));
        let plain = json!({"models": [{"model": "x", "capabilities": ["completion"]}]});
        assert_eq!(from_models(&plain, "x").vision, Some(false));
        // vLLM: says nothing
        let vllm = json!({"data": [{"id": "x", "max_model_len": 32768}]});
        assert_eq!(
            from_models(&vllm, "x"),
            EngineCaps {
                vision: None,
                context: Some(32768)
            }
        );
        let p = json!({"modalities": {"vision": false, "audio": false},
            "default_generation_settings": {"n_ctx": 8192}});
        assert_eq!(
            from_props(&p),
            EngineCaps {
                vision: Some(false),
                context: Some(8192)
            }
        );
    }

    #[test]
    fn the_served_id_picks_the_entry() {
        let v = json!({"data": [
            {"id": "a", "architecture": {"input_modalities": ["text"]}},
            {"id": "b", "architecture": {"input_modalities": ["text", "image"]}}]});
        assert_eq!(from_models(&v, "b").vision, Some(true));
        assert_eq!(
            from_models(&v, "nope").vision,
            Some(false),
            "else the first"
        );
    }

    #[test]
    fn merging_cards() {
        let mut m = HashMap::new();
        m.insert(
            "A".to_string(),
            EngineCaps {
                vision: Some(false),
                context: Some(65536),
            },
        );
        m.insert(
            "B".to_string(),
            EngineCaps {
                vision: Some(true),
                context: Some(32768),
            },
        );
        let ab = ["A".to_string(), "B".to_string()];
        assert_eq!(merged(&m, &ab).vision, Some(true));
        assert_eq!(merged(&m, &ab).context, Some(32768));
        assert_eq!(merged(&m, &ab[..1]).vision, Some(false));
        assert_eq!(merged(&m, &["C".to_string()]).vision, None);
    }
}
