//! `router.json`: what the router serves (ROUTER.md 4.3). Loaded into an immutable [`Snapshot`];
//! a reload swaps the whole snapshot, so a request keeps the one it started with.
//!
//! Upstream keys are read from `keyFile`s at load time and never appear in the config text, in
//! logs or in any answer.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// The whole file.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RouterConfig {
    /// Reported as `gateway_rev` in `/v1/model/info` (layoutctl's reload confirmation).
    #[serde(default)]
    pub rev: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<String>,
    /// Card id ("A", "B") -> the engine serving on that card.
    #[serde(default)]
    pub upstreams: IndexMap<String, Upstream>,
    /// The profiles API on-demand relay (models no card runs), when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay: Option<Relay>,
    /// Model names, LiteLLM style: one entry per (name, target); a name may have several.
    #[serde(default)]
    pub deployments: Vec<Deployment>,
    #[serde(default)]
    pub settings: Settings,
    #[serde(default)]
    pub slots: SlotsConfig,
    #[serde(default)]
    pub placement: PlacementConfig,
    #[serde(default)]
    pub on_demand: OnDemandConfig,
    #[serde(default)]
    pub observe: ObserveConfig,
    /// The Claude engine's virtual card (crate::claude); off by default.
    #[serde(default, skip_serializing_if = "claude_off")]
    pub claude: crate::claude::ClaudeConfig,
}

fn claude_off(c: &crate::claude::ClaudeConfig) -> bool {
    *c == crate::claude::ClaudeConfig::default()
}

/// Decision-only shadow (`observe`): follow the engines' own request logs, rebuild each card's
/// true load at every real arrival, run the placement policy on it and score that choice
/// against the card the request actually landed on. Reads files and GETs pig-slots `/list`;
/// sends nothing to the engines except the optional, capped canary.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct ObserveConfig {
    pub enabled: bool,
    /// card -> that card's ninfer request log (JSONL), followed read-only from its end.
    pub ninfer_logs: IndexMap<String, String>,
    /// pig-slots base URL (GET /list only): where the Lead holds a lease.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pig_slots_url: Option<String>,
    /// Decisions (decisions-YYYY-MM-DD.jsonl) and daily summaries go here (UK dates).
    pub dir: String,
    pub poll_ms: u64,
    /// A request that reused at least this many cached prompt tokens on its card stays there
    /// (the stand-in for session affinity: the logs carry no session key).
    pub cache_keep_tokens: u64,
    /// Decisions in the first minutes after start are marked warm-up (loads not yet known).
    pub warmup_seconds: u64,
    /// The engines' true load (from the logs) also feeds the router's own placement.
    pub feed_placement: bool,
    pub canary: CanaryConfig,
}

impl Default for ObserveConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            ninfer_logs: IndexMap::new(),
            pig_slots_url: None,
            dir: "observe".into(),
            poll_ms: 1000,
            cache_keep_tokens: 1024,
            warmup_seconds: 600,
            feed_placement: true,
            canary: CanaryConfig::default(),
        }
    }
}

/// A tiny identical chat sent through LiteLLM and through the router, alternating which goes
/// first, to measure the router's added latency and errors on real engines. Off unless
/// `everySeconds` > 0; skipped in the 06:45-07:30 UK window and whenever a card has fewer than
/// `minFreeLanes` free lanes (so it never queues behind, or ahead of, real work).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct CanaryConfig {
    pub every_seconds: u64,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub litellm_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub router_url: Option<String>,
    pub min_free_lanes: u32,
    pub max_tokens: u32,
}

impl Default for CanaryConfig {
    fn default() -> Self {
        Self {
            every_seconds: 0,
            model: "qwen3.8-27b".into(),
            litellm_url: None,
            router_url: None,
            min_free_lanes: 2,
            max_tokens: 8,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Upstream {
    /// Engine root, e.g. "http://gpu-host:8094" (no /v1).
    pub url: String,
    #[serde(default)]
    pub engine: String,
    /// The model id the engine answers to.
    #[serde(default)]
    pub served_id: String,
    /// Concurrent lanes (ninfer --max-concurrency): the balancing weight and admission limit.
    #[serde(default = "one")]
    pub lanes: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_file: Option<String>,
    #[serde(default = "yes")]
    pub online: bool,
    /// The catalog model the card runs (on-demand decisions).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog_model: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Relay {
    /// e.g. "http://gpu-host:8098/ondemand/v1".
    pub url: String,
    /// A non-secret placeholder the relay ignores ("ondemand-relay"), sent as the Bearer token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Deployment {
    pub name: String,
    /// Cards this deployment balances over (a pool, one card for pinned names).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cards: Vec<String>,
    /// Goes to the on-demand relay instead of a card.
    #[serde(default, skip_serializing_if = "is_false")]
    pub relay: bool,
    /// `model` is rewritten to this.
    #[serde(default)]
    pub served_id: String,
    /// Defaults added when the client did not send the key (LiteLLM litellm_params, e.g.
    /// `reasoning_effort: low` for Strata).
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub params: Map<String, Value>,
    /// Top-level fields removed before forwarding (LiteLLM additional_drop_params).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub drop_params: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weight: Option<u32>,
    /// LiteLLM's model_info keys (ours: engine, effort_levels, thinking_off_effort, live, ...),
    /// served verbatim by /v1/model/info; effort_levels and thinking_off_effort drive the
    /// effort rules.
    #[serde(default)]
    pub model_info: Map<String, Value>,
}

impl Deployment {
    pub fn effort_levels(&self) -> Option<Vec<String>> {
        let a = self.model_info.get("effort_levels")?.as_array()?;
        let v: Vec<String> = a
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
        (!v.is_empty()).then_some(v)
    }

    pub fn thinking_off_effort(&self) -> Option<String> {
        self.model_info
            .get("thinking_off_effort")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    }

    pub fn max_input_tokens(&self) -> Option<i64> {
        self.model_info
            .get("max_input_tokens")
            .and_then(Value::as_i64)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    /// A card that failed is skipped this long (LiteLLM cooldown_time).
    pub cooldown_seconds: u64,
    /// Whole request (LiteLLM timeout, nginx proxy_read_timeout).
    pub timeout_seconds: u64,
    /// Connect timeout; a connect failure fails over (nginx proxy_connect_timeout).
    pub connect_timeout_seconds: u64,
    pub max_body_bytes: usize,
    /// Failovers to another card before any byte was sent (nginx proxy_next_upstream_tries 2 = 1).
    pub failovers: u32,
    /// Top-level fields removed from every forwarded chat body (gateway-only fields; see
    /// ROUTER.md "Decisions", D-R5).
    pub strip_fields: Vec<String>,
    /// JSONL request log (no bodies); none = off.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_log: Option<String>,
    /// A key file that also allows `/admin/*` from the LAN (else loopback only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub admin_key_file: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            cooldown_seconds: 10,
            timeout_seconds: 3600,
            connect_timeout_seconds: 3,
            max_body_bytes: 64 << 20,
            failovers: 1,
            // LiteLLM consumes these itself and never forwards them (shadow replay 2026-11-25)
            strip_fields: vec!["metadata".into(), "user".into()],
            request_log: None,
            admin_key_file: None,
        }
    }
}

/// The pig-slots registry served by the router (phase 2).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct SlotsConfig {
    pub enabled: bool,
    pub ttl_seconds: f64,
    /// pig-slots' data/layout.json (slot list, frozen); none = slots from the upstreams' lanes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layout_file: Option<String>,
    /// leases.json (same format as pig-slots); none = in memory only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state_file: Option<String>,
}

impl Default for SlotsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            ttl_seconds: 90.0,
            layout_file: None,
            state_file: None,
        }
    }
}

/// Placement policy (phase 2). Everything is OFF by default until benched (ROUTER.md risk 5).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct PlacementConfig {
    /// A request carrying a lease (x-abhoth-slot-key, or a prompt_cache_key equal to a lease
    /// key) goes to that lease's card.
    pub lease_aware: bool,
    /// Per-card admission: at most `lanes` in flight, the rest queue (Lead first).
    pub admission: bool,
    /// Lanes kept free on the Lead's card for `kind: main` / `x-abhoth-priority: lead`.
    pub lead_reserve: u32,
    /// How long a queued request waits for a lane before it is sent anyway.
    pub queue_seconds: u64,
    /// Session affinity by prompt_cache_key / x-session-id / metadata.session_id.
    pub affinity: bool,
    pub affinity_ttl_seconds: u64,
    /// Least load leaves a request's current card (in the shadow: the card it actually landed
    /// on) only when that card has no free lane, or when the predicted net gain of moving
    /// (observe.rs `gain`: batching speed-up minus the cached prompt recomputed) exceeds this.
    pub min_move_gain_seconds: f64,
}

impl Default for PlacementConfig {
    fn default() -> Self {
        Self {
            lease_aware: false,
            admission: false,
            lead_reserve: 1,
            queue_seconds: 300,
            affinity: false,
            affinity_ttl_seconds: 900,
            min_move_gain_seconds: 1.0,
        }
    }
}

/// Built-in on-demand switching (phase 3; ondemand.py's settings).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct OnDemandConfig {
    /// The router decides itself instead of forwarding to the relay.
    pub enabled: bool,
    /// Decide and log, never ask for a switch (shadow).
    pub dry_run: bool,
    pub profiles_api: String,
    /// state/guards.json (video, bench, music; written by the layouts runner).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guards_file: Option<String>,
    /// state/litellm-hold (gateway --hold).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hold_file: Option<String>,
    /// state/live.json (the last switch: per-card last change).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub live_file: Option<String>,
    /// pig-slots for leases when the router's own registry is off (e.g. "http://h:8099").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pig_slots_url: Option<String>,
    /// Poll each card's engine `/slots` (is_processing) for idleness, as the profiles API does.
    pub poll_slots: bool,
    pub queue_seconds: u64,
    pub min_stay_seconds: u64,
    pub idle_grace_seconds: u64,
    pub switch_timeout_seconds: u64,
    pub failure_hold_seconds: u64,
    /// "reasoning" (progress notes as delta.reasoning_content) or "none" (SSE comments).
    pub stream_note: String,
    pub allow_both_cards: bool,
    /// Client name -> what to switch in.
    pub models: IndexMap<String, OnDemandModel>,
    /// How long a drain waits for requests in flight.
    pub drain_timeout_seconds: u64,
    /// The control plane's gate (ROUTER-CONFIG.md 7.4): set from `control.enabled` by the
    /// control plane. Off (the default and the data plane alone): on-demand behaves exactly as
    /// before the control plane, and `require_key`, the hourly limits and
    /// `x-abhoth-no-switch` are not looked at.
    pub control_gate: bool,
    /// With the gate: only callers with a key (a principal) may start a switch.
    pub require_key: bool,
    /// On-demand switches started per hour (server), and per principal.
    pub max_switches_per_hour: u32,
    pub max_switches_per_principal_per_hour: u32,
    /// Cards an on-demand switch never picks (dense-pi's card, a card on `on_demand = false`).
    pub exclude_cards: Vec<String>,
}

impl Default for OnDemandConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            dry_run: true,
            profiles_api: String::new(),
            guards_file: None,
            hold_file: None,
            live_file: None,
            pig_slots_url: None,
            poll_slots: true,
            queue_seconds: 300,
            min_stay_seconds: 300,
            idle_grace_seconds: 10,
            switch_timeout_seconds: 900,
            failure_hold_seconds: 60,
            stream_note: "reasoning".into(),
            allow_both_cards: false,
            models: IndexMap::new(),
            drain_timeout_seconds: 120,
            control_gate: false,
            require_key: true,
            max_switches_per_hour: 4,
            max_switches_per_principal_per_hour: 2,
            exclude_cards: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct OnDemandModel {
    /// Catalog key (profiles.json models).
    pub model: String,
    pub engine: String,
    pub engine_profile: String,
    #[serde(default)]
    pub served_id: String,
    /// The router's placement (D152): card -> the engine and profile it would use there, best
    /// first. Empty: any card, with `engine` / `engine_profile` (the pre-inventory pick).
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub per_card: IndexMap<String, CardPick>,
}

/// One card's on-demand choice (D152).
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CardPick {
    pub engine: String,
    pub engine_profile: String,
    /// 0 = the model's primary engine.
    #[serde(default)]
    pub rank: usize,
}

impl OnDemandModel {
    /// The pick for `card`: its placement when there is one, else the model's own.
    pub fn for_card(&self, card: &str) -> OnDemandModel {
        match self.per_card.get(card) {
            Some(p) => OnDemandModel {
                model: self.model.clone(),
                engine: p.engine.clone(),
                engine_profile: p.engine_profile.clone(),
                served_id: self.served_id.clone(),
                per_card: IndexMap::new(),
            },
            None => OnDemandModel {
                per_card: IndexMap::new(),
                ..self.clone()
            },
        }
    }
}

fn one() -> u32 {
    1
}
fn yes() -> bool {
    true
}
fn is_false(b: &bool) -> bool {
    !*b
}

/// File identity for change detection (as effort_hook: mtime, size).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileSig {
    pub modified: Option<SystemTime>,
    pub len: u64,
}

impl FileSig {
    pub fn of(path: &Path) -> Option<Self> {
        let m = std::fs::metadata(path).ok()?;
        Some(Self {
            modified: m.modified().ok(),
            len: m.len(),
        })
    }
}

/// A validated config plus the keys its files name.
#[derive(Debug)]
pub struct Snapshot {
    pub config: RouterConfig,
    keys: HashMap<String, String>,
    pub sig: Option<FileSig>,
    pub loaded_at: SystemTime,
}

impl Snapshot {
    /// Validate and read keys. Fails on an inconsistent config or a key file that can't be read.
    pub fn build(mut config: RouterConfig, base_dir: Option<&Path>) -> Result<Self, String> {
        crate::claude::expand(&mut config)?;
        if let Some(f) = config.claude.oauth_token_file.as_mut() {
            *f = resolve(base_dir, f).to_string_lossy().into_owned();
        }
        validate(&config)?;
        let mut keys = HashMap::new();
        for (card, up) in &config.upstreams {
            if let Some(f) = up.key_file.as_deref().filter(|f| !f.is_empty()) {
                let p = resolve(base_dir, f);
                let k = std::fs::read_to_string(&p)
                    .map_err(|e| format!("upstream {card}: key file {}: {e}", p.display()))?;
                let k = k.trim().to_string();
                if !k.is_empty() {
                    keys.insert(card.clone(), k);
                }
            }
        }
        Ok(Self {
            config,
            keys,
            sig: None,
            loaded_at: SystemTime::now(),
        })
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        let sig = FileSig::of(path);
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let config: RouterConfig =
            serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut s = Self::build(config, path.parent())?;
        s.sig = sig;
        Ok(s)
    }

    /// The same keys with another config (validated); `sig` is not carried over.
    pub fn with_config(&self, mut config: RouterConfig) -> Result<Self, String> {
        crate::claude::expand(&mut config)?;
        validate(&config)?;
        Ok(Self {
            config,
            keys: self.keys.clone(),
            sig: None,
            loaded_at: self.loaded_at,
        })
    }

    /// The Bearer key for a card's engine.
    pub fn key_for(&self, card: &str) -> Option<&str> {
        self.keys.get(card).map(String::as_str)
    }

    pub fn deployments_named(&self, name: &str) -> Vec<(usize, &Deployment)> {
        self.config
            .deployments
            .iter()
            .enumerate()
            .filter(|(_, d)| d.name == name)
            .collect()
    }

    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.config.settings.timeout_seconds.max(1))
    }

    /// Cards a qfn-lb style prefix addresses: "a" / "b" = that card, an engine name = its cards.
    pub fn lb_cards(&self, prefix: &str) -> Option<Vec<String>> {
        // the Claude engine's virtual card has no HTTP surface to pass through to
        if let Some((card, u)) = self
            .config
            .upstreams
            .iter()
            .find(|(c, _)| c.eq_ignore_ascii_case(prefix))
        {
            return (!u.is_virtual()).then(|| vec![card.clone()]);
        }
        let cards: Vec<String> = self
            .config
            .upstreams
            .iter()
            .filter(|(_, u)| !u.is_virtual() && u.engine.eq_ignore_ascii_case(prefix))
            .map(|(c, _)| c.clone())
            .collect();
        (!cards.is_empty()).then_some(cards)
    }

    /// qfn-lb's plain `/`: the engine with the most lanes (ties: the one on card A).
    pub fn main_pool(&self) -> Vec<String> {
        let mut by_engine: IndexMap<&str, (u32, bool)> = IndexMap::new();
        for (c, u) in &self.config.upstreams {
            if !u.online || u.is_virtual() {
                continue;
            }
            let e = by_engine.entry(u.engine.as_str()).or_default();
            e.0 += u.lanes;
            e.1 |= c == "A";
        }
        let Some((engine, _)) = by_engine.iter().max_by_key(|(_, v)| **v) else {
            return Vec::new();
        };
        self.config
            .upstreams
            .iter()
            .filter(|(_, u)| u.online && !u.is_virtual() && u.engine == *engine)
            .map(|(c, _)| c.clone())
            .collect()
    }
}

fn resolve(base: Option<&Path>, f: &str) -> PathBuf {
    let p = PathBuf::from(f);
    match base {
        Some(b) if p.is_relative() => b.join(p),
        _ => p,
    }
}

pub fn validate(c: &RouterConfig) -> Result<(), String> {
    for (card, u) in &c.upstreams {
        if card.is_empty() {
            return Err("an upstream has an empty card id".into());
        }
        if u.is_virtual() {
            // only the Claude engine's own card (crate::claude::expand) may be virtual
            if !(c.claude.enabled && c.claude.card == *card) {
                return Err(format!(
                    "upstream {card}: {} is the Claude engine's card; set `claude` instead",
                    u.url
                ));
            }
            continue;
        }
        if !(u.url.starts_with("http://") || u.url.starts_with("https://")) {
            return Err(format!("upstream {card}: url must be http(s): {}", u.url));
        }
        if u.lanes == 0 {
            return Err(format!("upstream {card}: lanes must be at least 1"));
        }
    }
    for (i, d) in c.deployments.iter().enumerate() {
        if d.name.trim().is_empty() {
            return Err(format!("deployment {i}: empty name"));
        }
        if d.relay {
            if c.relay.is_none() {
                return Err(format!(
                    "deployment {}: relay but no relay configured",
                    d.name
                ));
            }
            if !d.cards.is_empty() {
                return Err(format!("deployment {}: both relay and cards", d.name));
            }
        } else {
            if d.cards.is_empty() {
                return Err(format!("deployment {}: no cards", d.name));
            }
            for card in &d.cards {
                if !c.upstreams.contains_key(card) {
                    return Err(format!("deployment {}: unknown card {card}", d.name));
                }
            }
        }
        if d.served_id.trim().is_empty() {
            return Err(format!("deployment {}: empty servedId", d.name));
        }
    }
    if let Some(r) = &c.relay
        && !(r.url.starts_with("http://") || r.url.starts_with("https://"))
    {
        return Err(format!("relay url must be http(s): {}", r.url));
    }
    Ok(())
}
