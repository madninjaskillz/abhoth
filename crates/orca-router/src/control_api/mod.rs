//! `/admin/api/*`: the control plane's admin API (ROUTER-CONFIG.md 11), behind the access gate
//! ([`crate::access`]). Admins only, except `whoami`; a service key with `read` may read state
//! and jobs, one with `switch` may plan and switch.
//!
//! The handlers talk to a [`ControlBackend`]: the real control plane (config, orchestrator,
//! leases) implements it; [`demo::DemoControl`] is an in-memory one for the UI's tests and
//! screenshots. Live state goes out as one SSE stream, `GET /admin/api/events`
//! (`event: <kind>`, `data: <json>`), which the admin pages and the supervisor dashboard share.
//!
//! Nothing here exists unless the control plane is on (`control.enabled`).

pub mod demo;
pub mod form;
pub mod live;
pub mod model;
pub mod user_view;

use std::convert::Infallible;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use tokio::sync::broadcast;

use crate::access::{Access, AuthError, Perm, Principal};
use model::*;

/// One live event for `/admin/api/events`.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    /// state | telemetry | job | guard | config | lease | audit
    pub kind: &'static str,
    pub data: Value,
}

/// An error a backend action ends in (status + message for the page).
#[derive(Debug, Clone, PartialEq)]
pub struct ApiError {
    pub status: u16,
    pub message: String,
    /// Extra fields (e.g. `current` with both diffs on a 412).
    pub extra: Option<Value>,
}

impl ApiError {
    pub fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            extra: None,
        }
    }
}

pub type ApiResult<T> = Result<T, ApiError>;

/// What the admin API needs from the control plane. Reads are cheap (in-memory snapshots);
/// actions may run hooks or SSH and are async. `who` is the acting principal's label, for the
/// audit.
#[async_trait::async_trait]
pub trait ControlBackend: Send + Sync {
    fn state(&self) -> StateView;
    fn config(&self) -> ConfigDoc;
    fn jobs(&self, limit: usize) -> Vec<Job>;
    fn job(&self, id: &str) -> Option<Job>;
    fn audit(&self, limit: usize, before: Option<i64>) -> Vec<AuditRow>;
    fn leases(&self) -> LeasesView;
    fn routes(&self) -> Vec<RouteView>;
    fn subscribe(&self) -> broadcast::Receiver<Event>;

    async fn check_config(&self, text: &str) -> CheckResult;
    /// Save with optimistic concurrency: `if_match` must be the current rev.
    async fn save_config(
        &self,
        text: &str,
        if_match: &str,
        reason: &str,
        who: &str,
    ) -> ApiResult<CheckResult>;
    async fn plan(&self, server: &str, layout: &str) -> ApiResult<Plan>;
    /// Start a switch job (re-plans first, 5.3); returns the job.
    async fn switch(&self, server: &str, layout: &str, who: &str) -> ApiResult<Job>;
    async fn cancel_job(&self, id: &str, who: &str) -> ApiResult<Job>;
    async fn set_card(&self, card: &str, state: &str, reason: &str, who: &str) -> ApiResult<Job>;
    async fn release_lease(&self, key: &str, who: &str) -> ApiResult<()>;
    async fn edit_route(&self, name: &str, edit: &RouteEdit, who: &str) -> ApiResult<RouteView>;

    /// Write an audit row for something done outside the backend (a sign-in, an admin key, a
    /// `router.json` save; D151). `target`: what it was done to.
    #[allow(clippy::too_many_arguments)]
    fn record(
        &self,
        _who: &str,
        _action: &str,
        _target: &str,
        _old_rev: &str,
        _new_rev: &str,
        _diff: &str,
        _reason: &str,
    ) {
    }
}

/// The control plane as the HTTP side sees it.
pub struct ControlPlane {
    pub access: Access,
    pub backend: Arc<dyn ControlBackend>,
}

fn json_resp(status: u16, v: &Value) -> Response {
    let mut r = (
        StatusCode::from_u16(status).unwrap_or(StatusCode::OK),
        axum::Json(v.clone()),
    )
        .into_response();
    r.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    r
}

fn err(status: u16, msg: &str) -> Response {
    json_resp(status, &json!({"error": msg}))
}

fn api_err(e: ApiError) -> Response {
    let mut v = json!({"error": e.message});
    if let (Some(extra), Some(o)) = (e.extra, v.as_object_mut())
        && let Some(x) = extra.as_object()
    {
        for (k, val) in x {
            o.insert(k.clone(), val.clone());
        }
    }
    json_resp(e.status, &v)
}

fn ok<T: serde::Serialize>(v: &T) -> Response {
    json_resp(200, &serde_json::to_value(v).unwrap_or(Value::Null))
}

fn query(path_q: &str) -> Vec<(String, String)> {
    path_q
        .split_once('?')
        .map(|(_, q)| q)
        .unwrap_or("")
        .split('&')
        .filter(|s| !s.is_empty())
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (pct_decode(k), pct_decode(v))
        })
        .collect()
}

fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                match std::str::from_utf8(&b[i + 1..i + 3])
                    .ok()
                    .and_then(|h| u8::from_str_radix(h, 16).ok())
                {
                    Some(x) => {
                        out.push(x);
                        i += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn q<'a>(qs: &'a [(String, String)], k: &str) -> Option<&'a str> {
    qs.iter().find(|(a, _)| a == k).map(|(_, v)| v.as_str())
}

/// Which permission a call needs.
fn needs(method: &Method, sub: &str) -> Perm {
    let read = *method == Method::GET || *method == Method::HEAD;
    match sub {
        "plan" => Perm::Switch,
        "switch" => Perm::Switch,
        s if read && (s == "state" || s == "events" || s == "jobs" || s.starts_with("jobs/")) => {
            Perm::ReadControl
        }
        _ if read => Perm::ReadControl,
        _ => Perm::EditControl,
    }
}

/// Reads that hold config text or the audit: admins only, even for `read` service keys.
/// `leases` and `calendar/now` (what holds or blocks now) are open to `read` keys (HA).
fn admin_only_read(sub: &str) -> bool {
    let first = sub.split('/').next().unwrap_or("");
    first == "config"
        || first == "audit"
        || first == "routes"
        || first == "admin-keys"
        || first == "gateway"
        || first == "inventory"
        || first == "bridges"
        || first == "hf"
        || first == "downloads"
        || first == "catalog"
        || first == "engine-store"
        || (first == "calendar" && sub != "calendar/now")
}

/// Who is calling `/admin/api/<sub>` and may they? `Err` is the answer to send (401, 403, 503).
/// `whoami` is answered here too (anyone may ask).
#[allow(clippy::result_large_err)] // the error is the response itself, sent once
pub async fn authorize(
    cp: &ControlPlane,
    method: &Method,
    path: &str,
    headers: &HeaderMap,
    peer: Option<IpAddr>,
) -> Result<Principal, Response> {
    let principal = match cp
        .access
        .authenticate(headers, peer, method.as_str(), path)
        .await
    {
        Ok(p) => p,
        Err(AuthError::Unauthorized(m)) => return Err(err(401, m)),
        Err(AuthError::Unavailable) => {
            return Err(err(
                503,
                "the accounts server can't vouch for this key right now",
            ));
        }
    };
    let sub = path
        .strip_prefix("/admin/api")
        .unwrap_or("")
        .trim_start_matches('/');
    if sub == "whoami" {
        return Err(ok(&principal.to_json()));
    }
    if matches!(principal, Principal::Anonymous { .. }) {
        return Err(err(401, "an admin key or an admin session is needed"));
    }
    let perm = needs(method, sub);
    let allowed =
        cp.access.allows(&principal, perm) && (principal.is_admin() || !admin_only_read(sub));
    if !allowed {
        return Err(err(403, "only an admin can do that"));
    }
    Ok(principal)
}

/// Handle `/admin/api/<sub>[?query]`. `peer`: the TCP peer.
pub async fn handle(
    cp: &ControlPlane,
    method: &Method,
    path_q: &str,
    headers: &HeaderMap,
    peer: Option<IpAddr>,
    raw: &Bytes,
) -> Response {
    let path = path_q.split('?').next().unwrap_or("");
    let principal = match authorize(cp, method, path, headers, peer).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    let sub = path
        .strip_prefix("/admin/api")
        .unwrap_or("")
        .trim_start_matches('/');
    let who = principal.label();
    let qs = query(path_q);
    let body: Value = if raw.is_empty() {
        Value::Null
    } else {
        match serde_json::from_slice(raw) {
            Ok(v) => v,
            Err(_) => return err(400, "the body must be JSON"),
        }
    };
    let b = cp.backend.clone();
    let s = |v: &Value, k: &str| v[k].as_str().unwrap_or("").to_string();
    let parts: Vec<&str> = sub.split('/').collect();
    match (method.clone(), parts.as_slice()) {
        (Method::GET, ["state"]) => ok(&b.state()),
        (Method::GET, ["events"]) => events(b),
        (Method::GET, ["config"]) => {
            let doc = b.config();
            let mut r = ok(&doc);
            if let Ok(v) = header::HeaderValue::from_str(&format!("\"{}\"", doc.rev)) {
                r.headers_mut().insert(header::ETAG, v);
            }
            r
        }
        (Method::POST, ["config", "check"]) => ok(&b.check_config(&s(&body, "text")).await),
        // D151: the config as data (the forms), and form edits applied to the text.
        (Method::GET, ["config", "json"]) => {
            let doc = b.config();
            ok(&form::config_json(&doc))
        }
        (Method::POST, ["config", "edit"]) => {
            let base = match body.get("text").and_then(Value::as_str) {
                Some(t) => t.to_string(),
                None => b.config().text,
            };
            let edits = match form::parse_edits(&body) {
                Ok(e) => e,
                Err(m) => return err(400, &m),
            };
            match form::apply_edits(&base, &edits) {
                Ok(text) => {
                    let check = b.check_config(&text).await;
                    ok(&json!({"text": text, "check": check}))
                }
                Err(m) => err(422, &m),
            }
        }
        (Method::GET, ["admin-keys"]) => admin_keys_list(cp),
        (Method::POST, ["admin-keys"]) => {
            let Some(keys) = &cp.access.local_keys else {
                return err(404, "this router keeps no admin keys of its own");
            };
            match keys.create(&s(&body, "name"), &who) {
                Ok(k) => {
                    b.record(&who, "admin-key.create", &k.info.name, "", "", "", "");
                    ok(&k)
                }
                Err(m) => err(422, &m),
            }
        }
        (Method::DELETE, ["admin-keys", id]) => {
            let Some(keys) = &cp.access.local_keys else {
                return err(404, "this router keeps no admin keys of its own");
            };
            match keys.revoke(id) {
                Ok(name) => {
                    if let Ok(n) = id.parse::<i64>() {
                        cp.access.sessions.remove_local_id(n);
                    }
                    b.record(&who, "admin-key.revoke", &name, "", "", "", "");
                    ok(&json!({"ok": true}))
                }
                Err(m) => err(404, &m),
            }
        }
        (Method::PUT, ["config"]) => {
            let if_match = headers
                .get(header::IF_MATCH)
                .and_then(|v| v.to_str().ok())
                .map(|v| v.trim_matches('"').to_string());
            let Some(rev) = if_match else {
                return err(428, "send If-Match with the rev you edited");
            };
            match b
                .save_config(&s(&body, "text"), &rev, &s(&body, "reason"), &who)
                .await
            {
                Ok(r) => ok(&r),
                Err(e) => api_err(e),
            }
        }
        (Method::GET, ["plan"]) => {
            let (Some(server), Some(layout)) = (q(&qs, "server"), q(&qs, "layout")) else {
                return err(400, "plan needs server and layout");
            };
            match b.plan(server, layout).await {
                Ok(p) => ok(&p),
                Err(e) => api_err(e),
            }
        }
        (Method::POST, ["switch"]) => {
            match b
                .switch(&s(&body, "server"), &s(&body, "layout"), &who)
                .await
            {
                Ok(j) => json_resp(202, &serde_json::to_value(j).unwrap_or_default()),
                Err(e) => api_err(e),
            }
        }
        (Method::GET, ["jobs"]) => {
            let limit = q(&qs, "limit")
                .and_then(|l| l.parse().ok())
                .unwrap_or(50usize)
                .min(500);
            ok(&b.jobs(limit))
        }
        (Method::GET, ["jobs", id]) => match b.job(id) {
            Some(j) => ok(&j),
            None => err(404, "no such job"),
        },
        (Method::POST, ["jobs", id, "cancel"]) => match b.cancel_job(id, &who).await {
            Ok(j) => ok(&j),
            Err(e) => api_err(e),
        },
        (
            Method::POST,
            [
                "cards",
                card,
                action @ ("offline" | "online" | "exclude" | "include"),
            ],
        ) => match b.set_card(card, action, &s(&body, "reason"), &who).await {
            Ok(j) => json_resp(202, &serde_json::to_value(j).unwrap_or_default()),
            Err(e) => api_err(e),
        },
        (Method::GET, ["audit"]) => {
            let limit = q(&qs, "limit")
                .and_then(|l| l.parse().ok())
                .unwrap_or(100usize)
                .min(1000);
            let before = q(&qs, "before").and_then(|l| l.parse().ok());
            ok(&b.audit(limit, before))
        }
        (Method::GET, ["leases"]) => ok(&b.leases()),
        (Method::DELETE, ["leases", key]) => match b.release_lease(&pct_decode(key), &who).await {
            Ok(()) => ok(&json!({"ok": true})),
            Err(e) => api_err(e),
        },
        (Method::GET, ["routes"]) => ok(&b.routes()),
        (Method::PATCH, ["routes", name]) => {
            let Ok(edit) = serde_json::from_value::<RouteEdit>(body) else {
                return err(
                    400,
                    "expected {cold, fallback, access, hidden, allowUserSwitch}",
                );
            };
            match b.edit_route(&pct_decode(name), &edit, &who).await {
                Ok(r) => ok(&r),
                Err(e) => api_err(e),
            }
        }
        // D152: the inventory, engine capabilities, Hugging Face and downloads.
        (
            m,
            [
                first @ ("inventory" | "hf" | "downloads" | "catalog"),
                rest @ ..,
            ],
        ) => {
            match crate::control::downloads::installed_for(std::path::Path::new(&b.config().path)) {
                Some(svc) => inventory_api(&svc, m, first, rest, &qs, &body, &who).await,
                None => err(503, "the inventory needs the router started with --control"),
            }
        }
        // D153: the engine store and the package wizard.
        (m, ["engine-store", rest @ ..]) => {
            match crate::control::downloads::installed_for(std::path::Path::new(&b.config().path)) {
                Some(svc) => {
                    if m == Method::GET
                        && let ["my", id, "export"] = rest
                    {
                        return match crate::control::engine_store::export_my(&svc.state_dir, id) {
                            Ok(bytes) => {
                                let mut r = (StatusCode::OK, bytes).into_response();
                                let h = r.headers_mut();
                                h.insert(
                                    header::CONTENT_TYPE,
                                    header::HeaderValue::from_static("application/zip"),
                                );
                                if let Ok(v) = header::HeaderValue::from_str(&format!(
                                    "attachment; filename=\"{id}.zip\""
                                )) {
                                    h.insert(header::CONTENT_DISPOSITION, v);
                                }
                                r
                            }
                            Err(e) => err(404, &e),
                        };
                    }
                    let (status, v) =
                        crate::control::engine_store::api(&svc, m.as_str(), rest, &qs, &body, &who)
                            .await;
                    json_resp(status, &v)
                }
                None => err(
                    503,
                    "the engine store needs the router started with --control",
                ),
            }
        }
        // A bridge's own inventory, through the bridge (MACHINE.md 8).
        (m, ["bridges", id, "machine", rest @ ..]) => match crate::bridges::installed() {
            Some(hub) => {
                let audit = |action: &str, target: &str| {
                    b.record(&who, action, target, "", "", "", "");
                };
                let if_match = headers
                    .get(header::IF_MATCH)
                    .and_then(|v| v.to_str().ok())
                    .map(|v| v.trim_matches('"').to_string());
                let query = path_q.split_once('?').map(|x| x.1).unwrap_or("");
                let (status, v) = crate::bridges::api::machine(
                    &hub,
                    id,
                    &m,
                    &rest.join("/"),
                    query,
                    &body,
                    if_match.as_deref(),
                    &who,
                    &audit,
                )
                .await;
                json_resp(status, &v)
            }
            None => err(503, "bridges need the router started with --control"),
        },
        // Bridges (ROUTER-MEDIA-PLAN.md 13): tokens, settings, commands, test requests.
        (m, ["bridges", rest @ ..]) => match crate::bridges::installed() {
            Some(hub) => {
                let audit = |action: &str, target: &str| {
                    b.record(&who, action, target, "", "", "", "");
                };
                let (status, v) = crate::bridges::api::handle(&hub, &m, rest, &body, &audit).await;
                json_resp(status, &v)
            }
            None => err(503, "bridges need the router started with --control"),
        },
        (m, ["calendar", ..]) => match crate::calendar::installed() {
            Some(svc) => {
                let (status, v) =
                    crate::calendar::api::handle(&svc, m.as_str(), &parts, &qs, &body, &who);
                json_resp(status, &v)
            }
            None => err(404, "the calendar is off (calendar.enabled = false)"),
        },
        _ => err(404, "no such admin call"),
    }
}

/// `/admin/api/{inventory,catalog,hf,downloads}/…` (D152).
async fn inventory_api(
    svc: &Arc<crate::control::downloads::Service>,
    m: Method,
    first: &str,
    rest: &[&str],
    qs: &[(String, String)],
    body: &Value,
    who: &str,
) -> Response {
    use crate::control::{caps, downloads::StartReq, inventory, schema::ModelKind};
    let s = |k: &str| body[k].as_str().unwrap_or("").to_string();
    let c = match svc.config() {
        Ok(c) => c,
        Err(e) => return err(409, &format!("router.toml does not load: {e}")),
    };
    let fail = |e: String| err(422, &e);
    match (m, first, rest) {
        (Method::GET, "inventory", []) => {
            // router.toml with the connected bridges' servers, cards and engines merged in.
            let mut v = inventory::view(&crate::bridges::effective(&c));
            v["restartNeeded"] = json!(svc.restart_needed(&c));
            v["hf"] = svc.token_view(&c);
            ok(&v)
        }
        (Method::GET, "catalog", []) => ok(&caps::catalog_view()),
        (Method::POST, "inventory", ["detect"]) => match svc.detect(&s("server")).await {
            Ok(v) => ok(&v),
            Err(e) => fail(e),
        },
        (Method::POST, "inventory", ["gguf"]) => match svc.gguf(&s("server"), &s("path")).await {
            Ok(v) => ok(&v),
            Err(e) => fail(e),
        },
        (Method::GET, "inventory", ["browse"]) => {
            let off = q(qs, "offset").and_then(|x| x.parse().ok()).unwrap_or(0);
            let lim = q(qs, "limit").and_then(|x| x.parse().ok()).unwrap_or(200);
            browse_answer(
                svc.browse(
                    q(qs, "server").unwrap_or(""),
                    q(qs, "path").unwrap_or("/"),
                    off,
                    lim,
                )
                .await,
            )
        }
        (Method::GET, "inventory", ["stat"]) => browse_answer(
            svc.path_stat(q(qs, "server").unwrap_or(""), q(qs, "path").unwrap_or(""))
                .await,
        ),
        (Method::POST, "inventory", ["mkdir"]) => {
            browse_answer(svc.mkdir(&s("server"), &s("path"), who).await)
        }
        (Method::POST, "inventory", ["place"]) => {
            // where a model would run (the forms' preview): {model}
            let pin = inventory::Pin::default();
            ok(&json!({"placement": inventory::place(&c, &s("model"), &pin)}))
        }
        (Method::GET, "hf", ["search"]) => {
            let kind = q(qs, "kind").and_then(ModelKind::parse);
            match svc.hf(&c).search(q(qs, "q").unwrap_or(""), kind).await {
                Ok(v) => ok(&v),
                Err(e) => err(502, &e),
            }
        }
        (Method::GET, "hf", ["model"]) => {
            let repo = q(qs, "repo").unwrap_or("");
            match svc.hf(&c).model(repo).await {
                Ok(mut v) => {
                    v["fit"] = svc.fit(&c, &v);
                    ok(&v)
                }
                Err(e) => err(502, &e),
            }
        }
        (Method::GET, "hf", ["token"]) => ok(&svc.token_view(&c)),
        (Method::PUT, "hf", ["token"]) => match svc.set_token(&s("token")) {
            Ok(()) => {
                svc.backend
                    .record(who, "hf-token.set", "hf", "", "", "", "");
                ok(&svc.token_view(&c))
            }
            Err(e) => fail(e),
        },
        (Method::DELETE, "hf", ["token"]) => {
            let _ = svc.set_token("");
            svc.backend
                .record(who, "hf-token.remove", "hf", "", "", "", "");
            ok(&svc.token_view(&c))
        }
        (Method::GET, "downloads", []) => ok(&svc.list()),
        (Method::POST, "downloads", []) => match serde_json::from_value::<StartReq>(body.clone()) {
            Ok(r) => match svc.start(r, who) {
                Ok(d) => json_resp(202, &serde_json::to_value(d).unwrap_or_default()),
                Err(e) => fail(e),
            },
            Err(_) => err(
                400,
                "expected {server, repo, files: [{path, size}], model?, gated?}",
            ),
        },
        (Method::POST, "downloads", [id, "cancel"]) => match svc.cancel(id, who) {
            Ok(d) => ok(&d),
            Err(e) => err(404, &e),
        },
        (Method::POST, "downloads", [id, "resume"]) => match svc.resume(id, who) {
            Ok(d) => json_resp(202, &serde_json::to_value(d).unwrap_or_default()),
            Err(e) => fail(e),
        },
        (Method::DELETE, "downloads", [id]) => match svc.remove(id) {
            Ok(()) => ok(&json!({"ok": true})),
            Err(e) => fail(e),
        },
        _ => err(404, "no such admin call"),
    }
}

/// A folder-browsing answer: the listing, a refusal (422), or "not enabled here" (501 with
/// `enabled: false`, so the page keeps a plain text box).
fn browse_answer(r: Result<Value, Option<String>>) -> Response {
    match r {
        Ok(v) => ok(&v),
        Err(None) => json_resp(
            501,
            &json!({"enabled": false, "error": "Folder browsing isn't enabled on this server (its hostctl allowlist needs list-dir, path-stat and mkdir)."}),
        ),
        Err(Some(e)) => err(422, &e),
    }
}

fn admin_keys_list(cp: &ControlPlane) -> Response {
    match &cp.access.local_keys {
        None => ok(&json!({"keys": [], "local": false})),
        Some(k) => match k.list() {
            Ok(keys) => ok(&json!({"keys": keys, "local": true})),
            Err(m) => err(500, &m),
        },
    }
}

/// The SSE stream: a `state` event first, then whatever the backend publishes, with a
/// keepalive comment every 15 s. A slow reader that falls behind gets a fresh `state`.
fn events(b: Arc<dyn ControlBackend>) -> Response {
    let rx = b.subscribe();
    let first = Event {
        kind: "state",
        data: serde_json::to_value(b.state()).unwrap_or_default(),
    };
    let stream =
        futures_util::stream::unfold((rx, Some(first), b), |(mut rx, first, b)| async move {
            if let Some(e) = first {
                return Some((Ok::<_, Infallible>(sse(&e)), (rx, None, b)));
            }
            let next = tokio::time::timeout(Duration::from_secs(15), rx.recv()).await;
            let chunk = match next {
                Err(_) => Bytes::from_static(b": keepalive\n\n"),
                Ok(Ok(e)) => sse(&e),
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => sse(&Event {
                    kind: "state",
                    data: serde_json::to_value(b.state()).unwrap_or_default(),
                }),
                Ok(Err(broadcast::error::RecvError::Closed)) => return None,
            };
            Some((Ok(chunk), (rx, None, b)))
        });
    Response::builder()
        .status(200)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-store")
        .header("x-accel-buffering", "no")
        .body(Body::from_stream(stream))
        .unwrap()
}

pub fn sse(e: &Event) -> Bytes {
    Bytes::from(format!(
        "event: {}\ndata: {}\n\n",
        e.kind,
        serde_json::to_string(&e.data).unwrap_or_else(|_| "null".into())
    ))
}

/// Prometheus lines for the control plane (12.1), appended to `/metrics`.
pub fn metrics_text(cp: &ControlPlane) -> String {
    let st = cp.backend.state();
    let mut out = String::new();
    let mut gauge = |name: &str, help: &str, rows: Vec<(String, f64)>| {
        if rows.is_empty() {
            return;
        }
        out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} gauge\n"));
        for (labels, v) in rows {
            out.push_str(&format!("{name}{labels} {v}\n"));
        }
    };
    let lbl = |pairs: &[(&str, &str)]| {
        let inner: Vec<String> = pairs
            .iter()
            .map(|(k, v)| format!("{k}=\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\"")))
            .collect();
        format!("{{{}}}", inner.join(","))
    };
    let cards: Vec<&CardView> = st.servers.iter().flat_map(|s| s.cards.iter()).collect();
    let tel = |f: &dyn Fn(&Telemetry) -> f64| -> Vec<(String, f64)> {
        cards
            .iter()
            .filter_map(|c| {
                c.telemetry.as_ref().map(|t| {
                    (
                        lbl(&[
                            ("server", &c.server),
                            ("card", &c.id),
                            ("uuid", &c.gpu_uuid),
                        ]),
                        f(t),
                    )
                })
            })
            .collect()
    };
    gauge(
        "abhoth_gpu_memory_used_mib",
        "GPU memory used (MiB).",
        tel(&|t| t.mem_used_mib as f64),
    );
    gauge(
        "abhoth_gpu_memory_total_mib",
        "GPU memory (MiB).",
        tel(&|t| t.mem_total_mib as f64),
    );
    gauge(
        "abhoth_gpu_utilization_ratio",
        "GPU utilisation (0-1).",
        tel(&|t| t.util_pct as f64 / 100.0),
    );
    gauge(
        "abhoth_gpu_temperature_celsius",
        "GPU temperature.",
        tel(&|t| t.temp_c as f64),
    );
    gauge(
        "abhoth_gpu_memory_temperature_celsius",
        "GPU memory temperature.",
        cards
            .iter()
            .filter_map(|c| {
                let t = c.telemetry.as_ref()?;
                Some((
                    lbl(&[
                        ("server", &c.server),
                        ("card", &c.id),
                        ("uuid", &c.gpu_uuid),
                    ]),
                    t.mem_temp_c? as f64,
                ))
            })
            .collect(),
    );
    gauge(
        "abhoth_gpu_power_watts",
        "GPU power draw.",
        tel(&|t| t.power_w),
    );
    gauge(
        "abhoth_gpu_sm_clock_mhz",
        "SM clock.",
        tel(&|t| t.sm_mhz as f64),
    );
    gauge(
        "abhoth_gpu_sm_clock_max_mhz",
        "Maximum SM clock.",
        tel(&|t| t.sm_max_mhz as f64),
    );
    gauge(
        "abhoth_gpu_telemetry_age_seconds",
        "Age of the last sample.",
        tel(&|t| t.age_ms as f64 / 1000.0),
    );
    gauge(
        "abhoth_gpu_clock_ok",
        "1 when the card's clocks meet its expectation (6.6).",
        tel(&|t| if t.clock_check == "ok" { 1.0 } else { 0.0 }),
    );
    gauge(
        "abhoth_engine_state",
        "1 for the engine's current lifecycle state.",
        cards
            .iter()
            .filter_map(|c| {
                let e = c.engine.as_ref()?;
                Some((
                    lbl(&[
                        ("server", &c.server),
                        ("card", &c.id),
                        ("engine", &e.engine),
                        ("state", &e.state),
                    ]),
                    1.0,
                ))
            })
            .collect(),
    );
    gauge(
        "abhoth_card_lanes",
        "Lanes of the engine on the card.",
        cards
            .iter()
            .map(|c| {
                (
                    lbl(&[("server", &c.server), ("card", &c.id)]),
                    c.lanes as f64,
                )
            })
            .collect(),
    );
    gauge(
        "abhoth_card_online",
        "1 when the card is online (not offline, excluded or missing).",
        cards
            .iter()
            .filter(|c| c.managed)
            .map(|c| {
                (
                    lbl(&[("server", &c.server), ("card", &c.id)]),
                    if c.admin_state == "online" { 1.0 } else { 0.0 },
                )
            })
            .collect(),
    );
    gauge(
        "abhoth_server_up",
        "1 when the server is reachable for commands (SSH).",
        st.servers
            .iter()
            .map(|s| {
                (
                    lbl(&[("server", &s.id)]),
                    if s.reachability == "ok" { 1.0 } else { 0.0 },
                )
            })
            .collect(),
    );
    gauge(
        "abhoth_layout_drift",
        "1 when the active layout's definition differs from what runs.",
        st.servers
            .iter()
            .map(|s| (lbl(&[("server", &s.id)]), if s.drift { 1.0 } else { 0.0 }))
            .collect(),
    );
    gauge(
        "abhoth_layout_active_info",
        "The active layout per server.",
        st.servers
            .iter()
            .filter_map(|s| {
                Some((
                    lbl(&[("server", &s.id), ("layout", s.active_layout.as_deref()?)]),
                    1.0,
                ))
            })
            .collect(),
    );
    gauge(
        "abhoth_guard_active",
        "1 while the guard blocks something.",
        st.guards
            .iter()
            .map(|g| (lbl(&[("guard", &g.id)]), if g.active { 1.0 } else { 0.0 }))
            .collect(),
    );
    gauge(
        "abhoth_guard_stale",
        "1 while the guard's source has not reported for 45 s.",
        st.guards
            .iter()
            .map(|g| (lbl(&[("guard", &g.id)]), if g.stale { 1.0 } else { 0.0 }))
            .collect(),
    );
    gauge(
        "abhoth_job_running",
        "1 while a job runs, by server and kind.",
        st.job
            .iter()
            .map(|j| (lbl(&[("server", &j.server), ("kind", &j.kind)]), 1.0))
            .collect(),
    );
    gauge(
        "abhoth_config_rev_info",
        "The loaded config's rev.",
        vec![(lbl(&[("rev", &st.rev)]), 1.0)],
    );
    gauge(
        "abhoth_config_error",
        "1 while the config on disk does not load.",
        vec![(
            String::new(),
            if st.config_error.is_some() { 1.0 } else { 0.0 },
        )],
    );
    if let Some(k) = &cp.access.keys {
        use std::sync::atomic::Ordering::Relaxed;
        let s = &k.stats;
        out.push_str("# HELP abhoth_key_checks_total Key checks by outcome.\n# TYPE abhoth_key_checks_total counter\n");
        for (o, v) in [
            ("cache_hit", s.hits.load(Relaxed)),
            ("introspected", s.misses.load(Relaxed)),
            ("stale_served", s.stale_served.load(Relaxed)),
            ("accounts_unreachable", s.failures.load(Relaxed)),
            ("invalid", s.invalid.load(Relaxed)),
        ] {
            out.push_str(&format!("abhoth_key_checks_total{{outcome=\"{o}\"}} {v}\n"));
        }
    }
    out
}

#[cfg(test)]
mod tests;

/// Resolve a `secret:<id>` reference from `[secrets]` (file or env); never logged.
pub fn resolve_secret(c: &crate::control::schema::ControlConfig, r: &str) -> Option<Vec<u8>> {
    let id = r.strip_prefix("secret:")?;
    let s = c.secrets.get(id)?;
    if let Some(f) = &s.file {
        return std::fs::read(f).ok();
    }
    s.env
        .as_ref()
        .and_then(|e| std::env::var(e).ok())
        .map(String::into_bytes)
}

/// Say once at start-up how to get the first admin key when there is none (D151).
pub fn bootstrap_hint(
    keys: &crate::access::LocalKeys,
    c: &crate::control::schema::ControlConfig,
    main: &std::path::Path,
) {
    if keys.active_count() > 0 {
        return;
    }
    let linked = if c.access.accounts_url.is_some() {
        " (or sign in with an admin key from the linked Abhoth web server)"
    } else {
        ""
    };
    eprintln!(
        "abhoth router: no router admin keys yet; make one with `abhoth router admin-key create <name> --control {}` and sign in at /admin/{linked}",
        main.display()
    );
}

/// The admin pages without a control plane (`control.enabled = false`, D151): `router.toml`
/// editing and checking, `router.json` forms, admin keys and sign-in; switching answers 501.
pub fn install_admin_only(
    st: &Arc<crate::state::AppState>,
    main: &std::path::Path,
    c: &crate::control::schema::ControlConfig,
) -> Result<String, String> {
    let state_dir = std::path::PathBuf::from(&c.router.state_dir);
    std::fs::create_dir_all(&state_dir).map_err(|e| format!("{}: {e}", state_dir.display()))?;
    let store = Arc::new(crate::control::store::Store::open(
        &state_dir.join("router.db"),
    )?);
    let keys = crate::access::LocalKeys::open(store.clone())?;
    let token = c
        .access
        .accounts_token
        .as_deref()
        .and_then(|r| resolve_secret(c, r));
    if c.access.accounts_token.is_some() && token.is_none() {
        return Err("access.accounts_token: the secret could not be read".into());
    }
    bootstrap_hint(&keys, c, main);
    let access = Access::new(c.access.clone(), token)
        .with_local_keys(keys)
        .with_sso(&state_dir);
    let backend = live::ConfigBackend::new(main, &state_dir, store);
    crate::control::downloads::install(main, &state_dir, backend.clone());
    st.admin_only
        .set(Arc::new(ControlPlane { access, backend }))
        .map_err(|_| "the admin pages are already on".to_string())?;
    Ok("control plane off (control.enabled = false); admin pages on at /admin/".into())
}

/// Turn the admin API on from `router.toml` when `control.enabled` (`--control FILE`).
/// Returns what it did, for the start-up line.
pub fn install_from_toml(
    st: &crate::state::AppState,
    main: &std::path::Path,
) -> Result<String, String> {
    let l = crate::control::load::load(main).map_err(|issues| {
        issues
            .iter()
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    })?;
    let c = &l.config;
    if !c.control.enabled {
        return Ok("control plane off (control.enabled = false)".into());
    }
    let state_dir = std::path::PathBuf::from(&c.router.state_dir);
    std::fs::create_dir_all(&state_dir).map_err(|e| format!("{}: {e}", state_dir.display()))?;
    let store = Arc::new(crate::control::store::Store::open(
        &state_dir.join("router.db"),
    )?);
    let token = c
        .access
        .accounts_token
        .as_deref()
        .and_then(|r| resolve_secret(c, r));
    if c.access.accounts_token.is_some() && token.is_none() {
        return Err("access.accounts_token: the secret could not be read".into());
    }
    let access = Access::new(c.access.clone(), token)
        .with_local_keys(crate::access::LocalKeys::open(store.clone())?)
        .with_sso(&state_dir);
    // The Supervisor's calendar (SUPERVISOR.md 8), with its own flag.
    let calendar = crate::calendar::start(c, main, &l.origin, store.clone())?;
    let backend = live::ConfigBackend::new(main, &state_dir, store);
    st.control
        .set(Arc::new(ControlPlane { access, backend }))
        .map_err(|_| "the control plane is already installed".to_string())?;
    Ok(format!("control plane on (rev {}){calendar}", l.rev))
}
