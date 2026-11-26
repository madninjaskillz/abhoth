//! `[alerts]` config and the built-in rules (SUPERVISOR.md 10.1).

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    #[default]
    Warning,
    Critical,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Warning => "warning",
            Severity::Critical => "critical",
        }
    }
}

/// One rule as written (`[alerts.rules.<id>]`); every field is optional so a built-in rule can
/// be tuned with one line (`[alerts.rules.hot_card] cooldown = "1h"`).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// an `orca-sensors` expression (expression rules only)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<String>,
    #[serde(default, rename = "for", skip_serializing_if = "Option::is_none")]
    pub hold_for: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<Severity>,
    /// `GPU {card} at {value}°C`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// the parts of the identity, comma-separated (`card`)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dedupe: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repeat: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolve: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sinks: Option<Vec<String>>,
}

/// A rule with every field decided.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Rule {
    pub id: String,
    pub enabled: bool,
    pub when: Option<String>,
    pub hold_for: i64,
    pub severity: Severity,
    pub message: String,
    pub dedupe: Vec<String>,
    pub cooldown: i64,
    pub repeat: i64,
    pub resolve: bool,
    pub sinks: Vec<String>,
    pub builtin: bool,
}

/// `[alerts]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AlertsConfig {
    /// off until the supervisor is wired (the flag)
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "d_group")]
    pub group_window: String,
    /// sinks every rule uses unless it names its own
    #[serde(default = "d_sinks")]
    pub default_sinks: Vec<String>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub rules: IndexMap<String, RuleConfig>,
    /// sink settings by name (`[alerts.sinks.ha]`, `[alerts.sinks.ntfy]`)
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub sinks: IndexMap<String, Value>,
}

fn d_group() -> String {
    "30s".into()
}
fn d_sinks() -> Vec<String> {
    vec!["ha".into()]
}

impl Default for AlertsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            group_window: d_group(),
            default_sinks: d_sinks(),
            rules: IndexMap::new(),
            sinks: IndexMap::new(),
        }
    }
}

impl AlertsConfig {
    pub fn from_raw(raw: &IndexMap<String, Value>) -> Result<Self, String> {
        let v = Value::Object(raw.iter().map(|(k, v)| (k.clone(), v.clone())).collect());
        serde_json::from_value(v).map_err(|e| format!("alerts: {e}"))
    }
}

/// Durations: `60s`, `30m`, `2h`, `1d`, combinations, or seconds.
pub fn dur(s: &str) -> Result<i64, String> {
    let t = s.trim();
    if let Ok(n) = t.parse::<i64>() {
        return Ok(n.max(0));
    }
    let mut total = 0i64;
    let mut num = String::new();
    for c in t.chars() {
        if c.is_ascii_digit() {
            num.push(c);
            continue;
        }
        let u = match c {
            'd' => 86_400,
            'h' => 3_600,
            'm' => 60,
            's' => 1,
            _ => return Err(format!("bad duration '{s}'")),
        };
        total += num.parse::<i64>().map_err(|_| format!("bad duration '{s}'"))? * u;
        num.clear();
    }
    if !num.is_empty() || t.is_empty() {
        return Err(format!("bad duration '{s}'"));
    }
    Ok(total)
}

/// The built-in event rules: (id, severity, dedupe parts, `for`, message).
pub const BUILTINS: &[(&str, Severity, &str, i64, &str)] = &[
    ("oom_kill", Severity::Warning, "container,process", 0, "OOM kill: {process} in {container}"),
    ("engine_crash", Severity::Critical, "engine,card", 0, "Engine {engine} on card {card} crashed: {detail}"),
    ("switch_failed", Severity::Critical, "server", 0, "Switch to {layout} on {server} failed: {detail}"),
    ("hot_card", Severity::Warning, "card", 60, "GPU {card} at {value}°C"),
    ("fan_stale", Severity::Critical, "fan", 0, "Fan data stale on {fan}: {detail}"),
    ("fan_stalled", Severity::Critical, "fan", 15, "Fan {fan} stalled ({value} RPM)"),
    ("nic_hot", Severity::Warning, "", 0, "10G NIC at {value}°C"),
    ("clock_mismatch", Severity::Warning, "card", 0, "Card {card} clocks drifted: {detail}"),
    ("hostd_down", Severity::Critical, "server", 30, "hostd on {server} silent: {detail}"),
    ("calendar_overlap", Severity::Warning, "card", 0, "{detail}"),
    ("event_key_outside_window", Severity::Warning, "event", 0, "Event key for {event} used {count} times outside its window in 10 min (leak or clock problem?)"),
    ("event_key_preempt", Severity::Warning, "event,card", 0, "{event} preempted a request on card {card}"),
];

fn split_parts(s: &str) -> Vec<String> {
    s.split(',')
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .map(String::from)
        .collect()
}

/// Every rule: built-ins with their overrides, then expression rules. Problems are returned.
pub fn compile(cfg: &AlertsConfig) -> (Vec<Rule>, Vec<String>) {
    let mut out = Vec::new();
    let mut issues = Vec::new();
    let d = |v: &Option<String>, dflt: i64, what: &str, issues: &mut Vec<String>| match v {
        None => dflt,
        Some(s) => dur(s).unwrap_or_else(|e| {
            issues.push(format!("alerts.rules: {what}: {e}"));
            dflt
        }),
    };
    for (id, sev, parts, hold, msg) in BUILTINS {
        let c = cfg.rules.get(*id).cloned().unwrap_or_default();
        out.push(Rule {
            id: id.to_string(),
            enabled: c.enabled.unwrap_or(true),
            when: None,
            hold_for: d(&c.hold_for, *hold, id, &mut issues),
            severity: c.severity.unwrap_or(*sev),
            message: c.message.unwrap_or_else(|| msg.to_string()),
            dedupe: c.dedupe.as_deref().map(split_parts).unwrap_or_else(|| split_parts(parts)),
            cooldown: d(&c.cooldown, 1800, id, &mut issues),
            repeat: d(&c.repeat, 7200, id, &mut issues),
            resolve: c.resolve.unwrap_or(true),
            sinks: c.sinks.unwrap_or_else(|| cfg.default_sinks.clone()),
            builtin: true,
        });
    }
    for (id, c) in &cfg.rules {
        if BUILTINS.iter().any(|b| b.0 == id) {
            continue;
        }
        let Some(when) = c.when.clone() else {
            issues.push(format!("alerts.rules.{id}: `when` is required for a rule that is not built in"));
            continue;
        };
        out.push(Rule {
            id: id.clone(),
            enabled: c.enabled.unwrap_or(true),
            when: Some(when),
            hold_for: d(&c.hold_for, 0, id, &mut issues),
            severity: c.severity.unwrap_or_default(),
            message: c.message.clone().unwrap_or_else(|| format!("{id}: {{value}}")),
            dedupe: c.dedupe.as_deref().map(split_parts).unwrap_or_default(),
            cooldown: d(&c.cooldown, 1800, id, &mut issues),
            repeat: d(&c.repeat, 7200, id, &mut issues),
            resolve: c.resolve.unwrap_or(true),
            sinks: c.sinks.clone().unwrap_or_else(|| cfg.default_sinks.clone()),
            builtin: false,
        });
    }
    (out, issues)
}
