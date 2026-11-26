//! The public Glance (SUPERVISOR.md 12.1): its own listener, so a reverse proxy forwards only
//! this; the redacted feed; no controls, no cookies, no sign-in; streams and requests limited
//! per client address. Serves the small `glance.html` bundle and nothing else of the app.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

use orca_sensors::feed::TelemetryFeed;

use super::PublicOptions;

/// Reads a file of the client build (the override folder or the embedded copy).
pub type ClientFiles = Arc<dyn Fn(&str) -> Option<Vec<u8>> + Send + Sync>;

pub struct PublicState {
    feed: Arc<dyn TelemetryFeed>,
    options: PublicOptions,
    ping: Duration,
    files: ClientFiles,
    trusted_proxies: Vec<IpAddr>,
    limits: Mutex<Limits>,
}

#[derive(Default)]
struct Limits {
    streams: HashMap<IpAddr, usize>,
    total: usize,
    /// Requests per address in the current window.
    window: HashMap<IpAddr, (Instant, u32)>,
}

/// Requests (pages, assets, snapshots) per address per 10 s.
const REQUESTS_PER_WINDOW: u32 = 120;
const WINDOW: Duration = Duration::from_secs(10);

pub fn router(
    feed: Arc<dyn TelemetryFeed>,
    options: PublicOptions,
    ping: Duration,
    files: ClientFiles,
    trusted_proxies: Vec<IpAddr>,
) -> Router {
    let state = Arc::new(PublicState {
        feed,
        options,
        ping,
        files,
        trusted_proxies,
        limits: Mutex::new(Limits::default()),
    });
    Router::new()
        .route("/", get(page))
        .route("/public/glance", get(page))
        .route("/public/glance/snapshot", get(snapshot))
        .route("/public/glance/stream", get(stream))
        .route("/assets/{file}", get(asset))
        .fallback(|| async { StatusCode::NOT_FOUND })
        .with_state(state)
}

/// Binds the public listener and serves until the task is aborted.
pub async fn serve(
    router: Router,
    listen: &str,
) -> Result<(tokio::task::JoinHandle<()>, SocketAddr), String> {
    let addr: SocketAddr = listen
        .parse()
        .map_err(|_| format!("public Glance: not an address: {listen} (use host:port)"))?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("public Glance: could not listen on {listen}: {e}"))?;
    let bound = listener.local_addr().unwrap_or(addr);
    let task = tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
    });
    Ok((task, bound))
}

impl PublicState {
    /// The client: the peer, or the last X-Forwarded-For hop when the peer is a trusted proxy.
    fn client(&self, peer: SocketAddr, headers: &HeaderMap) -> IpAddr {
        if self.trusted_proxies.contains(&peer.ip())
            && let Some(ip) = headers
                .get("x-forwarded-for")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.rsplit(',').next())
                .and_then(|v| v.trim().parse().ok())
        {
            return ip;
        }
        peer.ip()
    }

    fn allow(&self, ip: IpAddr) -> bool {
        let mut l = self.limits.lock().unwrap();
        let now = Instant::now();
        if l.window.len() > 10_000 {
            l.window.retain(|_, (t, _)| now.duration_since(*t) < WINDOW);
        }
        let e = l.window.entry(ip).or_insert((now, 0));
        if now.duration_since(e.0) >= WINDOW {
            *e = (now, 0);
        }
        e.1 += 1;
        e.1 <= REQUESTS_PER_WINDOW
    }
}

/// Counts an open stream; released when the response body is dropped.
struct StreamSlot {
    state: Arc<PublicState>,
    ip: IpAddr,
}

impl Drop for StreamSlot {
    fn drop(&mut self) {
        let mut l = self.state.limits.lock().unwrap();
        l.total = l.total.saturating_sub(1);
        if let Some(n) = l.streams.get_mut(&self.ip) {
            *n -= 1;
            if *n == 0 {
                l.streams.remove(&self.ip);
            }
        }
    }
}

fn secure(mut r: Response, cache: &'static str) -> Response {
    let h = r.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    h.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    h.insert("x-frame-options", HeaderValue::from_static("DENY"));
    h.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    h.insert(
        "content-security-policy",
        HeaderValue::from_static(
            "default-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'",
        ),
    );
    r
}

fn too_many() -> Response {
    secure(
        (
            StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, "10")],
            "slow down",
        )
            .into_response(),
        "no-store",
    )
}

async fn page(
    State(st): State<Arc<PublicState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    if !st.allow(st.client(peer, &headers)) {
        return too_many();
    }
    match (st.files)("glance.html") {
        Some(b) => secure(
            ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], b).into_response(),
            "no-cache",
        ),
        None => (
            StatusCode::NOT_FOUND,
            "the Glance page is not in this build",
        )
            .into_response(),
    }
}

async fn asset(
    State(st): State<Arc<PublicState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(file): Path<String>,
) -> Response {
    if !st.allow(st.client(peer, &headers)) {
        return too_many();
    }
    let ok_name = !file.is_empty()
        && !file.starts_with('.')
        && file
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && (file.ends_with(".js") || file.ends_with(".css"));
    if !ok_name {
        return StatusCode::NOT_FOUND.into_response();
    }
    match (st.files)(&format!("assets/{file}")) {
        Some(b) => {
            let mime = if file.ends_with(".js") {
                "text/javascript"
            } else {
                "text/css"
            };
            secure(
                ([(header::CONTENT_TYPE, mime)], b).into_response(),
                "public, max-age=31536000, immutable",
            )
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn snapshot(
    State(st): State<Arc<PublicState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    if !st.allow(st.client(peer, &headers)) {
        return too_many();
    }
    secure(axum::Json(st.feed.snapshot()).into_response(), "no-store")
}

async fn stream(
    State(st): State<Arc<PublicState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    let ip = st.client(peer, &headers);
    if !st.allow(ip) {
        return too_many();
    }
    {
        let mut l = st.limits.lock().unwrap();
        let mine = l.streams.get(&ip).copied().unwrap_or(0);
        if mine >= st.options.streams_per_ip || l.total >= st.options.streams_total {
            drop(l);
            return too_many();
        }
        *l.streams.entry(ip).or_default() += 1;
        l.total += 1;
    }
    let slot = StreamSlot {
        state: st.clone(),
        ip,
    };
    let sse = super::feed::sse(
        st.feed.clone(),
        super::feed::last_event_id(&headers),
        st.ping,
    );
    // The slot lives as long as the body: dropping the connection frees it.
    let (parts, body) = sse.into_parts();
    let guarded = futures_util::StreamExt::map(body.into_data_stream(), move |chunk| {
        let _ = &slot;
        chunk
    });
    secure(
        Response::from_parts(parts, Body::from_stream(guarded)),
        "no-store",
    )
}
