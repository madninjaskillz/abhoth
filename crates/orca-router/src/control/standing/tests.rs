use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{Path as AxPath, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use serde_json::{Value, json};

use super::*;

fn w(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

fn config(docker: &str, whisper_port: u16) -> ControlConfig {
    toml::from_str(&format!(
        r#"
        version = 1
        [servers.dt]
        host = "127.0.0.1"
        docker_api = "{docker}"
        docker_mount_roots = ["/srv/sa3", "/srv/hf"]
        model_dir = "/srv/models"
        vars = {{ engine_state = "/srv/engines" }}
        [cards.X1]
        server = "dt"
        gpu_uuid = "GPU-11111111-2222-3333-4444-555555555555"
        vram_gib = 8.0
        managed = false
        engines = {{ llm = [], stt = ["whisper-fw"] }}
        [cards.CPU]
        server = "dt"
        device = "cpu"
        cores = 24
        ram_gib = 64.0
        engines = {{ music = ["sa3-run"], stt = ["whisper-fw"] }}
        [engines.sa3-run]
        driver = "package"
        lifecycle = "standing"
        memory_gib = 12.0
        port = {{ CPU = 18095 }}
        package = {{ id = "sa3", version = "1", params = {{ service_dir = "/srv/sa3/service", src_dir = "/srv/sa3/src", hf_cache = "/srv/hf" }} }}
        [engines.whisper-fw]
        driver = "package"
        lifecycle = "standing"
        memory_gib = 2.0
        model_dir = "/srv/models/whisper"
        port = {{ CPU = {whisper_port} }}
        package = {{ id = "whisper", version = "3.8.1" }}
        "#
    ))
    .expect("config")
}

#[test]
fn docker_run_words_parse_and_unknown_options_are_refused() {
    let a = parse_run(&w(&[
        "docker",
        "run",
        "-d",
        "--name",
        "x",
        "--restart",
        "on-failure:5",
        "--label",
        "a=b",
        "-p",
        "127.0.0.1:18095:8095",
        "-v",
        "/srv/hf:/srv/hf:ro",
        "-e",
        "K=V",
        "--gpus",
        "device=GPU-11111111-2222-3333-4444-555555555555",
        "--shm-size=2g",
        "img:1",
        "--flag",
        "v",
    ]))
    .unwrap();
    assert_eq!(a.name, "x");
    assert_eq!(a.image, "img:1");
    assert_eq!(a.cmd, w(&["--flag", "v"]));
    assert_eq!(
        a.ports,
        vec![("127.0.0.1".into(), 18095, 8095, "tcp".into())]
    );
    assert_eq!(a.binds, vec![("/srv/hf".into(), "/srv/hf".into(), true)]);
    assert_eq!(a.shm_size, Some(2 << 30));
    let b = create_body(&a);
    assert_eq!(b["HostConfig"]["RestartPolicy"]["Name"], "on-failure");
    assert_eq!(
        b["HostConfig"]["DeviceRequests"][0]["DeviceIDs"][0],
        "GPU-11111111-2222-3333-4444-555555555555"
    );
    assert_eq!(
        b["HostConfig"]["PortBindings"]["8095/tcp"][0]["HostPort"],
        "18095"
    );
    assert!(parse_run(&w(&["docker", "run", "--volumes-from", "y", "img"])).is_err());
}

#[test]
fn the_rules_refuse_what_a_standing_engine_may_not_have() {
    let roots = w(&["/srv/models", "/srv/engines"]);
    let ok = RunArgs {
        name: "x".into(),
        image: "i".into(),
        binds: vec![("/srv/models/a".into(), "/m".into(), false)],
        gpus: Some("device=GPU-11111111-2222-3333-4444-555555555555".into()),
        ports: vec![("127.0.0.1".into(), 1, 2, "tcp".into())],
        ..Default::default()
    };
    assert!(
        check(
            &ok,
            &roots,
            Some("GPU-11111111-2222-3333-4444-555555555555")
        )
        .is_ok()
    );
    let no = |f: &dyn Fn(&mut RunArgs)| {
        let mut a = ok.clone();
        f(&mut a);
        check(&a, &roots, Some("GPU-11111111-2222-3333-4444-555555555555")).unwrap_err()
    };
    assert!(no(&|a| a.privileged = true).contains("privileged"));
    assert!(no(&|a| a.network = Some("host".into())).contains("host network"));
    assert!(no(&|a| a.pid = Some("host".into())).contains("PID"));
    assert!(no(&|a| a.cap_add = vec!["SYS_ADMIN".into()]).contains("capabilities"));
    assert!(no(&|a| a.devices = vec!["/dev/sda".into()]).contains("raw devices"));
    assert!(no(&|a| a.binds = vec![("/etc".into(), "/e".into(), true)]).contains("outside"));
    assert!(
        no(&|a| a.binds = vec![("/srv/models/../../etc".into(), "/e".into(), true)])
            .contains("outside")
    );
    assert!(no(&|a| a.gpus = Some("all".into())).contains("only the card's own"));
    assert!(no(&|a| a.gpus = Some("device=GPU-other".into())).contains("only the card's own"));
    assert!(no(&|a| a.ports = vec![("".into(), 1, 2, "tcp".into())]).contains("every address"));
    // a CPU card: no GPU at all
    assert!(check(&ok, &roots, None).is_err());
}

#[test]
fn packages_render_into_checked_containers() {
    let c = config("http://127.0.0.1:1", 10301);
    let d = tempfile::tempdir().unwrap();
    let p = plan(&c, "sa3-run", "CPU", d.path()).unwrap();
    assert_eq!(p.run.image, "local/sa3:1");
    assert_eq!(p.port, 18095);
    assert!(
        p.run
            .ports
            .contains(&("127.0.0.1".into(), 18095, 8095, "tcp".into()))
    );
    assert!(p.run.binds.iter().any(|b| b.0 == "/srv/hf" && b.2));
    assert_eq!(p.run.labels[LABEL], "1");
    assert_eq!(p.health_http.as_deref(), Some("/health"));
    assert!(p.run.gpus.is_none());
    let p = plan(&c, "whisper-fw", "CPU", d.path()).unwrap();
    assert!(
        p.run
            .image
            .starts_with("docker.io/rhasspy/wyoming-whisper:3.8.1@sha256:")
    );
    assert!(
        p.run.cmd.contains(&"small.en".to_string()),
        "{:?}",
        p.run.cmd
    );
    // the CPU threads param (default 8) reaches the server
    let at = p.run.cmd.iter().position(|x| x == "--cpu-threads").expect("--cpu-threads");
    assert_eq!(p.run.cmd[at + 1], "8");
    assert!(
        p.run
            .binds
            .iter()
            .any(|b| b.0 == "/srv/models/whisper" && b.1 == "/data")
    );
    assert!(p.health_tcp);
    // a mount outside the roots is refused before anything is created
    let mut c2 = config("http://127.0.0.1:1", 10301);
    c2.servers.get_mut("dt").unwrap().docker_mount_roots = vec![];
    let e = plan(&c2, "sa3-run", "CPU", d.path()).err().unwrap();
    assert!(e.contains("refused") && e.contains("outside"), "{e}");
}

#[test]
fn standing_engines_hold_their_memory_and_are_validated() {
    let c = config("http://127.0.0.1:1", 10301);
    assert_eq!(c.standing_gib("CPU", None), 14.0);
    assert_eq!(c.standing_gib("CPU", Some("sa3-run")), 2.0);
    let errs: Vec<String> = crate::control::validate::validate(&c)
        .into_iter()
        .filter(|i| i.level == crate::control::validate::Level::Error)
        .map(|i| i.to_string())
        .collect();
    assert!(errs.is_empty(), "{errs:?}");
    let mut c2 = c.clone();
    c2.engines.get_mut("sa3-run").unwrap().memory_gib = Some(70.0);
    c2.servers.get_mut("dt").unwrap().docker_api = None;
    let errs: Vec<String> = crate::control::validate::validate(&c2)
        .into_iter()
        .filter(|i| i.level == crate::control::validate::Level::Error)
        .map(|i| i.to_string())
        .collect();
    assert!(
        errs.iter().any(|e| e.contains("standing engines hold")),
        "{errs:?}"
    );
    assert!(errs.iter().any(|e| e.contains("no docker_api")), "{errs:?}");
}

#[derive(Default)]
struct Fake {
    calls: Vec<String>,
    created: Option<Value>,
    running: bool,
    labelled: bool,
}

type F = Arc<Mutex<Fake>>;

async fn fake_docker(f: F) -> String {
    let app = Router::new()
        .route(
            "/containers/{name}/json",
            get(|State(f): State<F>, AxPath(n): AxPath<String>| async move {
                let mut g = f.lock().unwrap();
                g.calls.push(format!("inspect {n}"));
                if g.created.is_none() {
                    return (StatusCode::NOT_FOUND, axum::Json(json!({}))).into_response();
                }
                let lab = if g.labelled { json!({LABEL: "1"}) } else { json!({}) };
                (
                    StatusCode::OK,
                    axum::Json(json!({"Config": {"Labels": lab}, "State": {"Running": g.running, "StartedAt": "2020-01-01T00:00:00Z"}})),
                )
                    .into_response()
            }),
        )
        .route(
            "/images/{name}/json",
            get(|State(f): State<F>| async move {
                f.lock().unwrap().calls.push("image".into());
                StatusCode::OK
            }),
        )
        .route(
            "/containers/create",
            post(|State(f): State<F>, axum::Json(b): axum::Json<Value>| async move {
                let mut g = f.lock().unwrap();
                g.calls.push("create".into());
                g.created = Some(b);
                g.labelled = true;
                (StatusCode::CREATED, axum::Json(json!({"Id": "abc"})))
            }),
        )
        .route(
            "/containers/{name}/start",
            post(|State(f): State<F>, AxPath(n): AxPath<String>| async move {
                let mut g = f.lock().unwrap();
                g.calls.push(format!("start {n}"));
                g.running = true;
                StatusCode::NO_CONTENT
            }),
        )
        .with_state(f);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    format!("http://{a}")
}

#[tokio::test]
async fn the_supervisor_creates_starts_restarts_and_never_stops() {
    let f: F = Arc::default();
    let api = fake_docker(f.clone()).await;
    // the engine's health: a TCP port that answers
    let health = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let hp = health.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let _ = health.accept().await;
        }
    });
    let c = config(&api, hp);
    let d = tempfile::tempdir().unwrap();
    // missing: created (checked) and started
    tend(&c, "whisper-fw", "CPU", d.path()).await;
    {
        let g = f.lock().unwrap();
        assert!(g.calls.contains(&"create".to_string()), "{:?}", g.calls);
        assert!(
            g.calls.iter().any(|x| x.starts_with("start ")),
            "{:?}",
            g.calls
        );
        let b = g.created.as_ref().unwrap();
        assert_eq!(b["Labels"][LABEL], "1");
        assert_eq!(b["HostConfig"]["Privileged"], false);
    }
    assert_eq!(status_of("whisper-fw", "CPU").unwrap().state, "starting");
    // running and answering: ready
    tend(&c, "whisper-fw", "CPU", d.path()).await;
    assert_eq!(status_of("whisper-fw", "CPU").unwrap().state, "ready");
    // stopped (crashed): started again once the backoff allows, with a restart counted
    f.lock().unwrap().running = false;
    STATE
        .lock()
        .unwrap()
        .get_mut("whisper-fw/CPU")
        .unwrap()
        .next_try = None;
    tend(&c, "whisper-fw", "CPU", d.path()).await;
    let s = status_of("whisper-fw", "CPU").unwrap();
    assert_eq!(s.restarts, 1);
    assert!(f.lock().unwrap().running);
    // the supervisor never asked for a stop, kill or remove
    assert!(
        !f.lock()
            .unwrap()
            .calls
            .iter()
            .any(|x| x.contains("stop") || x.contains("kill") || x.contains("delete"))
    );
    // every create and start is audited
    let audit = std::fs::read_to_string(d.path().join("standing-audit.jsonl")).unwrap();
    assert!(audit.contains("\"op\":\"create\"") && audit.contains("\"op\":\"start\""));
    // a container of that name the router did not create is left alone
    f.lock().unwrap().labelled = false;
    tend(&c, "whisper-fw", "CPU", d.path()).await;
    assert_eq!(status_of("whisper-fw", "CPU").unwrap().state, "foreign");
}
