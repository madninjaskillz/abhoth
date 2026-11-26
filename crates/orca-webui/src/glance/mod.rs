//! Glance: the GPU host's live dashboard inside the web UI (SUPERVISOR.md 12), with an optional
//! public, redacted read-only view on its own listener.
//!
//! The feed: the supervisor's (`abhoth-supervisor`, `/api/v1/supervisor/stream`) for what it
//! covers (GPUs, host, BMC, hwmon, fans, clocks, engines, leases, alerts, health), the router's
//! calendar, and for the panels nothing in Abhoth publishes yet the legacy dash (`/api/state`,
//! read-only GETs, as the `dash` section) or a recording of it. Off unless configured.

pub mod endpoints;
pub mod feed;
pub mod mock;
pub mod public;
pub mod redact;
pub mod supervisor;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use orca_sensors::feed::{FeedEvent, Severity, TelemetryFeed, TelemetrySink};
use orca_supervisor::{HubConfig, TelemetryHub};
use serde_json::Value;

pub use redact::Redaction;

#[derive(Debug, Clone)]
pub enum GlanceSource {
    /// A JSON-lines recording of states, replayed in a loop with the clock moved to now.
    Fixture(PathBuf),
    /// GET this URL for the full state each interval.
    Poll(String),
}

impl GlanceSource {
    /// `http(s)://…` polls; anything else is a fixture file.
    pub fn parse(text: &str) -> Self {
        if text.starts_with("http://") || text.starts_with("https://") {
            Self::Poll(text.to_string())
        } else {
            Self::Fixture(PathBuf::from(text))
        }
    }
}

#[derive(Debug, Clone)]
pub struct PublicOptions {
    /// "0.0.0.0:8077".
    pub listen: String,
    pub redaction: Redaction,
    /// Open streams per client address, and in all.
    pub streams_per_ip: usize,
    pub streams_total: usize,
}

impl PublicOptions {
    pub fn new(listen: impl Into<String>) -> Self {
        Self {
            listen: listen.into(),
            redaction: Redaction::default(),
            streams_per_ip: 4,
            streams_total: 64,
        }
    }
}

#[derive(Debug, Clone)]
pub struct GlanceOptions {
    /// The legacy dash (or a recording of it), for the panels the supervisor does not cover.
    pub source: Option<GlanceSource>,
    /// `abhoth-supervisor`'s feed (`http://127.0.0.1:9890`).
    pub supervisor: Option<String>,
    /// How often the source is read.
    pub interval: Duration,
    /// How often the hub flushes a patch (1 s in production, as the supervisor's hub).
    pub flush: Duration,
    /// The public read-only view; `None` = off (the default).
    pub public: Option<PublicOptions>,
    /// How often an idle stream sends `ping`.
    pub ping: Duration,
}

impl GlanceOptions {
    pub fn new(source: GlanceSource) -> Self {
        Self {
            source: Some(source),
            supervisor: None,
            interval: Duration::from_millis(2500),
            flush: Duration::from_secs(1),
            public: None,
            ping: Duration::from_secs(5),
        }
    }
}

/// The running feeds: the supervisor's telemetry hub for signed-in users, and a second hub
/// with the redacted sections for the public view. When the supervisor runs in-process, `full`
/// is its hub and the mock source is not started.
pub struct Glance {
    pub options: GlanceOptions,
    pub full: Arc<TelemetryHub>,
    pub public: Option<Arc<TelemetryHub>>,
    tasks: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
    public_addr: std::sync::Mutex<Option<std::net::SocketAddr>>,
    /// The supervisor's feed has a `calendar` section (then the router's is not polled in).
    supervisor_calendar: AtomicBool,
    /// The router's calendar answered (then a recording's calendar is left out).
    router_calendar: AtomicBool,
}

impl Glance {
    pub fn new(options: GlanceOptions) -> Arc<Self> {
        let public = options
            .public
            .as_ref()
            .map(|_| TelemetryHub::new(HubConfig::default()));
        Arc::new(Self {
            options,
            full: TelemetryHub::new(HubConfig::default()),
            public,
            tasks: Default::default(),
            public_addr: Default::default(),
            supervisor_calendar: AtomicBool::new(false),
            router_calendar: AtomicBool::new(false),
        })
    }

    /// The feed signed-in users read.
    pub fn full_feed(&self) -> Arc<dyn TelemetryFeed> {
        self.full.clone()
    }

    pub fn public_feed(&self) -> Option<Arc<dyn TelemetryFeed>> {
        self.public.clone().map(|p| p as Arc<dyn TelemetryFeed>)
    }

    /// Where the public view listens (once started).
    pub fn public_addr(&self) -> Option<std::net::SocketAddr> {
        *self.public_addr.lock().unwrap()
    }

    pub fn set_public_addr(&self, a: std::net::SocketAddr) {
        *self.public_addr.lock().unwrap() = Some(a);
    }

    /// Sets sections on the full hub, and redacted on the public one. Published at the next
    /// flush.
    pub fn set_sections(&self, sections: &serde_json::Map<String, Value>) {
        for (k, v) in sections {
            self.full.set_section(k, v.clone());
        }
        if let (Some(p), Some(o)) = (&self.public, &self.options.public) {
            for (k, v) in o.redaction.apply_sections(sections) {
                p.set_section(&k, v);
            }
        }
    }

    /// A legacy dash state (fixture or poll), as hub sections. With the supervisor's feed on,
    /// only what it does not cover (the `dash` section, and the chart history once).
    pub fn publish_legacy(&self, state: &Value, with_history: bool) {
        let mut s = mock::sections(state, with_history, self.options.supervisor.is_some());
        if self.router_calendar.load(Ordering::Relaxed) {
            s.remove("calendar");
        }
        self.set_sections(&s);
    }

    /// A source's trouble (or its end), in the `feed_errors` section.
    pub fn set_feed_error(&self, source: &str, error: Option<&str>) {
        let cur = self.full.section("feed_errors");
        let next = supervisor::feed_errors(cur.clone(), source, error);
        if cur.unwrap_or(Value::Null) != next {
            let mut m = serde_json::Map::new();
            m.insert("feed_errors".into(), next);
            self.set_sections(&m);
        }
    }

    /// Flushes both hubs now (tests; the flushers do it every `flush`).
    pub fn flush(&self) {
        let now = orca_sensors::now_ms();
        self.full.flush(now);
        if let Some(p) = &self.public {
            p.flush(now);
        }
    }

    /// An immediate event, to both feeds (the public one without error text).
    pub fn event(&self, event: FeedEvent) {
        if let (Some(p), Some(o)) = (&self.public, &self.options.public) {
            p.event(o.redaction.public_event(&event));
        }
        self.full.event(event);
    }

    /// Starts reading the sources; `router`: the calendar, when the supervisor has none.
    pub fn start(
        self: &Arc<Self>,
        http: reqwest::Client,
        router: Option<crate::router_admin::RouterLink>,
    ) -> Result<(), String> {
        let mut tasks = Vec::new();
        match self.options.source.clone() {
            Some(GlanceSource::Fixture(path)) => {
                let frames = load_fixture(&path)?;
                let me = self.clone();
                tasks.push(tokio::spawn(async move { me.replay(frames).await }));
            }
            Some(GlanceSource::Poll(url)) => {
                let me = self.clone();
                let h = http.clone();
                tasks.push(tokio::spawn(async move { me.poll(h, url).await }));
            }
            None => {}
        }
        if let Some(url) = self.options.supervisor.clone() {
            let me = self.clone();
            let h = http.clone();
            tasks.push(tokio::spawn(
                async move { me.follow_supervisor(h, url).await },
            ));
        }
        if let Some(link) = router {
            let me = self.clone();
            tasks.push(tokio::spawn(async move { me.follow_calendar(link).await }));
        }
        tasks.push(self.full.spawn_flusher(self.options.flush));
        if let Some(p) = &self.public {
            tasks.push(p.spawn_flusher(self.options.flush));
        }
        self.tasks.lock().unwrap().extend(tasks);
        Ok(())
    }

    /// The router's calendar (orca-calendar: what is on now, the blocks, and the next 24 h) as
    /// the `calendar` section, every 30 s. When the supervisor's feed carries `calendar` (now
    /// and blocks, without what comes next), only the next occurrences, as `calendar_next`.
    async fn follow_calendar(self: Arc<Self>, link: crate::router_admin::RouterLink) {
        let me = orca_core::router_link::ActingUser {
            user_id: 0,
            name: "glance".into(),
            admin: true,
        };
        let mut tick = tokio::time::interval(Duration::from_secs(30));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let supervised = self.supervisor_calendar.load(Ordering::Relaxed);
            let t = chrono::Utc::now().timestamp();
            let now = if supervised {
                Ok((200, Value::Null))
            } else {
                link.call("GET", "calendar/now", &me, &Value::Null).await
            };
            let occ = link
                .call(
                    "GET",
                    &format!(
                        "calendar/occurrences?from={}&to={}",
                        t - 6 * 3600,
                        t + 86_400
                    ),
                    &me,
                    &Value::Null,
                )
                .await;
            match (now, occ) {
                (Ok((200, now)), Ok((200, occ))) => {
                    self.set_feed_error("router calendar", None);
                    self.router_calendar.store(true, Ordering::Relaxed);
                    let mut m = serde_json::Map::new();
                    let cal = calendar_section(&now, &occ, t);
                    if supervised {
                        m.insert("calendar_next".into(), cal["next"].clone());
                    } else {
                        m.insert("calendar".into(), cal);
                    }
                    self.set_sections(&m);
                }
                (Ok((404, _)), _) | (_, Ok((404, _))) => {
                    // No calendar on this router.
                }
                (Err(e), _) | (_, Err(e)) => self.set_feed_error("router calendar", Some(&e)),
                (Ok((a, _)), Ok((b, _))) => {
                    let st = if a != 200 { a } else { b };
                    self.set_feed_error("router calendar", Some(&format!("HTTP {st}")))
                }
            }
        }
    }

    pub fn add_task(&self, t: tokio::task::JoinHandle<()>) {
        self.tasks.lock().unwrap().push(t);
    }

    pub fn stop(&self) {
        for t in self.tasks.lock().unwrap().drain(..) {
            t.abort();
        }
    }

    async fn replay(self: Arc<Self>, frames: Vec<Value>) {
        let mut tick = tokio::time::interval(self.options.interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut i = 0usize;
        let mut was_switching = false;
        loop {
            tick.tick().await;
            let f = restamp(&frames[i % frames.len()], now_secs());
            self.publish_legacy(&f, i == 0);
            // The recording's switch job, as the control plane's events will come.
            let switching = f["profile"].get("job").is_some_and(|j| !j.is_null());
            if switching != was_switching {
                self.event(FeedEvent {
                    kind: "switch.step".into(),
                    severity: Severity::Info,
                    text: if switching {
                        "layout switch started".into()
                    } else {
                        "layout switch finished".into()
                    },
                    data: Value::Null,
                });
                was_switching = switching;
            }
            i = i.wrapping_add(1);
        }
    }

    async fn poll(self: Arc<Self>, http: reqwest::Client, url: String) {
        let mut tick = tokio::time::interval(self.options.interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut first = true;
        loop {
            tick.tick().await;
            let got = async {
                let r = http
                    .get(&url)
                    .timeout(Duration::from_secs(5))
                    .send()
                    .await
                    .map_err(|e| e.without_url().to_string())?;
                if !r.status().is_success() {
                    return Err(format!("HTTP {}", r.status().as_u16()));
                }
                r.json::<Value>()
                    .await
                    .map_err(|e| e.without_url().to_string())
            }
            .await;
            // On failure the last state stays, with the error among the collector warnings.
            match got {
                Ok(v) if v.is_object() => {
                    self.publish_legacy(&v, first);
                    self.set_feed_error("old dash", None);
                    first = false;
                }
                Ok(_) => self.set_feed_error(
                    "old dash",
                    Some("it answered something that is not a state"),
                ),
                Err(e) => self.set_feed_error("old dash", Some(&e)),
            }
        }
    }
}

/// The router's `calendar/now` and `calendar/occurrences` as the `calendar` section:
/// `{at, active: [Occurrence], next: [Occurrence], blocks: [CalBlock], holders: {card: Holder}}`
/// (orca-calendar's shapes; `next`: not started, not skipped, by start).
pub fn calendar_section(now: &Value, occ: &Value, at: i64) -> Value {
    let all = occ["occurrences"].as_array().cloned().unwrap_or_default();
    let mut next: Vec<Value> = all
        .into_iter()
        .filter(|o| {
            o["block_start"].as_i64().is_some_and(|b| b > at) && o["skipped"] != Value::Bool(true)
        })
        .collect();
    next.sort_by_key(|o| o["start"].as_i64().unwrap_or(i64::MAX));
    next.truncate(6);
    serde_json::json!({
        "at": at,
        "active": now["active"].clone(),
        "blocks": now["blocks"].clone(),
        "holders": now["holders"].clone(),
        "next": next,
    })
}

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Reads a JSON-lines recording (blank lines skipped).
pub fn load_fixture(path: &std::path::Path) -> Result<Vec<Value>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let frames: Vec<Value> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .enumerate()
        .map(|(n, l)| {
            serde_json::from_str(l).map_err(|e| format!("{} line {}: {e}", path.display(), n + 1))
        })
        .collect::<Result<_, _>>()?;
    if frames.is_empty() {
        return Err(format!("{}: no states", path.display()));
    }
    Ok(frames)
}

/// The state without its history (`hist`, `gpus[].hist`, `llm.*.hist`, `llm.*.slot_hist`).
pub fn feed_strip_hist(v: &mut Value) {
    let Some(o) = v.as_object_mut() else { return };
    o.remove("hist");
    if let Some(gpus) = o.get_mut("gpus").and_then(Value::as_array_mut) {
        for g in gpus.iter_mut().filter_map(Value::as_object_mut) {
            g.remove("hist");
        }
    }
    if let Some(llm) = o.get_mut("llm").and_then(Value::as_object_mut) {
        for i in llm.values_mut().filter_map(Value::as_object_mut) {
            i.remove("hist");
            i.remove("slot_hist");
        }
    }
}

/// A recorded state moved to `now`: its clock, its history and request times, and the news
/// log's mtime shift by the same amount.
pub fn restamp(frame: &Value, now: f64) -> Value {
    let mut v = frame.clone();
    let Some(t) = v.get("time").and_then(Value::as_f64) else {
        return v;
    };
    let delta = now - t;
    let shift = |x: &mut Value| {
        if let Some(n) = x.as_f64() {
            *x = serde_json::json!(n + delta);
        }
    };
    let shift_points = |h: &mut Value| {
        for series in h.as_object_mut().into_iter().flat_map(|m| m.values_mut()) {
            for p in series.as_array_mut().into_iter().flatten() {
                if let Some(t) = p.get_mut(0) {
                    shift(t);
                }
            }
        }
    };
    if let Some(o) = v.as_object_mut() {
        if let Some(x) = o.get_mut("time") {
            shift(x);
        }
        if let Some(h) = o.get_mut("hist") {
            shift_points(h);
        }
        if let Some(x) = o.get_mut("news").and_then(|n| n.get_mut("mtime")) {
            shift(x);
        }
        for g in o
            .get_mut("gpus")
            .and_then(Value::as_array_mut)
            .into_iter()
            .flatten()
        {
            if let Some(h) = g.get_mut("hist") {
                shift_points(h);
            }
        }
        for i in o
            .get_mut("llm")
            .and_then(Value::as_object_mut)
            .into_iter()
            .flat_map(|m| m.values_mut())
        {
            if let Some(h) = i.get_mut("hist").and_then(Value::as_array_mut) {
                for p in h.iter_mut() {
                    if let Some(t) = p.get_mut(0) {
                        shift(t);
                    }
                }
            }
            for r in i
                .get_mut("requests")
                .and_then(Value::as_array_mut)
                .into_iter()
                .flatten()
            {
                if let Some(t) = r.get_mut("time") {
                    shift(t);
                }
            }
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn restamp_moves_every_clock() {
        let f = json!({
            "time": 100.0,
            "hist": {"cpu": [[90.0, 1], [100.0, 2]]},
            "gpus": [{"hist": {"t": [[95.0, 3]]}}],
            "llm": {"a": {"hist": [[99.0, 4]], "requests": [{"time": 98.0}]}},
            "news": {"mtime": 50.0},
        });
        let v = restamp(&f, 1000.0);
        assert_eq!(v["time"], 1000.0);
        assert_eq!(v["hist"]["cpu"][0][0], 990.0);
        assert_eq!(v["gpus"][0]["hist"]["t"][0][0], 995.0);
        assert_eq!(v["llm"]["a"]["hist"][0][0], 999.0);
        assert_eq!(v["llm"]["a"]["requests"][0]["time"], 998.0);
        assert_eq!(v["news"]["mtime"], 950.0);
    }

    #[test]
    fn sources_parse() {
        assert!(matches!(
            GlanceSource::parse("http://x:1/api/state"),
            GlanceSource::Poll(_)
        ));
        assert!(matches!(
            GlanceSource::parse("C:/f.jsonl"),
            GlanceSource::Fixture(_)
        ));
    }

    #[test]
    fn the_recorded_fixture_loads_and_redacts_cleanly() {
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/glance-feed.jsonl");
        let frames = load_fixture(&path).unwrap();
        assert!(frames.len() > 10);
        let g = Glance::new(GlanceOptions {
            public: Some(PublicOptions::new("127.0.0.1:0")),
            ..GlanceOptions::new(GlanceSource::Fixture(path))
        });
        for (n, f) in frames.iter().enumerate() {
            g.publish_legacy(&restamp(f, now_secs()), n == 0);
            g.flush();
        }
        let public = g.public.as_ref().unwrap().snapshot().state.to_string();
        assert!(!public.contains("\"requests\":["));
        assert!(!public.contains("\"containers\""));
        assert!(!public.contains("oom-victim"));
        assert!(!public.contains("last_error"));
        let full = g.full.snapshot().state.to_string();
        assert!(full.contains("\"requests\":["));
        assert!(full.contains("\"history\""));
    }
}
