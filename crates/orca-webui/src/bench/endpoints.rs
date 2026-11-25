//! `BenchEndpoints`: `GET /api/bench` state (suites, queue, running runs, gate) ·
//! `GET /api/bench/tasks` · `GET /api/bench/results` (archive-*/ rows first, marked "archive") ·
//! `GET /api/bench/summary[?batch=&suite=&byTarget=]` · `GET /api/bench/events` (server-sent
//! events) · `GET /api/bench/runs/{id}/output` · `GET /api/bench/runs/{id}/logs[?tail=N]` ·
//! `POST /api/bench/suites {suite, overwrite?, start?}` · `POST /api/bench/start {suite}` ·
//! `POST /api/bench/runs/{id}/cancel` · `POST /api/bench/batches/{id}/cancel` ·
//! `POST /api/bench/{pause|resume|stop}` · `GET|POST /api/bench/mirror` ·
//! `GET /api/bench/queue[/plan]` · `POST /api/bench/queue/items {items}` ·
//! `DELETE /api/bench/queue/items/{id}` · `POST /api/bench/queue/items/{id}/{move|requeue}` ·
//! `POST /api/bench/queue/{clear|start|pause|requeue}`.

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use super::queue::{BenchQueue, NewItem};
use super::runner::BenchRunner;
use super::{BenchSuite, summary};
use crate::audit::Caller;
use crate::server::AppState;

fn caller(addr: SocketAddr) -> Caller {
    Caller {
        ip: Some(addr.ip().to_string()),
        user: None,
    }
}

fn json_status(status: StatusCode, v: Value) -> Response {
    (status, axum::Json(v)).into_response()
}

fn ok(v: Value) -> Response {
    axum::Json(v).into_response()
}

fn bad(msg: &str) -> Response {
    json_status(StatusCode::BAD_REQUEST, json!({"error": msg}))
}

fn text(t: String) -> Response {
    ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], t).into_response()
}

/// A JSON body (an empty one reads as the defaults).
fn body<T: DeserializeOwned + Default>(bytes: &Bytes) -> Result<T, String> {
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(T::default());
    }
    serde_json::from_slice(bytes).map_err(|e| format!("Invalid JSON body: {e}"))
}

fn runner(s: &AppState) -> &Arc<BenchRunner> {
    s.bench
        .as_ref()
        .expect("bench routes are mapped only with a runner")
}

fn queue(s: &AppState) -> &Arc<BenchQueue> {
    s.bench_queue
        .as_ref()
        .expect("bench routes are mapped only with a queue")
}

type S = State<Arc<AppState>>;
type Addr = ConnectInfo<SocketAddr>;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/bench", get(state))
        .route("/api/bench/tasks", get(tasks))
        .route("/api/bench/results", get(results))
        .route("/api/bench/summary", get(summary_rows))
        .route("/api/bench/events", get(events))
        .route("/api/bench/runs/{id}/output", get(output))
        .route("/api/bench/runs/{id}/logs", get(logs))
        .route("/api/bench/suites", post(save_suite))
        .route("/api/bench/start", post(start))
        .route("/api/bench/runs/{id}/cancel", post(cancel_run))
        .route("/api/bench/batches/{id}/cancel", post(cancel_batch))
        .route("/api/bench/mirror", get(mirror).post(refresh_mirror))
        .route("/api/bench/queue", get(queue_state))
        .route("/api/bench/queue/plan", get(queue_plan))
        .route("/api/bench/queue/items", post(queue_add))
        .route("/api/bench/queue/items/{id}", delete(queue_remove))
        .route("/api/bench/queue/items/{id}/move", post(queue_move))
        .route(
            "/api/bench/queue/items/{id}/requeue",
            post(queue_requeue_one),
        )
        .route("/api/bench/queue/requeue", post(queue_requeue_all))
        .route("/api/bench/queue/clear", post(queue_clear))
        .route("/api/bench/queue/start", post(queue_start))
        .route("/api/bench/queue/pause", post(queue_pause))
        .route("/api/bench/pause", post(pause))
        .route("/api/bench/resume", post(resume))
        .route("/api/bench/stop", post(stop))
}

async fn state(State(s): S) -> Response {
    ok(runner(&s).state())
}

async fn tasks(State(s): S) -> Response {
    match runner(&s).tasks().await {
        Ok(t) => ok(Value::Array(t)),
        Err(e) => json_status(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": e})),
    }
}

async fn results(State(s): S) -> Response {
    ok(Value::Array(runner(&s).results(true)))
}

async fn summary_rows(State(s): S, Query(q): Query<HashMap<String, String>>) -> Response {
    let get = |k: &str| {
        q.iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(k))
            .map(|(_, v)| v.clone())
    };
    let by_target = match get("byTarget") {
        None => false,
        Some(v) => match v.to_ascii_lowercase().as_str() {
            "true" => true,
            "false" | "" => false,
            _ => return bad("byTarget must be true or false."),
        },
    };
    let rows = runner(&s).results(false);
    let out = summary::compute(
        &rows,
        get("batch").as_deref(),
        get("suite").as_deref(),
        by_target,
    );
    ok(serde_json::to_value(out).unwrap_or_default())
}

async fn output(State(s): S, Path(id): Path<String>) -> Response {
    match runner(&s).output(&id) {
        Some(t) => text(t),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn logs(
    State(s): S,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let tail = q
        .get("tail")
        .and_then(|t| t.parse::<i64>().ok())
        .unwrap_or(120_000)
        .clamp(200, 1_000_000) as usize;
    match runner(&s).logs(&id, tail) {
        Some(t) => text(t),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct SaveBody {
    suite: Option<BenchSuite>,
    overwrite: Option<bool>,
    start: Option<bool>,
    confirm_switch: Option<bool>,
}

async fn save_suite(State(s): S, ConnectInfo(addr): Addr, bytes: Bytes) -> Response {
    let b: SaveBody = match body(&bytes) {
        Ok(b) => b,
        Err(e) => return bad(&e),
    };
    let Some(suite) = b.suite else {
        return bad("No suite.");
    };
    let r = runner(&s);
    let saved = match r.save_suite(suite, b.overwrite.unwrap_or(false)) {
        Ok(v) => v,
        Err(e) => return bad(&e),
    };
    let who = caller(addr);
    s.audit.write(
        Some(&who),
        "bench.suite_save",
        json!({"suite": saved.name, "targets": saved.targets.as_ref().map_or(0, Vec::len)}),
    );
    if b.start != Some(true) {
        return ok(json!({"suite": saved}));
    }
    match r.enqueue_batch(&saved.name, b.confirm_switch.unwrap_or(false)) {
        Ok((batch, n)) => {
            s.audit.write(
                Some(&who),
                "bench.start",
                json!({"suite": saved.name, "runs": n, "batch": batch}),
            );
            ok(json!({"suite": saved, "queued": n, "batch": batch}))
        }
        Err(e) => bad(&e),
    }
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct StartBody {
    suite: Option<String>,
    confirm_switch: Option<bool>,
}

async fn start(State(s): S, ConnectInfo(addr): Addr, bytes: Bytes) -> Response {
    let b: StartBody = match body(&bytes) {
        Ok(b) => b,
        Err(e) => return bad(&e),
    };
    match runner(&s).enqueue_batch(
        b.suite.as_deref().unwrap_or(""),
        b.confirm_switch.unwrap_or(false),
    ) {
        Ok((batch, n)) => {
            s.audit.write(
                Some(&caller(addr)),
                "bench.start",
                json!({"suite": b.suite, "runs": n, "batch": batch}),
            );
            ok(json!({"queued": n, "batch": batch}))
        }
        Err(e) => bad(&e),
    }
}

async fn cancel_run(State(s): S, ConnectInfo(addr): Addr, Path(id): Path<String>) -> Response {
    let done = runner(&s).cancel(&id);
    s.audit.write(
        Some(&caller(addr)),
        "bench.cancel_run",
        json!({"run": id, "ok": done}),
    );
    if done {
        ok(json!({"ok": done}))
    } else {
        json_status(
            StatusCode::NOT_FOUND,
            json!({"error": "That run is not queued or running."}),
        )
    }
}

async fn cancel_batch(State(s): S, ConnectInfo(addr): Addr, Path(id): Path<String>) -> Response {
    let n = runner(&s).cancel_batch(&id);
    s.audit.write(
        Some(&caller(addr)),
        "bench.cancel_batch",
        json!({"batch": id, "runs": n}),
    );
    ok(json!({"ok": true, "cancelled": n}))
}

async fn mirror(State(s): S) -> Response {
    ok(runner(&s).mirror().await)
}

async fn refresh_mirror(State(s): S, ConnectInfo(addr): Addr) -> Response {
    let r = runner(&s).refresh_mirror().await;
    s.audit.write(
        Some(&caller(addr)),
        "bench.mirror_refresh",
        json!({"ok": r.ok()}),
    );
    json_status(
        if r.ok() {
            StatusCode::OK
        } else {
            StatusCode::CONFLICT
        },
        json!({"ok": r.ok(), "output": r.output, "error": r.error}),
    )
}

async fn queue_state(State(s): S) -> Response {
    ok(queue(&s).state())
}

async fn queue_plan(State(s): S) -> Response {
    let rows = runner(&s).results(false);
    ok(queue(&s).plan(&rows).await)
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct QueueAddBody {
    items: Option<Vec<NewItem>>,
}

async fn queue_add(State(s): S, ConnectInfo(addr): Addr, bytes: Bytes) -> Response {
    let b: QueueAddBody = match body(&bytes) {
        Ok(b) => b,
        Err(e) => return bad(&e),
    };
    let items = match b.items {
        Some(i) if !i.is_empty() => i,
        _ => return bad("No items."),
    };
    if items.len() > 500 {
        return bad("At most 500 items at a time.");
    }
    let (added, skipped) = queue(&s).add(items).await;
    s.audit.write(
        Some(&caller(addr)),
        "bench.queue_add",
        json!({"added": added, "skipped": skipped.len()}),
    );
    ok(json!({"added": added, "skipped": skipped}))
}

async fn queue_remove(State(s): S, ConnectInfo(addr): Addr, Path(id): Path<String>) -> Response {
    let done = queue(&s).remove(&id);
    s.audit.write(
        Some(&caller(addr)),
        "bench.queue_remove",
        json!({"id": id, "ok": done}),
    );
    if done {
        ok(json!({"ok": done}))
    } else {
        json_status(StatusCode::NOT_FOUND, json!({"error": "No such item."}))
    }
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct MoveBody {
    to: i32,
}

async fn queue_move(State(s): S, Path(id): Path<String>, bytes: Bytes) -> Response {
    let b: MoveBody = match body(&bytes) {
        Ok(b) => b,
        Err(e) => return bad(&e),
    };
    if queue(&s).move_to(&id, b.to) {
        ok(json!({"ok": true}))
    } else {
        json_status(StatusCode::NOT_FOUND, json!({"error": "No such item."}))
    }
}

async fn queue_requeue_one(State(s): S, Path(id): Path<String>) -> Response {
    ok(json!({"requeued": queue(&s).requeue(Some(&id))}))
}

async fn queue_requeue_all(State(s): S) -> Response {
    ok(json!({"requeued": queue(&s).requeue(None)}))
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct ClearBody {
    finished_only: Option<bool>,
}

async fn queue_clear(State(s): S, ConnectInfo(addr): Addr, bytes: Bytes) -> Response {
    let b: ClearBody = match body(&bytes) {
        Ok(b) => b,
        Err(e) => return bad(&e),
    };
    let finished_only = b.finished_only.unwrap_or(false);
    let n = queue(&s).clear(finished_only);
    s.audit.write(
        Some(&caller(addr)),
        "bench.queue_clear",
        json!({"removed": n, "finishedOnly": finished_only}),
    );
    ok(json!({"removed": n}))
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct QueueStartBody {
    confirm: Option<bool>,
}

async fn queue_start(State(s): S, ConnectInfo(addr): Addr, bytes: Bytes) -> Response {
    let b: QueueStartBody = match body(&bytes) {
        Ok(b) => b,
        Err(e) => return bad(&e),
    };
    let error = queue(&s).start(b.confirm.unwrap_or(false)).await;
    s.audit.write(
        Some(&caller(addr)),
        "bench.queue_start",
        json!({"ok": error.is_none(), "error": error}),
    );
    match error {
        None => ok(json!({"ok": true})),
        Some(e) => json_status(StatusCode::CONFLICT, json!({"error": e})),
    }
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct QueuePauseBody {
    stop_current: Option<bool>,
}

async fn queue_pause(State(s): S, ConnectInfo(addr): Addr, bytes: Bytes) -> Response {
    let b: QueuePauseBody = match body(&bytes) {
        Ok(b) => b,
        Err(e) => return bad(&e),
    };
    let stop_current = b.stop_current.unwrap_or(false);
    queue(&s).pause(stop_current);
    s.audit.write(
        Some(&caller(addr)),
        "bench.queue_pause",
        json!({"stopCurrent": stop_current}),
    );
    ok(json!({"ok": true}))
}

async fn pause(State(s): S, ConnectInfo(addr): Addr) -> Response {
    runner(&s).pause();
    s.audit
        .write(Some(&caller(addr)), "bench.pause", Value::Null);
    ok(json!({"ok": true}))
}

async fn resume(State(s): S, ConnectInfo(addr): Addr) -> Response {
    runner(&s).resume();
    s.audit
        .write(Some(&caller(addr)), "bench.resume", Value::Null);
    ok(json!({"ok": true}))
}

async fn stop(State(s): S, ConnectInfo(addr): Addr) -> Response {
    runner(&s).stop();
    s.audit
        .write(Some(&caller(addr)), "bench.stop", Value::Null);
    ok(json!({"ok": true}))
}

// ── the live stream ─────────────────────────────────────────────────────────────────────

struct Stream {
    runner: Arc<BenchRunner>,
    changes: tokio::sync::watch::Receiver<u64>,
    sent: HashMap<String, String>,
    status: String,
    quiet: u32,
    started: bool,
    pending: std::collections::VecDeque<String>,
}

fn event(name: &str, json: &str) -> String {
    format!("event: {name}\ndata: {json}\n\n")
}

/// The next chunk of the stream: a `snapshot` (every run, and the status) first, then a `run`
/// event whenever a run's line changes and a `status` event when the queue's does; ": idle"
/// after 15 quiet seconds keeps proxies from closing it.
async fn next_chunk(mut st: Stream) -> Option<(Result<Bytes, Infallible>, Stream)> {
    if !st.started {
        st.started = true;
        let runs = st.runner.runs();
        for r in &runs {
            let id = r["id"].as_str().unwrap_or("").to_string();
            st.sent.insert(id, r.to_string());
        }
        let status = st.runner.status();
        st.status = status.to_string();
        let chunk = event(
            "snapshot",
            &json!({"runs": runs, "status": status}).to_string(),
        );
        return Some((Ok(Bytes::from(chunk)), st));
    }
    loop {
        if let Some(c) = st.pending.pop_front() {
            return Some((Ok(Bytes::from(c)), st));
        }
        let _ = tokio::time::timeout(Duration::from_secs(1), st.changes.changed()).await;
        let mut wrote = false;
        for r in st.runner.runs() {
            let j = r.to_string();
            let id = r["id"].as_str().unwrap_or("").to_string();
            if st.sent.get(&id) == Some(&j) {
                continue;
            }
            st.sent.insert(id, j.clone());
            st.pending.push_back(event("run", &j));
            wrote = true;
        }
        let now = st.runner.status().to_string();
        if now != st.status {
            st.status = now.clone();
            st.pending.push_back(event("status", &now));
            wrote = true;
        }
        st.quiet = if wrote { 0 } else { st.quiet + 1 };
        if st.quiet >= 15 {
            st.quiet = 0;
            st.pending.push_back(": idle\n\n".into());
        }
    }
}

async fn events(State(s): S) -> Response {
    let r = runner(&s).clone();
    let st = Stream {
        changes: r.subscribe(),
        runner: r,
        sent: HashMap::new(),
        status: String::new(),
        quiet: 0,
        started: false,
        pending: Default::default(),
    };
    let stream = futures_util::stream::unfold(st, next_chunk);
    Response::builder()
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache, no-transform")
        .header("X-Accel-Buffering", "no")
        .body(Body::from_stream(stream))
        .unwrap()
}
