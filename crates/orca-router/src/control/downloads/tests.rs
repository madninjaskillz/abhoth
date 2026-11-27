use super::*;
use crate::control::exec::LocalExecutor;
use crate::control::validate::tests::SAMPLE;

/// A router.toml in a temp dir whose server runs locally, with a model folder and the mock HF.
async fn setup() -> (tempfile::TempDir, Arc<Service>, String) {
    let (hf_url, _) = crate::control::hf::tests::mock().await;
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().to_string_lossy().replace('\\', "/");
    let mut text = String::new();
    for l in SAMPLE.lines() {
        if l.starts_with("ssh = {") {
            continue;
        }
        if l == "exec = \"ssh\"" {
            text.push_str(&format!("exec = \"local\"\nmodel_dir = \"{d}/models\"\n"));
            continue;
        }
        if l == "state_dir = \"/state\"" {
            text.push_str(&format!("state_dir = \"{d}/state\"\n"));
            continue;
        }
        text.push_str(l);
        text.push('\n');
        if l == "title = \"Small model\"" {
            text.push_str("servers = []\n");
        }
    }
    text.push_str(&format!("\n[hf]\nendpoint = \"{hf_url}\"\n"));
    let main = dir.path().join("router.toml");
    std::fs::write(&main, &text).unwrap();
    let state = dir.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    let store = Arc::new(crate::control::store::Store::open(&state.join("router.db")).unwrap());
    let backend = crate::control_api::live::ConfigBackend::new(&main, &state, store);
    let exec_for: ExecFor = Arc::new(|_c: &ControlConfig, _s: &str| {
        Ok(Arc::new(LocalExecutor::default()) as Arc<dyn Executor>)
    });
    let mut s = Service::new(&main, &state, backend, exec_for);
    s.poll = Duration::from_millis(20);
    (dir, Arc::new(s), d)
}

async fn wait_done(s: &Service, id: &str) -> Download {
    for _ in 0..200 {
        let d = s.get(id).unwrap();
        if d.state != "running" && d.state != "queued" {
            return d;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("download did not finish: {:?}", s.get(id));
}

#[tokio::test]
async fn a_download_lands_in_the_servers_model_folder_and_marks_the_model() {
    let (_dir, s, d) = setup().await;
    let c = s.config().unwrap();
    assert!(c.models["small"].servers.as_ref().unwrap().is_empty());
    let req = StartReq {
        server: "gpu".into(),
        repo: "example/Tiny-Chat-GGUF".into(),
        revision: None,
        files: vec![FileRef2 {
            path: "Tiny-Chat-Q4_K_M.gguf".into(),
            size: 100,
            sha256: None,
        }],
        model: Some("small".into()),
        gated: false,
        dir: None,
        role: None,
    };
    let dl = s.start(req.clone(), "admin").unwrap();
    assert!(
        dl.dir.ends_with("/models/example/Tiny-Chat-GGUF"),
        "{}",
        dl.dir
    );
    let done = wait_done(&s, &dl.id).await;
    assert_eq!(done.state, "done", "{done:?}");
    assert_eq!(done.bytes, 100);
    let f = format!("{d}/models/example/Tiny-Chat-GGUF/Tiny-Chat-Q4_K_M.gguf");
    assert_eq!(std::fs::read(&f).map(|b| b.len()).unwrap_or(0), 100);
    // the model is now on gpu (an audited save)
    let c = s.config().unwrap();
    assert_eq!(c.models["small"].servers, Some(vec!["gpu".to_string()]));
    // the list is kept across restarts
    let t = std::fs::read_to_string(s.state_dir.join("downloads.json")).unwrap();
    assert!(t.contains("Tiny-Chat") && !t.contains("Bearer"));
    // the same repo again: already there, done at once
    let again = s.start(req, "admin").unwrap();
    assert_eq!(wait_done(&s, &again.id).await.state, "done");
}

#[tokio::test]
async fn gated_needs_a_token_and_bad_requests_are_refused() {
    let (_dir, s, _) = setup().await;
    let mk = |repo: &str, file: &str, gated: bool| StartReq {
        server: "gpu".into(),
        repo: repo.into(),
        revision: None,
        files: vec![FileRef2 {
            path: file.into(),
            size: 0,
            sha256: None,
        }],
        model: None,
        gated,
        dir: None,
        role: None,
    };
    assert!(
        s.start(mk("example/voice-1", "model.safetensors", true), "a")
            .unwrap_err()
            .contains("token")
    );
    assert!(s.start(mk("../x", "a", false), "a").is_err());
    assert!(s.start(mk("o/m", "../../etc/passwd", false), "a").is_err());
    let mut r = mk("o/m", "a.gguf", false);
    r.server = "nope".into();
    assert!(s.start(r, "a").is_err());
    // a token makes the gated one go
    s.set_token("hf_testtoken").unwrap();
    let c = s.config().unwrap();
    assert_eq!(s.token_view(&c)["set"], true);
    assert!(!s.token_view(&c).to_string().contains("hf_test"));
    assert!(
        s.start(mk("example/voice-1", "model.safetensors", true), "a")
            .is_ok()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let m = std::fs::metadata(s.state_dir.join("secrets/hf-token"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(m & 0o777, 0o600);
    }
    s.set_token("").unwrap();
    assert_eq!(s.token_view(&s.config().unwrap())["set"], false);
    assert!(s.set_token("has space").is_err());
}

#[tokio::test]
async fn cancel_keeps_the_part_and_resume_finishes() {
    let (_dir, s, _) = setup().await;
    // an unknown size makes the mock's 100 bytes one quick request: cancel before it starts
    let req = StartReq {
        server: "gpu".into(),
        repo: "example/Tiny-Chat-GGUF".into(),
        revision: None,
        files: vec![FileRef2 {
            path: "big.gguf".into(),
            size: 100,
            sha256: None,
        }],
        model: None,
        gated: false,
        dir: None,
        role: None,
    };
    let dl = s.start(req, "admin").unwrap();
    s.cancel(&dl.id, "admin").unwrap();
    let d = wait_done(&s, &dl.id).await;
    assert!(d.state == "cancelled" || d.state == "done", "{d:?}");
    if d.state == "cancelled" {
        let r = s.resume(&dl.id, "admin").unwrap();
        assert_eq!(wait_done(&s, &r.id).await.state, "done");
    }
    assert!(s.remove(&dl.id).is_ok());
    assert!(s.get(&dl.id).is_none());
}

#[tokio::test]
async fn detect_reads_the_servers_gpus() {
    use crate::control::exec::FakeExecutor;
    let (_dir, s0, _) = setup().await;
    let fake = Arc::new(FakeExecutor::new());
    let line = "GPU-aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa, 0, Tesla V100, 30, 0, 0, 100, 32768, 50, 250, 135, 877, 1597, 1380, 0, P0, 0x0\nGPU-dddddddd-dddd-dddd-dddd-dddddddddddd, 1, RTX 4090, 30, 0, 0, 100, 24564, 50, 250, 135, 877, 1597, 1380, 0, P0, 0x0";
    fake.on(
        "telemetry",
        vec![Ok(crate::control::exec::ExecOutput::ok_with(line))],
    );
    let f2 = fake.clone();
    let exec_for: ExecFor =
        Arc::new(move |_c: &ControlConfig, _s: &str| Ok(f2.clone() as Arc<dyn Executor>));
    let s = Service::new(&s0.main, &s0.state_dir, s0.backend.clone(), exec_for);
    let v = s.detect("gpu").await.unwrap();
    let g = v["gpus"].as_array().unwrap();
    assert_eq!(g.len(), 2);
    assert_eq!(g[0]["card"], "A");
    assert_eq!(g[0]["vramGib"], 32.0);
    assert!(g[1]["card"].is_null());
    assert_eq!(g[1]["name"], "RTX 4090");
}

#[tokio::test]
async fn detect_on_a_cpu_only_server_still_offers_the_cpu() {
    use crate::control::exec::{ExecOutput, FakeExecutor};
    let (_dir, s0, _) = setup().await;
    let fake = Arc::new(FakeExecutor::new());
    fake.on(
        "telemetry",
        vec![Ok(ExecOutput::fail(127, "sh: 1: nvidia-smi: not found"))],
    );
    fake.on(
        "cpu-info",
        vec![Ok(ExecOutput::ok_with(
            r#"{"model":"Test CPU","threads":16,"cores":8,"ramGib":32.0,"availableGib":20.0,"simd":["avx2"]}"#,
        ))],
    );
    let f2 = fake.clone();
    let exec_for: ExecFor =
        Arc::new(move |_c: &ControlConfig, _s: &str| Ok(f2.clone() as Arc<dyn Executor>));
    let s = Service::new(&s0.main, &s0.state_dir, s0.backend.clone(), exec_for);
    let v = s.detect("gpu").await.unwrap();
    assert!(v["gpus"].as_array().unwrap().is_empty());
    assert_eq!(v["cpu"]["model"], "Test CPU");
    assert!(v["gpuError"].as_str().unwrap().contains("nvidia-smi"));

    // neither answers: an error, as before
    let fake = Arc::new(FakeExecutor::new());
    fake.on("telemetry", vec![Ok(ExecOutput::fail(127, "not found"))]);
    fake.on("cpu-info", vec![Ok(ExecOutput::fail(1, "no such builtin"))]);
    let f3 = fake.clone();
    let exec_for: ExecFor =
        Arc::new(move |_c: &ControlConfig, _s: &str| Ok(f3.clone() as Arc<dyn Executor>));
    let s = Service::new(&s0.main, &s0.state_dir, s0.backend.clone(), exec_for);
    assert!(s.detect("gpu").await.unwrap_err().contains("nvidia-smi"));
}
