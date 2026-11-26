use super::*;
use crate::clocks;
use crate::limits::Limits;
use crate::protocol::{CardBinding, Rates, read_frame, write_frame};
use orca_gpuctl::{FakeNvml, Mode};
use tokio::io::BufReader;

const LIMITS: &str = r#"
[gpus."GPU-a"]
label = "A"
actions = ["persistence", "power_limit", "lock_clocks", "reset_clocks"]
clock_mhz = [135, 1597]
power_limit_w = [250, 350]
"#;

fn shared() -> (Arc<Shared>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths {
        limits: dir.path().join("l"),
        state_dir: dir.path().join("state"),
        run_dir: dir.path().join("run"),
    };
    let limits = Limits::parse(LIMITS).unwrap();
    let (etx, _erx) = tokio::sync::mpsc::unbounded_channel();
    let (ctx, _h) = clocks::spawn(
        Box::new(FakeNvml::default().with_card("GPU-a", 1597)),
        limits.gpu.clone(),
        Mode::On,
        Some(paths.clone()),
        etx,
    );
    let s = Shared::new(
        "test",
        LoadedLimits {
            limits,
            hash: "h".into(),
        },
        Some(paths),
        ctx,
    );
    (s, dir)
}

fn policy(rev: u64, pl: u32) -> PolicyDoc {
    let mut d = PolicyDoc {
        rev,
        cards: Default::default(),
        sensors: Default::default(),
        fans: Default::default(),
        power: Default::default(),
    };
    d.cards.insert(
        "A".into(),
        CardBinding {
            uuid: "GPU-a".into(),
            bus: Some("21:00.0".into()),
        },
    );
    d.sensors.insert(
        "ctrl_A".into(),
        orca_sensors::Expr::parse("first(gpu.A.hbm, gpu.A.core)").unwrap(),
    );
    d.power.insert(
        "A".into(),
        toml::from_str(&format!(
            "persistence = true\npower_limit_w = {pl}\nserving = \"unlocked\""
        ))
        .unwrap(),
    );
    d
}

struct Client {
    w: tokio::io::WriteHalf<tokio::io::DuplexStream>,
    r: BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    buf: Vec<u8>,
    id: u64,
}

impl Client {
    fn connect(s: &Arc<Shared>) -> Client {
        let (a, b) = tokio::io::duplex(1 << 20);
        let (sr, sw) = tokio::io::split(b);
        tokio::spawn(serve_conn(s.clone(), BufReader::new(sr), sw));
        let (r, w) = tokio::io::split(a);
        Client {
            w,
            r: BufReader::new(r),
            buf: Vec::new(),
            id: 0,
        }
    }

    async fn call(&mut self, m: ToHostd) -> FromHostd {
        self.id += 1;
        write_frame(&mut self.w, &Envelope::new(Some(self.id), m))
            .await
            .unwrap();
        loop {
            let env: Envelope<FromHostd> = read_frame(&mut self.r, &mut self.buf)
                .await
                .unwrap()
                .unwrap();
            if env.id == Some(self.id) {
                return env.msg;
            }
        }
    }

    async fn raw(&mut self, line: &str) -> Envelope<FromHostd> {
        self.w.write_all(line.as_bytes()).await.unwrap();
        self.w.write_all(b"\n").await.unwrap();
        read_frame(&mut self.r, &mut self.buf)
            .await
            .unwrap()
            .unwrap()
    }
}

#[tokio::test]
async fn hello_policy_assert_status() {
    let (s, _d) = shared();
    let mut c = Client::connect(&s);
    match c
        .call(ToHostd::Hello {
            proto: PROTOCOL_VERSION,
            version: "x".into(),
            client: "t".into(),
        })
        .await
    {
        FromHostd::Hello {
            limits_hash,
            policy_rev: None,
            ..
        } => assert_eq!(limits_hash, "h"),
        r => panic!("{r:?}"),
    }
    assert!(matches!(
        c.call(ToHostd::Hello {
            proto: 99,
            version: "x".into(),
            client: "t".into()
        })
        .await,
        FromHostd::Error {
            code: ErrorCode::Version,
            ..
        }
    ));
    // Outside the limits: refused in full, nothing in force.
    match c.call(ToHostd::PolicyPut(policy(1, 400))).await {
        FromHostd::PolicyRefused { rev: 1, reasons } => assert!(!reasons.is_empty()),
        r => panic!("{r:?}"),
    }
    assert_eq!(s.policy_rev(), None);
    assert!(matches!(
        c.call(ToHostd::PolicyPut(policy(2, 350))).await,
        FromHostd::PolicyApplied { rev: 2 }
    ));
    // Last-known-good is on disk.
    assert_eq!(
        files::load_policy(s.paths.as_ref().unwrap())
            .unwrap()
            .unwrap()
            .rev,
        2
    );
    // Older revision refused.
    assert!(matches!(
        c.call(ToHostd::PolicyPut(policy(1, 350))).await,
        FromHostd::PolicyRefused { .. }
    ));
    match c.call(ToHostd::Assert { card: "A".into() }).await {
        FromHostd::AssertResult(r) => assert_eq!(r.card, "A"),
        r => panic!("{r:?}"),
    }
    assert!(matches!(
        c.call(ToHostd::Assert { card: "Z".into() }).await,
        FromHostd::Error {
            code: ErrorCode::NotFound,
            ..
        }
    ));
    // TTL over the limit.
    assert!(matches!(
        c.call(ToHostd::Hold {
            target: Target::Card("A".into()),
            ttl_s: 7200,
            reason: "x".into()
        })
        .await,
        FromHostd::Error {
            code: ErrorCode::Forbidden,
            ..
        }
    ));
    assert!(matches!(
        c.call(ToHostd::Hold {
            target: Target::All,
            ttl_s: 60,
            reason: "switch".into()
        })
        .await,
        FromHostd::Ack
    ));
    match c.call(ToHostd::Status).await {
        FromHostd::Status(st) => {
            assert_eq!(st.policy_rev, Some(2));
            assert_eq!(st.holds, vec!["A: switch".to_string()]);
        }
        r => panic!("{r:?}"),
    }
    assert!(matches!(
        c.call(ToHostd::Release {
            target: Target::All,
            reason: None
        })
        .await,
        FromHostd::Ack
    ));
    match c.call(ToHostd::Status).await {
        FromHostd::Status(st) => assert!(st.holds.is_empty()),
        r => panic!("{r:?}"),
    }
    // No fans in this policy.
    assert!(matches!(
        c.call(ToHostd::Override(Override::FanDuty {
            fan: "duct0".into(),
            duty: 100.0,
            ttl_s: 60
        }))
        .await,
        FromHostd::Error {
            code: ErrorCode::NotFound,
            ..
        }
    ));
}

#[tokio::test]
async fn rc_core_method_form_and_bad_frames() {
    let (s, _d) = shared();
    s.apply_policy(policy(1, 350), true).await.unwrap();
    let mut c = Client::connect(&s);
    // hostctl's relay: {"method", "params"}, one line back. Extra params are ignored.
    let r = c
        .raw(r#"{"method":"assert","params":{"card":"A","reason":"engine_ready"}}"#)
        .await;
    assert!(matches!(r.msg, FromHostd::AssertResult(_)));
    let r = c
        .raw(r#"{"method":"wake","params":{"card":"A","source":"router"}}"#)
        .await;
    assert!(matches!(r.msg, FromHostd::Ack));
    let r = c.raw(r#"{"id":9,"type":"no_such_thing"}"#).await;
    assert_eq!(r.id, Some(9));
    assert!(matches!(
        r.msg,
        FromHostd::Error {
            code: ErrorCode::BadRequest,
            ..
        }
    ));
    let r = c.raw("not json").await;
    assert!(matches!(
        r.msg,
        FromHostd::Error {
            code: ErrorCode::BadRequest,
            ..
        }
    ));
    // The connection is still usable.
    assert!(matches!(
        c.call(ToHostd::Status).await,
        FromHostd::Status(_)
    ));
}

#[tokio::test]
async fn subscription_streams_telemetry_and_survives_client_drop() {
    let (s, _d) = shared();
    let mut c = Client::connect(&s);
    assert!(matches!(
        c.call(ToHostd::TelemetrySubscribe(Rates::default())).await,
        FromHostd::Ack
    ));
    let mut b = TelemetryBatch {
        ts: 5,
        ..Default::default()
    };
    b.sensors.insert("ctrl_A".into(), 61.0);
    s.telemetry.send(Arc::new(b.clone())).unwrap();
    let env: Envelope<FromHostd> = read_frame(&mut c.r, &mut c.buf).await.unwrap().unwrap();
    assert_eq!(env.id, None);
    match env.msg {
        FromHostd::Telemetry(got) => assert_eq!(*got, b),
        r => panic!("{r:?}"),
    }
    // The supervisor goes away: hostd's state is untouched, a new client carries on.
    s.apply_policy(policy(4, 350), true).await.unwrap();
    drop(c);
    let mut c2 = Client::connect(&s);
    match c2
        .call(ToHostd::Hello {
            proto: PROTOCOL_VERSION,
            version: "x".into(),
            client: "t".into(),
        })
        .await
    {
        FromHostd::Hello { policy_rev, .. } => assert_eq!(policy_rev, Some(4)),
        r => panic!("{r:?}"),
    }
}

#[test]
fn virtual_sensors_from_a_batch() {
    // The path the telemetry loop takes: batch → readings → store → registry.
    let mut reg =
        orca_sensors::SensorRegistry::new([("ctrl_D", "ema(first(gpu.D.hbm, gpu.D.core), 4s)")])
            .unwrap();
    let (gpus, _) = crate::sample::gpu::parse_output(
        "2, GPU-d, 00000000:44:00.0, Quadro M4000, 30, N/A, 10.69, 120.00, 135, 324, 772, 0, 0, 4733, 8192, 46, P8, 0x1, Enabled, 772, 3005\n",
        1_000,
    );
    let mut b = TelemetryBatch {
        ts: 1_000,
        ..Default::default()
    };
    b.gpus.insert("D".into(), gpus[0].clone());
    let mut store = SensorStore::new();
    store.extend(batch_readings(&b));
    let r = reg.evaluate(&mut store, 1_000);
    assert_eq!(
        r[0].result.as_ref().unwrap().v,
        30.0,
        "M4000: no HBM, falls back to core"
    );
}

#[test]
fn a_new_gpu_sample_replaces_the_cards_metrics() {
    let line = |hbm: &str| {
        format!("1, GPU-a, 00000000:21:00.0, Tesla V100-SXM3-32GB, 40, {hbm}, 50.0, 350.00, 1597, 958, 1597, 10, 1, 4000, 32768, N/A, P0, 0x0, Enabled, 1380, 958
")
    };
    let mut store = SensorStore::new();
    let mut b = TelemetryBatch {
        ts: 1_000,
        ..Default::default()
    };
    b.gpus.insert(
        "A".into(),
        crate::sample::gpu::parse_output(&line("44"), 1_000).0[0].clone(),
    );
    ingest(&mut store, &b);
    let hbm = orca_sensors::SensorId::parse("gpu.A.hbm").unwrap();
    assert!(store.get(&hbm).is_some());
    b.ts = 1_500;
    b.gpus.insert(
        "A".into(),
        crate::sample::gpu::parse_output(&line("N/A"), 1_500).0[0].clone(),
    );
    ingest(&mut store, &b);
    assert!(
        store.get(&hbm).is_none(),
        "HBM went N/A: absent, not the old value"
    );
    assert!(
        store
            .get(&orca_sensors::SensorId::parse("gpu.A.core").unwrap())
            .is_some()
    );
}
