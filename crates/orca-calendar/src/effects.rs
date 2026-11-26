//! Effects (SUPERVISOR.md 8.2): what an active occurrence does. Written as one-key tables:
//! `{ block_switch = "server:deepthought" }`, `{ reserve_card = "B", on_reserve = "wait" }`,
//! `{ reserve_slots = "card:B", n = 2 }`, `{ hold_clocks = "B" }`, `{ fan_profile = "quiet" }`,
//! `{ mute_alerts = "hot_card" }`, `"info"`.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// What an effect applies to. A bare name is a card (`"B"`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "name")]
pub enum Target {
    /// everything (`"all"`)
    All,
    Server(String),
    Card(String),
    Route(String),
}

impl Target {
    pub fn parse(s: &str) -> Result<Self, String> {
        let t = s.trim();
        if t.is_empty() {
            return Err("an empty target".into());
        }
        if t == "all" || t == "*" {
            return Ok(Target::All);
        }
        Ok(match t.split_once(':') {
            Some(("server", n)) => Target::Server(n.into()),
            Some(("card", n)) => Target::Card(n.into()),
            Some(("route", n)) => Target::Route(n.into()),
            Some((k, _)) => return Err(format!("unknown target kind '{k}' (server:, card:, route:)")),
            None => Target::Card(t.into()),
        })
    }

    pub fn text(&self) -> String {
        match self {
            Target::All => "all".into(),
            Target::Server(n) => format!("server:{n}"),
            Target::Card(n) => format!("card:{n}"),
            Target::Route(n) => format!("route:{n}"),
        }
    }

    /// Does this target cover `card` (servers cover their cards: the caller passes a card's
    /// server in `server`)?
    pub fn covers_card(&self, card: &str, server: Option<&str>) -> bool {
        match self {
            Target::All => true,
            Target::Server(s) => server.is_none_or(|x| x == s),
            Target::Card(c) => c == card,
            Target::Route(_) => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnReserve {
    /// no new placements for others; in-flight work and leases run out (default, 15 Q11)
    #[default]
    Wait,
    /// as wait, and other users' leases are released at once
    Drain,
    /// as drain, and others' streams are cancelled after `preempt_after` (priority high+ only)
    Preempt,
}

impl OnReserve {
    pub fn as_str(self) -> &'static str {
        match self {
            OnReserve::Wait => "wait",
            OnReserve::Drain => "drain",
            OnReserve::Preempt => "preempt",
        }
    }
}

/// The minimum `preempt_after` (9.4).
pub const MIN_PREEMPT_AFTER: i64 = 30;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "effect")]
pub enum Effect {
    BlockSwitch { target: Target },
    BlockOnDemand { target: Target },
    BlockEnsure { target: Target },
    ReserveCard {
        card: String,
        on_reserve: OnReserve,
        /// seconds before others' streams are cancelled (`preempt` only)
        preempt_after: i64,
    },
    ReserveSlots { target: Target, n: u32 },
    HoldClocks { target: Target },
    ClockProfile { profile: String, target: Target },
    FanProfile { profile: String, fan: Option<String> },
    /// a rule selector: a rule id, `*`, or `prefix*`
    MuteAlerts { rules: String },
    Info,
}

fn s(v: &Value) -> Result<String, String> {
    v.as_str()
        .map(String::from)
        .ok_or_else(|| format!("expected text, got {v}"))
}

impl Effect {
    pub fn parse(v: &Value) -> Result<Self, String> {
        if v.as_str() == Some("info") {
            return Ok(Effect::Info);
        }
        let o = v
            .as_object()
            .ok_or_else(|| format!("an effect is a table like {{ block_switch = \"all\" }}, got {v}"))?;
        const KINDS: [&str; 10] = [
            "block_switch",
            "block_on_demand",
            "block_ensure",
            "reserve_card",
            "reserve_slots",
            "hold_clocks",
            "clock_profile",
            "fan_profile",
            "mute_alerts",
            "info",
        ];
        let kinds: Vec<&str> = o.keys().map(String::as_str).filter(|k| KINDS.contains(k)).collect();
        let [kind] = kinds.as_slice() else {
            return Err(format!("an effect names exactly one of {}", KINDS.join(", ")));
        };
        let allowed: &[&str] = match *kind {
            "reserve_card" => &["reserve_card", "on_reserve", "preempt_after"],
            "reserve_slots" => &["reserve_slots", "n"],
            "clock_profile" => &["clock_profile", "card", "target"],
            "fan_profile" => &["fan_profile", "fan"],
            k => std::slice::from_ref(match k {
                "block_switch" => &"block_switch",
                "block_on_demand" => &"block_on_demand",
                "block_ensure" => &"block_ensure",
                "hold_clocks" => &"hold_clocks",
                "mute_alerts" => &"mute_alerts",
                _ => &"info",
            }),
        };
        if let Some(extra) = o.keys().find(|k| !allowed.contains(&k.as_str())) {
            return Err(format!("{kind}: unknown field '{extra}'"));
        }
        let main = &o[*kind];
        let target = || Target::parse(&s(main)?);
        Ok(match *kind {
            "block_switch" => Effect::BlockSwitch { target: target()? },
            "block_on_demand" => Effect::BlockOnDemand { target: target()? },
            "block_ensure" => Effect::BlockEnsure { target: target()? },
            "hold_clocks" => Effect::HoldClocks { target: target()? },
            "reserve_card" => {
                let card = match Target::parse(&s(main)?)? {
                    Target::Card(c) => c,
                    t => return Err(format!("reserve_card takes one card, got {}", t.text())),
                };
                let on_reserve = match o.get("on_reserve") {
                    None => OnReserve::Wait,
                    Some(v) => serde_json::from_value(v.clone())
                        .map_err(|_| format!("on_reserve: wait, drain or preempt, got {v}"))?,
                };
                let preempt_after = match o.get("preempt_after") {
                    None => 60,
                    Some(Value::Number(n)) => n.as_i64().unwrap_or(60),
                    Some(Value::String(t)) => crate::dur::parse(t)?,
                    Some(v) => return Err(format!("preempt_after: a duration, got {v}")),
                };
                Effect::ReserveCard {
                    card,
                    on_reserve,
                    preempt_after: preempt_after.max(MIN_PREEMPT_AFTER),
                }
            }
            "reserve_slots" => Effect::ReserveSlots {
                target: target()?,
                n: o.get("n")
                    .and_then(Value::as_u64)
                    .filter(|n| *n >= 1)
                    .ok_or("reserve_slots needs n = 1 or more")? as u32,
            },
            "clock_profile" => Effect::ClockProfile {
                profile: s(main)?,
                target: match o.get("card").or(o.get("target")) {
                    Some(v) => Target::parse(&s(v)?)?,
                    None => Target::All,
                },
            },
            "fan_profile" => Effect::FanProfile {
                profile: s(main)?,
                fan: o.get("fan").map(s).transpose()?,
            },
            "mute_alerts" => Effect::MuteAlerts { rules: s(main)? },
            _ => Effect::Info,
        })
    }

    /// The effect as written in `calendar.toml` (JSON form of the one-key table).
    pub fn to_value(&self) -> Value {
        match self {
            Effect::BlockSwitch { target } => json!({"block_switch": target.text()}),
            Effect::BlockOnDemand { target } => json!({"block_on_demand": target.text()}),
            Effect::BlockEnsure { target } => json!({"block_ensure": target.text()}),
            Effect::HoldClocks { target } => json!({"hold_clocks": target.text()}),
            Effect::ReserveCard {
                card,
                on_reserve,
                preempt_after,
            } => {
                let mut v = json!({"reserve_card": card, "on_reserve": on_reserve.as_str()});
                if *on_reserve == OnReserve::Preempt {
                    v["preempt_after"] = json!(crate::dur::format(*preempt_after));
                }
                v
            }
            Effect::ReserveSlots { target, n } => json!({"reserve_slots": target.text(), "n": n}),
            Effect::ClockProfile { profile, target } => {
                json!({"clock_profile": profile, "target": target.text()})
            }
            Effect::FanProfile { profile, fan } => match fan {
                Some(f) => json!({"fan_profile": profile, "fan": f}),
                None => json!({"fan_profile": profile}),
            },
            Effect::MuteAlerts { rules } => json!({"mute_alerts": rules}),
            Effect::Info => json!("info"),
        }
    }

    /// One short line for the UI (`reserve card B (wait)`).
    pub fn describe(&self) -> String {
        match self {
            Effect::BlockSwitch { target } => format!("block switches on {}", target.text()),
            Effect::BlockOnDemand { target } => format!("block on-demand on {}", target.text()),
            Effect::BlockEnsure { target } => format!("block engine starts on {}", target.text()),
            Effect::HoldClocks { target } => format!("hold clocks on {}", target.text()),
            Effect::ReserveCard { card, on_reserve, .. } => {
                format!("reserve card {card} ({})", on_reserve.as_str())
            }
            Effect::ReserveSlots { target, n } => format!("reserve {n} slot(s) on {}", target.text()),
            Effect::ClockProfile { profile, target } => {
                format!("clock profile {profile} on {}", target.text())
            }
            Effect::FanProfile { profile, fan } => format!(
                "fan profile {profile}{}",
                fan.as_ref().map(|f| format!(" on {f}")).unwrap_or_default()
            ),
            Effect::MuteAlerts { rules } => format!("mute alerts {rules} (never critical)"),
            Effect::Info => "info only".into(),
        }
    }
}

/// Does a `mute_alerts` selector match a rule id (`*`, `hot_*`, `oom_kill`)?
pub fn selector_matches(sel: &str, rule: &str) -> bool {
    sel.split(',').map(str::trim).any(|s| {
        s == "*" || s == rule || s.strip_suffix('*').is_some_and(|p| rule.starts_with(p))
    })
}
