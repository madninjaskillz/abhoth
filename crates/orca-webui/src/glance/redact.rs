//! What the public Glance leaves out (SUPERVISOR.md 12.1, 15 Q5): the recent-requests table,
//! OOM process names, container names and error text. Everything else stays. Each part can be
//! turned off on its own.

use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redaction {
    /// `llm.*.requests` (prompts sizes, timings: who is using what, when).
    pub requests: bool,
    /// `ooms[].process` (and its pid).
    pub oom_processes: bool,
    /// Container names: `containers`, `llm.*.container`, `gpus[].procs[].container`,
    /// `ooms[].container`.
    pub containers: bool,
    /// Error text: `errors`, `*.error`, `*.last_error`, alert details.
    pub errors: bool,
}

impl Default for Redaction {
    fn default() -> Self {
        Self {
            requests: true,
            oom_processes: true,
            containers: true,
            errors: true,
        }
    }
}

impl Redaction {
    /// Parses "requests,oom,containers,errors" (what to drop); "none" keeps everything.
    pub fn parse(list: &str) -> Result<Self, String> {
        let mut r = Self {
            requests: false,
            oom_processes: false,
            containers: false,
            errors: false,
        };
        for part in list.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            match part {
                "none" => {}
                "all" => r = Self::default(),
                "requests" => r.requests = true,
                "oom" | "oom_processes" => r.oom_processes = true,
                "containers" => r.containers = true,
                "errors" => r.errors = true,
                other => {
                    return Err(format!(
                        "unknown redaction '{other}' (requests, oom, containers, errors, all, none)"
                    ));
                }
            }
        }
        Ok(r)
    }

    /// The state the public feed serves; `redacted` lists what was dropped.
    pub fn apply(&self, state: &Value) -> Value {
        let mut v = state.clone();
        let Some(o) = v.as_object_mut() else { return v };
        let mut dropped = Vec::new();
        if self.requests {
            dropped.push("recent requests");
            for i in llm(o) {
                i.remove("requests");
            }
        }
        if self.containers {
            dropped.push("container names");
            o.remove("containers");
            for i in llm(o) {
                i.remove("container");
                i.remove("container_health");
            }
            if let Some(gpus) = o.get_mut("gpus").and_then(Value::as_array_mut) {
                for g in gpus.iter_mut() {
                    if let Some(procs) = g.get_mut("procs").and_then(Value::as_array_mut) {
                        for p in procs.iter_mut().filter_map(Value::as_object_mut) {
                            p.remove("container");
                        }
                    }
                }
            }
            for r in ooms(o) {
                r.remove("container");
            }
        }
        if self.oom_processes {
            dropped.push("OOM process names");
            for r in ooms(o) {
                r.remove("process");
                r.remove("pid");
            }
        }
        if self.errors {
            dropped.push("error text");
            o.remove("errors");
            for i in llm(o) {
                i.remove("error");
            }
            for k in ["esp", "bmc"] {
                if let Some(x) = o.get_mut(k).and_then(Value::as_object_mut) {
                    x.remove("error");
                }
            }
            if let Some(n) = o.get_mut("news").and_then(Value::as_object_mut) {
                n.remove("last_error");
                n.remove("last_line");
            }
            if let Some(od) = o
                .get_mut("profile")
                .and_then(|p| p.get_mut("onDemand"))
                .and_then(Value::as_object_mut)
            {
                od.remove("error");
            }
            if let Some(alerts) = o.get_mut("alerts").and_then(Value::as_array_mut) {
                for a in alerts.iter_mut().filter_map(Value::as_object_mut) {
                    a.remove("detail");
                }
            }
            if let Some(engines) = o.get_mut("engines").and_then(Value::as_array_mut) {
                for e in engines.iter_mut().filter_map(Value::as_object_mut) {
                    if e.get("state").and_then(Value::as_str) == Some("failed") {
                        e.remove("detail");
                    }
                }
            }
        }
        if !dropped.is_empty() {
            o.insert("redacted".into(), json!(dropped));
        }
        v
    }
}

fn llm(
    o: &mut serde_json::Map<String, Value>,
) -> impl Iterator<Item = &mut serde_json::Map<String, Value>> {
    o.get_mut("llm")
        .and_then(Value::as_object_mut)
        .into_iter()
        .flat_map(|m| m.values_mut().filter_map(Value::as_object_mut))
}

fn ooms(
    o: &mut serde_json::Map<String, Value>,
) -> impl Iterator<Item = &mut serde_json::Map<String, Value>> {
    o.get_mut("ooms")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flat_map(|a| a.iter_mut().filter_map(Value::as_object_mut))
}

impl Redaction {
    /// The public hub's sections: the legacy `dash` section through [`Redaction::apply`]; the
    /// supervisor's own sections by their owners' shapes (alert details, failed engines'
    /// details, `facts` containers and OOM names, `health` errors); a `redacted` section lists
    /// what was left out.
    pub fn apply_sections(
        &self,
        sections: &serde_json::Map<String, Value>,
    ) -> Vec<(String, Value)> {
        let mut out = Vec::new();
        for (k, v) in sections {
            let mut v = v.clone();
            match k.as_str() {
                "dash" => {
                    v = self.apply(&v);
                    if let Some(o) = v.as_object_mut() {
                        o.remove("redacted");
                    }
                }
                "alerts" if self.errors => {
                    for a in v
                        .as_array_mut()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_object_mut)
                    {
                        a.remove("detail");
                    }
                }
                "engines" if self.errors => {
                    for e in v
                        .as_array_mut()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_object_mut)
                    {
                        if e.get("state").and_then(Value::as_str) == Some("failed") {
                            e.remove("detail");
                        }
                        e.remove("error");
                    }
                }
                "facts" => {
                    if let Some(o) = v.as_object_mut() {
                        if self.containers {
                            o.remove("containers");
                        }
                        for r in ooms(o) {
                            if self.containers {
                                r.remove("container");
                            }
                            if self.oom_processes {
                                r.remove("process");
                                r.remove("pid");
                                r.remove("cmdline");
                            }
                        }
                    }
                }
                "health" if self.errors => strip_keys(&mut v, &["error", "last_error"]),
                _ => {}
            }
            out.push((k.clone(), v));
        }
        let dropped = self.dropped();
        if !dropped.is_empty() {
            out.push(("redacted".into(), json!(dropped)));
        }
        out
    }

    fn dropped(&self) -> Vec<&'static str> {
        let mut d = Vec::new();
        if self.requests {
            d.push("recent requests");
        }
        if self.containers {
            d.push("container names");
        }
        if self.oom_processes {
            d.push("OOM process names");
        }
        if self.errors {
            d.push("error text");
        }
        d
    }

    /// An event for the public feed: its kind, severity and line stay; its data loses error
    /// text and output.
    pub fn public_event(&self, e: &orca_sensors::feed::FeedEvent) -> orca_sensors::feed::FeedEvent {
        let mut e = e.clone();
        if self.errors {
            strip_keys(
                &mut e.data,
                &["error", "detail", "output", "stderr", "last_error"],
            );
            if e.kind.contains("error") {
                e.text.clear();
            }
        }
        if self.containers {
            strip_keys(&mut e.data, &["container"]);
        }
        e
    }
}

fn strip_keys(v: &mut Value, keys: &[&str]) {
    match v {
        Value::Object(m) => {
            for k in keys {
                m.remove(*k);
            }
            for x in m.values_mut() {
                strip_keys(x, keys);
            }
        }
        Value::Array(a) => {
            for x in a {
                strip_keys(x, keys);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Value {
        json!({
            "time": 1,
            "llm": {"a:1": {"key": "a:1", "container": "ninfer-a", "error": "boom", "requests": [{"time": 1}], "tok_s": 3}},
            "gpus": [{"uuid": "g", "procs": [{"pid": 1, "process": "x", "container": "ix-app-1", "mem_mib": 5}]}],
            "ooms": [{"time": "t", "pid": 7, "process": "python3", "container": "ix-b-1", "anon_mb": 9}],
            "containers": [{"name": "ix-b-1"}],
            "errors": {"host": "stale"},
            "esp": {"live": false, "error": "timeout"},
            "news": {"status": "done", "last_error": "secret path"},
            "profile": {"onDemand": {"error": "x", "text": "ok"}},
            "alerts": [{"id": "1", "title": "Pool full", "detail": "/mnt/x"}],
        })
    }

    #[test]
    fn the_default_drops_requests_oom_names_containers_and_errors() {
        let out = Redaction::default().apply(&sample());
        let text = out.to_string();
        for gone in [
            "ninfer-a",
            "boom",
            "\"requests\":",
            "python3",
            "ix-app-1",
            "ix-b-1",
            "stale",
            "timeout",
            "secret path",
            "/mnt/x",
        ] {
            assert!(!text.contains(gone), "{gone} leaked: {text}");
        }
        assert_eq!(out["llm"]["a:1"]["tok_s"], 3);
        assert_eq!(
            out["gpus"][0]["procs"][0]["process"], "x",
            "GPU process names stay"
        );
        assert_eq!(out["ooms"][0]["anon_mb"], 9);
        assert_eq!(out["alerts"][0]["title"], "Pool full");
        assert_eq!(out["profile"]["onDemand"]["text"], "ok");
        assert_eq!(out["redacted"].as_array().unwrap().len(), 4);
    }

    #[test]
    fn sections_and_events() {
        let mut m = serde_json::Map::new();
        m.insert("dash".into(), sample());
        m.insert(
            "alerts".into(),
            json!([{"id": "1", "title": "t", "detail": "/mnt/x"}]),
        );
        m.insert("facts".into(), json!({"containers": [{"name": "ix-b-1"}], "ooms": [{"process": "p", "container": "c", "anon_mb": 1}]}));
        m.insert(
            "health".into(),
            json!({"hostd": {"last_error": "secret", "ts": 1}}),
        );
        let out: serde_json::Map<String, Value> = Redaction::default()
            .apply_sections(&m)
            .into_iter()
            .collect();
        let text = Value::Object(out.clone()).to_string();
        for gone in ["/mnt/x", "ix-b-1", "secret", "\"process\":\"p\"", "boom"] {
            assert!(!text.contains(gone), "{gone}");
        }
        assert_eq!(out["health"]["hostd"]["ts"], 1);
        assert_eq!(out["redacted"].as_array().unwrap().len(), 4);
        let e = orca_sensors::feed::FeedEvent {
            kind: "engine.error".into(),
            severity: Default::default(),
            text: "failed: /mnt/secret".into(),
            data: json!({"error": "x", "card": "A"}),
        };
        let p = Redaction::default().public_event(&e);
        assert!(p.text.is_empty());
        assert_eq!(p.data, json!({"card": "A"}));
    }

    #[test]
    fn each_part_can_be_kept() {
        let r = Redaction::parse("requests").unwrap();
        let out = r.apply(&sample());
        assert!(out["llm"]["a:1"].get("requests").is_none());
        assert_eq!(out["llm"]["a:1"]["container"], "ninfer-a");
        assert_eq!(out["errors"]["host"], "stale");
        assert_eq!(Redaction::parse("none").unwrap().apply(&sample()), sample());
        assert_eq!(Redaction::parse("all").unwrap(), Redaction::default());
        assert!(Redaction::parse("bogus").is_err());
    }
}
