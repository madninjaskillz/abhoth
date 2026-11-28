//! Build `router.json` from what layoutctl already generates: LiteLLM's `config.yaml` (names,
//! params, model_info, gateway_rev) and qfn-lb's `nginx.conf` (cards, ports, lanes), plus
//! `gateway.json`'s onDemand settings. Names therefore match LiteLLM exactly, while the router
//! talks to the engines directly (`/a/`, `/b/`, `/<engine>/` on qfn-lb become card lists).
//!
//! Keys in either file are never copied: upstream keys come from `keyFile`. The relay's
//! placeholder `ondemand-relay` is the only literal kept.

use std::collections::BTreeMap;

use indexmap::IndexMap;
use serde_json::{Map, Value};

use crate::config::{Deployment, OnDemandModel, Relay, RouterConfig, Upstream};

#[derive(Debug, Clone, Default)]
pub struct ImportOptions {
    /// Upstream key file (path as the router will see it).
    pub key_file: Option<String>,
    /// gateway.json (onDemand settings).
    pub gateway: Option<Value>,
    /// Keep these from an existing router.json (settings, slots, placement, onDemand toggles).
    pub base: Option<RouterConfig>,
}

/// Comment lines out (layoutctl's header), as effort_hook does; the rest is JSON.
pub fn parse_litellm(text: &str) -> Result<Value, String> {
    let body: String = text
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    serde_json::from_str(&body).map_err(|e| format!("LiteLLM config is not JSON: {e}"))
}

/// The layout name from the GENERATED header ("... gateway for layout NAME: do not edit").
pub fn layout_from_header(text: &str) -> Option<String> {
    let l = text.lines().find(|l| l.starts_with("# GENERATED"))?;
    let i = l.find("for layout ")? + "for layout ".len();
    let rest = &l[i..];
    Some(
        rest[..rest.find(':').unwrap_or(rest.len())]
            .trim()
            .to_string(),
    )
}

#[derive(Debug, Default, PartialEq)]
pub struct Lb {
    pub port: Option<u16>,
    /// upstream name -> [(host:port, weight)]
    pub upstreams: BTreeMap<String, Vec<(String, u32)>>,
}

pub fn parse_nginx(text: &str) -> Lb {
    let mut lb = Lb::default();
    let mut cur: Option<String> = None;
    for raw in text.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if let Some(rest) = line.strip_prefix("upstream ") {
            cur = Some(rest.trim_end_matches('{').trim().to_string());
            lb.upstreams.entry(cur.clone().unwrap()).or_default();
            continue;
        }
        if let Some(name) = &cur {
            if line.starts_with('}') {
                cur = None;
                continue;
            }
            if let Some(rest) = line.strip_prefix("server ") {
                let rest = rest.trim_end_matches(';');
                let mut parts = rest.split_whitespace();
                let addr = parts.next().unwrap_or("").to_string();
                let weight = parts
                    .find_map(|p| p.strip_prefix("weight="))
                    .and_then(|w| w.parse().ok())
                    .unwrap_or(1);
                lb.upstreams.get_mut(name).unwrap().push((addr, weight));
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("listen ") {
            lb.port = rest.trim_end_matches(';').trim().parse().ok();
        }
    }
    lb
}

fn split_base(url: &str) -> Option<(String, String)> {
    // "http://h:p/path" -> ("h:p", "/path")
    let rest = url.split_once("://")?.1;
    match rest.find('/') {
        Some(i) => Some((
            rest[..i].to_string(),
            rest[i..].trim_end_matches('/').to_string(),
        )),
        None => Some((rest.to_string(), String::new())),
    }
}

pub fn from_litellm(
    litellm_text: &str,
    nginx_text: &str,
    opts: &ImportOptions,
) -> Result<RouterConfig, String> {
    let doc = parse_litellm(litellm_text)?;
    let lb = parse_nginx(nginx_text);
    let ml = doc
        .get("model_list")
        .and_then(Value::as_array)
        .ok_or("LiteLLM config has no model_list")?;

    // cards from qfn-lb's card_<x> upstreams; engines from pool_<engine>
    let mut upstreams: IndexMap<String, Upstream> = IndexMap::new();
    let mut addr_card: BTreeMap<String, String> = BTreeMap::new();
    for (name, servers) in &lb.upstreams {
        if let Some(c) = name.strip_prefix("card_") {
            let card = c.to_uppercase();
            let Some((addr, w)) = servers.first() else {
                continue;
            };
            addr_card.insert(addr.clone(), card.clone());
            upstreams.insert(
                card,
                Upstream {
                    url: format!("http://{addr}"),
                    engine: String::new(),
                    served_id: String::new(),
                    lanes: (*w).max(1),
                    key_file: opts.key_file.clone(),
                    online: true,
                    catalog_model: None,
                    held: false,
                },
            );
        }
    }
    upstreams.sort_keys();
    let mut engine_cards: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (name, servers) in &lb.upstreams {
        if let Some(e) = name.strip_prefix("pool_") {
            for (addr, _) in servers {
                if let Some(card) = addr_card.get(addr) {
                    engine_cards
                        .entry(e.to_string())
                        .or_default()
                        .push(card.clone());
                    if let Some(u) = upstreams.get_mut(card) {
                        u.engine = e.to_string();
                    }
                }
            }
        }
    }
    let lb_hostport: Option<String> = ml.iter().find_map(|d| {
        let base = d.pointer("/litellm_params/api_base")?.as_str()?;
        let (hp, path) = split_base(base)?;
        let seg = path.trim_start_matches('/').split('/').next()?.to_string();
        (lb.port.is_some_and(|p| hp.ends_with(&format!(":{p}")))
            && (engine_cards.contains_key(&seg) || upstreams.contains_key(&seg.to_uppercase())))
        .then_some(hp)
    });

    let mut deployments = Vec::new();
    let mut relay: Option<Relay> = None;
    let mut rev = String::new();
    for d in ml {
        let name = d
            .get("model_name")
            .and_then(Value::as_str)
            .ok_or("deployment without model_name")?;
        let lp = d
            .get("litellm_params")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let mut mi = d
            .get("model_info")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        if rev.is_empty()
            && let Some(r) = mi.get("gateway_rev").and_then(Value::as_str)
        {
            rev = r.to_string();
        }
        mi.shift_remove("gateway_rev");
        let model = lp.get("model").and_then(Value::as_str).unwrap_or(name);
        let served = model.strip_prefix("openai/").unwrap_or(model).to_string();
        let base = lp.get("api_base").and_then(Value::as_str).unwrap_or("");
        let (hp, path) = split_base(base).ok_or_else(|| format!("{name}: bad api_base {base}"))?;
        let mut dep = Deployment {
            name: name.to_string(),
            served_id: served.clone(),
            model_info: mi,
            ..Deployment::default()
        };
        let on_lb = lb_hostport.as_deref() == Some(hp.as_str());
        if on_lb {
            let seg = path
                .trim_start_matches('/')
                .split('/')
                .next()
                .unwrap_or("")
                .to_string();
            let cards = if path == "/v1" || seg.is_empty() {
                main_pool(&upstreams)
            } else if let Some(c) = engine_cards.get(&seg) {
                c.clone()
            } else if upstreams.contains_key(&seg.to_uppercase()) {
                vec![seg.to_uppercase()]
            } else {
                // qfn-lb answers 503 for a card that is off: keep the name, no cards = refused
                return Err(format!("{name}: {base} names no live card or engine"));
            };
            dep.cards = cards;
        } else {
            let url = base.trim_end_matches('/').to_string();
            match &relay {
                Some(r) if r.url != url => {
                    return Err(format!(
                        "{name}: a second non-qfn-lb api_base {base} (only one relay is supported)"
                    ));
                }
                Some(_) => {}
                None => {
                    let key = lp
                        .get("api_key")
                        .and_then(Value::as_str)
                        .filter(|k| *k == "ondemand-relay")
                        .map(str::to_string);
                    relay = Some(Relay { url, key });
                }
            }
            dep.relay = true;
        }
        for (k, v) in &lp {
            match k.as_str() {
                "model" | "api_base" | "api_key" => {}
                "additional_drop_params" => {
                    dep.drop_params = v
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(Value::as_str)
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default();
                }
                "weight" => dep.weight = v.as_u64().map(|w| w as u32),
                _ => {
                    dep.params.insert(k.clone(), v.clone());
                }
            }
        }
        if !dep.relay {
            for c in &dep.cards {
                if let Some(u) = upstreams.get_mut(c)
                    && u.served_id.is_empty()
                {
                    u.served_id = served.clone();
                }
            }
            if dep.model_info.get("live") == Some(&Value::Bool(true))
                && let Some(cm) = dep.model_info.get("catalog_model").and_then(Value::as_str)
            {
                for c in &dep.cards {
                    if let Some(u) = upstreams.get_mut(c) {
                        u.catalog_model = Some(cm.to_string());
                    }
                }
            }
        }
        deployments.push(dep);
    }

    let mut cfg = opts.base.clone().unwrap_or_default();
    cfg.rev = rev;
    cfg.layout = layout_from_header(litellm_text);
    cfg.upstreams = upstreams;
    cfg.relay = relay;
    cfg.deployments = deployments;
    if opts.base.is_none() {
        let rs = doc.get("router_settings");
        if let Some(c) = rs
            .and_then(|r| r.get("cooldown_time"))
            .and_then(Value::as_u64)
        {
            cfg.settings.cooldown_seconds = c;
        }
        if let Some(t) = rs.and_then(|r| r.get("timeout")).and_then(Value::as_u64) {
            cfg.settings.timeout_seconds = t;
        }
    }
    // on-demand: what each relay name switches in (model_info.switch_to + catalog_model)
    if let Some(gw) = &opts.gateway
        && let Some(od) = gw.get("onDemand").and_then(Value::as_object)
    {
        apply_ondemand(&mut cfg, od);
    }
    cfg.on_demand.models = cfg
        .deployments
        .iter()
        .filter(|d| d.model_info.get("ondemand") == Some(&Value::Bool(true)))
        .filter_map(|d| {
            let cm = d.model_info.get("catalog_model")?.as_str()?.to_string();
            let st = d.model_info.get("switch_to");
            let engine = st
                .and_then(|s| s.get("engine"))
                .and_then(Value::as_str)
                .or_else(|| d.model_info.get("engine").and_then(Value::as_str))
                .unwrap_or_default()
                .to_string();
            let profile = st
                .and_then(|s| s.get("engineProfile"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            Some((
                d.name.clone(),
                OnDemandModel {
                    model: cm,
                    engine,
                    engine_profile: profile,
                    served_id: if d.relay {
                        String::new()
                    } else {
                        d.served_id.clone()
                    },
                    per_card: Default::default(),
                },
            ))
        })
        .collect();
    crate::config::validate(&cfg)?;
    Ok(cfg)
}

fn main_pool(upstreams: &IndexMap<String, Upstream>) -> Vec<String> {
    let mut by: IndexMap<&str, (u32, bool)> = IndexMap::new();
    for (c, u) in upstreams {
        let e = by.entry(u.engine.as_str()).or_default();
        e.0 += u.lanes;
        e.1 |= c == "A";
    }
    let Some((engine, _)) = by.iter().max_by_key(|(_, v)| **v) else {
        return Vec::new();
    };
    upstreams
        .iter()
        .filter(|(_, u)| u.engine == *engine)
        .map(|(c, _)| c.clone())
        .collect()
}

fn apply_ondemand(cfg: &mut RouterConfig, od: &Map<String, Value>) {
    let n = |k: &str| od.get(k).and_then(Value::as_u64);
    let o = &mut cfg.on_demand;
    if let Some(v) = n("queueSeconds") {
        o.queue_seconds = v;
    }
    if let Some(v) = n("minStaySeconds") {
        o.min_stay_seconds = v;
    }
    if let Some(v) = n("idleGraceSeconds") {
        o.idle_grace_seconds = v;
    }
    if let Some(v) = n("switchTimeoutSeconds") {
        o.switch_timeout_seconds = v;
    }
    if let Some(v) = n("failureHoldSeconds") {
        o.failure_hold_seconds = v;
    }
    if let Some(v) = od.get("streamNote").and_then(Value::as_str) {
        o.stream_note = v.to_string();
    }
    if let Some(v) = od.get("allowBothCards").and_then(Value::as_bool) {
        o.allow_both_cards = v;
    }
    // gateway.json enabled=false is THE off switch: never on in the router when it is off there
    if od.get("enabled") == Some(&Value::Bool(false)) {
        o.enabled = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NGINX: &str = include_str!("../tests/fixtures/nginx.conf");
    const LITELLM: &str = include_str!("../tests/fixtures/litellm-config.yaml");

    #[test]
    fn live_layout_imports() {
        let cfg = from_litellm(
            LITELLM,
            NGINX,
            &ImportOptions {
                key_file: Some("/run/secrets/ninfer-key".into()),
                ..ImportOptions::default()
            },
        )
        .unwrap();
        assert_eq!(cfg.rev, "816595f96a9a");
        assert_eq!(cfg.layout.as_deref(), Some("merged-inkling-x2"));
        let a = &cfg.upstreams["A"];
        assert_eq!(a.url, "http://gpu-host:8094");
        assert_eq!(
            (a.engine.as_str(), a.served_id.as_str(), a.lanes),
            ("ninfer", "inkling-2-mini", 4)
        );
        assert_eq!(a.catalog_model.as_deref(), Some("inkling2-mini"));
        assert_eq!(cfg.upstreams["B"].url, "http://gpu-host:8096");
        let names: Vec<&str> = cfg.deployments.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names.len(), 14);
        let alias = cfg
            .deployments
            .iter()
            .find(|d| d.name == "jarvis-brain")
            .unwrap();
        assert_eq!(alias.cards, vec!["A", "B"]);
        assert_eq!(alias.drop_params, vec!["response_format"]);
        assert_eq!(alias.thinking_off_effort().as_deref(), Some("none"));
        let pin = cfg
            .deployments
            .iter()
            .find(|d| d.name == "qwen3.8-flash-next-b")
            .unwrap();
        assert_eq!(pin.cards, vec!["B"]);
        let fl = cfg
            .deployments
            .iter()
            .find(|d| d.name == "flash-next")
            .unwrap();
        assert!(fl.relay);
        assert_eq!(fl.served_id, "flash-next");
        assert_eq!(fl.params.get("reasoning_effort"), Some(&Value::from("low")));
        assert_eq!(
            cfg.relay.as_ref().unwrap().url,
            "http://gpu-host:8098/ondemand/v1"
        );
        assert_eq!(
            cfg.relay.as_ref().unwrap().key.as_deref(),
            Some("ondemand-relay")
        );
        let ink: Vec<&Deployment> = cfg
            .deployments
            .iter()
            .filter(|d| d.name == "inkling-2-mini")
            .collect();
        assert_eq!(ink.len(), 2);
        assert_eq!(ink[0].weight, Some(4));
        assert_eq!(
            cfg.on_demand.models["dense-stock"].engine_profile,
            "ninfer-2lane"
        );
        // no key from either file ends up in the config
        let text = serde_json::to_string(&cfg).unwrap();
        assert!(!text.contains("REDACTED") && !text.contains("Bearer"));
    }

    #[test]
    fn nginx_parse() {
        let lb = parse_nginx(NGINX);
        assert_eq!(lb.port, Some(8092));
        assert_eq!(lb.upstreams["pool_ninfer"].len(), 2);
        assert_eq!(
            lb.upstreams["card_b"],
            vec![("gpu-host:8096".to_string(), 4)]
        );
    }
}
