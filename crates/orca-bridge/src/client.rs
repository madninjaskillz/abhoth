//! The bridge: dial the router, say who we are, answer its commands and carry its requests to
//! the local engines. No port opens on this host.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

use crate::Log;
use crate::config::{BridgeConfig, ws_url};
use crate::detect;
use crate::mux::{Mux, Out};
use crate::proto::{
    CLOSE_BAD_TOKEN, CLOSE_BAD_VERSION, CLOSE_REFUSED, Descriptor, Frame, PING_SECONDS, PROTOCOL,
    SILENCE_SECONDS, WS_BINARY, WS_TEXT, hop_by_hop,
};
use crate::runner::Supervisor;

/// What the bridge is doing (the desktop's settings page and `status` show it).
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Status {
    pub connected: bool,
    /// The server id the router gave this bridge.
    pub server: Option<String>,
    pub enabled: Option<bool>,
    pub last_error: Option<String>,
    pub connects: u64,
    pub router: String,
}

/// A running bridge: its status, and how to stop it.
#[derive(Clone)]
pub struct Handle {
    pub status: Arc<Mutex<Status>>,
    stop: watch::Sender<bool>,
    refresh: Arc<watch::Sender<u64>>,
}

impl Handle {
    pub fn stop(&self) {
        let _ = self.stop.send(true);
    }
    /// Shape the descriptor again and send it (the embedding app's shares or revision changed).
    pub fn refresh(&self) {
        self.refresh.send_modify(|n| *n += 1);
    }
    pub fn status(&self) -> Status {
        self.status.lock().unwrap().clone()
    }
}

/// How one connection ended.
#[derive(Debug, PartialEq)]
enum Ended {
    /// Stop was asked for.
    Stopped,
    /// The router refused the token (or the protocol): wait long before trying again.
    Refused(String),
    /// Anything else: back off and redial.
    Lost(String),
}

/// Tunables (tests make them short).
#[derive(Debug, Clone)]
pub struct Timing {
    pub backoff_min: Duration,
    pub backoff_max: Duration,
    /// After a refused token.
    pub refused_wait: Duration,
    pub ping: Duration,
    pub silence: Duration,
    pub reap_every: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            backoff_min: Duration::from_secs(1),
            backoff_max: Duration::from_secs(60),
            refused_wait: Duration::from_secs(60),
            ping: Duration::from_secs(PING_SECONDS),
            silence: Duration::from_secs(SILENCE_SECONDS),
            reap_every: Duration::from_secs(15),
        }
    }
}

/// Commands the bridge passes on to whoever embeds it (the desktop app's remote config,
/// MACHINE.md 8): `None` from `call` means "not mine", and the bridge answers "unknown command".
#[async_trait::async_trait]
pub trait Remote: Send + Sync {
    async fn call(&self, op: &str, args: &Value) -> Option<Result<Value, String>>;
}

/// Shapes the descriptor before it is sent (leave out what is not shared, add models, the
/// remote-config flag and revision).
pub type Shape = Arc<dyn Fn(&mut Descriptor) + Send + Sync>;
/// Whether the router may use an engine (start, stop, streams). Unshared engines answer no.
pub type Allow = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// How an embedding app runs the bridge (the desktop app, D155). Default: a bridge of its own.
#[derive(Clone, Default)]
pub struct Options {
    /// Run engines with this supervisor (the app's own, shared with its local use) instead of
    /// a new one. The bridge then never starts, reaps or stops engines itself.
    pub sup: Option<Arc<Supervisor>>,
    pub shape: Option<Shape>,
    pub allow: Option<Allow>,
    pub remote: Option<Arc<dyn Remote>>,
}

struct Ctx {
    cfg: BridgeConfig,
    sup: Arc<Supervisor>,
    log: Log,
    /// The descriptor as detected, before [`Options::shape`].
    base: Mutex<Descriptor>,
    descriptor: Mutex<Descriptor>,
    opts: Options,
    status: Arc<Mutex<Status>>,
    proxy: reqwest::Client,
    version: String,
}

/// Start the bridge in the background; it runs until [`Handle::stop`].
pub fn spawn(cfg: BridgeConfig, version: &str, log: Log, timing: Timing) -> Handle {
    spawn_with(cfg, version, log, timing, Options::default())
}

/// [`spawn`] for an embedding app ([`Options`]).
pub fn spawn_with(
    cfg: BridgeConfig,
    version: &str,
    log: Log,
    timing: Timing,
    opts: Options,
) -> Handle {
    let (tx, rx) = watch::channel(false);
    let (rtx, rrx) = watch::channel(0u64);
    let status = Arc::new(Mutex::new(Status {
        router: cfg.bridge.router.clone(),
        ..Status::default()
    }));
    let h = Handle {
        status: status.clone(),
        stop: tx,
        refresh: Arc::new(rtx),
    };
    let version = version.to_string();
    tokio::spawn(async move {
        run_inner(cfg, version, log, timing, status, rx, opts, rrx).await;
    });
    h
}

/// Run the bridge until `stop` turns true.
pub async fn run(
    cfg: BridgeConfig,
    version: &str,
    log: Log,
    timing: Timing,
    stop: watch::Receiver<bool>,
) {
    let status = Arc::new(Mutex::new(Status {
        router: cfg.bridge.router.clone(),
        ..Status::default()
    }));
    let (_rtx, rrx) = watch::channel(0u64);
    run_inner(
        cfg,
        version.to_string(),
        log,
        timing,
        status,
        stop,
        Options::default(),
        rrx,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn run_inner(
    mut cfg: BridgeConfig,
    version: String,
    log: Log,
    timing: Timing,
    status: Arc<Mutex<Status>>,
    mut stop: watch::Receiver<bool>,
    opts: Options,
    mut refresh: watch::Receiver<u64>,
) {
    if let Err(e) = crate::package::fill(&mut cfg) {
        log(&format!("config: {e}"));
        status.lock().unwrap().last_error = Some(e);
        return;
    }
    let gpus = if cfg.detect.nvidia {
        detect::gpus().await
    } else {
        Vec::new()
    };
    let cpu = detect::cpu_for(&cfg).await;
    let base = detect::descriptor(&cfg, &gpus, &cpu);
    let mut descriptor = base.clone();
    if let Some(f) = &opts.shape {
        f(&mut descriptor);
    }
    log(&format!(
        "bridge {} (abhoth {version}): {} card(s) [{}], {} engine(s)",
        cfg.bridge.id,
        descriptor.cards.len(),
        descriptor
            .cards
            .iter()
            .map(|c| c.id.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        descriptor.engines.len()
    ));
    for p in descriptor.problems() {
        log(&format!("descriptor: {p}"));
    }
    if let Some(f) = crate::runner::free_ram(
        cfg.limits.count_zfs_arc,
        cfg.limits
            .arc_margin_gib
            .unwrap_or(crate::runner::DEFAULT_ARC_MARGIN_GIB),
    ) {
        log(&format!("RAM for engines: {}", f.describe()));
    }
    let own = opts.sup.is_none();
    let sup = match &opts.sup {
        Some(s) => s.clone(),
        None => {
            let s = Supervisor::new(cfg.clone(), gpus, log.clone());
            s.adopt_all().await;
            // images and packages now, not at the first request (a failed pull shows in status)
            let p = s.clone();
            tokio::spawn(async move { p.prepare_all().await });
            s
        }
    };
    let ctx = Arc::new(Ctx {
        cfg,
        sup: sup.clone(),
        log: log.clone(),
        base: Mutex::new(base),
        descriptor: Mutex::new(descriptor),
        opts,
        status,
        proxy: reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .build()
            .expect("http client"),
        version,
    });
    if own {
        let sup = sup.clone();
        tokio::spawn(async move { sup.start_always().await });
    }
    let reaper = own.then(|| {
        let sup = sup.clone();
        let every = timing.reap_every;
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(every).await;
                sup.reap_idle().await;
            }
        })
    });

    let mut backoff = timing.backoff_min;
    loop {
        if *stop.borrow() || stop.has_changed().is_err() {
            break;
        }
        let before = ctx.status.lock().unwrap().connects;
        let ended = connect_once(&ctx, &timing, &mut stop, &mut refresh).await;
        if ctx.status.lock().unwrap().connects > before {
            backoff = timing.backoff_min; // that connection was accepted: start again from 1 s
        }
        {
            let mut s = ctx.status.lock().unwrap();
            s.connected = false;
            match &ended {
                Ended::Refused(e) | Ended::Lost(e) => s.last_error = Some(e.clone()),
                Ended::Stopped => {}
            }
        }
        let wait = match &ended {
            Ended::Stopped => break,
            Ended::Refused(e) => {
                log(&format!("the router refused this bridge: {e}"));
                backoff = timing.backoff_min;
                timing.refused_wait
            }
            Ended::Lost(e) => {
                log(&format!("connection: {e}"));
                let w = jitter(backoff);
                backoff = (backoff * 2).min(timing.backoff_max);
                w
            }
        };
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = stop.changed() => {}
        }
    }
    if let Some(r) = reaper {
        r.abort();
        log("bridge stopping: stopping the engines it started");
        sup.stop_all().await;
    } else {
        log("bridge stopping (the app keeps its engines)");
    }
}

impl Ctx {
    /// The descriptor from the base again, through the app's shape.
    fn reshape(&self) -> Descriptor {
        let mut d = self.base.lock().unwrap().clone();
        if let Some(f) = &self.opts.shape {
            f(&mut d);
        }
        *self.descriptor.lock().unwrap() = d.clone();
        d
    }

    fn allowed(&self, engine: &str) -> Result<(), String> {
        match &self.opts.allow {
            Some(f) if !engine.is_empty() && !f(engine) => {
                Err(format!("engine `{engine}` is not shared with the router"))
            }
            _ => Ok(()),
        }
    }

    async fn acquire(
        self: &Arc<Self>,
        engine: &str,
        card: &str,
    ) -> Result<crate::runner::Lease, String> {
        self.allowed(engine)?;
        self.sup.acquire(engine, card).await
    }
}

/// 75 %..125 % of `d`.
fn jitter(d: Duration) -> Duration {
    let mut b = [0u8; 1];
    let _ = getrandom::fill(&mut b);
    let f = 0.75 + (b[0] as f64 / 255.0) * 0.5;
    d.mul_f64(f)
}

async fn connect_once(
    ctx: &Arc<Ctx>,
    timing: &Timing,
    stop: &mut watch::Receiver<bool>,
    refresh: &mut watch::Receiver<u64>,
) -> Ended {
    let url = match ws_url(&ctx.cfg.bridge.router) {
        Ok(u) => u,
        Err(e) => return Ended::Refused(format!("bridge.router: {e}")),
    };
    let token = match ctx.cfg.token() {
        Ok(t) => t,
        Err(e) => return Ended::Refused(e),
    };
    let mut req = match url.as_str().into_client_request() {
        Ok(r) => r,
        Err(e) => return Ended::Refused(format!("{url}: {e}")),
    };
    match format!("Bearer {token}").parse() {
        Ok(v) => {
            req.headers_mut().insert("authorization", v);
        }
        Err(_) => return Ended::Refused("the token is not a valid header value".into()),
    }
    let connect = tokio::time::timeout(
        Duration::from_secs(15),
        tokio_tungstenite::connect_async(req),
    );
    let ws = tokio::select! {
        r = connect => match r {
            Ok(Ok((ws, _))) => ws,
            Ok(Err(tokio_tungstenite::tungstenite::Error::Http(resp))) => {
                let code = resp.status().as_u16();
                return if code == 401 || code == 403 {
                    Ended::Refused(format!("HTTP {code} (the token)"))
                } else {
                    Ended::Lost(format!("HTTP {code} from {url}"))
                };
            }
            Ok(Err(e)) => return Ended::Lost(format!("{url}: {e}")),
            Err(_) => return Ended::Lost(format!("{url}: timed out")),
        },
        _ = stop.changed() => return Ended::Stopped,
    };
    let (mut sink, mut stream) = ws.split();
    let (mux, mut out) = Mux::new();
    let hello = Frame::Hello {
        v: PROTOCOL,
        bridge: ctx.cfg.bridge.id.clone(),
        version: ctx.version.clone(),
        descriptor: ctx.descriptor.lock().unwrap().clone(),
    };
    mux.send(&hello);
    let writer = tokio::spawn(async move {
        while let Some(o) = out.recv().await {
            let m = match o {
                Out::Text(t) => Message::Text(t.into()),
                Out::Binary(b) => Message::Binary(b.into()),
                Out::Close(code, reason) => {
                    let _ = sink
                        .send(Message::Close(Some(CloseFrame {
                            code: CloseCode::from(code),
                            reason: reason.into(),
                        })))
                        .await;
                    break;
                }
            };
            if sink.send(m).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });
    // Engine state changes go to the router as events.
    {
        let events = mux.clone();
        ctx.sup.set_on_state(Arc::new(move |v: Value| {
            events.send(&Frame::Event {
                kind: "engine-state".into(),
                data: v,
            });
        }));
        // what was fetched (or failed to) before this connection
        for (engine, v) in ctx.sup.prepared() {
            mux.send(&Frame::Event {
                kind: "engine-state".into(),
                data: json!({"engine": engine, "prepare": v}),
            });
        }
    }
    let mut ping = tokio::time::interval(timing.ping);
    ping.tick().await;
    let ended = loop {
        tokio::select! {
            m = stream.next() => match m {
                Some(Ok(Message::Text(t))) => match Frame::parse(&t) {
                    Ok(f) => {
                        if let Some(f) = mux.on_text(f)
                            && let Err(e) = handle(ctx, &mux, f).await
                        {
                            break e;
                        }
                    }
                    Err(e) => (ctx.log)(&format!("from the router: {e}")),
                },
                Some(Ok(Message::Binary(b))) => mux.on_binary(&b),
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => mux.on_other(),
                Some(Ok(Message::Close(c))) => {
                    let (code, reason) = c
                        .map(|c| (u16::from(c.code), c.reason.to_string()))
                        .unwrap_or((1005, String::new()));
                    break match code {
                        CLOSE_BAD_TOKEN => Ended::Refused(format!("bad token ({code})")),
                        CLOSE_BAD_VERSION => Ended::Refused(format!("protocol version ({code}): {reason}")),
                        CLOSE_REFUSED => Ended::Refused(format!("{reason} ({code})")),
                        _ => Ended::Lost(format!("closed by the router ({code}) {reason}")),
                    };
                }
                Some(Ok(Message::Frame(_))) => {}
                Some(Err(e)) => break Ended::Lost(e.to_string()),
                None => break Ended::Lost("the connection ended".into()),
            },
            _ = ping.tick() => {
                if mux.silent_for() > timing.silence {
                    break Ended::Lost(format!("no word from the router for {}s", timing.silence.as_secs()));
                }
                mux.send(&Frame::Ping { at: now_ms() });
            }
            _ = stop.changed() => {
                mux.close(1001, "the bridge is stopping");
                break Ended::Stopped;
            }
            r = refresh.changed() => {
                if r.is_ok() {
                    mux.send(&Frame::Descriptor { descriptor: ctx.reshape() });
                }
            }
        }
    };
    let why = match &ended {
        Ended::Stopped => "stopped".to_string(),
        Ended::Refused(e) | Ended::Lost(e) => e.clone(),
    };
    mux.shutdown(&why);
    let _ = tokio::time::timeout(Duration::from_secs(2), writer).await;
    ended
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A frame the mux handed back: welcome, a command, or a stream to open.
async fn handle(ctx: &Arc<Ctx>, mux: &Arc<Mux>, f: Frame) -> Result<(), Ended> {
    match f {
        Frame::Welcome {
            v,
            server,
            settings,
        } => {
            if v != PROTOCOL {
                return Err(Ended::Refused(format!("the router speaks v{v}")));
            }
            (ctx.log)(&format!(
                "connected to {} as server {server}{}",
                ctx.cfg.bridge.router,
                if settings.enabled {
                    String::new()
                } else {
                    " (disabled by the router's admin)".into()
                }
            ));
            let mut s = ctx.status.lock().unwrap();
            s.connected = true;
            s.server = Some(server);
            s.enabled = Some(settings.enabled);
            s.last_error = None;
            s.connects += 1;
        }
        Frame::Cmd { id, op, args } => {
            let (ctx, mux) = (ctx.clone(), mux.clone());
            tokio::spawn(async move {
                let r = command(&ctx, &mux, &op, &args).await;
                mux.send(&match r {
                    Ok(data) => Frame::Result {
                        id,
                        ok: true,
                        data: Some(data),
                        error: None,
                    },
                    Err(e) => Frame::Result {
                        id,
                        ok: false,
                        data: None,
                        error: Some(e),
                    },
                });
            });
        }
        Frame::Open {
            sid,
            engine,
            card,
            method,
            path,
            headers,
            ws,
        } => {
            // Register the stream now: its body frames are already on their way.
            let stream = mux.accept(sid);
            let (ctx, mux) = (ctx.clone(), mux.clone());
            tokio::spawn(async move {
                if method.eq_ignore_ascii_case(crate::proto::TCP_METHOD) {
                    proxy_tcp(&ctx, &mux, stream, &engine, &card).await
                } else if ws {
                    proxy_ws(&ctx, &mux, stream, &engine, &card, &path, headers).await
                } else {
                    proxy_http(&ctx, &mux, stream, &engine, &card, &method, &path, headers).await
                }
            });
        }
        Frame::Event { .. } | Frame::Descriptor { .. } | Frame::Hello { .. } => {}
        _ => {}
    }
    Ok(())
}

fn arg<'a>(args: &'a Value, k: &str) -> &'a str {
    args.get(k).and_then(Value::as_str).unwrap_or("")
}

/// The router's commands (13.1).
async fn command(ctx: &Arc<Ctx>, mux: &Arc<Mux>, op: &str, args: &Value) -> Result<Value, String> {
    let (engine, card) = (arg(args, "engine"), arg(args, "card"));
    match op {
        "status" => {
            let mut v = ctx.sup.status();
            v["bridge"] = json!(ctx.cfg.bridge.id);
            v["version"] = json!(ctx.version);
            let lim = &ctx.cfg.limits;
            let free = crate::runner::free_ram(
                lim.count_zfs_arc,
                lim.arc_margin_gib
                    .unwrap_or(crate::runner::DEFAULT_ARC_MARGIN_GIB),
            );
            v["free_ram_gib"] = json!(free.map(|f| f.total()));
            v["ram"] = json!(free.map(|f| json!({"available_gib": f.available_gib,
                "arc_reclaimable_gib": f.arc_gib, "text": f.describe()})));
            Ok(v)
        }
        "start" | "stop" | "logs" | "probe" if ctx.allowed(engine).is_err() => {
            ctx.allowed(engine).map(|_| Value::Null)
        }
        "start" => ctx
            .sup
            .start(engine, card)
            .await
            .map(|_| json!({"ok": true})),
        "stop" => ctx
            .sup
            .stop(engine, card)
            .await
            .map(|_| json!({"ok": true})),
        "logs" => {
            let lines = args.get("lines").and_then(Value::as_u64).unwrap_or(200) as u32;
            ctx.sup
                .logs(engine, card, lines)
                .await
                .map(|t| json!({"text": t}))
        }
        "probe" => ctx.sup.probe(engine, card).await,
        "detect" => {
            let gpus = if ctx.cfg.detect.nvidia {
                detect::gpus().await
            } else {
                Vec::new()
            };
            *ctx.base.lock().unwrap() =
                detect::descriptor(&ctx.cfg, &gpus, &detect::cpu_for(&ctx.cfg).await);
            let d = ctx.reshape();
            mux.send(&Frame::Descriptor {
                descriptor: d.clone(),
            });
            serde_json::to_value(d).map_err(|e| e.to_string())
        }
        "files" => {
            let dir = ctx
                .cfg
                .bridge
                .model_dir
                .as_deref()
                .ok_or("this bridge has no model_dir")?;
            let folders: Vec<String> = match args.get("folders").and_then(Value::as_array) {
                Some(a) => a
                    .iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect(),
                None => ctx.cfg.bridge.file_folders.clone(),
            };
            Ok(json!({"files": detect::files(Path::new(dir), &folders)}))
        }
        "download" => download(ctx, mux, args).await,
        "install" | "uninstall" | "sync-receive" => Err(format!(
            "`{op}` is not a bridge command yet: an engine with `package = \"...\"` in bridge.toml installs on its first start"
        )),
        _ => match &ctx.opts.remote {
            Some(r) => match r.call(op, args).await {
                Some(res) => res,
                None => Err(format!("unknown command `{op}`")),
            },
            None => Err(format!("unknown command `{op}`")),
        },
    }
}

/// A path under `root` from a relative one, or an error (no `..`, no absolute paths).
pub fn under(root: &Path, rel: &str) -> Result<PathBuf, String> {
    let rel = rel.trim_start_matches('/');
    if rel.is_empty()
        || rel
            .split(['/', '\\'])
            .any(|p| p == ".." || p.is_empty() || p.contains(':'))
    {
        return Err(format!("`{rel}` is not a plain relative path"));
    }
    Ok(root.join(rel))
}

/// `download {url, dest, sha256?, token?}`: one file into `model_dir/dest`, through a part file.
async fn download(ctx: &Arc<Ctx>, mux: &Arc<Mux>, args: &Value) -> Result<Value, String> {
    use sha2::Digest;
    let root = ctx
        .cfg
        .bridge
        .model_dir
        .as_deref()
        .ok_or("this bridge has no model_dir")?;
    let url = arg(args, "url");
    if !url.starts_with("https://") {
        return Err("downloads must be https://".into());
    }
    let dest = under(Path::new(root), arg(args, "dest"))?;
    let part = PathBuf::from(format!("{}.part", dest.display()));
    if let Some(d) = dest.parent() {
        tokio::fs::create_dir_all(d)
            .await
            .map_err(|e| format!("{}: {e}", d.display()))?;
    }
    let mut req = ctx.proxy.get(url);
    let token = arg(args, "token");
    if !token.is_empty() {
        req = req.bearer_auth(token);
    }
    let resp = req.send().await.map_err(|e| format!("download: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("download: HTTP {}", resp.status().as_u16()));
    }
    let total = resp.content_length();
    let mut f = tokio::fs::File::create(&part)
        .await
        .map_err(|e| format!("{}: {e}", part.display()))?;
    let mut hash = sha2::Sha256::new();
    let mut got: u64 = 0;
    let mut last = std::time::Instant::now();
    let mut body = resp.bytes_stream();
    while let Some(c) = body.next().await {
        let c = c.map_err(|e| format!("download: {e}"))?;
        hash.update(&c);
        tokio::io::AsyncWriteExt::write_all(&mut f, &c)
            .await
            .map_err(|e| format!("{}: {e}", part.display()))?;
        got += c.len() as u64;
        if last.elapsed() > Duration::from_secs(2) {
            last = std::time::Instant::now();
            mux.send(&Frame::Event {
                kind: "download".into(),
                data: json!({"dest": arg(args, "dest"), "bytes": got, "total": total}),
            });
        }
    }
    tokio::io::AsyncWriteExt::flush(&mut f)
        .await
        .map_err(|e| e.to_string())?;
    drop(f);
    let sum: String = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
    let want = arg(args, "sha256");
    if !want.is_empty() && !want.eq_ignore_ascii_case(&sum) {
        let _ = tokio::fs::remove_file(&part).await;
        return Err(format!("sha256 mismatch (got {sum})"));
    }
    tokio::fs::rename(&part, &dest)
        .await
        .map_err(|e| format!("{}: {e}", dest.display()))?;
    Ok(json!({"dest": arg(args, "dest"), "bytes": got, "sha256": sum}))
}

fn error_head(mux: &Mux, sid: u32, status: u16) {
    mux.head(
        sid,
        status,
        vec![("content-type".into(), "application/json".into())],
    );
}

#[allow(clippy::too_many_arguments)]
async fn proxy_http(
    ctx: &Arc<Ctx>,
    mux: &Arc<Mux>,
    s: crate::mux::Stream,
    engine: &str,
    card: &str,
    method: &str,
    path: &str,
    headers: Vec<(String, String)>,
) {
    let sid = s.sid;
    let (tx, rx) = s.split();
    let lease = match ctx.acquire(engine, card).await {
        Ok(l) => l,
        Err(e) => {
            // Not reading the request: let go of it first, so the reply's end is not a reset.
            drop(rx);
            error_head(mux, sid, 503);
            let _ = tx
                .send(
                    json!({"error": {"message": e, "type": "bridge_engine_unavailable"}})
                        .to_string()
                        .as_bytes(),
                )
                .await;
            tx.end();
            return;
        }
    };
    if !path.starts_with('/') {
        drop(rx);
        error_head(mux, sid, 400);
        tx.end();
        return;
    }
    let url = format!("http://{}:{}{path}", lease.host, lease.port);
    let m = reqwest::Method::from_bytes(method.as_bytes()).unwrap_or(reqwest::Method::GET);
    let body = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv()
            .await
            .map(|r| (r.map_err(std::io::Error::other), rx))
    });
    let mut req = ctx
        .proxy
        .request(m, url)
        .body(reqwest::Body::wrap_stream(body));
    for (k, v) in &headers {
        if !hop_by_hop(k) && !k.eq_ignore_ascii_case("authorization") {
            req = req.header(k.as_str(), v.as_str());
        }
    }
    match req.send().await {
        Ok(resp) => {
            let hs: Vec<(String, String)> = resp
                .headers()
                .iter()
                .filter(|(k, _)| !hop_by_hop(k.as_str()))
                .filter_map(|(k, v)| Some((k.to_string(), v.to_str().ok()?.to_string())))
                .collect();
            mux.head(sid, resp.status().as_u16(), hs);
            let mut bs = resp.bytes_stream();
            while let Some(c) = bs.next().await {
                match c {
                    Ok(b) => {
                        if tx.send(&b).await.is_err() {
                            return;
                        }
                    }
                    Err(e) => {
                        tx.reset(&format!("engine body: {e}"));
                        return;
                    }
                }
            }
            tx.end();
        }
        Err(e) => {
            error_head(mux, sid, 502);
            let _ = tx
                .send(json!({"error": {"message": format!("{engine} on {card}: {e}"), "type": "bridge_engine_error"}}).to_string().as_bytes())
                .await;
            tx.end();
        }
    }
    drop(lease);
}

async fn proxy_ws(
    ctx: &Arc<Ctx>,
    mux: &Arc<Mux>,
    s: crate::mux::Stream,
    engine: &str,
    card: &str,
    path: &str,
    headers: Vec<(String, String)>,
) {
    let sid = s.sid;
    let (tx, mut rx) = s.split();
    let lease = match ctx.acquire(engine, card).await {
        Ok(l) => l,
        Err(_) => {
            drop(rx);
            error_head(mux, sid, 503);
            tx.end();
            return;
        }
    };
    let url = format!("ws://{}:{}{path}", lease.host, lease.port);
    let mut req = match url.as_str().into_client_request() {
        Ok(r) => r,
        Err(_) => {
            error_head(mux, sid, 400);
            tx.end();
            return;
        }
    };
    for (k, v) in &headers {
        let lk = k.to_ascii_lowercase();
        if (lk == "sec-websocket-protocol" || lk == "cookie" || lk.starts_with("x-"))
            && let (Ok(n), Ok(v)) = (
                tokio_tungstenite::tungstenite::http::HeaderName::from_bytes(k.as_bytes()),
                v.parse(),
            )
        {
            req.headers_mut().insert(n, v);
        }
    }
    let ws = match tokio_tungstenite::connect_async(req).await {
        Ok((ws, _)) => ws,
        Err(_) => {
            error_head(mux, sid, 502);
            tx.end();
            return;
        }
    };
    mux.head(sid, 101, Vec::new());
    let (mut sink, mut stream) = ws.split();
    let up = async {
        while let Some(m) = rx.recv().await {
            let Ok(b) = m else { break };
            if b.is_empty() {
                continue;
            }
            let msg = match b[0] {
                WS_TEXT => Message::Text(String::from_utf8_lossy(&b[1..]).into_owned().into()),
                _ => Message::Binary(b.slice(1..)),
            };
            if sink.send(msg).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    };
    let down = async {
        while let Some(Ok(m)) = stream.next().await {
            let mut v;
            match m {
                Message::Text(t) => {
                    v = vec![WS_TEXT];
                    v.extend_from_slice(t.as_bytes());
                }
                Message::Binary(b) => {
                    v = vec![WS_BINARY];
                    v.extend_from_slice(&b);
                }
                Message::Close(_) => break,
                _ => continue,
            }
            if tx.send_message(&v).await.is_err() {
                return;
            }
        }
        tx.end();
    };
    tokio::select! {
        _ = up => {}
        _ = down => {}
    }
    drop(lease);
}

/// A raw TCP stream to the engine's port (Wyoming, 13.1 `method = "TCP"`): the head is 200 once
/// connected, then the stream's binary frames are the bytes each way.
async fn proxy_tcp(
    ctx: &Arc<Ctx>,
    mux: &Arc<Mux>,
    s: crate::mux::Stream,
    engine: &str,
    card: &str,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let sid = s.sid;
    let (tx, mut rx) = s.split();
    let lease = match ctx.acquire(engine, card).await {
        Ok(l) => l,
        Err(e) => {
            drop(rx);
            (ctx.log)(&format!("tcp to {engine} on {card}: {e}"));
            error_head(mux, sid, 503);
            tx.end();
            return;
        }
    };
    let conn = match tokio::net::TcpStream::connect((lease.host.as_str(), lease.port)).await {
        Ok(c) => c,
        Err(_) => {
            drop(rx);
            error_head(mux, sid, 502);
            tx.end();
            return;
        }
    };
    let _ = conn.set_nodelay(true);
    mux.head(sid, 200, Vec::new());
    let (mut r, mut w) = conn.into_split();
    let up = async {
        while let Some(m) = rx.recv().await {
            let Ok(b) = m else { break };
            if w.write_all(&b).await.is_err() {
                break;
            }
        }
        let _ = w.shutdown().await;
    };
    let down = async {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match r.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(&buf[..n]).await.is_err() {
                        return;
                    }
                }
            }
        }
        tx.end();
    };
    // both halves: a client that stops sending may still be reading the answer
    tokio::join!(up, down);
    drop(lease);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn download_paths_stay_under_the_model_dir() {
        let root = Path::new("/models");
        assert_eq!(
            under(root, "hf/a/b.gguf").unwrap(),
            Path::new("/models/hf/a/b.gguf")
        );
        assert!(under(root, "../etc/passwd").is_err());
        assert!(under(root, "a/../../x").is_err());
        assert!(under(root, "").is_err());
        assert!(under(root, "C:/x").is_err());
    }

    #[test]
    fn jitter_stays_within_a_quarter() {
        for _ in 0..50 {
            let j = jitter(Duration::from_secs(4));
            assert!(
                j >= Duration::from_secs(3) && j <= Duration::from_secs(5),
                "{j:?}"
            );
        }
    }
}
