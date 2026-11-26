//! `policy.put` validation: the whole policy against the limits file, refused in full on any
//! violation (SUPERVISOR.md 2.3). Nothing is applied until everything checks out.

use std::collections::BTreeMap;

use orca_sensors::SensorRegistry;

use crate::clocks::CardSpec;
use crate::limits::Limits;
use crate::protocol::PolicyDoc;

/// A policy that passed validation, ready to apply.
pub struct Validated {
    pub doc: PolicyDoc,
    pub registry: SensorRegistry,
    /// For the clocks loop: (label, uuid, config).
    pub cards: Vec<CardSpec>,
    /// UUID → label, for telemetry.
    pub labels: BTreeMap<String, String>,
    /// Non-fatal notes (shown in `status`).
    pub notes: Vec<String>,
}

/// Validates `doc` against `limits`. `current_rev` is the revision in force, if any.
pub fn validate(doc: PolicyDoc, limits: &Limits, current_rev: Option<u64>) -> Result<Validated, Vec<String>> {
    let mut why = Vec::new();
    let mut notes = Vec::new();
    if let Some(cur) = current_rev
        && doc.rev < cur
    {
        why.push(format!("rev {} is older than the policy in force ({cur})", doc.rev));
    }
    let mut labels = BTreeMap::new();
    for (label, b) in &doc.cards {
        if label.is_empty() || label == "all" || !label.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
            why.push(format!("cards: bad label {label:?}"));
        }
        if !b.uuid.starts_with("GPU-") {
            why.push(format!("cards.{label}: {:?} is not a GPU UUID", b.uuid));
        }
        if labels.insert(b.uuid.clone(), label.clone()).is_some() {
            why.push(format!("cards.{label}: UUID bound twice"));
        }
    }
    let registry = match SensorRegistry::new(doc.sensors.iter().map(|(k, v)| (k.clone(), v.text().to_string()))) {
        Ok(r) => Some(r),
        Err(e) => {
            why.push(e.to_string());
            None
        }
    };
    let mut cards = Vec::new();
    for (label, cfg) in &doc.power {
        match doc.cards.get(label) {
            None => why.push(format!("power.{label}: no such card in cards")),
            Some(b) => cards.push((label.clone(), b.uuid.clone(), cfg.clone())),
        }
    }
    if let Err(e) = limits.gpu.validate_policy(cards.iter().map(|(_, u, c)| (u.as_str(), c))) {
        why.extend(e.into_iter().map(|m| format!("power: {m}")));
    }
    if let Err(e) = crate::fans::validate(&doc.fans, limits) {
        why.extend(e);
    }
    for f in &doc.fans.fans {
        if doc.fans.backends.get(&f.backend).is_some_and(|b| b.mode == orca_fans::config::RunMode::Act)
            && !doc.fans.backends.get(&f.backend).is_some_and(|b| limits.backend_writes(&crate::fans::kind_name(b.kind)))
        {
            notes.push(format!("fans.{}: backend is act in config but the limits do not let it write: shadow", f.id));
        }
    }
    if !why.is_empty() {
        return Err(why);
    }
    Ok(Validated { registry: registry.expect("checked"), cards, labels, notes, doc })
}

/// Builds a policy from supervisor-style TOML, for running hostd before the supervisor does it
/// (`abhoth hostd policy-put`):
///
/// ```toml
/// rev = 1                                   # optional: default one above the policy on disk
/// [cards.A]  uuid = "GPU-…", bus = "21:00.0"
/// [sensors.ctrl_A] expr = "ema(first(gpu.A.hbm, gpu.A.core), 4s)"
/// [power.A]  persistence = true, serving = "unlocked", idle = { after = "60s" }
/// [fan_backends.*] / [fan_defaults] / [fan_profiles.*] / [fans.*]   # as in ROUTER-CONFIG
/// ```
pub fn from_toml(text: &str, limits: &Limits, default_rev: u64) -> Result<PolicyDoc, String> {
    let mut t: toml::Table = toml::from_str(text).map_err(|e| e.to_string())?;
    let rev = match t.remove("rev") {
        Some(v) => v.as_integer().filter(|r| *r >= 0).ok_or("rev: a non-negative integer")? as u64,
        None => default_rev,
    };
    let take = |t: &mut toml::Table, k: &str| t.remove(k).unwrap_or(toml::Value::Table(Default::default()));
    let cards = take(&mut t, "cards").try_into().map_err(|e| format!("cards: {e}"))?;
    let power = take(&mut t, "power").try_into().map_err(|e| format!("power: {e}"))?;
    #[derive(serde::Deserialize)]
    struct S {
        expr: orca_sensors::Expr,
    }
    let sensors: BTreeMap<String, S> = take(&mut t, "sensors").try_into().map_err(|e| format!("sensors: {e}"))?;
    let mut fan = toml::Table::new();
    for k in ["fan_backends", "fan_defaults", "fan_profiles", "fans"] {
        if let Some(v) = t.remove(k) {
            fan.insert(k.into(), v);
        }
    }
    if let Some(k) = t.keys().next() {
        return Err(format!("unknown key {k:?}"));
    }
    let fans = if fan.is_empty() {
        Default::default()
    } else {
        let cfg = orca_fans::config::FansConfig::from_toml(&toml::to_string(&fan).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        orca_fans::FansPolicy::compile(&cfg, &limits.fans, &|k| limits.backend_writes(&crate::fans::kind_name(k))).map_err(|e| e.to_string())?
    };
    Ok(PolicyDoc { rev, cards, sensors: sensors.into_iter().map(|(k, s)| (k, s.expr)).collect(), fans, power })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::CardBinding;

    fn limits() -> Limits {
        Limits::parse(
            r#"
[gpus."GPU-a"]
label = "A"
actions = ["persistence", "power_limit", "lock_clocks", "reset_clocks"]
clock_mhz = [135, 1597]
power_limit_w = [250, 350]
"#,
        )
        .unwrap()
    }

    fn doc(rev: u64, pl: u32) -> PolicyDoc {
        let mut d = PolicyDoc {
            rev,
            cards: Default::default(),
            sensors: Default::default(),
            fans: Default::default(),
            power: Default::default(),
        };
        d.cards.insert("A".into(), CardBinding { uuid: "GPU-a".into(), bus: None });
        d.sensors.insert("ctrl_A".into(), orca_sensors::Expr::parse("ema(first(gpu.A.hbm, gpu.A.core), 4s)").unwrap());
        d.power.insert("A".into(), toml::from_str(&format!("persistence = true\npower_limit_w = {pl}")).unwrap());
        d
    }

    #[test]
    fn deploy_examples_pass() {
        let l = Limits::parse(include_str!("../deploy/hostd-limits.example.toml")).unwrap();
        let d = from_toml(include_str!("../deploy/policy.example.toml"), &l, 1).unwrap();
        let v = validate(d, &l, None).unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(v.cards.len(), 2);
        assert_eq!(v.doc.fans.fans.len(), 2);
    }

    #[test]
    fn builds_from_toml() {
        let d = from_toml(
            r#"
[cards.A]
uuid = "GPU-a"
bus = "21:00.0"
[sensors.ctrl_A]
expr = "ema(first(gpu.A.hbm, gpu.A.core), 4s)"
[power.A]
persistence = true
power_limit_w = 350
serving = "unlocked"
[fan_backends.dev]
kind = "fake"
[fans.duct1]
backend = "dev"
channel = 1
temp = "sensor.ctrl_A"
load = "gpu.A.power_pct"
curve = [[60, 30], [82, 100]]
"#,
            &limits(),
            7,
        )
        .unwrap();
        assert_eq!(d.rev, 7);
        assert_eq!(d.fans.fans.len(), 1);
        validate(d, &limits(), None).unwrap_or_else(|e| panic!("{e:?}"));
        assert!(from_toml("bogus = 1", &limits(), 1).is_err());
    }

    #[test]
    fn accepts_within_limits() {
        let v = validate(doc(2, 350), &limits(), Some(1)).unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(v.cards.len(), 1);
        assert_eq!(v.labels["GPU-a"], "A");
        assert_eq!(v.registry.len(), 1);
    }

    #[test]
    fn refuses_in_full() {
        // Power limit above the limits file.
        let e = validate(doc(2, 400), &limits(), None).err().unwrap();
        assert!(e.iter().any(|m| m.starts_with("power:")), "{e:?}");
        // Older revision.
        assert!(validate(doc(1, 350), &limits(), Some(5)).is_err());
        // A card the limits file does not list.
        let mut d = doc(2, 350);
        d.cards.insert("B".into(), CardBinding { uuid: "GPU-b".into(), bus: None });
        d.power.insert("B".into(), toml::from_str("persistence = true").unwrap());
        assert!(validate(d, &limits(), None).is_err());
        // Power for an unbound label; a sensor cycle.
        let mut d = doc(2, 350);
        d.power.insert("Z".into(), toml::from_str("persistence = true").unwrap());
        d.sensors.insert("x".into(), orca_sensors::Expr::parse("sensor.x").unwrap());
        let e = validate(d, &limits(), None).err().unwrap();
        assert!(e.len() >= 2, "{e:?}");
    }
}
