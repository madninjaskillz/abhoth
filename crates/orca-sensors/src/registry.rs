//! Named expressions (`[sensors.<name>] expr = "…"`) evaluated into the `sensor.*` namespace.
//!
//! The registry orders sensors so that `sensor.a` referencing `sensor.b` evaluates `b` first,
//! rejects cycles and references to undefined virtual sensors, and keeps one [`EvalState`] per
//! sensor so `ema` works across ticks. A missing result removes the `sensor.<name>` reading
//! (absent, never 0), and the reason is returned for the dashboard.

use std::collections::{BTreeMap, BTreeSet};

use crate::expr::{EvalState, Expr, ExprError, Missing, Value};
use crate::reading::{Reading, SensorStore};
use crate::{Namespace, SensorId, TimestampMs};

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum RegistryError {
    #[error("sensors.{name}: {err}")]
    Parse { name: String, err: ExprError },
    #[error("sensors.{name}: references undefined sensor.{missing}")]
    Undefined { name: String, missing: String },
    #[error("sensors: cycle through {0}")]
    Cycle(String),
    #[error("sensors.{0}: a name must be a plain identifier")]
    BadName(String),
}

#[derive(Debug, Clone)]
struct Entry {
    name: String,
    id: SensorId,
    expr: Expr,
    state: EvalState,
}

/// The compiled `[sensors.*]` table.
#[derive(Debug, Clone, Default)]
pub struct SensorRegistry {
    /// In evaluation (dependency) order.
    entries: Vec<Entry>,
}

/// One evaluation result.
#[derive(Debug, Clone, PartialEq)]
pub struct Evaluated {
    pub name: String,
    pub id: SensorId,
    pub result: Result<Value, Missing>,
}

impl SensorRegistry {
    /// Compiles `(name, expr text)` pairs.
    pub fn new<I, N, E>(defs: I) -> Result<SensorRegistry, RegistryError>
    where
        I: IntoIterator<Item = (N, E)>,
        N: Into<String>,
        E: AsRef<str>,
    {
        let mut parsed: BTreeMap<String, Expr> = BTreeMap::new();
        for (n, e) in defs {
            let name: String = n.into();
            if name.is_empty()
                || !name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            {
                return Err(RegistryError::BadName(name));
            }
            let expr = Expr::parse(e.as_ref()).map_err(|err| RegistryError::Parse {
                name: name.clone(),
                err,
            })?;
            parsed.insert(name, expr);
        }
        // Dependencies on other virtual sensors.
        let mut deps: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for (name, expr) in &parsed {
            let mut d = BTreeSet::new();
            for src in expr.sources() {
                if src.namespace() == Namespace::Sensor {
                    if src.is_pattern() {
                        // sensor.* patterns: depend on every matching virtual sensor.
                        for other in parsed.keys() {
                            if SensorId::of(Namespace::Sensor, &[other]).matches(&src) {
                                d.insert(other.clone());
                            }
                        }
                        continue;
                    }
                    let target = src.segments().join(".");
                    if !parsed.contains_key(&target) {
                        return Err(RegistryError::Undefined {
                            name: name.clone(),
                            missing: target,
                        });
                    }
                    d.insert(target);
                }
            }
            deps.insert(name.clone(), d);
        }
        // Topological order (Kahn), deterministic by name.
        let mut order = Vec::new();
        let mut done: BTreeSet<String> = BTreeSet::new();
        while done.len() < parsed.len() {
            let ready: Vec<String> = deps
                .iter()
                .filter(|(n, d)| !done.contains(*n) && d.iter().all(|x| done.contains(x)))
                .map(|(n, _)| n.clone())
                .collect();
            if ready.is_empty() {
                let stuck: Vec<&str> = deps
                    .keys()
                    .filter(|n| !done.contains(*n))
                    .map(String::as_str)
                    .collect();
                return Err(RegistryError::Cycle(stuck.join(", ")));
            }
            for n in ready {
                done.insert(n.clone());
                order.push(n);
            }
        }
        let entries = order
            .into_iter()
            .map(|name| {
                let expr = parsed.remove(&name).expect("parsed");
                Entry {
                    id: SensorId::of(Namespace::Sensor, &[&name]),
                    state: expr.new_state(),
                    expr,
                    name,
                }
            })
            .collect();
        Ok(SensorRegistry { entries })
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// `(name, expr)` in evaluation order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Expr)> {
        self.entries.iter().map(|e| (e.name.as_str(), &e.expr))
    }

    pub fn get(&self, name: &str) -> Option<&Expr> {
        self.entries
            .iter()
            .find(|e| e.name == name)
            .map(|e| &e.expr)
    }

    /// Evaluates every sensor at `now`, writing `sensor.<name>` readings into `store` (or
    /// removing them when missing). Returns each result in evaluation order.
    pub fn evaluate(&mut self, store: &mut SensorStore, now: TimestampMs) -> Vec<Evaluated> {
        let mut out = Vec::with_capacity(self.entries.len());
        for e in &mut self.entries {
            let result = e.expr.eval(store, now, &mut e.state);
            match &result {
                Ok(v) => store.put(
                    e.id.clone(),
                    Reading::new(v.v, v.unit, v.ts.unwrap_or(now), "expr"),
                ),
                Err(_) => store.remove(&e.id),
            }
            out.push(Evaluated {
                name: e.name.clone(),
                id: e.id.clone(),
                result,
            });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reading::{SensorSource, Unit};

    #[test]
    fn orders_dependencies_and_writes_readings() {
        let mut reg = SensorRegistry::new([
            ("duct0", "max(sensor.ctrl_A, sensor.ctrl_B)"),
            ("ctrl_A", "ema(first(gpu.A.hbm, gpu.A.core), 4s)"),
            ("ctrl_B", "first(gpu.B.hbm, gpu.B.core)"),
        ])
        .unwrap();
        let names: Vec<&str> = reg.iter().map(|(n, _)| n).collect();
        assert_eq!(names, ["ctrl_A", "ctrl_B", "duct0"]);
        let mut s = SensorStore::new();
        s.put(
            SensorId::parse("gpu.A.core").unwrap(),
            Reading::new(61.0, Unit::Celsius, 1_000, "t"),
        );
        s.put(
            SensorId::parse("gpu.B.hbm").unwrap(),
            Reading::new(66.0, Unit::Celsius, 1_000, "t"),
        );
        let r = reg.evaluate(&mut s, 1_200);
        assert!(r.iter().all(|e| e.result.is_ok()));
        let d = s.lookup(&SensorId::parse("sensor.duct0").unwrap(), 1_200);
        assert_eq!(d.fresh().unwrap().value, 66.0);
        assert_eq!(d.fresh().unwrap().unit, Unit::Celsius);
        // B disappears: ctrl_B and duct0 become absent, not 0.
        s.remove(&SensorId::parse("gpu.B.hbm").unwrap());
        let r = reg.evaluate(&mut s, 1_400);
        assert!(r[1].result.is_err() && r[2].result.is_err());
        assert!(s.get(&SensorId::parse("sensor.duct0").unwrap()).is_none());
    }

    #[test]
    fn rejects_cycles_and_undefined() {
        assert!(matches!(
            SensorRegistry::new([("a", "sensor.b + 1"), ("b", "sensor.a")]),
            Err(RegistryError::Cycle(_))
        ));
        assert!(matches!(
            SensorRegistry::new([("a", "sensor.a")]),
            Err(RegistryError::Cycle(_))
        ));
        assert!(matches!(
            SensorRegistry::new([("a", "sensor.zz")]),
            Err(RegistryError::Undefined { .. })
        ));
        assert!(matches!(
            SensorRegistry::new([("a", "max(")]),
            Err(RegistryError::Parse { .. })
        ));
        assert!(matches!(
            SensorRegistry::new([("a b", "1")]),
            Err(RegistryError::BadName(_))
        ));
    }
}
