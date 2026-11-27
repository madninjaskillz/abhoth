//! ExecHost against a fake host: a FakeExecutor whose handler models docker containers by
//! name, GPU memory and the builtins.

use std::collections::BTreeMap;
use std::sync::Mutex;

use super::*;
use crate::control::exec::{ExecRequest, FakeExecutor, Op};
use crate::control::hostd::NoHostd;
use crate::control::validate::tests::sample;

#[derive(Default)]
struct World {
    /// container -> (running, label JSON)
    containers: BTreeMap<String, (bool, String)>,
    gpu_used: BTreeMap<String, u64>,
    builtins: Vec<(String, Value)>,
    commands: Vec<String>,
}

fn host() -> (ExecHost, Arc<Mutex<World>>, FakeExecutor) {
    let c = Arc::new(sample());
    let w = Arc::new(Mutex::new(World::default()));
    let ww = w.clone();
    let cc = c.clone();
    let fake = FakeExecutor::new();
    fake.with_handler(move |r: &ExecRequest| {
        let mut w = ww.lock().unwrap();
        match &r.op {
            Op::Command { id, text, .. } => {
                w.commands.push(id.clone());
                if id.starts_with("docker-inspect:") {
                    let found = w.containers.iter().find(|(n, _)| text.contains(n.as_str())).map(|(n, v)| (n.clone(), v.clone()));
                    return Some(Ok(match found {
                        Some((_, (running, label))) => ExecOutput::ok_with(&format!("{{\"Running\":{running},\"Status\":\"x\"}}|{label}")),
                        None => ExecOutput::fail(1, "No such object"),
                    }));
                }
                if id.starts_with("docker-run:") {
                    // docker run -d --name NAME ... --label layouts.spec=JSON ...
                    let cname = text.split_whitespace().skip_while(|t| *t != "--name").nth(1).unwrap_or("").to_string();
                    let card = id.split(':').nth(2).unwrap_or("A").to_string();
                    let uuid = cc.cards[&card].gpu_uuid.clone();
                    w.containers.insert(cname, (true, "{}".into()));
                    w.gpu_used.insert(uuid, 20000);
                }
                Some(Ok(ExecOutput::ok_with("")))
            }
            Op::Builtin { name, args } => {
                w.builtins.push((name.clone(), args.clone()));
                Some(Ok(match name.as_str() {
                    "docker-rm-robust" => {
                        let n = args["name"].as_str().unwrap_or("").to_string();
                        let card = n.rsplit('-').next().unwrap_or("").to_uppercase();
                        if let Some(uuid) = cc.cards.get(&card).map(|cd| cd.gpu_uuid.clone()) {
                            w.gpu_used.insert(uuid, 5);
                        }
                        w.containers.remove(&n);
                        ExecOutput::ok_with("removed")
                    }
                    "telemetry" => {
                        let lines: Vec<String> = cc
                            .cards
                            .values()
                            .enumerate()
                            .map(|(i, cd)| {
                                format!(
                                    "{}, {i}, V100, 40, 0, 0, {}, 32768, 50, 300, 1380, 877, 1597, 1380, [N/A], P0, 0x0",
                                    cd.gpu_uuid,
                                    w.gpu_used.get(&cd.gpu_uuid).copied().unwrap_or(5)
                                )
                            })
                            .collect();
                        ExecOutput::ok_with(&lines.join("\n"))
                    }
                    "compute-apps" => ExecOutput::ok_with(&format!(
                        "[{{\"gpu_uuid\":\"{}\",\"pid\":7,\"used_mib\":900,\"container\":\"comfyui\",\"process\":\"python\"}}]",
                        cc.cards["B"].gpu_uuid
                    )),
                    "oom-kills" => ExecOutput::ok_with("[\"Out of memory: Killed process 1 (x)\"]"),
                    "file-exists" => ExecOutput::ok_with("yes"),
                    _ => ExecOutput::ok_with("ok"),
                }))
            }
            Op::Hostd { .. } => Some(Ok(ExecOutput::ok_with("{}"))),
        }
    });
    let h = ExecHost::new(
        c,
        "gpu",
        Arc::new(fake.clone()),
        Arc::new(NoHostd),
        None,
        Scrubber::default(),
    );
    (h, w, fake)
}

fn spec_of(h: &ExecHost, engine: &str, profile: &str, model: &str) -> CardSpec {
    wiring::spec_of(
        &h.cfg(),
        &schema::CardSpec {
            engine: engine.into(),
            profile: profile.into(),
            model: model.into(),
            devices: Vec::new(),
        },
    )
}

#[tokio::test]
async fn observe_reads_labels_and_foreign_holders() {
    let (h, w, _) = host();
    let c = h.cfg();
    let s = spec_of(&h, "ninfer", "4lane", "dense");
    let label = resolve::py_json(&resolve::flat_spec(&c, &ExecHost::schema_spec(&s)).unwrap());
    w.lock()
        .unwrap()
        .containers
        .insert("ninfer-a".into(), (true, label));
    w.lock()
        .unwrap()
        .containers
        .insert("ninfer-b".into(), (false, "{}".into()));
    let o = h.observe().await.unwrap();
    assert_eq!(o.spec["A"], s);
    assert!(o.spec["B"].is_off(), "a stopped container is off");
    assert_eq!(
        o.foreign.get("B").cloned().unwrap_or_default(),
        vec!["comfyui".to_string()]
    );
    assert!(!o.foreign.contains_key("A"));
}

#[tokio::test]
async fn unlabelled_instances_are_adopted_from_the_active_layout_or_marked_unknown() {
    let (mut h, w, _) = host();
    w.lock()
        .unwrap()
        .containers
        .insert("ninfer-a".into(), (true, "".into()));
    let o = h.observe().await.unwrap();
    assert_eq!(o.spec["A"].engine, "ninfer");
    assert!(o.spec["A"].detail.contains_key("unknown"));
    h.active = Arc::new(|| Some("dense-x2".into()));
    let o = h.observe().await.unwrap();
    assert_eq!(o.spec["A"], spec_of(&h, "ninfer", "4lane", "dense"));
}

#[tokio::test]
async fn start_stop_hooks_locks_and_readings() {
    let (h, w, fake) = host();
    let s = spec_of(&h, "ninfer", "4lane", "dense");
    h.start("A", &s).await.unwrap();
    assert!(w.lock().unwrap().containers.contains_key("ninfer-a"));
    assert_eq!(h.gpu_used_mib("A").await.unwrap(), 20000);
    assert!(h.stop_once("A", &s).await.unwrap());
    assert!(h.gpu_used_mib("A").await.unwrap() < 1024);
    assert_eq!(h.clocks("A").await.unwrap().max_sm_mhz, 1597);
    assert_eq!(h.oom_kills().await.unwrap().len(), 1);

    // Strata's before_start hooks render {lanes} into the json-set value.
    let st = spec_of(
        &h,
        "strata",
        &h.cfg().engines["strata"]
            .profiles
            .keys()
            .next()
            .unwrap()
            .clone(),
        "flash",
    );
    let notes = h.before_start("A", &st).await.unwrap();
    assert!(!notes.is_empty());
    let b = w.lock().unwrap().builtins.clone();
    let js = b
        .iter()
        .find(|(n, _)| n == "json-set")
        .expect("json-set ran");
    assert_eq!(js.1["value"], st.lanes.to_string());

    h.lock().await.unwrap();
    h.unlock().await;
    let flocks: Vec<String> = w
        .lock()
        .unwrap()
        .builtins
        .iter()
        .filter(|(n, _)| n == "flock")
        .map(|(_, a)| a["op"].as_str().unwrap().to_string())
        .collect();
    let locks = h.cfg().servers["gpu"].locks.len();
    assert_eq!(flocks.len(), 2 * locks);
    // Every root command the host saw is in the exported allowlist.
    let al = crate::control::hostctl::Allowlist::parse(
        &crate::control::hostctl::export(
            &h.cfg(),
            "gpu",
            crate::control::hostctl::suggested_paths(&h.cfg(), "gpu"),
        )
        .unwrap(),
    )
    .unwrap();
    for r in fake.requests() {
        if let Op::Command { .. } = r.op {
            assert_eq!(al.check(&r), Ok(()), "{:?}", r.op.label());
        }
    }
}

#[tokio::test]
async fn health_of_a_dead_engine_is_exited_with_its_log() {
    let (h, w, _) = host();
    let s = spec_of(&h, "ninfer", "4lane", "dense");
    w.lock()
        .unwrap()
        .containers
        .insert("ninfer-a".into(), (false, "{}".into()));
    // Nothing listens on the engine port in tests: the status read says it is gone.
    match h.health("A", &s).await {
        Health::Exited(_) => {}
        other => panic!("{other:?}"),
    }
    w.lock()
        .unwrap()
        .containers
        .insert("ninfer-a".into(), (true, "{}".into()));
    assert!(matches!(h.health("A", &s).await, Health::NotYet(_)));
}

#[tokio::test]
async fn preconditions_refuse_unrenderable_specs() {
    let (h, _, _) = host();
    let mut t = Spec::new();
    t.insert("A".into(), CardSpec::new("ninfer", "nope", "dense", 4));
    assert!(h.preconditions(&t).await.is_err());
    let mut t = Spec::new();
    t.insert("A".into(), spec_of(&h, "ninfer", "4lane", "dense"));
    t.insert("B".into(), CardSpec::off());
    h.preconditions(&t).await.unwrap();
}
