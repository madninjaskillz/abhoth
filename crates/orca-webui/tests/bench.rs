//! The bench (ports of BenchRunnerTests and BenchTargetTests): suites run through a fake task
//! kit (a small Python script standing in for benchkit.py) and fake run sessions that write
//! usage log lines; profile switches go to a fake profiles API on 127.0.0.1. No model, no real
//! runs, no live service.

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use async_trait::async_trait;
use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use common::{FakeSession, Host};
use orca_core::live::{LiveEventBus, LiveSnapshot};
use orca_core::profiles::service::ProfileService;
use orca_webui::audit::AuditLog;
use orca_webui::bench::queue::NewItem;
use orca_webui::bench::{
    BenchArm, BenchOptions, BenchQueue, BenchResultRow, BenchRunner, BenchSuite, BenchTarget,
    CancelToken, result_row, summary, time,
};
use orca_webui::live::LiveSession;
use orca_webui::projects::manager::{ProjectEvent, ProjectManager};
use orca_webui::projects::store::ProjectStore;
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};

const SECRET_ENV: &str = "OPENORCA_TEST_BENCH_KEY";
const SECRET_VALUE: &str = "not-a-real-key-7f3a9c";

/// benchkit.py's stand-in: tasks "demo" and "other"; a run passes when answer.txt says 42.
const FAKE_KIT: &str = r#"import json, os, sys
kit, cmd, *rest = sys.argv[1:]
if cmd == "list":
    print(json.dumps([{"name": "demo", "timeout": 60}, {"name": "other", "timeout": 60}]))
elif cmd == "setup":
    os.makedirs(rest[1], exist_ok=True)
    open(os.path.join(rest[1], "README.md"), "w").write("write 42 to answer.txt")
    print(json.dumps({"prompt": "Write 42 to answer.txt", "timeout": 60}))
elif cmd == "check":
    os.makedirs(rest[2], exist_ok=True)
    f = os.path.join(rest[1], "answer.txt")
    ok = os.path.exists(f) and open(f).read().strip() == "42"
    open(os.path.join(rest[2], "score.json"), "w").write(json.dumps({"ok": ok}))
    print(json.dumps({"pass": ok, "reason": "ok" if ok else "wrong", "tests_passed": 3 if ok else 1, "tests_total": 3}))
"#;

fn python() -> Option<&'static str> {
    let exe = if cfg!(windows) { "python" } else { "python3" };
    std::process::Command::new(exe)
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|_| exe)
}

macro_rules! need_python {
    () => {
        if python().is_none() {
            eprintln!("python is not on PATH: skipped");
            return;
        }
    };
}

#[derive(Clone, Copy, PartialEq)]
enum Style {
    /// BenchRunnerTests' run: busy 400 ms, then the answer and two usage lines (slots A1, B1).
    Runner,
    /// BenchTargetTests' run: one usage line now, the answer and a second line on Finish.
    Held { auto_finish: bool },
}

/// A run's session: works on the prompt as its style says.
struct RunSession {
    bus: Arc<LiveEventBus>,
    cwd: String,
    home: PathBuf,
    style: Style,
    busy: Arc<AtomicBool>,
    started: AtomicBool,
    disposed: AtomicBool,
    cancelled: Arc<AtomicBool>,
    finish: CancelToken,
    started_at: Mutex<Option<Instant>>,
    ended_at: Arc<Mutex<Option<Instant>>>,
}

impl RunSession {
    fn config(&self) -> Value {
        serde_json::from_str(&std::fs::read_to_string(self.home.join("config.json")).unwrap())
            .unwrap()
    }

    fn endpoint(&self) -> Option<String> {
        self.config()["lmStudio"]["baseUrl"]
            .as_str()
            .map(str::to_string)
    }

    fn finish(&self) {
        self.finish.cancel();
    }

    fn started(&self) -> bool {
        self.started.load(Ordering::SeqCst)
    }

    fn disposed(&self) -> bool {
        self.disposed.load(Ordering::SeqCst)
    }
}

fn append(file: &Path, line: &str) {
    use std::io::Write;
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file)
        .unwrap();
    writeln!(f, "{line}").unwrap();
}

#[async_trait]
impl LiveSession for RunSession {
    fn bus(&self) -> Arc<LiveEventBus> {
        self.bus.clone()
    }
    fn working_directory(&self) -> String {
        self.cwd.clone()
    }
    fn mode(&self) -> String {
        "worker".into()
    }
    async fn get_snapshot(&self) -> Result<LiveSnapshot, String> {
        Ok(LiveSnapshot {
            session_id: Some("s".into()),
            title: None,
            working_directory: self.cwd.clone(),
            mode: "worker".into(),
            model: Some(
                if self.style == Style::Runner {
                    "m"
                } else {
                    "fake-model"
                }
                .into(),
            ),
            busy: self.busy.load(Ordering::SeqCst),
            queued: 0,
            last_seq: 0,
            history: Vec::new(),
            current_turn: Vec::new(),
            agents: Vec::new(),
            slots: None,
            pending_permissions: Vec::new(),
            engine: None,
            pending_questions: None,
            uncensored: false,
        })
    }
    async fn submit_prompt(&self, _text: &str, _source: &str) -> Option<String> {
        self.busy.store(true, Ordering::SeqCst);
        self.started.store(true, Ordering::SeqCst);
        *self.started_at.lock().unwrap() = Some(Instant::now());
        let (busy, cancelled, ended, finish) = (
            self.busy.clone(),
            self.cancelled.clone(),
            self.ended_at.clone(),
            self.finish.clone(),
        );
        let (cwd, home) = (PathBuf::from(&self.cwd), self.home.clone());
        match self.style {
            Style::Runner => {
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(400)).await;
                    std::fs::write(cwd.join("answer.txt"), "42").unwrap();
                    std::fs::create_dir_all(home.join("logs")).unwrap();
                    std::fs::write(
                        home.join("logs").join("openorca-2026-11-17.log"),
                        "21:12:34.625 [INF] Slots: Slot usage lead on A1: prompt 7543, cached 4663, re-read 2880, output 142, reasoning 90, 31390 ms | content=0 tool=52 calls=bash prompt_n=2880 cache_n=4663 prompt_ms=5216 predicted_n=142 predicted_ms=5602\n\
21:13:43.754 [INF] Slots: Slot usage agent 1 (Undo) on B1: prompt 8085, cached 4663, re-read 3422, output 641, reasoning 130, 68604 ms | content=498 tool=13 calls=- prompt_n=3422 cache_n=4663 prompt_ms=5341 predicted_n=641 predicted_ms=43889\n",
                    )
                    .unwrap();
                    busy.store(false, Ordering::SeqCst);
                });
            }
            Style::Held { auto_finish } => {
                let log = home.join("logs").join("openorca-2026-11-20.log");
                append(&log, "10:00:00.000 [INF] Slots: Slot usage lead: prompt 1000, cached 400, re-read 600, output 300, reasoning 200, 900 ms | content=40 tool=60 calls=bash predicted_n=300 predicted_ms=600");
                if auto_finish {
                    finish.cancel();
                }
                tokio::spawn(async move {
                    finish.cancelled().await;
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    if !cancelled.load(Ordering::SeqCst) {
                        std::fs::write(cwd.join("answer.txt"), "42").unwrap();
                        append(&log, "10:00:05.000 [INF] Slots: Slot usage lead: prompt 2000, cached 1500, re-read 500, output 100, reasoning 10, 500 ms | content=70 tool=20 calls=- predicted_n=100 predicted_ms=400");
                    }
                    *ended.lock().unwrap() = Some(Instant::now());
                    busy.store(false, Ordering::SeqCst);
                });
            }
        }
        None
    }
    async fn cancel(&self) -> bool {
        self.cancelled.store(true, Ordering::SeqCst);
        self.finish.cancel();
        true
    }
    async fn resolve_permission(&self, _id: &str, _decision: &str) -> bool {
        false
    }
    async fn shutdown(&self) {
        self.disposed.store(true, Ordering::SeqCst);
        let mut e = self.ended_at.lock().unwrap();
        if e.is_none() {
            *e = Some(Instant::now());
        }
        drop(e);
        self.finish.cancel();
    }
}

type Sessions = Arc<Mutex<Vec<Arc<RunSession>>>>;

struct Env {
    temp: tempfile::TempDir,
    projects: Arc<ProjectManager>,
    sessions: Sessions,
}

impl Env {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let primary = temp.path().join("primary");
        std::fs::create_dir_all(&primary).unwrap();
        let projects = ProjectManager::new(
            ProjectStore::new(temp.path().join("projects.json")),
            FakeSession::new(&primary),
            None,
        );
        Self {
            temp,
            projects,
            sessions: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn path(&self) -> &Path {
        self.temp.path()
    }

    fn sessions(&self) -> Vec<Arc<RunSession>> {
        self.sessions.lock().unwrap().clone()
    }

    fn started(&self) -> usize {
        self.sessions().iter().filter(|s| s.started()).count()
    }

    /// The options both test classes share: the fake kit, a base config, a quiet window twelve
    /// hours away, fast polling.
    fn options(&self, style: Style, base_config: &str, registry: Option<&str>) -> BenchOptions {
        let kit = self.path().join("benchkit.py");
        std::fs::write(&kit, FAKE_KIT).unwrap();
        let config = self.path().join("base-config.json");
        std::fs::write(&config, base_config).unwrap();
        let sessions = self.sessions.clone();
        let factory: orca_webui::bench::BenchSessionFactory =
            Arc::new(move |folder: &str, env: &BTreeMap<String, String>| {
                let s = Arc::new(RunSession {
                    bus: Arc::new(LiveEventBus::new()),
                    cwd: folder.to_string(),
                    home: PathBuf::from(&env["OPENORCA_HOME"]),
                    style,
                    busy: Arc::new(AtomicBool::new(false)),
                    started: AtomicBool::new(false),
                    disposed: AtomicBool::new(false),
                    cancelled: Arc::new(AtomicBool::new(false)),
                    finish: CancelToken::new(),
                    started_at: Mutex::new(None),
                    ended_at: Arc::new(Mutex::new(None)),
                });
                sessions.lock().unwrap().push(s.clone());
                s as Arc<dyn LiveSession>
            });
        let mut o = BenchOptions::new(
            self.path().join("bench"),
            self.path().join("kit"),
            kit,
            config,
            factory,
        );
        o.python = python().unwrap_or("python").into();
        o.registry_url = registry.map(str::to_string);
        let far = (time::uk_minutes(SystemTime::now()) + 12 * 60) % 1440;
        o.quiet_from = far;
        o.quiet_to = (far + 1) % 1440;
        o.poll_interval = Duration::from_millis(if style == Style::Runner { 100 } else { 50 });
        o.gate_interval = Duration::from_millis(if style == Style::Runner { 200 } else { 100 });
        o.switch_poll = Duration::from_millis(50);
        o
    }

    fn audit(&self) -> Arc<AuditLog> {
        Arc::new(AuditLog::new(Some(self.path().join("audit.log"))))
    }

    /// BenchRunnerTests.NewRunner: the "demo" suite (one arm with overrides).
    fn runner_tests_runner(&self, registry: Option<&str>, profiles: Option<Arc<ProfileService>>) -> Arc<BenchRunner> {
        let o = self.options(
            Style::Runner,
            r#"{"agent":{"leadMode":"hands-on"},"slots":{"copies":[{"name":"A","slots":3}]}}"#,
            registry,
        );
        std::fs::create_dir_all(self.path().join("bench").join("suites")).unwrap();
        std::fs::write(
            self.path().join("bench").join("suites").join("demo.json"),
            r#"{"arms":[{"name":"auto","leadMode":"auto","set":["slots.copies.0.slots=5","pi.extraTools=true"]}],"tasks":["demo"],"reps":1}"#,
        )
        .unwrap();
        BenchRunner::new(o, self.projects.clone(), self.audit(), profiles)
    }

    fn target_options(&self, auto_finish: bool, registry: Option<&str>) -> BenchOptions {
        self.options(
            Style::Held { auto_finish },
            r#"{"agent":{"leadMode":"hands-on"},"slots":{"enabled":true,"registryUrl":"http://127.0.0.1:9","copies":[{"name":"A","slots":3}]}}"#,
            registry,
        )
    }

    /// BenchTargetTests.NewRunner.
    fn target_runner(&self, auto_finish: bool, registry: Option<&str>) -> Arc<BenchRunner> {
        BenchRunner::new(
            self.target_options(auto_finish, registry),
            self.projects.clone(),
            self.audit(),
            None,
        )
    }

    fn write_suite(&self, name: &str, json: &str) {
        std::fs::write(
            self.path().join("bench").join("suites").join(format!("{name}.json")),
            json,
        )
        .unwrap();
    }
}

async fn until(what: &str, seconds: u64, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
}

fn s(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).map(str::to_string)
}

fn keys_are_declared(row: &Value) {
    for k in row.as_object().unwrap().keys() {
        assert!(
            result_row::KEYS.contains(&k.as_str()),
            "{k} is not a BenchResultRow key"
        );
    }
}

fn a() -> BenchTarget {
    BenchTarget::new("a", "http://127.0.0.1:18080/v1")
}

fn b() -> BenchTarget {
    BenchTarget {
        name: "b".into(),
        endpoint: "http://127.0.0.1:18091/v1".into(),
        model: Some("iq3s".into()),
        api_key_env: Some(SECRET_ENV.into()),
    }
}

// ── the fake profiles API ─────────────────────────────────────────────────────────────────

/// The inference server's profiles API: flash2 live; a switch takes two polls.
#[derive(Default)]
struct FakeApi {
    live: Mutex<String>,
    switches: AtomicUsize,
    polls: AtomicUsize,
}

struct ApiServer {
    api: Arc<FakeApi>,
    url: String,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for ApiServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl ApiServer {
    async fn start() -> Self {
        let api = Arc::new(FakeApi {
            live: Mutex::new("flash2".into()),
            ..FakeApi::default()
        });
        async fn profiles(State(a): State<Arc<FakeApi>>) -> axum::response::Response {
            let live = a.live.lock().unwrap().clone();
            let body = r#"{"layouts":{"flash2":{"cards":{"A":{"engine":"strata","slots":3},"B":{"engine":"strata","slots":2}}},
                        "dense2":{"cards":{"A":{"engine":"ninfer","slots":2},"B":{"engine":"ninfer","slots":2}}}},
             "profiles":{"flash2":{"layout":"flash2","client":{"agent.maxConcurrent":4}},
                         "dense2":{"layout":"dense2","client":{"agent.peerMode":true,"slots.model":"qwen3.8-27b"}}},
             "live":{"layout":"LIVE","healthy":true}}"#
                .replace("LIVE", &live);
            ([("content-type", "application/json")], body).into_response()
        }
        async fn switch(State(a): State<Arc<FakeApi>>, body: String) -> axum::response::Response {
            a.switches.fetch_add(1, Ordering::SeqCst);
            let sent: Value = serde_json::from_str(&body).unwrap();
            let want = sent["profile"]
                .as_str()
                .or(sent["layout"].as_str())
                .unwrap()
                .to_string();
            (
                StatusCode::ACCEPTED,
                axum::Json(json!({"id": format!("j-{want}"), "state": "queued", "layout": want})),
            )
                .into_response()
        }
        async fn job(
            State(a): State<Arc<FakeApi>>,
            axum::extract::Path(id): axum::extract::Path<String>,
        ) -> axum::response::Response {
            let layout = id.trim_start_matches("j-").to_string();
            let done = a.polls.fetch_add(1, Ordering::SeqCst) + 1 >= 2;
            if done {
                *a.live.lock().unwrap() = layout.clone();
                a.polls.store(0, Ordering::SeqCst);
            }
            axum::Json(if done {
                json!({"id": id, "state": "done", "seconds": 3, "steps": [{"t": 1, "msg": "stopping"}, {"t": 3, "msg": "done"}]})
            } else {
                json!({"id": id, "state": "running", "steps": [{"t": 1, "msg": "stopping"}]})
            })
            .into_response()
        }

        let app = axum::Router::new()
            .route("/profiles", get(profiles))
            .route("/switch", post(switch))
            .route("/jobs/{id}", get(job))
            .with_state(api.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self { api, url, task }
    }

    fn service(&self) -> Arc<ProfileService> {
        Arc::new(ProfileService::new(Some(&self.url), None, Duration::from_secs(5)))
    }

    fn live(&self) -> String {
        self.api.live.lock().unwrap().clone()
    }

    fn switches(&self) -> usize {
        self.api.switches.load(Ordering::SeqCst)
    }
}

// ── BenchRunnerTests ──────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn profile_arms_run_profile_by_profile_switch_the_layout_once_and_record_their_settings() {
    need_python!();
    let env = Env::new();
    let api = ApiServer::start().await;
    let runner = env.runner_tests_runner(None, Some(api.service()));
    env.write_suite(
        "two",
        r#"{"arms":[{"name":"dense2","profile":"dense2"},{"name":"flash2","profile":"flash2"},{"name":"dense-ho","leadMode":"hands-on","profile":"dense2"}],"tasks":["demo"],"reps":1}"#,
    );
    assert_eq!(runner.enqueue_batch("two", true).unwrap().1, 3);
    until("three rows", 90, || runner.results(false).len() >= 3).await;
    let rows = runner.results(false);
    assert_eq!(rows.len(), 3);
    // Profile by profile: both dense2 arms together, one switch for them, flash2 back once.
    let col = |k: &str| rows.iter().map(|r| s(r, k).unwrap()).collect::<Vec<_>>();
    assert_eq!(col("profile"), ["dense2", "dense2", "flash2"]);
    assert_eq!(col("layout"), ["dense2", "dense2", "flash2"]);
    assert_eq!(api.switches(), 2);
    assert_eq!(api.live(), "flash2"); // the last layout stays up
    rows.iter().for_each(keys_are_declared);

    let dense = &rows[0];
    assert_eq!(dense["settings"]["agent"]["peerMode"], true);
    assert_eq!(dense["settings"]["slots"]["model"], "qwen3.8-27b");
    assert_eq!(dense["settings"]["profile"], "dense2");
    assert_eq!(rows[1]["settings"]["agent"]["leadMode"], "hands-on");
    assert_eq!(rows[2]["settings"]["agent"]["maxConcurrent"], 4);

    // Decode speed per copy, the slots and helpers used, and the run's own logs.
    assert_eq!(dense["copy_tps"]["A"], time::round(142.0 * 1000.0 / 5602.0, 1));
    assert_eq!(dense["copy_tps"]["B"], time::round(641.0 * 1000.0 / 43889.0, 1));
    assert_eq!(dense["slots"], json!(["A1", "B1"]));
    assert_eq!(dense["helper_routes"], json!(["agent 1 (Undo)"]));
    assert!(runner.logs(&s(dense, "id").unwrap(), 120_000).unwrap().contains("Slot usage"));
    assert!(runner.logs("../x", 120_000).is_none());
    runner.stop_all().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn off_live_profile_without_confirmation_is_refused_and_never_switches() {
    need_python!();
    let env = Env::new();
    let api = ApiServer::start().await;
    let runner = env.runner_tests_runner(None, Some(api.service()));
    env.write_suite("off", r#"{"arms":[{"name":"d","profile":"dense2"}],"tasks":["demo"],"reps":1}"#);
    runner.enqueue("off").unwrap();
    until("the queue to empty", 30, || {
        runner.state()["queue"].as_array().unwrap().is_empty()
    })
    .await;
    let recent = &runner.state()["recent"];
    assert!(s(&recent[0], "note").unwrap().contains("nobody confirmed"));
    assert_eq!(api.switches(), 0);
    assert_eq!(api.live(), "flash2");
    runner.stop_all().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn confirmed_suite_on_another_layout_switches_back_when_it_ends() {
    need_python!();
    let env = Env::new();
    let api = ApiServer::start().await;
    let runner = env.runner_tests_runner(None, Some(api.service()));
    env.write_suite("dense", r#"{"arms":[{"name":"d","profile":"dense2"}],"tasks":["demo"],"reps":1}"#);
    runner.enqueue_batch("dense", true).unwrap();
    until("the row and the switch back", 90, || {
        !runner.results(false).is_empty() && api.live() == "flash2" && api.switches() >= 2
    })
    .await;
    assert_eq!(runner.results(false)[0]["layout"], "dense2");
    assert_eq!(api.switches(), 2); // there, and back
    assert_eq!(api.live(), "flash2");
    runner.stop_all().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn queue_runs_grouped_by_layout_on_one_confirmation_switches_back_and_survives_a_restart() {
    need_python!();
    let env = Env::new();
    let api = ApiServer::start().await;
    let profiles = api.service();
    let runner = env.runner_tests_runner(None, Some(profiles.clone()));
    let dir = env.path().join("bench");
    {
        let queue = BenchQueue::new(runner.clone(), dir.clone(), Some(profiles.clone()), true);
        let (added, skipped) = queue
            .add(vec![
                NewItem::new("demo", Some("dense2"), Some("peer"), 1),
                NewItem::new("demo", Some("flash2"), Some("hands-on"), 1),
                NewItem::new("demo", Some("dense2"), None, 1), // the profile's own mode (peer)
                NewItem::new("demo", Some("nope"), Some("auto"), 1),
                NewItem::new("demo", Some("flash2"), Some("sideways"), 1),
            ])
            .await;
        assert_eq!(added, 3);
        assert_eq!(skipped.len(), 2);

        let plan = queue.plan(&runner.results(false)).await;
        assert_eq!(plan["live"], "flash2");
        assert_eq!(plan["groups"].as_array().unwrap().len(), 2); // flash2 first (live), then dense2

        assert!(queue.start(false).await.is_some()); // no confirmation, no start
        assert_eq!(queue.start(true).await, None);

        let deadline = Instant::now() + Duration::from_secs(120);
        while Instant::now() < deadline {
            let st = queue.state();
            if st["items"].as_array().unwrap().iter().all(|i| i["status"] == "done")
                && api.live() == "flash2"
                && api.switches() >= 2
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let rows = runner.results(false);
        let col = |k: &str| rows.iter().map(|r| s(r, k).unwrap()).collect::<Vec<_>>();
        assert_eq!(col("profile"), ["flash2", "dense2", "dense2"]);
        assert_eq!(col("arm"), ["hands-on", "peer", "peer"]);
        assert_eq!(api.switches(), 2); // to dense2 once for both of its items, and back
        assert_eq!(api.live(), "flash2");
        queue.stop().await;
    }
    // queue.json: the items and their outcome are still there for a new process.
    let again = BenchQueue::new(runner.clone(), dir, Some(profiles), false);
    let st = again.state();
    let items = st["items"].as_array().unwrap();
    assert_eq!(items.len(), 3);
    assert!(items.iter().all(|i| i["status"] == "done"));
    assert_eq!(st["running"], false);
    runner.stop_all().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_profile_fails_its_runs_without_switching() {
    need_python!();
    let env = Env::new();
    let api = ApiServer::start().await;
    let runner = env.runner_tests_runner(None, Some(api.service()));
    env.write_suite("bad", r#"{"arms":[{"name":"x","profile":"nope"}],"tasks":["demo"],"reps":2}"#);
    runner.enqueue("bad").unwrap();
    until("the queue to empty", 30, || {
        runner.state()["queue"].as_array().unwrap().is_empty()
    })
    .await;
    let st = runner.state();
    let recent = st["recent"].as_array().unwrap();
    assert_eq!(recent.len(), 2);
    assert!(recent
        .iter()
        .all(|r| s(r, "note").unwrap().contains("unknown profile nope")));
    assert_eq!(api.switches(), 0);
    runner.stop_all().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn run_config_must_lease_from_the_registry_when_the_bench_is_gated() {
    let env = Env::new();
    // The base config has copies but no registry: a gated runner refuses to write it, an arm
    // that adds the registry is fine.
    let gated = env.runner_tests_runner(Some("http://127.0.0.1:1"), None);
    let home = env.path().join("h1");
    assert!(gated.write_home(&home, &BenchArm::new("x", "auto", None, None), None, None).is_err());
    gated
        .write_home(
            &home,
            &BenchArm::new("x", "auto", Some(vec!["slots.registryUrl=http://127.0.0.1:1".into()]), None),
            None,
            None,
        )
        .unwrap();
    assert!(std::fs::read_to_string(home.join("config.json")).unwrap().contains("registryUrl"));
    gated.stop_all().await;
}

fn git(cwd: &Path, args: &[&str]) {
    let out = std::process::Command::new("git").current_dir(cwd).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

#[tokio::test(flavor = "multi_thread")]
async fn refresh_mirror_fetches_new_commits_from_origin_and_reports_the_head() {
    let env = Env::new();
    let upstream = env.path().join("upstream");
    std::fs::create_dir_all(&upstream).unwrap();
    git(&upstream, &["init", "-q", "-b", "main"]);
    std::fs::write(upstream.join("a.txt"), "1").unwrap();
    git(&upstream, &["add", "."]);
    git(&upstream, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "one"]);
    std::fs::create_dir_all(env.path().join("kit")).unwrap();
    git(
        env.path(),
        &["clone", "-q", "--mirror", upstream.to_str().unwrap(), env.path().join("kit").join("openorca.git").to_str().unwrap()],
    );
    let runner = env.runner_tests_runner(None, None);
    assert!(runner.mirror().await.to_string().contains("one"));

    std::fs::write(upstream.join("a.txt"), "2").unwrap();
    git(&upstream, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-am", "two"]);
    let r = runner.refresh_mirror().await;
    assert!(r.ok(), "{}", r.error);
    assert!(runner.mirror().await.to_string().contains("two"));
    runner.stop_all().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_suite_run_sets_up_watches_scores_and_records() {
    need_python!();
    let env = Env::new();
    let runner = env.runner_tests_runner(None, None);
    assert!(runner.suites().iter().any(|s| s.name == "demo"));

    let saw_tab = Arc::new(AtomicBool::new(false));
    {
        let (saw, pm) = (saw_tab.clone(), Arc::downgrade(&env.projects));
        env.projects.subscribe(Arc::new(move |e| {
            if matches!(e, ProjectEvent::ListChanged)
                && let Some(pm) = pm.upgrade()
                && pm.list().iter().any(|p| p.read_only)
            {
                saw.store(true, Ordering::SeqCst);
            }
        }));
    }
    assert_eq!(runner.enqueue("demo").unwrap(), 1);
    until("a row", 60, || !runner.results(false).is_empty()).await;
    let rows = runner.results(false);
    assert_eq!(rows.len(), 1);
    let row = &rows[0];

    assert!(saw_tab.load(Ordering::SeqCst));
    assert!(!env.projects.list().iter().any(|p| p.read_only)); // closed after the run
    let sessions = env.sessions();
    assert_eq!(sessions.len(), 1);
    assert!(sessions[0].disposed());
    assert_eq!(row["status"], "passed");
    keys_are_declared(row);
    assert_eq!(BenchResultRow::parse(row).unwrap().task, "demo");
    assert_eq!(row["pass"], true);
    assert_eq!(row["tests_passed"], 3);
    assert_eq!(row["requests"], 2);
    assert_eq!(row["helpers"], 1);
    assert_eq!(row["output_tokens"], 783);
    assert_eq!(row["decode_tps"], time::round(783.0 * 1000.0 / (5602.0 + 43889.0), 1));
    assert_eq!(row["lead_tps"], time::round(142.0 * 1000.0 / 5602.0, 1));
    assert!(row["wall_s"].as_f64().unwrap() > 0.0);

    // The run's home: the arm's lead mode and settings, tools auto-approved.
    let config = sessions[0].config();
    assert_eq!(config["agent"]["leadMode"], "auto");
    assert_eq!(config["slots"]["copies"][0]["slots"], 5);
    assert_eq!(config["pi"]["extraTools"], true);
    assert_eq!(config["permissions"]["autoApproveAll"], true);
    assert!(runner.output(&s(row, "id").unwrap()).unwrap().contains("score.json"));
    assert!(runner.output("../../etc").is_none());
    assert!(std::fs::read_to_string(env.path().join("audit.log")).unwrap().contains("\"bench.result\""));
    runner.stop_all().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn results_with_archived_adds_archive_folders_marked_and_a_bad_line_skips_only_itself() {
    let env = Env::new();
    let runner = env.runner_tests_runner(None, None);
    let dir = env.path().join("bench");
    let archive = dir.join("archive-20261121");
    std::fs::create_dir_all(&archive).unwrap();
    // Real rows, cut down: the 2026-11-22 overnight grid and an older row moved aside.
    let current = [
        r#"{"id":"20261122-021755-001-fanout-ez-codex-r1","suite":"fanout-ez-arms","arm":"codex","lead_mode":"auto","sets":[],"task":"fanout-ez","rep":1,"profile":"codex","layout":"dense-abl-x2","settings":{"profile":"codex","agent":{"engine":"codex"}},"pass":false,"status":"failed","started":"2026-11-22T02:28:41.1586046+00:00","wall_s":162.6,"output_tokens":0,"reasoning_tokens":0}"#,
        r#"{"id":"20261122-032802-001-fanout-ez-auto-r1","suite":"fanout-ez-arms","arm":"auto","lead_mode":"auto","sets":["agent.peerMode=false"],"task":"fanout-ez","rep":1,"profile":"flash-x2","pass":true,"status":"passed","wall_s":512.3,"output_tokens":61000,"reasoning_tokens":30000}"#,
        "{not json",
        "",
        r#"{"id":"20261122-120217-001-fanout-ez-hands-on-r1","suite":"fanout-ez-arms","arm":"hands-on","lead_mode":"hands-on","sets":["agent.peerMode=false"],"task":"fanout-ez","rep":1,"profile":"qwenpi-llamacpp-a-flash-b","status":"stopped","note":"stopped","wall_s":853.3,"output_tokens":84149,"reasoning_tokens":41951}"#,
    ]
    .join("\n");
    std::fs::write(runner.results_file(), format!("{current}\n")).unwrap();
    std::fs::write(
        archive.join("results.jsonl"),
        "{\"id\":\"fm-20261118T180456Z-dense2-hands-on-r1\",\"suite\":\"fanout-matrix\",\"arm\":\"hands-on\",\"lead_mode\":\"hands-on\",\"sets\":[\"agent.leadMode=hands-on\"],\"task\":\"fanout-ez\",\"rep\":1,\"profile\":\"dense2\",\"status\":\"passed\",\"pass\":true,\"wall_s\":400}\n",
    )
    .unwrap();
    let before = std::fs::read(runner.results_file()).unwrap();

    // The queue's time estimates read results.jsonl alone, as before.
    let st: Vec<String> = runner.results(false).iter().map(|r| s(r, "status").unwrap()).collect();
    assert_eq!(st, ["failed", "passed", "stopped"]);

    let all = runner.results(true);
    let ids: Vec<String> = all.iter().map(|r| s(r, "id").unwrap()).collect();
    assert_eq!(
        ids,
        [
            "fm-20261118T180456Z-dense2-hands-on-r1",
            "20261122-021755-001-fanout-ez-codex-r1",
            "20261122-032802-001-fanout-ez-auto-r1",
            "20261122-120217-001-fanout-ez-hands-on-r1"
        ]
    );
    assert_eq!(all[0]["archive"], "archive-20261121");
    assert!(all[1..].iter().all(|r| r.get("archive").is_none()));
    assert_eq!(all[0]["profile"], "dense2");
    assert_eq!(all[1]["settings"]["agent"]["engine"], "codex");
    assert_eq!(all[3]["reasoning_tokens"], 41951);
    assert!(all.iter().all(|r| BenchResultRow::parse(r).is_some()));
    assert_eq!(std::fs::read(runner.results_file()).unwrap(), before); // read, never rewritten
    runner.stop_all().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn queue_after_a_restart_with_items_left_neither_starts_nor_switches_and_clears_a_settled_switch_back() {
    need_python!();
    // queue.json as a stopped overnight queue left it: not running, items still queued, and
    // the switch back already done but still flagged as scheduled.
    let env = Env::new();
    let api = ApiServer::start().await;
    let profiles = api.service();
    let runner = env.runner_tests_runner(None, Some(profiles.clone()));
    let dir = env.path().join("bench");
    let file = dir.join("queue.json");
    std::fs::write(
        &file,
        r#"{"running":false,"current":null,"restoreLayout":"flash2","restoreScheduled":true,"confirmedAt":"2026-11-22T02:17:55+00:00",
 "items":[{"id":"a1","suite":"demo","profile":"dense2","mode":"peer","reps":1,"status":"queued","runs":[],"added":"2026-11-22T02:08:16+00:00"},
          {"id":"a2","suite":"demo","profile":"flash2","mode":"auto","reps":1,"status":"cancelled","runs":["r"],"added":"2026-11-22T02:08:16+00:00"}]}"#,
    )
    .unwrap();
    let saved = || -> Value { serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap() };
    {
        let queue = BenchQueue::new(runner.clone(), dir.clone(), Some(profiles.clone()), true);
        let deadline = Instant::now() + Duration::from_secs(30);
        while saved()["restoreScheduled"] == true && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        tokio::time::sleep(Duration::from_millis(1500)).await; // time to (wrongly) start something

        let q = saved();
        assert_eq!(q["restoreScheduled"], false);
        assert!(q["restoreLayout"].is_null());
        assert_eq!(q["running"], false);
        let statuses: Vec<String> = q["items"].as_array().unwrap().iter().map(|i| s(i, "status").unwrap()).collect();
        assert_eq!(statuses, ["queued", "cancelled"]);
        assert_eq!(api.switches(), 0);
        assert!(runner.results(false).is_empty());
        assert!(runner.state()["queue"].as_array().unwrap().is_empty());
        queue.stop().await;
    }
    // The next restart has nothing to switch back to.
    let _again = BenchQueue::new(runner.clone(), dir, Some(profiles), false);
    assert!(!runner.has_restore("queue"));
    runner.stop_all().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_empties_the_queue() {
    let env = Env::new();
    let runner = env.runner_tests_runner(None, None);
    runner.pause();
    runner.enqueue("demo").unwrap();
    runner.enqueue("demo").unwrap();
    let st = runner.state();
    assert_eq!(st["queue"].as_array().unwrap().len(), 2);
    assert_eq!(st["paused"], true); // queuing doesn't resume a paused runner
    runner.stop();
    assert!(runner.state()["queue"].as_array().unwrap().is_empty());
    assert!(runner.enqueue("no-such-suite").is_err());
    runner.stop_all().await;
}

// ── BenchTargetTests ──────────────────────────────────────────────────────────────────────

fn runs_of(r: &BenchRunner) -> Vec<Value> {
    r.runs()
}

#[tokio::test(flavor = "multi_thread")]
async fn targeted_suite_runs_one_per_target_targets_in_parallel_without_the_lease_gate() {
    need_python!();
    let env = Env::new();
    // The registry is unreachable: an untargeted run would wait on it for ever; targeted runs
    // don't ask it.
    let runner = env.target_runner(false, Some("http://127.0.0.1:9"));
    runner
        .save_suite(
            BenchSuite {
                reps: 2,
                targets: Some(vec![a(), b()]),
                ..BenchSuite::new(
                    "two-targets",
                    vec![
                        BenchArm::new("base", "hands-on", None, None),
                        BenchArm::new("cap", "hands-on", Some(vec!["agent.reasoning.maxStepTokens=4096".into()]), None),
                    ],
                    &["demo"],
                )
            },
            false,
        )
        .unwrap();
    let (batch, n) = runner.enqueue_batch("two-targets", false).unwrap();
    assert_eq!(n, 4);

    // Two start (one per target) and no third while both targets are busy.
    until("two runs to start", 60, || env.started() == 2).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(env.sessions().len(), 2);
    let mut endpoints: Vec<String> = env.sessions().iter().map(|s| s.endpoint().unwrap()).collect();
    endpoints.sort();
    assert_eq!(endpoints, ["http://127.0.0.1:18080/v1", "http://127.0.0.1:18091/v1"]);
    let st = runner.state();
    assert_eq!(st["running"].as_array().unwrap().len(), 2);
    assert_eq!(st["queue"].as_array().unwrap().len(), 2);
    assert!(st["gate"].is_null());

    // Live counts while it runs: the one request logged so far.
    until("live counts", 60, || {
        runs_of(&runner).iter().any(|r| {
            s(r, "status").as_deref() == Some("running") && r["live"]["requests"] == 1
        })
    })
    .await;
    let live = runs_of(&runner)
        .into_iter()
        .find(|r| s(r, "status").as_deref() == Some("running"))
        .unwrap();
    assert_eq!(live["live"]["reasoningTokens"], 200);
    assert_eq!(s(&live, "batch").unwrap(), batch);
    assert!(["a", "b"].contains(&s(&live, "target").unwrap().as_str()));

    // Let them finish one at a time; each freed target takes the next queued run.
    let deadline = Instant::now() + Duration::from_secs(60);
    while runner.results(false).len() < 4 {
        assert!(Instant::now() < deadline, "timed out waiting for four rows");
        for s in env.sessions() {
            s.finish();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let sessions = env.sessions();
    assert_eq!(sessions.len(), 4);
    assert!(sessions.iter().all(|s| s.disposed()));
    // Never two runs on one endpoint at the same time.
    for ep in ["http://127.0.0.1:18080/v1", "http://127.0.0.1:18091/v1"] {
        let mut group: Vec<&Arc<RunSession>> = sessions.iter().filter(|s| s.endpoint().as_deref() == Some(ep)).collect();
        group.sort_by_key(|s| s.started_at.lock().unwrap().unwrap());
        for w in group.windows(2) {
            let start = w[1].started_at.lock().unwrap().unwrap();
            let end = w[0].ended_at.lock().unwrap().unwrap();
            assert!(start >= end, "two runs overlapped on {ep}");
        }
    }

    let rows = runner.results(false);
    for r in &rows {
        assert_eq!(r["status"], "passed");
        assert_eq!(s(r, "batch").unwrap(), batch);
        assert!(["a", "b"].contains(&s(r, "target").unwrap().as_str()));
        keys_are_declared(r);
        assert_eq!(r["requests"], 2);
        assert_eq!(r["reasoning_tokens"], 210);
        assert_eq!(r["content_tokens"], 110);
        assert_eq!(r["tool_tokens"], 80);
        assert_eq!(r["model"], "fake-model");
    }
    assert_eq!(rows.iter().filter(|r| r["arm"] == "cap").count(), 2);

    // Each run's home: straight to its target, no slot leasing, the arm's override, and the
    // key by name only.
    let on_b = sessions
        .iter()
        .find(|s| s.endpoint().as_deref() == Some("http://127.0.0.1:18091/v1"))
        .unwrap()
        .config();
    assert_eq!(on_b["slots"]["enabled"], false);
    assert_eq!(on_b["lmStudio"]["models"][0], "iq3s");
    assert_eq!(on_b["lmStudio"]["apiKeyEnv"], SECRET_ENV);
    assert!(sessions
        .iter()
        .any(|s| s.config()["agent"]["reasoning"]["maxStepTokens"] == 4096));
    runner.stop_all().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn each_mode_runs_every_cell_on_every_target_and_the_summary_splits_by_target() {
    need_python!();
    let env = Env::new();
    let runner = env.target_runner(true, None);
    runner
        .save_suite(
            BenchSuite {
                reps: 2,
                targets: Some(vec![a(), b()]),
                target_mode: Some("each".into()),
                ..BenchSuite::new("ab", vec![BenchArm::new("base", "hands-on", None, None)], &["demo"])
            },
            false,
        )
        .unwrap();
    let (batch, n) = runner.enqueue_batch("ab", false).unwrap();
    assert_eq!(n, 4);
    until("four rows", 60, || runner.results(false).len() == 4).await;
    let rows = runner.results(false);
    assert_eq!(rows.iter().filter(|r| r["target"] == "a").count(), 2);
    assert_eq!(rows.iter().filter(|r| r["target"] == "b").count(), 2);
    let ids: std::collections::HashSet<String> = rows.iter().map(|r| s(r, "id").unwrap()).collect();
    assert_eq!(ids.len(), 4);

    let by_target = summary::compute(&rows, Some(&batch), None, true);
    let mut targets: Vec<String> = by_target.iter().map(|s| s.target.clone().unwrap()).collect();
    targets.sort();
    assert_eq!(targets, ["a", "b"]);
    assert!(by_target.iter().all(|s| (s.runs, s.passes) == (2, 2)));
    let together = summary::compute(&rows, Some(&batch), None, false);
    assert_eq!(together.len(), 1);
    assert_eq!((together[0].runs, together[0].scored, together[0].passes), (4, 4, 4));
    assert_eq!(together[0].median_steps, Some(2.0));
    assert!(summary::compute(&rows, Some("another-batch"), None, false).is_empty());
    runner.stop_all().await;
}

/// Known flaky in C# (BenchTargetTests.UntargetedRun...); the port waits on the session's own
/// start flag, so it does not race the run's setup.
#[tokio::test(flavor = "multi_thread")]
async fn untargeted_run_runs_alone_and_holds_back_what_is_queued_behind_it() {
    need_python!();
    let env = Env::new();
    let runner = env.target_runner(false, None);
    runner
        .save_suite(BenchSuite::new("classic", vec![BenchArm::new("auto", "auto", None, None)], &["demo"]), false)
        .unwrap();
    runner
        .save_suite(
            BenchSuite {
                reps: 2,
                targets: Some(vec![a(), b()]),
                ..BenchSuite::new("targeted", vec![BenchArm::new("base", "hands-on", None, None)], &["demo"])
            },
            false,
        )
        .unwrap();
    runner.enqueue("classic").unwrap();
    runner.enqueue("targeted").unwrap();
    until("the untargeted run", 60, || env.started() == 1).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    let sessions = env.sessions();
    assert_eq!(sessions.len(), 1);
    let only = &sessions[0];
    assert_ne!(only.endpoint().as_deref(), Some("http://127.0.0.1:18080/v1"));
    assert_eq!(only.config()["slots"]["enabled"], true); // the base config's slots, untouched

    only.finish();
    until("both targeted runs, in parallel", 60, || env.started() == 3).await;
    for s in env.sessions() {
        s.finish();
    }
    until("three rows", 60, || runner.results(false).len() == 3).await;
    let first = &runner.results(false)[0];
    assert!(s(first, "batch").unwrap().ends_with("-classic"), "first row is not the classic run");
    assert!(first.get("target").is_none());
    runner.stop_all().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_stops_only_that_run_a_queued_one_leaves_the_queue_and_a_batch_goes_together() {
    need_python!();
    let env = Env::new();
    let runner = env.target_runner(false, None);
    runner
        .save_suite(
            BenchSuite {
                reps: 2,
                targets: Some(vec![a(), b()]),
                ..BenchSuite::new("cancel", vec![BenchArm::new("base", "hands-on", None, None)], &["demo", "other"])
            },
            false,
        )
        .unwrap();
    let (batch, _) = runner.enqueue_batch("cancel", false).unwrap();
    until("two runs to start", 60, || env.started() == 2).await;
    let runs = runs_of(&runner);
    let running: Vec<String> = runs
        .iter()
        .filter(|r| matches!(s(r, "status").as_deref(), Some("running" | "setup")))
        .map(|r| s(r, "id").unwrap())
        .collect();
    let queued: Vec<String> = runs
        .iter()
        .filter(|r| s(r, "status").as_deref() == Some("queued"))
        .map(|r| s(r, "id").unwrap())
        .collect();
    assert_eq!(running.len(), 2);
    assert_eq!(queued.len(), 2);

    // A queued run: off the queue, no row, nothing started for it.
    assert!(runner.cancel(&queued[1]));
    assert!(!runner.cancel(&queued[1]));
    assert!(!runner.cancel("no-such-run"));
    let status_of = |id: &str| {
        runs_of(&runner)
            .into_iter()
            .find(|r| s(r, "id").as_deref() == Some(id))
            .and_then(|r| s(&r, "status"))
    };
    assert_eq!(status_of(&queued[1]).as_deref(), Some("stopped"));

    // A running run: its own session is cancelled and closed; the other target's run carries on.
    let first = env.sessions()[0].clone();
    let first_target = if first.endpoint().as_deref() == Some("http://127.0.0.1:18080/v1") { "a" } else { "b" };
    let first_id = runs_of(&runner)
        .into_iter()
        .find(|r| s(r, "target").as_deref() == Some(first_target) && s(r, "status").as_deref() == Some("running"))
        .and_then(|r| s(&r, "id"))
        .unwrap();
    assert!(runner.cancel(&first_id));
    until("the cancelled run's session to close", 60, || first.disposed()).await;
    until("the cancelled run's row", 60, || {
        runner.results(false).iter().any(|r| s(r, "id").as_deref() == Some(first_id.as_str()))
    })
    .await;
    let stopped = runner
        .results(false)
        .into_iter()
        .find(|r| s(r, "id").as_deref() == Some(first_id.as_str()))
        .unwrap();
    assert_eq!(stopped["status"], "stopped");
    assert!(stopped.get("pass").is_none());
    let other = env.sessions()[1].clone();
    assert!(!other.disposed());
    assert!(!other.cancelled.load(Ordering::SeqCst));

    // Its target is free again: the remaining queued run starts there.
    until("the next run on the freed target", 60, || env.started() == 3).await;
    assert_eq!(env.sessions()[2].endpoint(), first.endpoint());

    // The batch: everything left of it stops; nothing else was queued.
    assert_eq!(runner.cancel_batch(&batch), 2);
    until("rows for the stopped runs", 60, || runner.results(false).len() == 3).await;
    assert!(runner.results(false).iter().all(|r| r["status"] == "stopped"));
    until("every session closed", 30, || env.sessions().iter().all(|s| s.disposed())).await;
    let st = runner.state();
    assert!(st["running"].as_array().unwrap().is_empty());
    assert!(st["queue"].as_array().unwrap().is_empty());
    assert_eq!(runner.cancel_batch(&batch), 0);
    runner.stop_all().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn results_are_persisted_as_json_lines_without_the_key() {
    need_python!();
    let env = Env::new();
    // The workspace forbids unsafe code (std::env::set_var), so the key goes into the
    // environment the runner hands the kit and each run's session instead of the process's.
    let with_key = || {
        let mut o = env.target_options(true, None);
        o.environment.insert(SECRET_ENV.into(), SECRET_VALUE.into());
        BenchRunner::new(o, env.projects.clone(), env.audit(), None)
    };
    let file;
    {
        let runner = with_key();
        runner
            .save_suite(
                BenchSuite {
                    targets: Some(vec![b()]),
                    ..BenchSuite::new("persist", vec![BenchArm::new("base", "hands-on", None, None)], &["demo"])
                },
                false,
            )
            .unwrap();
        runner.enqueue("persist").unwrap();
        until("a row", 60, || runner.results(false).len() == 1).await;
        file = runner.results_file();
        runner.stop_all().await;
    }
    // One JSON object per line; a second runner on the same folder reads it back and appends.
    let text = std::fs::read_to_string(&file).unwrap();
    let lines: Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 1);
    let row = BenchResultRow::parse(&serde_json::from_str(lines[0]).unwrap()).unwrap();
    assert_eq!(
        (row.task.as_str(), row.arm.as_str(), row.target.as_deref(), row.pass),
        ("demo", "base", Some("b"), Some(true))
    );
    assert_eq!(row.target_endpoint.as_deref(), Some("http://127.0.0.1:18091/v1"));
    assert!(row.wall_seconds.unwrap() >= 0.0);
    {
        let again = with_key();
        assert_eq!(again.results(false).len(), 1);
        again.enqueue("persist").unwrap();
        until("a second row", 60, || again.results(false).len() == 2).await;
        again.stop_all().await;
    }
    let text = std::fs::read_to_string(&file).unwrap();
    assert_eq!(text.lines().filter(|l| !l.is_empty()).count(), 2);
    // The key is named, never written: not in results, suites, run homes or the audit log.
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else {
                out.push(p);
            }
        }
    }
    let mut files = Vec::new();
    walk(env.path(), &mut files);
    for f in files {
        let bytes = std::fs::read(&f).unwrap();
        assert!(
            !String::from_utf8_lossy(&bytes).contains(SECRET_VALUE),
            "{} holds the key",
            f.display()
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn save_suite_validates_what_the_page_composed() {
    let env = Env::new();
    let runner = env.target_runner(true, None);
    let ok = BenchSuite {
        reps: 3,
        targets: Some(vec![a()]),
        ..BenchSuite::new("my.suite-1", vec![BenchArm::new("base", "auto", None, None)], &["demo"])
    };
    runner.save_suite(ok.clone(), false).unwrap();
    let saved = runner.suites().into_iter().find(|s| s.name == "my.suite-1").unwrap();
    assert_eq!(
        (saved.reps, saved.targets.as_ref().unwrap()[0].name.as_str(), saved.arms[0].lead_mode.as_str()),
        (3, "a", "auto")
    );
    assert!(runner.save_suite(ok.clone(), false).unwrap_err().contains("already exists"));
    runner.save_suite(BenchSuite { reps: 5, ..ok.clone() }, true).unwrap();
    assert_eq!(runner.suites().into_iter().find(|s| s.name == "my.suite-1").unwrap().reps, 5);

    let refused = |s: BenchSuite, why: &str| {
        let e = runner.save_suite(s, false).unwrap_err();
        assert!(e.contains(why), "{e} should say {why}");
    };
    let x = || BenchSuite { name: "x".into(), ..ok.clone() };
    refused(BenchSuite { name: "../evil".into(), ..ok.clone() }, "suite name");
    refused(BenchSuite { arms: vec![], ..x() }, "no arms");
    refused(
        BenchSuite { arms: vec![BenchArm::new("a", "auto", None, None), BenchArm::new("a", "auto", None, None)], ..x() },
        "name of its own",
    );
    refused(BenchSuite { arms: vec![BenchArm::new("a", "auto", Some(vec!["nokey".into()]), None)], ..x() }, "key=value");
    refused(BenchSuite { reps: 0, ..x() }, "Reps");
    refused(BenchSuite { targets: Some(vec![BenchTarget::new("t", "ftp://host/x")]), ..x() }, "http(s)");
    refused(
        BenchSuite { targets: Some(vec![a(), BenchTarget { name: "a2".into(), ..a() }]), ..x() },
        "its own endpoint",
    );
    refused(
        BenchSuite { targets: Some(vec![BenchTarget { api_key_env: Some("sk-not-a-name!".into()), ..a() }]), ..x() },
        "NAME of an environment variable",
    );
    refused(BenchSuite { target_mode: Some("random".into()), ..x() }, "targetMode");
    // Profile arms switch the whole server: not in a suite with targets (fine without).
    refused(BenchSuite { arms: vec![BenchArm::new("dense", "auto", None, Some("dense2"))], ..x() }, "Profile arms");
    refused(BenchSuite { profile: Some("flash2".into()), ..x() }, "Profile arms");
    runner
        .save_suite(BenchSuite { targets: None, arms: vec![BenchArm::new("dense", "auto", None, Some("dense2"))], ..x() }, false)
        .unwrap();
    assert!(runner.suites().into_iter().find(|s| s.name == "x").unwrap().targets.is_none());
    runner.stop_all().await;
}

/// Reads the event stream into a shared list of (name, data).
fn read_events(resp: reqwest::Response) -> (Arc<Mutex<Vec<(String, Value)>>>, tokio::task::JoinHandle<()>) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let ev = events.clone();
    let task = tokio::spawn(async move {
        use futures_util::StreamExt;
        let mut stream = resp.bytes_stream();
        let mut buf = String::new();
        let mut name: Option<String> = None;
        while let Some(Ok(chunk)) = stream.next().await {
            buf.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(i) = buf.find('\n') {
                let line = buf[..i].to_string();
                buf.drain(..=i);
                if let Some(n) = line.strip_prefix("event: ") {
                    name = Some(n.to_string());
                } else if let (Some(d), Some(n)) = (line.strip_prefix("data: "), &name) {
                    ev.lock().unwrap().push((n.clone(), serde_json::from_str(d).unwrap()));
                }
            }
        }
    });
    (events, task)
}

#[tokio::test(flavor = "multi_thread")]
async fn http_a_suite_is_saved_started_streamed_summarised_and_cancelled() {
    need_python!();
    let env = Env::new();
    let web = env.path().join("web");
    std::fs::create_dir_all(&web).unwrap();
    let bench = env.target_options(false, None);
    let host = Host::start_with(FakeSession::new(&web), |o| o.bench = Some(bench)).await;
    let tasks: Value = host.api(Method::GET, "/api/bench/tasks").send().await.unwrap().json().await.unwrap();
    assert!(tasks.as_array().unwrap().iter().any(|t| t["name"] == "demo"));

    // The stream first: a snapshot, then every change of the runs we start.
    let stream = host.api(Method::GET, "/api/bench/events").send().await.unwrap();
    assert!(stream.headers()["content-type"].to_str().unwrap().starts_with("text/event-stream"));
    let (events, reader) = read_events(stream);
    let evs = || events.lock().unwrap().clone();

    let bad = host
        .api(Method::POST, "/api/bench/suites")
        .json(&json!({"suite": {"name": "bad", "arms": [{"name": "p", "profile": "dense2"}], "tasks": ["demo"], "targets": [{"name": "a", "endpoint": a().endpoint}]}}))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
    assert!(bad.text().await.unwrap().contains("Profile arms"));

    let save = host
        .api(Method::POST, "/api/bench/suites")
        .json(&json!({
            "start": true,
            "suite": {
                "name": "web",
                "arms": [{"name": "base", "leadMode": "hands-on", "set": ["agent.reasoning.maxStepTokens=0"]}],
                "tasks": ["demo"],
                "reps": 3,
                "targets": [{"name": "a", "endpoint": a().endpoint}],
            },
        }))
        .send()
        .await
        .unwrap();
    assert!(save.status().is_success());
    let started: Value = save.json().await.unwrap();
    assert_eq!(started["queued"], 3);
    let batch = s(&started, "batch").unwrap();

    // queued → running, with elapsed time and counts.
    until("a running event with counts and elapsed time", 60, || {
        evs().iter().any(|(n, d)| {
            n == "run" && d["status"] == "running" && d["live"].is_object() && d["elapsed"].as_f64().unwrap_or(0.0) >= 1.0
        })
    })
    .await;
    let all = evs();
    assert_eq!(all[0].0, "snapshot");
    assert!(all.iter().any(|(n, d)| n == "run" && d["status"] == "queued" && s(d, "batch").as_deref() == Some(batch.as_str())));
    assert!(all.iter().any(|(n, d)| n == "status" && d["running"] == 1));
    let running_id = all
        .iter()
        .find(|(n, d)| n == "run" && d["status"] == "running")
        .and_then(|(_, d)| s(d, "id"))
        .unwrap();
    let logs = host
        .api(Method::GET, &format!("/api/bench/runs/{running_id}/logs?tail=400"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(logs.contains("Slot usage"));

    // The first run passes; the second is cancelled while it runs; the rest of the batch too.
    env.sessions()[0].finish();
    until("a passed event", 60, || {
        evs().iter().any(|(n, d)| n == "run" && s(d, "id").as_deref() == Some(running_id.as_str()) && d["status"] == "passed" && d["pass"] == true)
    })
    .await;
    until("the second run", 60, || env.started() == 2).await;
    let second_id = evs()
        .iter()
        .rev()
        .find(|(n, d)| n == "run" && matches!(d["status"].as_str(), Some("running" | "setup")))
        .and_then(|(_, d)| s(d, "id"))
        .unwrap();
    assert_ne!(running_id, second_id);
    assert!(host.api(Method::POST, &format!("/api/bench/runs/{second_id}/cancel")).send().await.unwrap().status().is_success());
    assert_eq!(
        host.api(Method::POST, "/api/bench/runs/nope/cancel").send().await.unwrap().status(),
        StatusCode::NOT_FOUND
    );
    let cancelled: Value = host
        .api(Method::POST, &format!("/api/bench/batches/{batch}/cancel"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(cancelled["cancelled"].as_i64().unwrap() >= 1);
    until("a stopped event", 60, || {
        evs().iter().any(|(n, d)| n == "run" && s(d, "id").as_deref() == Some(second_id.as_str()) && d["status"] == "stopped")
    })
    .await;
    until("an idle status", 60, || {
        evs().iter().any(|(n, d)| n == "status" && d["running"] == 0 && d["queued"] == 0)
    })
    .await;

    let summary: Value = host
        .api(Method::GET, &format!("/api/bench/summary?batch={batch}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let lines = summary.as_array().unwrap();
    assert_eq!(lines.len(), 1);
    let line = &lines[0];
    assert_eq!((line["task"].as_str(), line["arm"].as_str(), line["passes"].as_i64()), (Some("demo"), Some("base"), Some(1)));
    // The passed run (2 requests) is the one scored; the stopped ones are counted only.
    assert!((2..=3).contains(&line["runs"].as_i64().unwrap()));
    assert_eq!(line["scored"], 1);
    assert_eq!(line["medianSteps"], 2.0);
    let results: Value = host.api(Method::GET, "/api/bench/results").send().await.unwrap().json().await.unwrap();
    assert!(results
        .as_array()
        .unwrap()
        .iter()
        .any(|r| s(r, "id").as_deref() == Some(running_id.as_str()) && r["status"] == "passed"));

    // The page's state, the queue and /api/info.
    let state: Value = host.api(Method::GET, "/api/bench").send().await.unwrap().json().await.unwrap();
    assert!(state["suites"].as_array().unwrap().iter().any(|s| s["name"] == "web"));
    let q: Value = host.api(Method::GET, "/api/bench/queue").send().await.unwrap().json().await.unwrap();
    assert_eq!(q["running"], false);
    let info: Value = host.api(Method::GET, "/api/info").send().await.unwrap().json().await.unwrap();
    assert_eq!(info["bench"], true);
    reader.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn http_queue_endpoints_add_move_requeue_clear_and_refuse_an_unconfirmed_start() {
    let env = Env::new();
    let web = env.path().join("web");
    std::fs::create_dir_all(&web).unwrap();
    let bench = env.target_options(true, None);
    std::fs::create_dir_all(env.path().join("bench").join("suites")).unwrap();
    env.write_suite("demo", r#"{"arms":[{"name":"auto"}],"tasks":["demo"]}"#);
    let host = Host::start_with(FakeSession::new(&web), |o| o.bench = Some(bench)).await;

    let add: Value = host
        .api(Method::POST, "/api/bench/queue/items")
        .json(&json!({"items": [{"suite": "demo", "mode": "auto", "reps": 2}, {"suite": "demo", "mode": "hands-on"}, {"suite": "nope"}]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(add["added"], 2);
    assert_eq!(add["skipped"], json!(["nope · (config) · -: no such suite"]));
    let empty = host.api(Method::POST, "/api/bench/queue/items").json(&json!({"items": []})).send().await.unwrap();
    assert_eq!(empty.status(), StatusCode::BAD_REQUEST);

    let q: Value = host.api(Method::GET, "/api/bench/queue").send().await.unwrap().json().await.unwrap();
    let ids: Vec<String> = q["items"].as_array().unwrap().iter().map(|i| s(i, "id").unwrap()).collect();
    assert_eq!(q["items"][0]["mode"], "auto");
    assert_eq!(q["items"][0]["reps"], 2);
    assert!(host
        .api(Method::POST, &format!("/api/bench/queue/items/{}/move", ids[1]))
        .json(&json!({"to": 0}))
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
    let q: Value = host.api(Method::GET, "/api/bench/queue").send().await.unwrap().json().await.unwrap();
    assert_eq!(s(&q["items"][0], "id").unwrap(), ids[1]);

    let plan: Value = host.api(Method::GET, "/api/bench/queue/plan").send().await.unwrap().json().await.unwrap();
    assert_eq!(plan["items"].as_array().unwrap().len(), 2);
    assert_eq!(plan["unknownTasks"], 2);

    let refused = host.api(Method::POST, "/api/bench/queue/start").json(&json!({})).send().await.unwrap();
    assert_eq!(refused.status(), StatusCode::CONFLICT);
    assert!(refused.text().await.unwrap().contains("confirmation"));
    let requeued: Value = host.api(Method::POST, "/api/bench/queue/requeue").send().await.unwrap().json().await.unwrap();
    assert_eq!(requeued["requeued"], 0);
    assert_eq!(
        host.api(Method::DELETE, "/api/bench/queue/items/nope").send().await.unwrap().status(),
        StatusCode::NOT_FOUND
    );
    assert!(host
        .api(Method::DELETE, &format!("/api/bench/queue/items/{}", ids[0]))
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
    let cleared: Value = host
        .api(Method::POST, "/api/bench/queue/clear")
        .json(&json!({"finishedOnly": false}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(cleared["removed"], 1);
    let file: Value = serde_json::from_str(&std::fs::read_to_string(env.path().join("bench").join("queue.json")).unwrap()).unwrap();
    assert!(file["items"].as_array().unwrap().is_empty());
}
