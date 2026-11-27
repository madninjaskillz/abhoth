//! What the public Glance leaves out (SUPERVISOR.md 12.1, 15 Q5): the recent-requests table,
//! OOM process names, container names, error text and network endpoints (model servers'
//! host:port keys, service ports, lease hosts). Everything else stays. Each part can be turned
//! off on its own.

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
    /// Network endpoints: model servers keyed `name:port` become their role (`ninfer-b`),
    /// without `port`/`url`/`host`; services lose their ports; leases their client host.
    pub endpoints: bool,
}

impl Default for Redaction {
    fn default() -> Self {
        Self {
            requests: true,
            oom_processes: true,
            containers: true,
            errors: true,
            endpoints: true,
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
            endpoints: false,
        };
        for part in list.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            match part {
                "none" => {}
                "all" => r = Self::default(),
                "requests" => r.requests = true,
                "oom" | "oom_processes" => r.oom_processes = true,
                "containers" => r.containers = true,
                "errors" => r.errors = true,
                "endpoints" => r.endpoints = true,
                other => {
                    return Err(format!(
                        "unknown redaction '{other}' (requests, oom, containers, errors, endpoints, all, none)"
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
        if self.endpoints {
            dropped.push("network endpoints");
            redact_endpoints(o);
        }
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
            if v.is_null() {
                out.push((k.clone(), v));
                continue;
            }
            match k.as_str() {
                "feed_errors" if self.errors => {
                    if let Some(m) = v.as_object_mut() {
                        for e in m.values_mut() {
                            *e = json!("unavailable");
                        }
                    }
                }
                // The model servers' chart history is keyed host:port (and goes once, at start).
                "history" if self.endpoints => {
                    if let Some(m) = v.as_object_mut() {
                        m.remove("llm");
                    }
                }
                "leases" if self.endpoints => strip_keys(&mut v, &["host", "client", "addr", "ip"]),
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
                // The supervisor's host-local sections (orca-supervisor local.rs).
                "pipelines" if self.errors => strip_keys(&mut v, &["last_error", "last_line"]),
                "gpu_procs" if self.containers => strip_keys(&mut v, &["container"]),
                "services" if self.endpoints => strip_keys(&mut v, &["port"]),
                "containers" if self.containers => v = json!([]),
                "ooms" => {
                    for r in v
                        .as_array_mut()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_object_mut)
                    {
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
                "models" => {
                    for s in v
                        .get_mut("servers")
                        .and_then(Value::as_object_mut)
                        .into_iter()
                        .flat_map(|m| m.values_mut())
                        .filter_map(Value::as_object_mut)
                    {
                        if self.requests {
                            s.remove("recent");
                        }
                        if self.endpoints {
                            s.remove("port");
                        }
                        if self.errors {
                            s.remove("error");
                        }
                    }
                }
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
        if self.endpoints {
            d.push("network endpoints");
        }
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

/// Model servers keyed by role instead of `host:port`, and no ports or client hosts.
fn redact_endpoints(o: &mut serde_json::Map<String, Value>) {
    let mut renamed: Vec<(String, String)> = Vec::new();
    if let Some(Value::Object(llm)) = o.remove("llm") {
        let mut out = serde_json::Map::new();
        for (n, (key, mut i)) in llm.into_iter().enumerate() {
            let base: String = i["role"]
                .as_str()
                .map(|r| {
                    r.trim()
                        .to_lowercase()
                        .chars()
                        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                        .collect()
                })
                .filter(|r: &String| !r.is_empty())
                .unwrap_or_else(|| format!("server-{}", n + 1));
            let mut id = base.clone();
            let mut k = 2;
            while out.contains_key(&id) {
                id = format!("{base}-{k}");
                k += 1;
            }
            if let Some(m) = i.as_object_mut() {
                m.insert("key".into(), json!(id));
                for f in ["port", "url", "base_url", "host"] {
                    m.remove(f);
                }
            }
            renamed.push((key, id.clone()));
            out.insert(id, i);
        }
        o.insert("llm".into(), Value::Object(out));
    }
    let map = |k: &str| renamed.iter().find(|(a, _)| a == k).map(|(_, b)| b.clone());
    for g in o
        .get_mut("gpus")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
    {
        if let Some(l) = g.get_mut("llm").and_then(Value::as_array_mut) {
            for x in l.iter_mut() {
                if let Some(n) = x.as_str().and_then(map) {
                    *x = json!(n);
                }
            }
        }
    }
    if let Some(Value::Object(c)) = o.get_mut("copies") {
        let old = std::mem::take(c);
        for (k, v) in old {
            c.insert(map(&k).unwrap_or(k), v);
        }
    }
    for svc in o
        .get_mut("services")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
    {
        if let Some(m) = svc.as_object_mut() {
            m.remove("port");
        }
    }
    if let Some(p) = o.get_mut("pig") {
        strip_keys(p, &["host"]);
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
        assert_eq!(
            out["llm"]["server-1"]["tok_s"], 3,
            "keyed by role (none here), not host:port"
        );
        assert_eq!(
            out["gpus"][0]["procs"][0]["process"], "x",
            "GPU process names stay"
        );
        assert_eq!(out["ooms"][0]["anon_mb"], 9);
        assert_eq!(out["alerts"][0]["title"], "Pool full");
        assert_eq!(out["profile"]["onDemand"]["text"], "ok");
        assert_eq!(out["redacted"].as_array().unwrap().len(), 5);
    }

    #[test]
    fn endpoints_become_roles() {
        let st = json!({
            "llm": {"ninfer-b:8096": {"key": "ninfer-b:8096", "role": "ninfer B", "port": 8096, "tok_s": 1},
                    "x:1": {"key": "x:1", "role": "ninfer B"}},
            "gpus": [{"uuid": "u", "llm": ["ninfer-b:8096"]}],
            "services": [{"name": "LiteLLM", "port": 8010, "up": true}],
            "pig": {"slots": {"A1": {"label": "bench", "host": "my-pc"}}},
        });
        let out = Redaction::default().apply(&st);
        let text = out.to_string();
        for gone in ["8096", "8010", "my-pc", "x:1"] {
            assert!(!text.contains(gone), "{gone}: {text}");
        }
        assert_eq!(out["llm"]["ninfer-b"]["tok_s"], 1);
        assert_eq!(out["llm"]["ninfer-b"]["key"], "ninfer-b");
        assert!(out["llm"]["ninfer-b-2"].is_object());
        assert_eq!(out["gpus"][0]["llm"][0], "ninfer-b");
        let kept = Redaction::parse("requests").unwrap().apply(&st);
        assert!(kept["llm"]["ninfer-b:8096"].is_object());
    }

    #[test]
    fn the_supervisors_local_sections() {
        let mut m = serde_json::Map::new();
        m.insert(
            "pipelines".into(),
            json!([{"name": "AI News", "status": "done", "last_error": "/secret/x", "last_line": "l"}]),
        );
        m.insert(
            "gpu_procs".into(),
            json!({"A": [{"process": "p", "container": "ix-app-1"}]}),
        );
        m.insert("containers".into(), json!([{"name": "ix-app-1"}]));
        m.insert(
            "ooms".into(),
            json!([{"container": "ix-b-1", "process": "python3", "anon_mb": 9}]),
        );
        m.insert(
            "models".into(),
            json!({"servers": {"n": {"port": 8096, "recent": [{"out": 1}], "error": "boom", "tok_s": 2}}}),
        );
        m.insert(
            "services".into(),
            json!([{"name": "x", "port": 8010, "up": true}]),
        );
        let out: serde_json::Map<String, Value> = Redaction::default()
            .apply_sections(&m)
            .into_iter()
            .collect();
        let text = Value::Object(out.clone()).to_string();
        for gone in [
            "/secret/x",
            "ix-app-1",
            "ix-b-1",
            "python3",
            "8096",
            "boom",
            "\"recent\"",
            "8010",
        ] {
            assert!(!text.contains(gone), "{gone} leaked: {text}");
        }
        assert_eq!(out["pipelines"][0]["status"], "done");
        assert_eq!(out["gpu_procs"]["A"][0]["process"], "p");
        assert_eq!(out["ooms"][0]["anon_mb"], 9);
        assert_eq!(out["models"]["servers"]["n"]["tok_s"], 2);
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
        assert_eq!(out["redacted"].as_array().unwrap().len(), 5);
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
