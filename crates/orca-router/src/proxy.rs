//! The request path (ROUTER.md 4.2): pick a card, rewrite, send, fail over once before any
//! byte reached the client, then stream the upstream body through untouched.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Instant, SystemTime};

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::Stream;
use serde_json::{Map, Value, json};

use crate::config::{Deployment, Snapshot};
use crate::placement::{self, Placement};
use crate::rewrite;
use crate::state::{AppState, CardGuard};

/// Who asked, for the log.
#[derive(Debug, Clone, Default)]
pub struct RequestCtx {
    pub id: u64,
    pub client: String,
    pub started: Option<Instant>,
    /// The TCP peer (never a forwarded header): access rules use this.
    pub peer: Option<std::net::IpAddr>,
}

/// LiteLLM's error shape: `{"error": {"message", "type", "param", "code"}}`, code = the status.
pub fn error(status: u16, message: &str, etype: &str, param: Option<&str>) -> Response {
    let body = json!({"error": {"message": message, "type": etype, "param": param,
                                "code": status.to_string()}});
    json_response(status, &body)
}

pub fn json_response(status: u16, body: &Value) -> Response {
    let mut r = (
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        body.to_string(),
    )
        .into_response();
    r.headers_mut()
        .insert("content-type", HeaderValue::from_static("application/json"));
    r
}

fn retry_after(mut r: Response, secs: u64) -> Response {
    if let Ok(v) = HeaderValue::from_str(&secs.to_string()) {
        r.headers_mut().insert("retry-after", v);
    }
    r
}

const HOP: [&str; 9] = [
    "connection",
    "keep-alive",
    "transfer-encoding",
    "content-length",
    "te",
    "trailer",
    "upgrade",
    "proxy-authenticate",
    "proxy-authorization",
];

/// Outcome of one upstream attempt.
enum Attempt {
    Sent(reqwest::Response),
    /// Connect error, timeout before headers, 502 or 503: worth another card.
    Failed(String, Option<reqwest::Response>),
}

async fn send(
    st: &AppState,
    snap: &Snapshot,
    method: Method,
    url: &str,
    key: Option<&str>,
    headers: &HeaderMap,
    body: Bytes,
) -> Attempt {
    let mut rb = st
        .http
        .request(method, url)
        .timeout(snap.timeout())
        .body(body);
    for name in ["content-type", "accept", "x-request-id"] {
        if let Some(v) = headers.get(name) {
            rb = rb.header(name, v.clone());
        }
    }
    if let Some(k) = key {
        rb = rb.header("authorization", format!("Bearer {k}"));
    }
    match rb.send().await {
        Ok(r) if matches!(r.status().as_u16(), 502 | 503) => {
            Attempt::Failed(format!("HTTP {}", r.status().as_u16()), Some(r))
        }
        Ok(r) => Attempt::Sent(r),
        Err(e) => Attempt::Failed(short_error(&e), None),
    }
}

fn short_error(e: &reqwest::Error) -> String {
    let kind = if e.is_connect() {
        "connect error"
    } else if e.is_timeout() {
        "timeout"
    } else {
        "request error"
    };
    let mut s = format!("{kind}: {e}");
    s.truncate(300);
    s
}

/// Fields for the request log, filled as the request goes.
#[derive(Debug, Clone, Default)]
pub struct LogRec {
    pub fields: Map<String, Value>,
}

impl LogRec {
    pub fn set(&mut self, k: &str, v: impl Into<Value>) {
        self.fields.insert(k.into(), v.into());
    }
}

/// POST /v1/chat/completions.
pub async fn chat(st: Arc<AppState>, ctx: RequestCtx, headers: HeaderMap, raw: Bytes) -> Response {
    let snap = st.snapshot();
    let started = ctx.started.unwrap_or_else(Instant::now);
    let mut rec = LogRec::default();
    rec.set(
        "at",
        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    );
    rec.set("id", ctx.id);
    rec.set("client", ctx.client.clone());
    rec.set("path", "/v1/chat/completions");
    let parsed: Result<Value, _> = serde_json::from_slice(&raw);
    let body = match parsed {
        Ok(Value::Object(m)) => m,
        Ok(_) => {
            return finish_error(
                &st,
                rec,
                started,
                "",
                400,
                "Invalid JSON payload: expected an object",
                "invalid_request_error",
                Some("request_body"),
            );
        }
        Err(e) => {
            return finish_error(
                &st,
                rec,
                started,
                "",
                400,
                &format!("Invalid JSON payload: {e}"),
                "invalid_request_error",
                Some("request_body"),
            );
        }
    };
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    rec.set("model", model.clone());
    let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    rec.set("stream", stream);
    let deps = snap.deployments_named(&model);
    if deps.is_empty() {
        let msg = format!(
            "/chat/completions: Invalid model name passed in model={model}. Call `/v1/models` to view available models for your key."
        );
        return finish_error(
            &st,
            rec,
            started,
            &model,
            400,
            &msg,
            "invalid_request_error",
            None,
        );
    }

    // on demand (phase 3): a name that runs nowhere is decided by the router itself when enabled
    let card_deps: Vec<(usize, &Deployment)> =
        deps.iter().filter(|(_, d)| !d.relay).cloned().collect();
    // The Claude engine's card is for `claude.access` callers only (admins by default): others
    // keep the route's other cards, or get 403 when it has none.
    let mut owned: Vec<Deployment> = card_deps.iter().map(|(_, d)| (*d).clone()).collect();
    if owned.iter().any(|d| {
        d.cards
            .iter()
            .any(|c| crate::claude::is_claude_card(&snap, c))
    }) && !crate::claude::allowed(&st, &snap, &headers, ctx.peer).await
    {
        for d in &mut owned {
            d.cards.retain(|c| !crate::claude::is_claude_card(&snap, c));
        }
        owned.retain(|d| !d.cards.is_empty());
        if owned.is_empty() {
            let msg = format!(
                "model={model} is limited to `{}` callers on this router",
                snap.config.claude.access
            );
            return finish_error(&st, rec, started, &model, 403, &msg, "forbidden", None);
        }
    }
    if card_deps.is_empty() {
        let (_, dep) = deps[0];
        if snap.config.on_demand.enabled {
            // the only place a ChatAdmission is made: a parsed chat body for a resolved route
            // (ROUTER-CONFIG.md 7.4); probes and other endpoints never get here
            let caller = if snap.config.on_demand.control_gate {
                crate::switch::gate::chat_caller(&st, &headers, ctx.peer).await
            } else {
                crate::guards::Caller::default()
            };
            let adm = crate::switch::gate::ChatAdmission::from_chat(&dep.name, &headers, caller);
            return crate::ondemand::handle(
                st.clone(),
                snap.clone(),
                ctx,
                headers,
                body,
                dep.clone(),
                adm,
                rec,
            )
            .await;
        }
        return relay(&st, &snap, &headers, &body, dep, rec, started).await;
    }

    forward_cards(&st, &snap, &headers, &body, &owned, &model, rec, started).await
}

/// Send a chat to the cards of `card_deps` (placement, failover, streaming back).
#[allow(clippy::too_many_arguments)]
pub async fn forward_cards(
    st: &Arc<AppState>,
    snap: &Arc<Snapshot>,
    headers: &HeaderMap,
    body: &Map<String, Value>,
    card_deps: &[Deployment],
    model: &str,
    mut rec: LogRec,
    started: Instant,
) -> Response {
    let (st, snap, headers, body) = (st.clone(), snap.clone(), headers.clone(), body);
    let model = model.to_string();
    // candidate cards in deployment order (deduplicated)
    let mut candidates: Vec<String> = Vec::new();
    for d in card_deps {
        for c in &d.cards {
            if !candidates.contains(c) {
                candidates.push(c.clone());
            }
        }
    }
    // Calendar reservations (SUPERVISOR.md 9.4): others stay off held cards; the event's own
    // key goes to its cards first.
    let mut mine: Vec<String> = Vec::new();
    if let Some(cal) = crate::calendar::installed() {
        use crate::calendar::reserve::ChatAccess;
        match cal.chat_access(crate::access::bearer(&headers), &model, &candidates) {
            ChatAccess::Denied(status, reason) => {
                return finish_error(
                    &st,
                    rec,
                    started,
                    &model,
                    status,
                    &reason,
                    "permission_error",
                    None,
                );
            }
            ChatAccess::Holds(holds) => {
                mine = holds
                    .iter()
                    .filter(|h| h.allowed)
                    .map(|h| h.card.clone())
                    .collect();
                let theirs: Vec<_> = holds.iter().filter(|h| !h.allowed).collect();
                if let Some(first) = theirs.first() {
                    let rest: Vec<String> = candidates
                        .iter()
                        .filter(|c| !theirs.iter().any(|h| &h.card == *c))
                        .cloned()
                        .collect();
                    rec.set("reserved", first.entry.clone());
                    if rest.is_empty() {
                        let wait = (first.until - crate::calendar::reserve::now()).max(1);
                        let mut resp = error(503, &first.message(), "service_unavailable", None);
                        if let Ok(v) = HeaderValue::from_str(&wait.to_string()) {
                            resp.headers_mut().insert("retry-after", v);
                        }
                        return finish_resp(&st, rec, started, &model, resp);
                    }
                    candidates = rest;
                }
            }
        }
    }
    let place = placement::place(&st, &snap, &headers, body, &candidates, &mut rec).await;
    let Placement { mut order, permit } = match place {
        Ok(p) => p,
        Err(resp) => return finish_resp(&st, rec, started, &model, *resp),
    };
    if !mine.is_empty() && permit.is_none() {
        // the event's reserved lanes first, then the free ones
        order.sort_by_key(|c| !mine.contains(c));
        rec.set("placement", "reserved");
    }
    let mut tried: Vec<String> = Vec::new();
    let max_attempts = 1 + snap.config.settings.failovers as usize;
    let mut last_err = String::new();
    while tried.len() < max_attempts {
        let eligible = st.eligible(&snap, &candidates, &tried);
        let card = order
            .iter()
            .find(|c| eligible.contains(c))
            .cloned()
            .or_else(|| st.pick(&snap, &eligible));
        let Some(card) = card else { break };
        let dep = card_deps
            .iter()
            .find(|d| d.cards.contains(&card))
            .expect("candidate card comes from a deployment");
        let up = &snap.config.upstreams[&card];
        let out = rewrite::for_deployment(body, dep, &snap.config.settings);
        let virtual_card = up.is_virtual();
        let guard = st.begin(&card);
        tried.push(card.clone());
        let attempt = if virtual_card {
            // the Claude engine: no HTTP hop; it keeps its own busy and rate-limit state
            match crate::claude::send(&st, &snap, &card, &out).await {
                Ok(r) => Attempt::Sent(r),
                Err(f) => {
                    let (why, r) = *f;
                    Attempt::Failed(why, r)
                }
            }
        } else {
            let bytes = Bytes::from(serde_json::to_vec(&Value::Object(out)).unwrap_or_default());
            let url = format!("{}/v1/chat/completions", up.url.trim_end_matches('/'));
            send(
                &st,
                &snap,
                Method::POST,
                &url,
                snap.key_for(&card),
                &headers,
                bytes,
            )
            .await
        };
        match attempt {
            Attempt::Sent(resp) => {
                rec.set("card", card.clone());
                rec.set("served", dep.served_id.clone());
                if tried.len() > 1 {
                    rec.set("failover", tried[..tried.len() - 1].join(","));
                }
                let preempt = preemptible(&mine, &card);
                return stream_back(
                    &st,
                    rec,
                    started,
                    &model,
                    resp,
                    guard,
                    permit,
                    &dep.served_id,
                    &card,
                    &up.url,
                    preempt,
                );
            }
            Attempt::Failed(why, resp) => {
                drop(guard);
                if !virtual_card {
                    st.cool(&card, snap.config.settings.cooldown_seconds);
                }
                tracing::warn!("router: card {card} failed for {model}: {why}");
                last_err = format!("card {card}: {why}");
                if tried.len() >= max_attempts || st.eligible(&snap, &candidates, &tried).is_empty()
                {
                    // nothing left: a 502/503 answer goes back as it came
                    if let Some(resp) = resp {
                        rec.set("card", card.clone());
                        rec.set("served", dep.served_id.clone());
                        let g = st.begin(&card);
                        return stream_back(
                            &st,
                            rec,
                            started,
                            &model,
                            resp,
                            g,
                            permit,
                            &dep.served_id,
                            &card,
                            &up.url,
                            preemptible(&mine, &card),
                        );
                    }
                } else {
                    st.metrics.failover();
                }
            }
        }
    }
    drop(permit);
    rec.set("error", last_err.clone());
    let msg = if last_err.is_empty() {
        format!("no upstream available for {model}")
    } else {
        format!("no upstream available for {model} ({last_err})")
    };
    let r = retry_after(error(503, &msg, "no_upstream", None), 10);
    finish_resp(&st, rec, started, &model, r)
}

async fn relay(
    st: &Arc<AppState>,
    snap: &Arc<Snapshot>,
    headers: &HeaderMap,
    body: &Map<String, Value>,
    dep: &Deployment,
    mut rec: LogRec,
    started: Instant,
) -> Response {
    let model = dep.name.clone();
    let Some(r) = &snap.config.relay else {
        return finish_error(
            st,
            rec,
            started,
            &model,
            503,
            "no on-demand relay configured",
            "no_upstream",
            None,
        );
    };
    let out = rewrite::for_deployment(body, dep, &snap.config.settings);
    let bytes = Bytes::from(serde_json::to_vec(&Value::Object(out)).unwrap_or_default());
    let url = format!("{}/chat/completions", r.url.trim_end_matches('/'));
    rec.set("card", "relay");
    rec.set("served", dep.served_id.clone());
    let guard = st.begin("relay");
    match send(
        st,
        snap,
        Method::POST,
        &url,
        r.key.as_deref(),
        headers,
        bytes,
    )
    .await
    {
        Attempt::Sent(resp) | Attempt::Failed(_, Some(resp)) => stream_back(
            st,
            rec,
            started,
            &model,
            resp,
            guard,
            None,
            &dep.served_id,
            "relay",
            &r.url,
            None,
        ),
        Attempt::Failed(why, None) => {
            drop(guard);
            finish_error(
                st,
                rec,
                started,
                &model,
                502,
                &format!("on-demand relay: {why}"),
                "no_upstream",
                None,
            )
        }
    }
}

/// qfn-lb compatible passthrough: `/<card>/...`, `/<engine>/...` or (compat listener) `/...`.
pub async fn lb(
    st: Arc<AppState>,
    ctx: RequestCtx,
    cards: Vec<String>,
    method: Method,
    rest: String,
    headers: HeaderMap,
    raw: Bytes,
) -> Response {
    let snap = st.snapshot();
    let started = ctx.started.unwrap_or_else(Instant::now);
    let mut rec = LogRec::default();
    rec.set(
        "at",
        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    );
    rec.set("id", ctx.id);
    rec.set("client", ctx.client.clone());
    rec.set("path", rest.clone());
    let mut tried = Vec::new();
    let max_attempts = 1 + snap.config.settings.failovers as usize;
    while tried.len() < max_attempts {
        let eligible = st.eligible(&snap, &cards, &tried);
        let Some(card) = st.pick(&snap, &eligible) else {
            break;
        };
        let up = &snap.config.upstreams[&card];
        let url = format!("{}{}", up.url.trim_end_matches('/'), rest);
        let guard = st.begin(&card);
        tried.push(card.clone());
        match send(
            &st,
            &snap,
            method.clone(),
            &url,
            snap.key_for(&card),
            &headers,
            raw.clone(),
        )
        .await
        {
            Attempt::Sent(resp) => {
                rec.set("card", card.clone());
                return stream_back(
                    &st,
                    rec,
                    started,
                    "",
                    resp,
                    guard,
                    None,
                    &up.served_id,
                    &card,
                    &up.url,
                    None,
                );
            }
            Attempt::Failed(why, resp) => {
                drop(guard);
                st.cool(&card, snap.config.settings.cooldown_seconds);
                if tried.len() >= max_attempts || st.eligible(&snap, &cards, &tried).is_empty() {
                    if let Some(resp) = resp {
                        rec.set("card", card.clone());
                        let g = st.begin(&card);
                        return stream_back(
                            &st,
                            rec,
                            started,
                            "",
                            resp,
                            g,
                            None,
                            &up.served_id,
                            &card,
                            &up.url,
                            None,
                        );
                    }
                    rec.set("error", why);
                } else {
                    st.metrics.failover();
                }
            }
        }
    }
    // as qfn-lb for a card that is off: 503 with a small JSON error
    let names: Vec<String> = cards.clone();
    let body = json!({"error": format!("card(s) {} not available", names.join(", "))});
    finish_resp(&st, rec, started, "", json_response(503, &body))
}

#[allow(clippy::too_many_arguments)]
fn stream_back(
    st: &Arc<AppState>,
    mut rec: LogRec,
    started: Instant,
    model: &str,
    resp: reqwest::Response,
    guard: CardGuard,
    permit: Option<placement::Permit>,
    served: &str,
    card: &str,
    upstream: &str,
    preempt: Option<tokio::sync::watch::Receiver<u64>>,
) -> Response {
    let status = resp.status().as_u16();
    rec.set("status", status);
    let ttfb = started.elapsed().as_secs_f64();
    rec.set("headersSeconds", round3(ttfb));
    let mut out = Response::builder().status(status);
    let h = out.headers_mut().expect("builder");
    for (k, v) in resp.headers() {
        if HOP.contains(&k.as_str()) {
            continue;
        }
        h.append(k.clone(), v.clone());
    }
    if !served.is_empty() {
        // LiteLLM's header carries its provider prefix ("openai/inkling-2-mini");
        // ServedModels::clean drops it, so clients read the same model from either header.
        for name in orca_core::client::served_models::HEADERS {
            let value = if name == "x-litellm-model-name" {
                format!("openai/{served}")
            } else {
                served.to_string()
            };
            if let Ok(v) = HeaderValue::from_str(&value) {
                h.insert(HeaderName::from_static(name), v);
            }
        }
    }
    if let Ok(v) = HeaderValue::from_str(card) {
        h.insert("x-abhoth-card", v);
    }
    if let Ok(v) = HeaderValue::from_str(upstream) {
        h.insert("x-abhoth-upstream", v);
    }
    let sse = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/event-stream"));
    let preempt: Option<Preempt> = preempt.map(|mut rx| {
        rx.borrow_and_update();
        let f: Preempt = Box::pin(async move {
            if rx.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        });
        f
    });
    let tracked = Tracked {
        inner: Box::pin(resp.bytes_stream()),
        preempt,
        sse,
        preempted: false,
        fin: Some(Finish {
            st: st.clone(),
            rec,
            started,
            model: model.to_string(),
            card: card.to_string(),
            status,
            _guard: guard,
            _permit: permit,
        }),
        tail: Vec::new(),
        first: None,
        errored: false,
        ended: false,
    };
    out.body(Body::from_stream(tracked))
        .unwrap_or_else(|_| error(500, "response build failed", "internal_error", None))
}

fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

struct Finish {
    st: Arc<AppState>,
    rec: LogRec,
    started: Instant,
    model: String,
    card: String,
    status: u16,
    _guard: CardGuard,
    _permit: Option<placement::Permit>,
}

type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>;
type Preempt = Pin<Box<dyn std::future::Future<Output = ()> + Send>>;

/// The preempt watch for a request on `card` (SUPERVISOR.md 9.4), unless the request is the
/// reserving event's own (`mine`). None without a calendar.
fn preemptible(mine: &[String], card: &str) -> Option<tokio::sync::watch::Receiver<u64>> {
    crate::calendar::installed()?;
    (!mine.iter().any(|c| c == card)).then(|| crate::calendar::reserve::preempt_watch(card))
}

/// The SSE event a preempted stream ends with.
const PREEMPTED_SSE: &[u8] = b"data: {\"error\":{\"message\":\"preempted by a calendar reservation of this card; retry\",\"type\":\"preempted_by_reservation\",\"code\":\"503\"}}\n\n";

/// The upstream body on its way to the client: counts TTFT, finds the last `usage`, and logs
/// the request when it ends (completed, truncated by the upstream, or abandoned by the client).
struct Tracked {
    inner: ByteStream,
    /// resolves when a calendar reservation preempts the card
    preempt: Option<Preempt>,
    sse: bool,
    preempted: bool,
    fin: Option<Finish>,
    tail: Vec<u8>,
    first: Option<Instant>,
    errored: bool,
    ended: bool,
}

const TAIL: usize = 16 * 1024;

impl Stream for Tracked {
    type Item = Result<Bytes, reqwest::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self;
        if this.preempted {
            return Poll::Ready(None);
        }
        if let Some(p) = this.preempt.as_mut()
            && p.as_mut().poll(cx).is_ready()
        {
            this.preempt = None;
            this.preempted = true;
            if this.sse {
                return Poll::Ready(Some(Ok(Bytes::from_static(PREEMPTED_SSE))));
            }
            return Poll::Ready(None);
        }
        match this.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(b))) => {
                this.first.get_or_insert_with(Instant::now);
                this.tail.extend_from_slice(&b);
                if this.tail.len() > TAIL {
                    let cut = this.tail.len() - TAIL;
                    this.tail.drain(..cut);
                }
                Poll::Ready(Some(Ok(b)))
            }
            Poll::Ready(Some(Err(e))) => {
                this.errored = true;
                Poll::Ready(Some(Err(e)))
            }
            Poll::Ready(None) => {
                this.ended = true;
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for Tracked {
    fn drop(&mut self) {
        let Some(mut f) = self.fin.take() else { return };
        let total = f.started.elapsed().as_secs_f64();
        let ttft = self.first.map(|t| (t - f.started).as_secs_f64());
        if let Some(t) = ttft {
            f.rec.set("ttftSeconds", round3(t));
        }
        f.rec.set("seconds", round3(total));
        if self.preempted {
            f.rec.set("end", "preempted");
            if let Some(cal) = crate::calendar::installed() {
                cal.preempted(&f.card, &f.model);
            }
        } else if self.errored {
            f.rec.set("end", "truncated");
            f.st.metrics.truncated();
        } else if !self.ended {
            f.rec.set("end", "client_gone");
        }
        if let Some(u) = last_usage(&self.tail) {
            f.rec.set("usage", u);
        }
        f.st.metrics
            .request(&f.model, &f.card, f.status, ttft, total);
        f.st.log
            .write(&Value::Object(std::mem::take(&mut f.rec.fields)));
    }
}

/// The last non-null `"usage": {...}` object in the tail of a body (JSON or SSE).
pub fn last_usage(tail: &[u8]) -> Option<Value> {
    let needle = b"\"usage\"";
    let mut end = tail.len();
    while let Some(pos) = find_last(&tail[..end], needle) {
        let mut rest = &tail[pos + needle.len()..];
        while let Some((c, r)) = rest.split_first() {
            if c.is_ascii_whitespace() || *c == b':' {
                rest = r;
            } else {
                break;
            }
        }
        if let Some(Ok(v)) = serde_json::Deserializer::from_slice(rest)
            .into_iter::<Value>()
            .next()
            && v.is_object()
        {
            return Some(v);
        }
        end = pos;
    }
    None
}

fn find_last(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if hay.len() < needle.len() {
        return None;
    }
    (0..=hay.len() - needle.len())
        .rev()
        .find(|&i| &hay[i..i + needle.len()] == needle)
}

#[allow(clippy::too_many_arguments)]
fn finish_error(
    st: &AppState,
    rec: LogRec,
    started: Instant,
    model: &str,
    status: u16,
    msg: &str,
    etype: &str,
    param: Option<&str>,
) -> Response {
    finish_resp(st, rec, started, model, error(status, msg, etype, param))
}

/// Log a response the router made itself.
pub fn finish_resp(
    st: &AppState,
    mut rec: LogRec,
    started: Instant,
    model: &str,
    r: Response,
) -> Response {
    let status = r.status().as_u16();
    rec.set("status", status);
    rec.set("seconds", round3(started.elapsed().as_secs_f64()));
    st.metrics
        .request(model, "-", status, None, started.elapsed().as_secs_f64());
    st.log.write(&Value::Object(rec.fields));
    r
}

pub fn now_epoch() -> f64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_from_sse_tail() {
        let sse = b"data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}],\"usage\":null}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2}}\n\ndata: [DONE]\n\n";
        assert_eq!(last_usage(sse).unwrap()["prompt_tokens"], 5);
        let plain = br#"{"id":"x","usage": {"total_tokens": 9}}"#;
        assert_eq!(last_usage(plain).unwrap()["total_tokens"], 9);
        assert!(last_usage(b"data: {\"usage\":null}").is_none());
    }
}
