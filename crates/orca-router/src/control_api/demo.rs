//! An in-memory [`ControlBackend`] with a made-up two-card GPU host: the admin pages' tests,
//! screenshots and UI development run against it (`abhoth router admin-demo`). It never runs a
//! command; a "switch" walks the 12 steps of 5.4 on a timer. Names and ids are placeholders.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;
use tokio::sync::broadcast;

use super::model::*;
use super::{ApiError, ApiResult, ControlBackend, Event};

const STEPS: [&str; 12] = [
    "Plan and preconditions",
    "Guards",
    "Prepare (model files, secret files)",
    "Withdraw slots",
    "Lease check",
    "Drain",
    "Lock",
    "Stop",
    "Start",
    "Verify",
    "Commit",
    "Post-checks",
];

pub const DEMO_CONFIG: &str = r#"# router.toml (demo): abhoth router config. `abhoth router config check` validates.
version = 1

[router]
listen   = "0.0.0.0:8010"
timezone = "Europe/London"

[control]
enabled = true
mode    = "act"

[access]
accounts_url   = "http://abhoth-web:8192"
accounts_token = "secret:accounts_token"

[secrets]
engine_key     = { file = "/run/secrets/engine-key" }
accounts_token = { file = "/run/secrets/abhoth-link" }

[servers.gpu-host]
title = "GPU host"
host  = "gpu-host"
exec  = "ssh"

[cards.A]
server   = "gpu-host"
gpu_uuid = "GPU-aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"
label    = "V100-SXM3 32 GB (fast)"
vram_gib = 32.0

[cards.B]
server   = "gpu-host"
gpu_uuid = "GPU-bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb"
label    = "V100-PCIe 32 GB"
vram_gib = 32.0

[layouts.merged-inkling-x2]
title   = "Inkling 2 Mini on both cards"
cards.A = { engine = "ninfer", profile = "merged-inkling-4lane", model = "inkling2-mini" }
cards.B = "same"

[layouts.merged-a-inkling-b]
title   = "Dense 27B on A + Inkling 2 Mini on B"
cards.A = { engine = "ninfer", profile = "merged-4lane", model = "dense-pi-abl-v3fork" }
cards.B = { engine = "ninfer", profile = "merged-inkling-4lane", model = "inkling2-mini" }

[layouts.flash-x2]
title   = "Flash on both cards"
cards.A = { engine = "strata", profile = "3slot", model = "flash-next" }
cards.B = { engine = "strata", profile = "2slot", model = "flash-next" }

[routes."qwen3.8-27b"]
targets = [{ pool = "live" }]

[routes."inkling-2-mini"]
targets  = [{ model = "inkling2-mini" }]
cold     = "fallback"
fallback = ["qwen3.8-27b"]

[routes."dense-stock"]
targets = [{ model = "dense-stock" }]
cold    = "reject"
"#;

struct Inner {
    state: StateView,
    config: ConfigDoc,
    jobs: Vec<Job>,
    audit: Vec<AuditRow>,
    leases: LeasesView,
    routes: Vec<RouteView>,
    next_job: u64,
}

pub struct DemoControl {
    inner: Arc<Mutex<Inner>>,
    tx: broadcast::Sender<Event>,
    /// Seconds per switch step (tests use 0).
    pub step_seconds: f64,
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub fn rev_of(text: &str) -> String {
    use sha2::{Digest, Sha256};
    let normal: String = text
        .lines()
        .map(|l| l.split('#').next().unwrap_or("").trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    Sha256::digest(normal.as_bytes())
        .iter()
        .take(6)
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn tel(name: &str, used: u64, util: u32, temp: u32, power: f64, sm: u32, max: u32) -> Telemetry {
    Telemetry {
        name: name.into(),
        mem_used_mib: used,
        mem_total_mib: 32768,
        util_pct: util,
        temp_c: temp,
        mem_temp_c: Some(temp + 4),
        power_w: power,
        power_limit_w: Some(300.0),
        sm_mhz: sm,
        sm_max_mhz: max,
        app_clock_mhz: Some(1380),
        clock_reasons: if util == 0 {
            vec!["idle".into()]
        } else {
            vec![]
        },
        clock_check: "ok".into(),
        age_ms: 800,
        stale: false,
    }
}

fn card(id: &str, label: &str, engine: Option<EngineView>, t: Telemetry, lanes: u32) -> CardView {
    CardView {
        id: id.into(),
        server: "gpu-host".into(),
        gpu_uuid: format!("GPU-{0}{0}{0}{0}{0}{0}{0}{0}-…", id.to_lowercase()),
        label: label.into(),
        managed: true,
        admin_state: "online".into(),
        admin_note: None,
        engine,
        telemetry: Some(t),
        lanes,
        inflight: 0,
        queued: 0,
        leases: 0,
        on_demand: true,
        foreign: vec![],
    }
}

fn inkling(since: i64) -> EngineView {
    EngineView {
        engine: "ninfer".into(),
        profile: "merged-inkling-4lane".into(),
        model: "inkling2-mini".into(),
        served_id: "inkling-2-mini".into(),
        state: "ready".into(),
        since,
        healthy: true,
    }
}

fn layouts(active: &str) -> Vec<LayoutView> {
    let mk = |id: &str, title: &str, a: &str, b: &str| LayoutView {
        id: id.into(),
        title: title.into(),
        server: "gpu-host".into(),
        mode: "auto".into(),
        cards: BTreeMap::from([("A".into(), a.into()), ("B".into(), b.into())]),
        active: id == active,
        source: "config".into(),
    };
    vec![
        mk(
            "merged-inkling-x2",
            "Inkling 2 Mini on both cards",
            "ninfer / merged-inkling-4lane / inkling2-mini",
            "same",
        ),
        mk(
            "merged-a-inkling-b",
            "Dense 27B on A + Inkling 2 Mini on B",
            "ninfer / merged-4lane / dense-pi-abl-v3fork",
            "ninfer / merged-inkling-4lane / inkling2-mini",
        ),
        mk(
            "flash-x2",
            "Flash on both cards",
            "strata / 3slot / flash-next",
            "strata / 2slot / flash-next",
        ),
    ]
}

impl DemoControl {
    pub fn new() -> Arc<Self> {
        let since = now_ms() - 3 * 3600 * 1000;
        let mut a = card(
            "A",
            "V100-SXM3 32 GB (fast)",
            Some(inkling(since)),
            tel("Tesla V100-SXM3-32GB", 30410, 63, 61, 212.4, 1597, 1597),
            4,
        );
        a.inflight = 3;
        a.leases = 2;
        let mut b = card(
            "B",
            "V100-PCIe 32 GB",
            Some(inkling(since)),
            tel("Tesla V100-PCIE-32GB", 30102, 0, 38, 41.0, 135, 1380),
            4,
        );
        b.leases = 1;
        let mut c = card(
            "C",
            "V100-SXM2 16 GB (image API)",
            None,
            tel("Tesla V100-SXM2-16GB", 9120, 12, 44, 61.0, 1290, 1530),
            0,
        );
        c.managed = false;
        c.admin_state = "unmanaged".into();
        c.foreign = vec!["comfyui".into()];
        c.telemetry.as_mut().unwrap().mem_total_mib = 16384;
        let state = StateView {
            rev: rev_of(DEMO_CONFIG),
            config_error: None,
            mode: "act".into(),
            servers: vec![ServerView {
                id: "gpu-host".into(),
                title: "GPU host".into(),
                reachability: "ok".into(),
                exec: "ssh".into(),
                active_layout: Some("merged-inkling-x2".into()),
                drift: false,
                hostctl_ok: true,
                cards: vec![a, b, c],
            }],
            guards: vec![
                GuardView {
                    id: "quiet-window".into(),
                    kind: "schedule".into(),
                    active: false,
                    stale: false,
                    blocks: vec!["switch".into(), "on_demand".into()],
                    reason: Some("06:45-07:30".into()),
                    retry_seconds: 0,
                },
                GuardView {
                    id: "bench".into(),
                    kind: "json_file".into(),
                    active: true,
                    stale: false,
                    blocks: vec!["on_demand".into()],
                    reason: Some("bench queue: running (2 of 5)".into()),
                    retry_seconds: 600,
                },
                GuardView {
                    id: "video".into(),
                    kind: "process".into(),
                    active: false,
                    stale: false,
                    blocks: vec!["on_demand".into()],
                    reason: None,
                    retry_seconds: 300,
                },
                GuardView {
                    id: "music".into(),
                    kind: "file_exists".into(),
                    active: false,
                    stale: false,
                    blocks: vec!["switch".into(), "on_demand".into(), "ensure".into()],
                    reason: None,
                    retry_seconds: 0,
                },
            ],
            job: None,
            layouts: layouts("merged-inkling-x2"),
            updated: now_ms(),
        };
        let t0 = now_ms() / 1000;
        let done_job = Job {
            id: "j-0041".into(),
            server: "gpu-host".into(),
            kind: "switch".into(),
            from: Some("merged-a-inkling-b".into()),
            target: "merged-inkling-x2".into(),
            requested_by: "web:admin#1".into(),
            state: "done".into(),
            started: (t0 - 3 * 3600) * 1000,
            ended: Some((t0 - 3 * 3600 + 51) * 1000),
            error: None,
            steps: STEPS
                .iter()
                .enumerate()
                .map(|(i, t)| JobStep {
                    n: i as u32 + 1,
                    title: (*t).into(),
                    state: "ok".into(),
                    seconds: [0.4, 0.1, 0.0, 0.0, 1.5, 0.2, 0.1, 6.8, 38.2, 2.9, 0.1, 0.9][i],
                    output: (i == 8).then(|| "card A: healthy after 36.1 s\n".into()),
                })
                .collect(),
        };
        let refused = Job {
            id: "j-0040".into(),
            server: "gpu-host".into(),
            kind: "switch".into(),
            from: Some("merged-a-inkling-b".into()),
            target: "flash-x2".into(),
            requested_by: "service:jarvis#3".into(),
            state: "refused".into(),
            started: (t0 - 5 * 3600) * 1000,
            ended: Some((t0 - 5 * 3600 + 2) * 1000),
            error: Some("lease A2 held by bench-lead (main)".into()),
            steps: vec![JobStep {
                n: 5,
                title: "Lease check".into(),
                state: "failed".into(),
                seconds: 1.5,
                output: None,
            }],
        };
        let audit = vec![
            AuditRow {
                id: 3,
                time: (t0 - 3 * 3600) * 1000,
                who: "web:admin#1".into(),
                action: "switch".into(),
                target: "gpu-host → merged-inkling-x2".into(),
                ..Default::default()
            },
            AuditRow {
                id: 2,
                time: (t0 - 4 * 3600) * 1000,
                who: "web:admin#1".into(),
                action: "config.save".into(),
                target: "router.toml".into(),
                rev_from: Some("1f0c2a9e44b0".into()),
                rev_to: Some(rev_of(DEMO_CONFIG)),
                diff: Some("-cold = \"reject\"\n+cold = \"fallback\"".into()),
                reason: Some("inkling falls back to the live model".into()),
            },
            AuditRow {
                id: 1,
                time: (t0 - 26 * 3600) * 1000,
                who: "file".into(),
                action: "config.reload".into(),
                target: "router.toml".into(),
                ..Default::default()
            },
        ];
        let leases = LeasesView {
            frozen: false,
            leases: vec![
                LeaseView {
                    key: "bench-lead".into(),
                    slot: "A1".into(),
                    card: "A".into(),
                    kind: "main".into(),
                    holder: "bench run 2 of 5".into(),
                    principal: "service:bench#6".into(),
                    expires_in_seconds: 71,
                    reason: None,
                },
                LeaseView {
                    key: "agent-7f3".into(),
                    slot: "A2".into(),
                    card: "A".into(),
                    kind: "agent".into(),
                    holder: "abhoth worker".into(),
                    principal: "user:bob#2".into(),
                    expires_in_seconds: 44,
                    reason: None,
                },
                LeaseView {
                    key: "bench-guard".into(),
                    slot: "-".into(),
                    card: "B".into(),
                    kind: "guard".into(),
                    holder: "bench".into(),
                    principal: "service:bench#6".into(),
                    expires_in_seconds: 88,
                    reason: Some("bench".into()),
                },
            ],
        };
        let route =
            |name: &str, targets: &[&str], cold: &str, fb: &[&str], access: &str, now: &[&str]| {
                RouteView {
                    name: name.into(),
                    targets: targets.iter().map(|s| s.to_string()).collect(),
                    cold: cold.into(),
                    fallback: fb.iter().map(|s| s.to_string()).collect(),
                    access: access.into(),
                    adapters: vec![],
                    hidden: false,
                    same_as: None,
                    serving_now: now.iter().map(|s| s.to_string()).collect(),
                    allow_user_switch: false,
                    model: targets
                        .first()
                        .and_then(|t| t.strip_prefix("model:"))
                        .map(str::to_string),
                    vision: None,
                }
            };
        let mut routes = vec![
            route(
                "qwen3.8-27b",
                &["pool:live"],
                "reject",
                &[],
                "users",
                &["card A (ninfer)", "card B (ninfer)"],
            ),
            route(
                "qwen3.8-27b-a",
                &["card:A", "card:B (fallback)"],
                "reject",
                &[],
                "users",
                &["card A (ninfer)"],
            ),
            route(
                "inkling-2-mini",
                &["model:inkling2-mini"],
                "fallback",
                &["qwen3.8-27b"],
                "users",
                &["card A (ninfer)", "card B (ninfer)"],
            ),
            route(
                "dense-stock",
                &["model:dense-stock"],
                "reject",
                &[],
                "users",
                &[],
            ),
            route(
                "claude-sonnet",
                &["upstream:anthropic/claude-sonnet-5-5"],
                "reject",
                &[],
                "admins",
                &["anthropic"],
            ),
        ];
        // The stock weights take no images (catalog `vision` unset).
        if let Some(r) = routes.iter_mut().find(|r| r.name == "dense-stock") {
            r.vision = Some(false);
        }
        let (tx, _) = broadcast::channel(256);
        Arc::new(Self {
            inner: Arc::new(Mutex::new(Inner {
                state,
                config: ConfigDoc {
                    rev: rev_of(DEMO_CONFIG),
                    path: "/state/router.toml".into(),
                    text: DEMO_CONFIG.into(),
                    error: None,
                },
                jobs: vec![done_job, refused],
                audit,
                leases,
                routes,
                next_job: 42,
            })),
            tx,
            step_seconds: 1.2,
        })
    }

    /// Wiggle the telemetry every 2 s and publish it (`telemetry` events).
    pub fn spawn_ticker(self: &Arc<Self>) {
        let me = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut n: u64 = 0;
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                let Some(me) = me.upgrade() else { break };
                n += 1;
                let cards = {
                    let mut g = me.inner.lock().unwrap();
                    g.state.updated = now_ms();
                    for c in g.state.servers.iter_mut().flat_map(|s| s.cards.iter_mut()) {
                        let busy = c.inflight > 0;
                        if let Some(t) = c.telemetry.as_mut() {
                            let w = ((n * 7 + c.id.len() as u64 * 13) % 11) as f64;
                            if busy {
                                t.util_pct = 55 + (w as u32 * 3) % 40;
                                t.power_w = 190.0 + w * 6.0;
                                t.temp_c = 60 + (w as u32) % 5;
                            }
                            t.age_ms = 300 + (w as u64) * 90;
                        }
                    }
                    g.state
                        .servers
                        .iter()
                        .flat_map(|s| s.cards.clone())
                        .collect::<Vec<_>>()
                };
                let _ = me.tx.send(Event {
                    kind: "telemetry",
                    data: json!({"cards": cards, "updated": now_ms()}),
                });
            }
        });
    }

    fn publish_state(&self) {
        let st = self.inner.lock().unwrap().state.clone();
        let _ = self.tx.send(Event {
            kind: "state",
            data: serde_json::to_value(st).unwrap_or_default(),
        });
    }

    fn audit(&self, who: &str, action: &str, target: &str, extra: impl FnOnce(&mut AuditRow)) {
        let mut g = self.inner.lock().unwrap();
        let id = g.audit.iter().map(|a| a.id).max().unwrap_or(0) + 1;
        let mut row = AuditRow {
            id,
            time: now_ms(),
            who: who.into(),
            action: action.into(),
            target: target.into(),
            ..Default::default()
        };
        extra(&mut row);
        g.audit.insert(0, row.clone());
        drop(g);
        let _ = self.tx.send(Event {
            kind: "audit",
            data: serde_json::to_value(row).unwrap_or_default(),
        });
    }

    fn layout_cards(&self, id: &str) -> Option<BTreeMap<String, String>> {
        let g = self.inner.lock().unwrap();
        g.state
            .layouts
            .iter()
            .find(|l| l.id == id)
            .map(|l| l.cards.clone())
    }
}

/// The demo's validation: inline keys, unbalanced brackets and quotes, missing `version`.
pub fn demo_check(text: &str) -> Vec<Problem> {
    let mut out = Vec::new();
    if !text.lines().any(|l| l.trim_start().starts_with("version")) {
        out.push(Problem {
            path: "version".into(),
            line: Some(1),
            message: "version = 1 is required".into(),
            severity: "error".into(),
        });
    }
    let mut section = String::new();
    for (i, l) in text.lines().enumerate() {
        let t = l.split('#').next().unwrap_or("").trim();
        let line = Some(i as u32 + 1);
        if t.starts_with('[') {
            if !t.ends_with(']') {
                out.push(Problem {
                    path: section.clone(),
                    line,
                    message: "unclosed table header".into(),
                    severity: "error".into(),
                });
            }
            section = t.trim_matches(['[', ']']).to_string();
            continue;
        }
        if t.is_empty() {
            continue;
        }
        let key = t.split('=').next().unwrap_or("").trim();
        let path = if section.is_empty() {
            key.to_string()
        } else {
            format!("{section}.{key}")
        };
        if !t.contains('=') && !t.starts_with(']') && !t.starts_with('}') && !t.ends_with(',') {
            out.push(Problem {
                path: path.clone(),
                line,
                message: "expected key = value".into(),
                severity: "error".into(),
            });
        }
        if t.matches('"').count() % 2 == 1 {
            out.push(Problem {
                path: path.clone(),
                line,
                message: "unterminated string".into(),
                severity: "error".into(),
            });
        }
        for marker in ["\"abk_", "\"sk-", "\"Bearer "] {
            if t.contains(marker) {
                out.push(Problem {
                    path: path.clone(),
                    line,
                    message:
                        "looks like a key: put it in [secrets] and reference it as secret:<id>"
                            .into(),
                    severity: "error".into(),
                });
            }
        }
        if path.ends_with("cold") && t.contains("on_demand") && section.contains("live") {
            out.push(Problem {
                path,
                line,
                message: "on_demand only on routes whose targets are catalog models".into(),
                severity: "error".into(),
            });
        }
    }
    out
}

fn diff(old: &str, new: &str) -> String {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let mut out = String::new();
    let (mut i, mut j) = (0, 0);
    while i < a.len() || j < b.len() {
        if i < a.len() && j < b.len() && a[i] == b[j] {
            i += 1;
            j += 1;
        } else if j < b.len() && (i >= a.len() || !a[i..].contains(&b[j])) {
            out.push_str(&format!("+{}\n", b[j]));
            j += 1;
        } else {
            out.push_str(&format!("-{}\n", a[i]));
            i += 1;
        }
    }
    out
}

#[async_trait::async_trait]
impl ControlBackend for DemoControl {
    fn state(&self) -> StateView {
        self.inner.lock().unwrap().state.clone()
    }
    fn config(&self) -> ConfigDoc {
        self.inner.lock().unwrap().config.clone()
    }
    fn jobs(&self, limit: usize) -> Vec<Job> {
        let g = self.inner.lock().unwrap();
        let mut v = g.jobs.clone();
        v.sort_by_key(|j| std::cmp::Reverse(j.started));
        v.truncate(limit);
        v
    }
    fn job(&self, id: &str) -> Option<Job> {
        self.inner
            .lock()
            .unwrap()
            .jobs
            .iter()
            .find(|j| j.id == id)
            .cloned()
    }
    fn audit(&self, limit: usize, before: Option<i64>) -> Vec<AuditRow> {
        self.inner
            .lock()
            .unwrap()
            .audit
            .iter()
            .filter(|a| before.is_none_or(|b| a.id < b))
            .take(limit)
            .cloned()
            .collect()
    }
    fn leases(&self) -> LeasesView {
        self.inner.lock().unwrap().leases.clone()
    }
    fn routes(&self) -> Vec<RouteView> {
        self.inner.lock().unwrap().routes.clone()
    }
    fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }

    async fn check_config(&self, text: &str) -> CheckResult {
        let problems = demo_check(text);
        let cur = self.config();
        let ok = !problems.iter().any(|p| p.severity == "error");
        let d = diff(&cur.text, text);
        let mut consequences = Vec::new();
        if d.lines().any(|l| l.contains("cold")) {
            consequences.push("route cold policies change for new requests".to_string());
        }
        if d.lines().any(|l| l.contains("cards.")) {
            consequences.push(
                "a layout definition changed: if it is the active one, Overview shows drift with a plan (nothing restarts)"
                    .to_string(),
            );
        }
        CheckResult {
            ok,
            problems,
            diff: d,
            consequences,
            plan: None,
            rev: Some(rev_of(text)),
        }
    }

    async fn save_config(
        &self,
        text: &str,
        if_match: &str,
        reason: &str,
        who: &str,
    ) -> ApiResult<CheckResult> {
        let cur = self.config();
        if if_match != cur.rev {
            return Err(ApiError {
                status: 412,
                message: "the file changed since you loaded it; reload and merge".into(),
                extra: Some(json!({"currentRev": cur.rev})),
            });
        }
        let check = self.check_config(text).await;
        if !check.ok {
            return Err(ApiError {
                status: 422,
                message: "the config has errors; nothing was saved".into(),
                extra: Some(json!({"problems": check.problems})),
            });
        }
        let rev = rev_of(text);
        {
            let mut g = self.inner.lock().unwrap();
            g.config.text = text.into();
            g.config.rev = rev.clone();
            g.state.rev = rev.clone();
        }
        let d = check.diff.clone();
        self.audit(who, "config.save", "router.toml", |r| {
            r.rev_from = Some(cur.rev.clone());
            r.rev_to = Some(rev.clone());
            r.diff = Some(d);
            r.reason = (!reason.is_empty()).then(|| reason.to_string());
        });
        let _ = self.tx.send(Event {
            kind: "config",
            data: json!({"rev": rev}),
        });
        Ok(check)
    }

    async fn plan(&self, server: &str, layout: &str) -> ApiResult<Plan> {
        let st = self.state();
        let Some(srv) = st.servers.iter().find(|s| s.id == server) else {
            return Err(ApiError::new(404, "no such server"));
        };
        let Some(to) = self.layout_cards(layout) else {
            return Err(ApiError::new(404, "no such layout"));
        };
        let from_id = srv.active_layout.clone();
        let from = from_id
            .as_deref()
            .and_then(|f| self.layout_cards(f))
            .unwrap_or_default();
        let resolve = |m: &BTreeMap<String, String>, c: &str| {
            let v = m.get(c).cloned().unwrap_or_else(|| "off".into());
            if v == "same" {
                m.get("A").cloned().unwrap_or_default()
            } else {
                v
            }
        };
        let mut cards = Vec::new();
        let mut moving = Vec::new();
        for c in ["A", "B"] {
            let (b, a) = (resolve(&from, c), resolve(&to, c));
            let change = a != b;
            let strata_coupled = c == "B"
                && a.starts_with("strata")
                && b.starts_with("strata")
                && cards.iter().any(|x: &PlanCard| x.action != "keep");
            let action = if change || strata_coupled {
                "restart"
            } else {
                "keep"
            };
            if action != "keep" {
                moving.push(c.to_string());
            }
            cards.push(PlanCard {
                card: c.into(),
                before: b.clone(),
                after: a.clone(),
                action: action.into(),
                reason: if change {
                    "spec changed".into()
                } else if strata_coupled {
                    "coupling expert-arena with A".into()
                } else {
                    "unchanged".into()
                },
            });
        }
        let card_of = |c: &str| srv.cards.iter().find(|x| x.id == c);
        let leases = self.leases();
        let blockers: Vec<Blocker> = leases
            .leases
            .iter()
            .filter(|l| moving.contains(&l.card) && l.kind != "guard")
            .map(|l| Blocker {
                kind: "lease".into(),
                id: l.slot.clone(),
                reason: format!("{} lease on card {}", l.kind, l.card),
                holder: Some(format!("{} ({})", l.key, l.principal)),
                retry_seconds: l.expires_in_seconds,
            })
            .chain(
                st.guards
                    .iter()
                    .filter(|g| g.active && g.blocks.iter().any(|b| b == "switch"))
                    .map(|g| Blocker {
                        kind: "guard".into(),
                        id: g.id.clone(),
                        reason: g.reason.clone().unwrap_or_default(),
                        holder: None,
                        retry_seconds: g.retry_seconds,
                    }),
            )
            .collect();
        let restores = if layout == "flash-x2" {
            vec![Restore {
                file: "qwen3.8-flash-next-q4.strata".into(),
                bytes: 19_800_000_000,
                seconds: 41,
            }]
        } else {
            vec![]
        };
        let est: u64 = if moving.is_empty() {
            0
        } else {
            10 + 40 * moving.len() as u64 + restores.iter().map(|r| r.seconds).sum::<u64>()
        };
        let steps: Vec<PlanStep> = if moving.is_empty() {
            vec![]
        } else {
            STEPS
                .iter()
                .enumerate()
                .map(|(i, t)| PlanStep {
                    n: i as u32 + 1,
                    title: (*t).into(),
                    detail: match i {
                        2 if !restores.is_empty() => {
                            "restore 1 file from the archive (sha256 checked)".into()
                        }
                        3 => format!("withdraw slots of card {}", moving.join(", ")),
                        7 => format!(
                            "stop card {}; wait until under 1 GiB used",
                            moving.join(", ")
                        ),
                        8 => format!("start card {} and wait for /health", moving.join(", ")),
                        9 => "one-token chat on restarted cards; clocks; compute apps".into(),
                        _ => String::new(),
                    },
                    estimate_seconds: match i {
                        2 => restores.iter().map(|r| r.seconds).sum(),
                        7 => 8,
                        8 => 38 * moving.len() as u64,
                        9 => 3,
                        _ => 0,
                    },
                })
                .collect()
        };
        let drains = moving
            .iter()
            .filter_map(|c| card_of(c))
            .filter(|c| c.inflight > 0)
            .map(|c| Drain {
                card: c.id.clone(),
                inflight: c.inflight,
            })
            .collect();
        let slots = |cs: &[&str]| -> Vec<String> {
            cs.iter()
                .flat_map(|c| (1..=4).map(move |n| format!("{c}{n}")))
                .collect()
        };
        let preconditions = vec![
            Check {
                name: "builds present".into(),
                status: "pass".into(),
                reason: None,
            },
            Check {
                name: "images present".into(),
                status: "pass".into(),
                reason: None,
            },
            Check {
                name: "model files present or restorable".into(),
                status: "pass".into(),
                reason: (!restores.is_empty()).then(|| "1 file to restore".into()),
            },
            Check {
                name: "templates render".into(),
                status: "pass".into(),
                reason: None,
            },
            Check {
                name: "hostctl allowlist up to date".into(),
                status: "pass".into(),
                reason: None,
            },
            Check {
                name: "VRAM fits (model + profile ≤ card − reserve)".into(),
                status: "pass".into(),
                reason: None,
            },
        ];
        Ok(Plan {
            server: server.into(),
            from: from_id.clone(),
            to: layout.into(),
            runnable: blockers.is_empty() && !moving.is_empty(),
            note: moving.is_empty().then(|| "already live".into()),
            cards,
            steps,
            preconditions,
            restores,
            drains,
            routes: if moving.is_empty() {
                vec![]
            } else {
                vec![RouteChange {
                    route: "inkling-2-mini".into(),
                    before: vec!["card A".into(), "card B".into()],
                    after: if layout == "flash-x2" {
                        vec![]
                    } else {
                        vec!["card B".into()]
                    },
                }]
            },
            slots_before: slots(&["A", "B"]),
            slots_after: slots(&["A", "B"]),
            estimate_seconds: est,
            blockers,
        })
    }

    async fn switch(&self, server: &str, layout: &str, who: &str) -> ApiResult<Job> {
        let plan = self.plan(server, layout).await?;
        if let Some(j) = self.state().job {
            return Err(ApiError::new(
                409,
                format!("job {} is running on this server", j.id),
            ));
        }
        if plan.note.as_deref() == Some("already live") {
            return Err(ApiError::new(409, "already live"));
        }
        if !plan.blockers.is_empty() {
            return Err(ApiError {
                status: 409,
                message: format!(
                    "refused: {}",
                    plan.blockers
                        .iter()
                        .map(|b| format!("{} {}", b.kind, b.id))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                extra: Some(json!({"plan": plan})),
            });
        }
        let id = {
            let mut g = self.inner.lock().unwrap();
            g.next_job += 1;
            format!("j-{:04}", g.next_job)
        };
        let job = Job {
            id: id.clone(),
            server: server.into(),
            kind: "switch".into(),
            from: plan.from.clone(),
            target: layout.into(),
            requested_by: who.into(),
            state: "running".into(),
            started: now_ms(),
            ended: None,
            error: None,
            steps: STEPS
                .iter()
                .enumerate()
                .map(|(i, t)| JobStep {
                    n: i as u32 + 1,
                    title: (*t).into(),
                    state: "pending".into(),
                    seconds: 0.0,
                    output: None,
                })
                .collect(),
        };
        {
            let mut g = self.inner.lock().unwrap();
            g.jobs.push(job.clone());
            g.state.job = Some(JobSummary {
                id: id.clone(),
                server: server.into(),
                kind: "switch".into(),
                target: layout.into(),
                state: "running".into(),
                step: 1,
                steps: STEPS.len() as u32,
                title: STEPS[0].into(),
                started: job.started,
                estimate_seconds: plan.estimate_seconds,
            });
        }
        self.audit(who, "switch", &format!("{server} → {layout}"), |_| {});
        self.publish_state();
        let inner = self.inner.clone();
        let tx = self.tx.clone();
        let step = self.step_seconds;
        let moving: Vec<String> = plan
            .cards
            .iter()
            .filter(|c| c.action != "keep")
            .map(|c| c.card.clone())
            .collect();
        let layout = layout.to_string();
        tokio::spawn(async move {
            for i in 0..STEPS.len() {
                let secs = if i == 8 { step * 3.0 } else { step * 0.5 };
                {
                    let mut g = inner.lock().unwrap();
                    if let Some(j) = g.jobs.iter_mut().find(|j| j.id == id) {
                        j.steps[i].state = "running".into();
                    }
                    if let Some(s) = g.state.job.as_mut() {
                        s.step = i as u32 + 1;
                        s.title = STEPS[i].into();
                    }
                    if i == 7 {
                        for c in g.state.servers.iter_mut().flat_map(|s| s.cards.iter_mut()) {
                            if moving.contains(&c.id)
                                && let Some(e) = c.engine.as_mut()
                            {
                                e.state = "stopping".into();
                                e.since = now_ms();
                            }
                        }
                    }
                    if i == 8 {
                        for c in g.state.servers.iter_mut().flat_map(|s| s.cards.iter_mut()) {
                            if moving.contains(&c.id)
                                && let Some(e) = c.engine.as_mut()
                            {
                                e.state = "starting".into();
                                e.since = now_ms();
                            }
                        }
                    }
                    let j = g.jobs.iter().find(|j| j.id == id).cloned();
                    drop(g);
                    let _ = tx.send(Event {
                        kind: "job",
                        data: serde_json::to_value(j).unwrap_or_default(),
                    });
                }
                if secs > 0.0 {
                    tokio::time::sleep(Duration::from_secs_f64(secs)).await;
                }
                let mut g = inner.lock().unwrap();
                if let Some(j) = g.jobs.iter_mut().find(|j| j.id == id) {
                    j.steps[i].state = "ok".into();
                    j.steps[i].seconds = (secs * 10.0).round() / 10.0;
                }
            }
            let mut g = inner.lock().unwrap();
            let layout_cards = g
                .state
                .layouts
                .iter()
                .find(|l| l.id == layout)
                .map(|l| l.cards.clone())
                .unwrap_or_default();
            for s in g.state.servers.iter_mut() {
                s.active_layout = Some(layout.clone());
                for c in s.cards.iter_mut() {
                    if !moving.contains(&c.id) {
                        continue;
                    }
                    let mut spec = layout_cards.get(&c.id).cloned().unwrap_or_default();
                    if spec == "same" {
                        spec = layout_cards.get("A").cloned().unwrap_or_default();
                    }
                    let p: Vec<&str> = spec.split(" / ").collect();
                    if let (Some(e), [eng, prof, model]) = (c.engine.as_mut(), p.as_slice()) {
                        e.engine = eng.to_string();
                        e.profile = prof.to_string();
                        e.model = model.to_string();
                        e.served_id = model.to_string();
                        e.state = "ready".into();
                        e.since = now_ms();
                    }
                }
            }
            for l in g.state.layouts.iter_mut() {
                l.active = l.id == layout;
            }
            g.state.job = None;
            if let Some(j) = g.jobs.iter_mut().find(|j| j.id == id) {
                j.state = "done".into();
                j.ended = Some(now_ms());
            }
            let j = g.jobs.iter().find(|j| j.id == id).cloned();
            let st = g.state.clone();
            drop(g);
            let _ = tx.send(Event {
                kind: "job",
                data: serde_json::to_value(j).unwrap_or_default(),
            });
            let _ = tx.send(Event {
                kind: "state",
                data: serde_json::to_value(st).unwrap_or_default(),
            });
        });
        Ok(job)
    }

    async fn cancel_job(&self, id: &str, who: &str) -> ApiResult<Job> {
        let Some(j) = self.job(id) else {
            return Err(ApiError::new(404, "no such job"));
        };
        let at = j
            .steps
            .iter()
            .position(|s| s.state == "running")
            .unwrap_or(0);
        if j.state != "running" || at >= 7 {
            return Err(ApiError::new(
                409,
                "a job can be cancelled only before it stops anything (step 8)",
            ));
        }
        self.audit(who, "job.cancel", id, |_| {});
        Err(ApiError::new(501, "the demo does not cancel jobs"))
    }

    async fn set_card(&self, card: &str, state: &str, reason: &str, who: &str) -> ApiResult<Job> {
        {
            let mut g = self.inner.lock().unwrap();
            let Some(c) = g
                .state
                .servers
                .iter_mut()
                .flat_map(|s| s.cards.iter_mut())
                .find(|c| c.id == card)
            else {
                return Err(ApiError::new(404, "no such card"));
            };
            if !c.managed {
                return Err(ApiError::new(409, "this card is not managed"));
            }
            match state {
                "offline" => c.admin_state = "offline".into(),
                "online" | "include" => c.admin_state = "online".into(),
                "exclude" => c.on_demand = false,
                _ => {}
            }
            c.admin_note = (!reason.is_empty()).then(|| format!("{reason} ({who})"));
        }
        self.audit(who, &format!("card.{state}"), card, |r| {
            r.reason = (!reason.is_empty()).then(|| reason.to_string())
        });
        self.publish_state();
        Ok(Job {
            id: "j-card".into(),
            server: "gpu-host".into(),
            kind: state.into(),
            target: card.into(),
            requested_by: who.into(),
            state: "done".into(),
            started: now_ms(),
            ended: Some(now_ms()),
            ..Default::default()
        })
    }

    async fn release_lease(&self, key: &str, who: &str) -> ApiResult<()> {
        {
            let mut g = self.inner.lock().unwrap();
            let before = g.leases.leases.len();
            g.leases.leases.retain(|l| l.key != key);
            if g.leases.leases.len() == before {
                return Err(ApiError::new(404, "no such lease"));
            }
        }
        self.audit(who, "lease.release", key, |_| {});
        let _ = self.tx.send(Event {
            kind: "lease",
            data: serde_json::to_value(self.leases()).unwrap_or_default(),
        });
        Ok(())
    }

    async fn edit_route(&self, name: &str, edit: &RouteEdit, who: &str) -> ApiResult<RouteView> {
        if let Some(c) = &edit.cold
            && !["reject", "fallback", "on_demand"].contains(&c.as_str())
        {
            return Err(ApiError::new(
                422,
                "cold must be reject, fallback or on_demand",
            ));
        }
        let mut g = self.inner.lock().unwrap();
        let names: Vec<String> = g.routes.iter().map(|r| r.name.clone()).collect();
        let Some(r) = g.routes.iter_mut().find(|r| r.name == name) else {
            return Err(ApiError::new(404, "no such route"));
        };
        let cold = edit.cold.clone().unwrap_or_else(|| r.cold.clone());
        let fallback = edit.fallback.clone().unwrap_or_else(|| r.fallback.clone());
        if cold == "on_demand" && !r.targets.iter().all(|t| t.starts_with("model:")) {
            return Err(ApiError::new(
                422,
                "on_demand only on routes whose targets are catalog models (generic and pinned names never switch)",
            ));
        }
        if cold == "fallback" && fallback.is_empty() {
            return Err(ApiError::new(
                422,
                "cold = fallback needs at least one fallback route",
            ));
        }
        if let Some(bad) = fallback.iter().find(|f| !names.contains(f) || *f == name) {
            return Err(ApiError::new(422, format!("unknown fallback route: {bad}")));
        }
        if let Some(a) = &edit.access
            && !(a == "users" || a == "admins" || a.starts_with("keys:"))
        {
            return Err(ApiError::new(
                422,
                "access must be users, admins or keys:<names>",
            ));
        }
        r.cold = cold;
        r.fallback = fallback;
        if let Some(a) = &edit.access {
            r.access = a.clone();
        }
        if let Some(h) = edit.hidden {
            r.hidden = h;
        }
        if let Some(u) = edit.allow_user_switch {
            r.allow_user_switch = u;
        }
        let out = r.clone();
        drop(g);
        self.audit(who, "route.edit", name, |row| {
            row.diff = serde_json::to_string(edit).ok()
        });
        Ok(out)
    }
}
