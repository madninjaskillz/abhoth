//! `/admin/api/bridges…` (admins only):
//!
//! - `GET    bridges`                 every bridge: token, settings, online, descriptor;
//! - `POST   bridges {id, rotate?}`   mint a token (in the answer once; only its hash is kept);
//! - `DELETE bridges/<id>`            revoke its token and disconnect it;
//! - `PUT    bridges/<id>/settings {enabled?, priority?}`;
//! - `POST   bridges/<id>/cmd {op, args}`   `status`, `probe`, `logs`, `detect`, `files`,
//!   `start`, `stop`;
//! - `POST   bridges/<id>/request {engine, card, method, path, body?}`   a test request through
//!   the tunnel (the answer's first 64 KiB).

use std::sync::Arc;
use std::time::Duration;

use axum::http::{HeaderMap, Method};
use serde_json::{Value, json};

use super::{Hub, tokens};

/// `(status, body)`; `audit(action, target)` records what changed.
pub async fn handle(
    hub: &Arc<Hub>,
    method: &Method,
    rest: &[&str],
    body: &Value,
    audit: &(dyn Fn(&str, &str) + Send + Sync),
) -> (u16, Value) {
    let s = |k: &str| {
        body.get(k)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    match (method.clone(), rest) {
        (Method::GET, []) => (200, hub.view()),
        (Method::POST, []) => {
            let id = s("id");
            let rotate = body.get("rotate").and_then(Value::as_bool).unwrap_or(false);
            match tokens::mint(&hub.state_dir, &id, rotate) {
                Ok(token) => {
                    audit(
                        if rotate {
                            "bridge.token-rotate"
                        } else {
                            "bridge.token-create"
                        },
                        &id,
                    );
                    if rotate && let Some(m) = hub.mux(&id) {
                        m.close(orca_bridge::proto::CLOSE_BAD_TOKEN, "the token was rotated");
                    }
                    (
                        200,
                        json!({"id": id, "token": token, "path": orca_bridge::proto::WS_PATH}),
                    )
                }
                Err(e) => (
                    if e.contains("already") { 409 } else { 422 },
                    json!({"error": e}),
                ),
            }
        }
        (Method::DELETE, [id]) => match tokens::revoke(&hub.state_dir, id) {
            Ok(()) => {
                hub.remove(id);
                audit("bridge.token-revoke", id);
                (200, json!({"ok": true}))
            }
            Err(e) => (404, json!({"error": e})),
        },
        (Method::PUT, [id, "settings"]) => {
            if !tokens::exists(&hub.state_dir, id) && !hub.is_bridge(id) {
                return (404, json!({"error": format!("no bridge {id}")}));
            }
            let mut st = hub.settings(id);
            if let Some(e) = body.get("enabled").and_then(Value::as_bool) {
                st.enabled = e;
            }
            if let Some(p) = body.get("priority").and_then(Value::as_i64) {
                st.priority = p.clamp(-100, 100) as i32;
            }
            match hub.set_settings(id, st.clone()) {
                Ok(()) => {
                    audit("bridge.settings", id);
                    (
                        200,
                        json!({"id": id, "enabled": st.enabled, "priority": st.priority}),
                    )
                }
                Err(e) => (500, json!({"error": e})),
            }
        }
        (Method::POST, [id, "cmd"]) => {
            let op = s("op");
            if !matches!(
                op.as_str(),
                "status" | "probe" | "logs" | "detect" | "files" | "start" | "stop"
            ) {
                return (
                    422,
                    json!({"error": format!("`{op}` is not a command the admin pages send")}),
                );
            }
            if matches!(op.as_str(), "start" | "stop") {
                audit(&format!("bridge.{op}"), id);
            }
            let args = body.get("args").cloned().unwrap_or(json!({}));
            let timeout = Duration::from_secs(if op == "start" { 900 } else { 60 });
            match hub.call(id, &op, args, timeout).await {
                Ok(v) => (200, v),
                Err(e) => (502, json!({"error": e})),
            }
        }
        (Method::POST, [id, "request"]) => {
            let mut h = HeaderMap::new();
            h.insert("content-type", "application/json".parse().unwrap());
            h.insert(orca_bridge::proto::NO_SWITCH, "1".parse().unwrap());
            let b = match body.get("body") {
                None | Some(Value::Null) => Vec::new(),
                Some(Value::String(t)) => t.clone().into_bytes(),
                Some(v) => v.to_string().into_bytes(),
            };
            let method = match s("method") {
                m if m.is_empty() => "GET".to_string(),
                m => m.to_ascii_uppercase(),
            };
            let path = match s("path") {
                p if p.starts_with('/') => p,
                p => format!("/{p}"),
            };
            let r = super::forward(
                hub,
                id,
                &s("engine"),
                &s("card"),
                &method,
                &path,
                &h,
                b.into(),
            )
            .await;
            let (status, headers, text) = super::body_text(r, 1 << 20).await;
            let cut: String = text.chars().take(65536).collect();
            (
                200,
                json!({"status": status, "headers": headers, "body": cut}),
            )
        }
        _ => (404, json!({"error": "no such bridges call"})),
    }
}
