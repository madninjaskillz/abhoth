//! The profiles API (`:8098`) compatibility shim (ROUTER-CONFIG.md 8.4): the paths and JSON
//! shapes of `profiles_api.py`, answered from the control plane, for Abhoth's profile picker,
//! Home Assistant, Jarvis and the dash.
//!
//! - `GET /health`, `/live`, `/cards`, `/jobs?limit=N`, `/jobs/<id>`, `/plan?layout=|profile=`,
//!   `/profiles` (and `/`), `/ondemand`: anonymous (no secrets in them).
//! - `POST /switch`, `/compose`, `/profiles/save`, `/profiles/delete`,
//!   `/cards/<X>/offline|online`: **a key is required** (an admin key, or a service key with
//!   the switch permission, or an event key whose grant allows it). Anonymous answers 401 with
//!   a message that names the admin UI. The key is never logged.
//!
//! The data comes from a [`ShimBackend`] (the control plane's state and orchestrator); keys are
//! checked by a [`KeyCheck`] (the accounts integration). Both are traits so the shim is tested
//! on its own, and so the listener can be wired once the control plane's state is in place.

use axum::http::{HeaderMap, Method};
use serde_json::{Map, Value, json};

use crate::guards::Caller;

/// What the shim needs from the control plane. Methods that start work (switch, card
/// offline/online) only queue it and answer at once, as the old API did (202 + job id).
pub trait ShimBackend: Send + Sync {
    /// The catalog in profiles.json's shape with summaries (None: not available yet).
    fn profiles(&self) -> Option<Value>;
    fn live(&self) -> Value;
    fn cards(&self) -> Value;
    /// `history.jsonl` lines, newest first.
    fn history(&self, limit: usize) -> Vec<Value>;
    fn job(&self, id: &str) -> Option<Value>;
    /// `(status, body)` for `GET /plan`.
    fn plan(&self, layout: Option<&str>, profile: Option<&str>) -> (u16, Value);
    fn ondemand(&self) -> Value;
    /// `POST /switch`: 200 already live, 202 queued, 409 refused.
    fn switch(
        &self,
        layout: Option<&str>,
        profile: Option<&str>,
        requested_by: &str,
        reason: &str,
        caller: &Caller,
    ) -> (u16, Value);
    /// `POST /cards/<X>/offline|online`.
    fn card_action(
        &self,
        card: &str,
        online: bool,
        requested_by: &str,
        reason: &str,
        dry_run: bool,
        caller: &Caller,
    ) -> (u16, Value);
    /// `POST /compose`, `/profiles/save`, `/profiles/delete`: config writes.
    fn catalog_write(&self, what: &str, body: &Map<String, Value>, caller: &Caller)
    -> (u16, Value);
}

/// Who a bearer key belongs to, and whether it may switch layouts.
pub trait KeyCheck: Send + Sync {
    fn check(&self, key: &str) -> Option<(Caller, bool)>;
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    let v = headers.get("authorization")?.to_str().ok()?;
    let k = v
        .strip_prefix("Bearer ")
        .or_else(|| v.strip_prefix("bearer "))?;
    let k = k.trim();
    (!k.is_empty()).then_some(k)
}

fn query(q: &str) -> Vec<(String, String)> {
    q.split('&')
        .filter_map(|p| p.split_once('='))
        .map(|(k, v)| {
            (
                k.to_string(),
                v.replace('+', " ")
                    .replace("%20", " ")
                    .replace("%3A", ":")
                    .replace("%2F", "/"),
            )
        })
        .collect()
}

const NEED_KEY: &str = "changing layouts or cards needs an admin key or a service key with the \
switch permission (Authorization: Bearer ...); people use the admin UI (Router > Layouts)";

/// Answer one request. `path_q` includes the query string.
pub fn handle(
    b: &dyn ShimBackend,
    keys: &dyn KeyCheck,
    method: &Method,
    path_q: &str,
    headers: &HeaderMap,
    raw: &[u8],
) -> (u16, Value) {
    let (path, q) = path_q.split_once('?').unwrap_or((path_q, ""));
    let q = query(q);
    let get = |k: &str| q.iter().find(|(x, _)| x == k).map(|(_, v)| v.as_str());
    if *method == Method::GET {
        return match path {
            "/health" => (
                200,
                json!({"status": "ok", "service": "profiles-api", "impl": "abhoth-router"}),
            ),
            "/profiles" | "/" => match b.profiles() {
                Some(p) => (200, p),
                None => (503, json!({"error": "the catalog is not available yet"})),
            },
            "/live" => (200, b.live()),
            "/cards" => (200, b.cards()),
            "/ondemand" => (200, b.ondemand()),
            "/plan" => b.plan(
                if get("profile").is_some() {
                    None
                } else {
                    get("layout")
                },
                get("profile"),
            ),
            "/jobs" => {
                let n = get("limit").and_then(|l| l.parse().ok()).unwrap_or(20);
                (200, json!({"jobs": b.history(n)}))
            }
            p if p.starts_with("/jobs/") => {
                let id: String = p["/jobs/".len()..]
                    .chars()
                    .filter(|c| c.is_ascii_alphanumeric() || "._-".contains(*c))
                    .collect();
                match b.job(&id) {
                    Some(j) => (200, j),
                    None => (404, json!({"error": "no such job"})),
                }
            }
            _ => (404, json!({"error": "not found"})),
        };
    }
    if *method != Method::POST {
        return (404, json!({"error": "not found"}));
    }
    // every POST changes something: a key first, before even reading the body
    let Some((caller, may_switch)) = bearer(headers).and_then(|k| keys.check(k)) else {
        return (401, json!({"error": NEED_KEY}));
    };
    if !may_switch {
        return (403, json!({"error": NEED_KEY}));
    }
    let body: Map<String, Value> = if raw.is_empty() || raw.len() >= 65536 {
        Map::new()
    } else {
        match serde_json::from_slice::<Value>(raw) {
            Ok(Value::Object(m)) => m,
            _ => return (400, json!({"error": "bad json"})),
        }
    };
    let s = |k: &str, n: usize| -> String {
        body.get(k)
            .and_then(Value::as_str)
            .unwrap_or("")
            .chars()
            .take(n)
            .collect()
    };
    let by = {
        let r = s("requestedBy", 80);
        if r.is_empty() { "api".to_string() } else { r }
    };
    let reason = s("reason", 200);
    if let Some(rest) = path.strip_prefix("/cards/") {
        let mut it = rest.split('/');
        if let (Some(card), Some(action), None) = (it.next(), it.next(), it.next())
            && (action == "offline" || action == "online")
            && card.len() == 1
            && card.chars().all(|c| c.is_ascii_alphabetic())
        {
            let dry = body.get("dryRun").and_then(Value::as_bool).unwrap_or(false);
            return b.card_action(
                &card.to_ascii_uppercase(),
                action == "online",
                &by,
                &reason,
                dry,
                &caller,
            );
        }
        return (404, json!({"error": "not found"}));
    }
    match path {
        "/switch" => {
            let profile = body.get("profile").and_then(Value::as_str);
            let layout = body.get("layout").and_then(Value::as_str);
            if profile.is_none() && layout.is_none() {
                return (400, json!({"error": "profile or layout required"}));
            }
            b.switch(
                if profile.is_some() { None } else { layout },
                profile,
                &by,
                &reason,
                &caller,
            )
        }
        "/compose" | "/profiles/save" | "/profiles/delete" => {
            b.catalog_write(&path[1..], &body, &caller)
        }
        _ => (404, json!({"error": "not found"})),
    }
}

#[cfg(test)]
mod shim_tests {
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct B {
        calls: Mutex<Vec<String>>,
    }

    impl ShimBackend for B {
        fn profiles(&self) -> Option<Value> {
            None
        }
        fn live(&self) -> Value {
            json!({"layout": "L", "cards": {}, "job": null})
        }
        fn cards(&self) -> Value {
            json!({"layout": "L", "cards": {}, "job": null, "checked": "t"})
        }
        fn history(&self, limit: usize) -> Vec<Value> {
            (0..limit.min(2)).map(|i| json!({"id": i})).collect()
        }
        fn job(&self, id: &str) -> Option<Value> {
            (id == "j1").then(|| json!({"id": "j1", "state": "done", "steps": []}))
        }
        fn plan(&self, layout: Option<&str>, profile: Option<&str>) -> (u16, Value) {
            (200, json!({"layout": layout, "profile": profile}))
        }
        fn ondemand(&self) -> Value {
            json!({"enabled": false})
        }
        fn switch(
            &self,
            layout: Option<&str>,
            profile: Option<&str>,
            by: &str,
            _r: &str,
            c: &Caller,
        ) -> (u16, Value) {
            self.calls.lock().unwrap().push(format!(
                "switch {layout:?} {profile:?} {by} {:?}",
                c.principal
            ));
            (202, json!({"id": "j2", "state": "queued"}))
        }
        fn card_action(
            &self,
            card: &str,
            online: bool,
            _b: &str,
            _r: &str,
            dry: bool,
            _c: &Caller,
        ) -> (u16, Value) {
            self.calls
                .lock()
                .unwrap()
                .push(format!("card {card} {online} {dry}"));
            (202, json!({"id": "j3"}))
        }
        fn catalog_write(&self, what: &str, _b: &Map<String, Value>, _c: &Caller) -> (u16, Value) {
            self.calls.lock().unwrap().push(what.to_string());
            (200, json!({}))
        }
    }

    struct K;
    impl KeyCheck for K {
        fn check(&self, key: &str) -> Option<(Caller, bool)> {
            let c = |p: &str| Caller {
                principal: Some(p.into()),
                grants: vec![],
            };
            match key {
                "admin-key" => Some((c("admin"), true)),
                "user-key" => Some((c("user"), false)),
                _ => None,
            }
        }
    }

    fn h(key: Option<&str>) -> HeaderMap {
        let mut h = HeaderMap::new();
        if let Some(k) = key {
            h.insert("authorization", format!("Bearer {k}").parse().unwrap());
        }
        h
    }

    #[test]
    fn gets_are_anonymous_posts_need_a_switch_key() {
        let b = B::default();
        let g = |p: &str| handle(&b, &K, &Method::GET, p, &h(None), b"");
        assert_eq!(g("/health").1["service"], "profiles-api");
        assert_eq!(g("/live").0, 200);
        assert_eq!(g("/cards").1["checked"], "t");
        assert_eq!(g("/jobs?limit=5").1["jobs"].as_array().unwrap().len(), 2);
        assert_eq!(g("/jobs/j1").1["state"], "done");
        assert_eq!(g("/jobs/../../etc").0, 404);
        assert_eq!(g("/plan?layout=flash-x2").1["layout"], "flash-x2");
        assert_eq!(g("/plan?layout=x&profile=p").1["layout"], Value::Null);
        assert_eq!(g("/profiles").0, 503);
        assert_eq!(g("/nope").0, 404);

        let body = br#"{"layout": "flash-x2", "requestedBy": "ha", "reason": "button"}"#;
        let post = |p: &str, key: Option<&str>| handle(&b, &K, &Method::POST, p, &h(key), body);
        // anonymous and unknown keys: 401, a key without the permission: 403; nothing reached
        for p in [
            "/switch",
            "/cards/B/offline",
            "/cards/A/online",
            "/compose",
            "/profiles/save",
            "/profiles/delete",
        ] {
            let (code, v) = post(p, None);
            assert_eq!(code, 401, "{p}");
            assert!(v["error"].as_str().unwrap().contains("admin UI"));
            assert_eq!(post(p, Some("wrong")).0, 401);
            assert_eq!(post(p, Some("user-key")).0, 403);
        }
        assert!(b.calls.lock().unwrap().is_empty());
        // with an admin or service key
        assert_eq!(post("/switch", Some("admin-key")).0, 202);
        assert_eq!(post("/cards/b/offline", Some("admin-key")).0, 202);
        assert_eq!(post("/cards/AB/offline", Some("admin-key")).0, 404);
        assert_eq!(post("/compose", Some("admin-key")).0, 200);
        let calls = b.calls.lock().unwrap().clone();
        assert_eq!(
            calls,
            [
                "switch Some(\"flash-x2\") None ha Some(\"admin\")",
                "card B false false",
                "compose"
            ]
        );
        assert_eq!(
            handle(
                &b,
                &K,
                &Method::POST,
                "/switch",
                &h(Some("admin-key")),
                b"[1]"
            )
            .0,
            400
        );
        assert_eq!(
            handle(
                &b,
                &K,
                &Method::POST,
                "/switch",
                &h(Some("admin-key")),
                b"{}"
            )
            .0,
            400
        );
        assert_eq!(
            handle(
                &b,
                &K,
                &Method::DELETE,
                "/switch",
                &h(Some("admin-key")),
                b""
            )
            .0,
            404
        );
    }
}
