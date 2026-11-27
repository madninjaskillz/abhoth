//! Where a media request goes (ROUTER-MEDIA-PLAN.md 4.1, 13.4): the cards whose `engines` list
//! the kind, ordered DeepThought (servers the router reaches itself) first, then bridges by their
//! priority (lower first), then fewest requests in flight. A target that failed is cooled for
//! 30 s and tried last.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::control::caps;
use crate::control::schema::{ControlConfig, ExecKind, MediaApi, ModelKind};

/// One engine on one card.
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub server: String,
    pub card: String,
    pub engine: String,
    pub api: MediaApi,
    /// Reached through a bridge's connection.
    pub bridge: bool,
    /// The bridge's priority (0 for servers the router reaches itself).
    pub priority: i32,
    /// The engine's port on the card.
    pub port: u16,
}

impl Target {
    pub fn key(&self) -> String {
        format!("{}/{}@{}", self.engine, self.card, self.server)
    }

    /// What cooling and in-flight counts are kept under.
    fn slot(&self) -> String {
        format!("{}:{}", self.key(), self.port)
    }
}

/// How the media routes talk to `engine`: its `media_api`, else its catalog kind's usual API.
pub fn api_of(c: &ControlConfig, engine: &str) -> Option<MediaApi> {
    let e = c.engines.get(engine)?;
    if let Some(a) = e.media_api {
        return Some(a);
    }
    let base = caps::caps(c, engine)
        .base
        .unwrap_or_else(|| engine.to_string());
    match base.as_str() {
        "comfyui" => Some(MediaApi::Comfyui),
        "sa3" => Some(MediaApi::Sa3),
        "whisper" => Some(MediaApi::Wyoming),
        "kokoro" | "chatterbox" => Some(MediaApi::OpenaiSpeech),
        _ => None,
    }
}

/// What a request needs.
#[derive(Debug, Clone, Default)]
pub struct Want<'a> {
    pub kinds: &'a [ModelKind],
    pub apis: &'a [MediaApi],
    /// A model name the client gave: when it is a model in the inventory, only its engines.
    pub model: Option<&'a str>,
    /// `name@server`, or `/comfyui/@server/`.
    pub server: Option<&'a str>,
}

/// Split `name@server` (the shim's pin).
pub fn split_pin(model: &str) -> (&str, Option<&str>) {
    match model.rsplit_once('@') {
        Some((m, s)) if !s.is_empty() && !m.is_empty() => (m, Some(s)),
        _ => (model, None),
    }
}

/// The candidates, best first. `bridge_priority(server)` is `Some(priority)` for a bridge that is
/// connected and enabled, `None` otherwise (its cards are skipped).
pub fn candidates(
    c: &ControlConfig,
    w: &Want,
    bridge_priority: &dyn Fn(&str) -> Option<i32>,
) -> Vec<Target> {
    let model = w.model.and_then(|m| model_named(c, m));
    let mut out: Vec<Target> = Vec::new();
    for (card_id, card) in &c.cards {
        let Some(map) = &card.engines else { continue };
        let Some(server) = c.servers.get(&card.server) else {
            continue;
        };
        if w.server.is_some_and(|s| s != card.server) {
            continue;
        }
        let bridge = server.exec == ExecKind::Bridge;
        let priority = if bridge {
            match bridge_priority(&card.server) {
                Some(p) => p,
                None => continue,
            }
        } else {
            0
        };
        for kind in w.kinds {
            for engine in map.get(kind.as_str()).into_iter().flatten() {
                let Some(e) = c.engines.get(engine) else {
                    continue;
                };
                let Some(port) = e.port.get(card_id).copied() else {
                    continue;
                };
                let Some(api) = api_of(c, engine) else {
                    continue;
                };
                if !w.apis.contains(&api) {
                    continue;
                }
                if let Some((_, m)) = model
                    && m.kind == Some(*kind)
                    && !model_runs_on(m, engine)
                {
                    continue;
                }
                let t = Target {
                    server: card.server.clone(),
                    card: card_id.clone(),
                    engine: engine.clone(),
                    api,
                    bridge,
                    priority,
                    port,
                };
                if !out.contains(&t) {
                    out.push(t);
                }
            }
        }
    }
    order(&mut out);
    out
}

/// A model by id or alias.
fn model_named<'c>(
    c: &'c ControlConfig,
    name: &str,
) -> Option<(&'c String, &'c crate::control::schema::Model)> {
    c.models.get_key_value(name).or_else(|| {
        c.models
            .iter()
            .find(|(_, m)| m.aliases.iter().any(|a| a == name))
    })
}

fn model_runs_on(m: &crate::control::schema::Model, engine: &str) -> bool {
    if m.deny.iter().any(|d| d == engine) {
        return false;
    }
    m.primary.as_deref() == Some(engine)
        || m.artifacts.contains_key(engine)
        || m.allow.iter().any(|a| a == engine)
        || (m.primary.is_none() && m.artifacts.is_empty() && m.allow.is_empty())
}

fn order(v: &mut [Target]) {
    let st = state();
    let g = st.lock().unwrap();
    let now = Instant::now();
    v.sort_by_key(|t| {
        let k = t.slot();
        let cooled = g.cooled.get(&k).is_some_and(|u| *u > now);
        let busy = g.inflight.get(&k).copied().unwrap_or(0);
        (cooled, t.bridge, t.priority, busy)
    });
}

#[derive(Default)]
struct State {
    inflight: HashMap<String, usize>,
    cooled: HashMap<String, Instant>,
}

fn state() -> &'static Mutex<State> {
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(Default::default)
}

/// How long a target that failed is tried last.
pub const COOL: Duration = Duration::from_secs(30);

pub fn cool(t: &Target) {
    let mut g = state().lock().unwrap();
    let now = Instant::now();
    g.cooled.retain(|_, u| *u > now);
    g.cooled.insert(t.slot(), now + COOL);
}

pub fn is_cooled(t: &Target) -> bool {
    let g = state().lock().unwrap();
    g.cooled.get(&t.slot()).is_some_and(|u| *u > Instant::now())
}

/// A request in flight on a target (dropped when its answer has been sent).
pub struct Busy(String);

pub fn begin(t: &Target) -> Busy {
    let k = t.slot();
    *state()
        .lock()
        .unwrap()
        .inflight
        .entry(k.clone())
        .or_default() += 1;
    Busy(k)
}

impl Drop for Busy {
    fn drop(&mut self) {
        let mut g = state().lock().unwrap();
        if let Some(n) = g.inflight.get_mut(&self.0) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                g.inflight.remove(&self.0);
            }
        }
    }
}

/// Requests in flight per target (for `/status`-style views and tests).
pub fn inflight(t: &Target) -> usize {
    state()
        .lock()
        .unwrap()
        .inflight
        .get(&t.slot())
        .copied()
        .unwrap_or(0)
}
