use super::*;

fn cpu() -> orca_bridge::detect::Cpu {
    orca_bridge::detect::Cpu {
        cores: 8,
        ram_gib: 31.9,
        simd: vec!["avx2".into()],
    }
}

fn gpu() -> orca_bridge::detect::Gpu {
    orca_bridge::detect::Gpu {
        index: 0,
        uuid: "GPU-abc".into(),
        name: "RTX 4090".into(),
        vram_gib: 24.0,
    }
}

fn load_text(dir: &Path, text: &str) -> ControlConfig {
    let p = dir.join("router.toml");
    std::fs::write(&p, text).unwrap();
    orca_router::control::load::load(&p)
        .map(|l| l.config)
        .unwrap_or_else(|i| panic!("{i:?}\n{text}"))
}

#[test]
fn server_ids_come_from_the_hostname() {
    assert_eq!(server_id("HEYEMDEE"), "heyemdee");
    assert_eq!(server_id("James's PC.local"), "james-s-pc-local");
    assert_eq!(server_id("--"), "pc");
    assert_eq!(server_id(&"x".repeat(50)).len(), 32);
}

#[test]
fn a_new_machine_config_loads_with_the_detected_cards_and_old_overrides() {
    let d = tempfile::tempdir().unwrap();
    let p = Paths::of(d.path());
    let old: BridgeConfig = toml::from_str(
        r#"
        [bridge]
        id = "pc"
        model_dir = "D:/models"
        [cards.GPU0]
        label = "big card"
        engines = { image = ["comfyui"] }
        "#,
    )
    .unwrap();
    let text = render_main(
        &p,
        "HEYEMDEE",
        &[gpu()],
        &cpu(),
        &d.path().join("models"),
        Some(&old),
    );
    let c = load_text(d.path(), &text);
    let (id, s) = the_server(&c).unwrap();
    assert_eq!((id.as_str(), s.title.as_str()), ("heyemdee", "HEYEMDEE"));
    assert_eq!(s.model_dir.as_deref(), Some("D:/models"));
    let g = &c.cards["GPU0"];
    assert_eq!(
        (g.label.as_str(), g.vram_gib, g.managed, g.on_demand),
        ("big card", Some(24.0), true, true)
    );
    assert_eq!(
        g.engines.as_ref().unwrap()["image"],
        vec!["comfyui".to_string()]
    );
    assert_eq!(c.cards["CPU"].ram_gib, Some(31.9));
    assert!(!c.control.enabled);
}

fn manual() -> Manual {
    toml::from_str(
        r#"
        [engines.comfyui]
        kinds = ["image"]
        cards = ["GPU0"]
        port = 18188
        health = "/system_stats"
        [engines.comfyui.process]
        command = ["C:/ComfyUI/python.exe", "main.py", "--port", "18188"]
        "#,
    )
    .unwrap()
}

#[test]
fn manual_engines_are_mirrored_as_external_engines_and_translate_back() {
    let d = tempfile::tempdir().unwrap();
    let p = Paths::of(d.path());
    let text = render_main(&p, "pc1", &[gpu()], &cpu(), &d.path().join("models"), None);
    let text = text
        + "\n[engines.kokoro]\ndriver = \"package\"\nport = { CPU = 18890 }\npackage = { id = \"kokoro\", version = \"0.9.4\" }\n";
    let m = manual();
    let mirrored = mirror_text(&text, "pc1", &m).unwrap();
    let c = load_text(d.path(), &mirrored);
    let e = &c.engines["comfyui"];
    assert_eq!(e.driver, DriverKind::External);
    assert_eq!(e.port.get("GPU0"), Some(&18188));
    assert_eq!(e.labels[MANUAL_LABEL], "manual");
    // again: nothing changes; removed: the mirror goes, the package engine stays
    assert_eq!(mirror_text(&mirrored, "pc1", &m).unwrap(), mirrored);
    let gone = mirror_text(&mirrored, "pc1", &Manual::default()).unwrap();
    let c2 = load_text(d.path(), &gone);
    assert!(!c2.engines.contains_key("comfyui"));
    assert!(c2.engines.contains_key("kokoro"));

    let (b, problems) = to_bridge(&c, &m, &p.state);
    assert!(problems.is_empty(), "{problems:?}");
    assert_eq!(b.bridge.id, "pc1");
    let cf = &b.engines["comfyui"];
    assert!(cf.process.is_some(), "the manual engine runs as written");
    let k = &b.engines["kokoro"];
    assert_eq!(k.package.as_deref(), Some("kokoro@0.9.4"));
    assert_eq!(k.ports.get("CPU"), Some(&18890));
    assert_eq!(k.lifecycle, "on_demand");
    assert!(b.bridge.state_dir.ends_with("packages/pc1"));
    assert_eq!(b.cards["GPU0"].label.as_deref(), Some("RTX 4090"));
}

#[test]
fn a_router_engine_id_is_never_taken_over_by_a_manual_one() {
    let text = "version = 1\n[engines.comfyui]\ndriver = \"external\"\nport = { GPU0 = 1 }\n";
    assert!(
        mirror_text(text, "pc1", &manual())
            .unwrap_err()
            .contains("already an engine")
    );
}

#[test]
fn router_only_drivers_are_reported_not_run() {
    let d = tempfile::tempdir().unwrap();
    let p = Paths::of(d.path());
    let text = render_main(&p, "pc1", &[], &cpu(), &d.path().join("m"), None)
        + "\n[engines.llama]\ndriver = \"docker\"\nimage = \"x\"\nport = { CPU = 9000 }\n";
    let c = load_text(d.path(), &text);
    let (b, problems) = to_bridge(&c, &Manual::default(), &p.state);
    assert!(!b.engines.contains_key("llama"));
    assert!(problems[0].contains("engines.llama"), "{problems:?}");
}

#[test]
fn remote_changes_are_refused_or_wait_for_approval() {
    let d = tempfile::tempdir().unwrap();
    let p = Paths::of(d.path());
    let base = render_main(&p, "pc1", &[gpu()], &cpu(), &d.path().join("models"), None);
    let old = load_text(d.path(), &base);
    // a card's engines and a store package: fine
    let ok = base.clone()
        + "\n[engines.kokoro]\ndriver = \"package\"\nport = { CPU = 18890 }\npackage = { id = \"kokoro\", version = \"0.9.4\" }\n";
    assert_eq!(
        remote_config_check(&old, &load_text(d.path(), &ok)).unwrap(),
        Vec::<String>::new()
    );
    // a host process: waits for the user
    let proc_ = base.clone()
        + "\n[engines.x]\ndriver = \"process\"\nport = { CPU = 1 }\nprocess = { command = \"/bin/x\" }\n";
    let need = remote_config_check(&old, &load_text(d.path(), &proc_)).unwrap();
    assert!(need[0].contains("engines.x"), "{need:?}");
    // a host package method
    let host = base.clone()
        + "\n[engines.k]\ndriver = \"package\"\nport = { CPU = 1 }\npackage = { id = \"kokoro\", version = \"0.9.4\", method = \"pip\" }\n";
    assert!(
        !remote_config_check(&old, &load_text(d.path(), &host))
            .unwrap()
            .is_empty()
    );
    // the model folder
    let moved = base.replace("model_dir = ", "model_dir = \"C:/elsewhere\"\n# was ");
    assert!(
        remote_config_check(&old, &load_text(d.path(), &moved)).unwrap()[0]
            .contains("model folder")
    );
    // refused outright: the control plane, a second server
    let ctl = base.replace("[control]\nenabled = false", "[control]\nenabled = true");
    assert!(remote_config_check(&old, &toml::from_str(&ctl).unwrap()).is_err());
    let two = base.clone() + "\n[servers.other]\nhost = \"x\"\n";
    assert!(
        remote_config_check(&old, &toml::from_str(&two).unwrap())
            .unwrap_err()
            .contains("server")
    );
}

#[test]
fn manual_engines_from_a_router_need_approval_for_processes_flags_and_mounts() {
    let none = Manual::default();
    assert!(manual_check(&none, &manual(), Some("D:/models"))[0].contains("a process"));
    let docker: Manual = toml::from_str(
        r#"
        [engines.a]
        port = 1
        [engines.a.docker]
        image = "x"
        container_port = 80
        mounts = ["D:/models/hf:/models"]
        [engines.b]
        port = 2
        [engines.b.docker]
        image = "y"
        container_port = 80
        mounts = ["C:/Users:/u"]
        flags = ["--shm-size=1g"]
        "#,
    )
    .unwrap();
    let need = manual_check(&none, &docker, Some("D:/models"));
    assert!(need.iter().all(|n| n.starts_with("engines.b")), "{need:?}");
    assert_eq!(need.len(), 2);
    // unchanged engines are not asked about again
    assert!(manual_check(&docker, &docker, Some("D:/models")).is_empty());
}

#[test]
fn paths_inside_the_model_folder() {
    assert!(inside("D:/models/hf/x", "D:/models"));
    assert!(inside("d:\\Models\\a", "D:/models/"));
    assert!(!inside("D:/models2", "D:/models"));
    assert!(!inside("D:/models/../x", "D:/models"));
    assert!(!inside("/x", ""));
}

#[test]
fn sharing_defaults_follow_bridge_mode_and_shape_the_descriptor() {
    let d = tempfile::tempdir().unwrap();
    let p = Paths::of(d.path());
    let text = render_main(&p, "pc1", &[gpu()], &cpu(), &d.path().join("models"), None)
        + "\n[engines.a]\ndriver = \"external\"\nport = { GPU0 = 1 }\n";
    let c = load_text(d.path(), &text);
    let mut s = Share::default();
    assert!(s.reconcile(&c, false));
    assert!(!s.engine("a"));
    let text2 = text + "\n[engines.b]\ndriver = \"external\"\nport = { GPU0 = 2 }\n";
    let c2 = load_text(d.path(), &text2);
    assert!(s.reconcile(&c2, true));
    assert!(s.engine("b") && !s.engine("a"));
    let mut desc: Descriptor = serde_json::from_value(json!({"v": 1, "server": {"id": "pc1"},
        "cards": [{"id": "GPU0", "device": "gpu", "engines": {"image": ["a", "b"]}}],
        "engines": [{"id": "a", "ports": {"GPU0": 1}}, {"id": "b", "ports": {"GPU0": 2}}],
        "files": {"hf": ["me/secret", "me/public"]}}))
    .unwrap();
    orca_bridge::detect::share_only(&mut desc, &|e| s.engine(e), &["me/secret".into()]);
    assert_eq!(desc.engines.len(), 1);
    assert_eq!(desc.cards[0].engines["image"], vec!["b".to_string()]);
    assert_eq!(desc.files["hf"], vec!["me/public".to_string()]);
}

// ── the machine itself, in-process router and all ──────────────────────────────────────────

fn desktop_from_file(home: &Path) -> DesktopConfig {
    std::fs::read_to_string(home.join("desktop.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn set_desktop(home: &Path, v: Value) {
    std::fs::write(home.join("desktop.json"), v.to_string()).unwrap();
}

#[tokio::test]
async fn the_machine_edits_through_its_own_router_and_guards_remote_changes() {
    let d = tempfile::tempdir().unwrap();
    let home = d.path();
    set_desktop(home, json!({"bridgeMode": false}));
    let m = Machine::open(home, desktop_from_file).await.unwrap();
    // the router's admin API, in-process
    let (s, v, _) = m.admin(Method::GET, "inventory", None, Bytes::new()).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["servers"].as_array().unwrap().len(), 1);
    let (text, rev) = m.config_doc().await.unwrap();

    // remote config off: read-only
    let r = m
        .remote("config.get", &json!({"who": "admin:ann"}), false)
        .await;
    assert_eq!(r["status"], 403);
    assert_eq!(r["body"]["readOnly"], true);

    set_desktop(
        home,
        json!({"bridgeMode": true, "bridgeRemoteConfig": true}),
    );
    let r = m
        .remote("config.get", &json!({"who": "admin:ann"}), false)
        .await;
    assert_eq!(
        (r["status"].as_u64(), r["body"]["rev"].as_str()),
        (Some(200), Some(rev.as_str()))
    );

    // a stale revision: 412, nothing written
    let changed = text.replace("label = \"CPU\"", "label = \"the CPU\"");
    assert_ne!(changed, text);
    let r = m
        .remote(
            "config.apply",
            &json!({"text": changed, "rev": "stale", "who": "admin:ann"}),
            false,
        )
        .await;
    assert_eq!(r["status"], 412, "{r}");
    assert_eq!(r["body"]["currentRev"], rev.as_str());
    // the right one: saved, logged, undoable
    let r = m
        .remote(
            "config.apply",
            &json!({"text": changed, "rev": rev, "who": "admin:ann", "reason": "rename"}),
            false,
        )
        .await;
    assert_eq!(r["status"], 200, "{r}");
    assert!(
        std::fs::read_to_string(&m.paths.main)
            .unwrap()
            .contains("the CPU")
    );
    let log = m.remote_changes(5);
    assert_eq!(
        (log[0].who.as_str(), log[0].what.as_str()),
        ("admin:ann", "rename")
    );
    let (s, v) = m.undo(&log[0].id).await;
    assert_eq!(s, 200, "{v}");
    assert!(
        !std::fs::read_to_string(&m.paths.main)
            .unwrap()
            .contains("the CPU")
    );
    assert!(m.remote_changes(5)[0].undone);

    // something that needs approval waits, and runs when the user approves it
    let (text, rev) = m.config_doc().await.unwrap();
    let proc_ = text
        + "\n[engines.x]\ndriver = \"process\"\nport = { CPU = 1 }\nprocess = { command = \"x\" }\n";
    let r = m
        .remote(
            "config.apply",
            &json!({"text": proc_, "rev": rev, "who": "admin:ann"}),
            false,
        )
        .await;
    assert_eq!(r["status"], 202, "{r}");
    assert!(
        !std::fs::read_to_string(&m.paths.main)
            .unwrap()
            .contains("[engines.x]")
    );
    let pend = m.pending();
    assert_eq!(pend.len(), 1);
    let (s, v) = m.decide(&pend[0].id, true).await;
    assert_eq!(s, 200, "{v}");
    assert!(
        std::fs::read_to_string(&m.paths.main)
            .unwrap()
            .contains("[engines.x]")
    );
    assert!(m.pending().is_empty());

    // the router may not call anything else, nor browse outside the model folder
    let r = m
        .remote(
            "machine.api",
            &json!({"method": "GET", "path": "admin-keys", "who": "a"}),
            false,
        )
        .await;
    assert_eq!(r["status"], 403);
    let r = m
        .remote(
            "machine.api",
            &json!({"method": "GET", "path": "inventory/browse", "query": "server=x&path=C%3A%2FUsers", "who": "a"}),
            false,
        )
        .await;
    assert_eq!(r["status"], 403, "{r}");
    // a store package that needs approvals waits
    let r = m
        .remote(
            "machine.api",
            &json!({"method": "POST", "path": "engine-store/install", "who": "a",
                    "body": {"store": "default", "id": "comfyui", "approvals": {"privileged": true}}}),
            false,
        )
        .await;
    assert_eq!(r["status"], 202, "{r}");

    // manual engines: saved with a revision and mirrored into router.toml
    let doc = m.engines_doc();
    let (s, v) = m
        .save_engines(
            json!({"tts": {"kinds": ["tts"], "port": 18900, "lifecycle": "external"}}),
            doc["rev"].as_str().unwrap(),
            "me",
        )
        .await;
    assert_eq!(s, 200, "{v}");
    let (s, _) = m
        .save_engines(json!({}), doc["rev"].as_str().unwrap(), "me")
        .await;
    assert_eq!(s, 412);
    let c = m.config().unwrap();
    assert_eq!(c.engines["tts"].labels[MANUAL_LABEL], "manual");
    m.tick().await;
    let st = m.status();
    assert_eq!(st["server"], server_id(&hostname()));
    // added while bridged: shared by default
    assert_eq!(st["share"]["engines"]["tts"], true);
    // and the engines run under a supervisor rebuilt for the change
    let running: Vec<&str> = st["engines"]["engines"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["engine"].as_str())
        .collect();
    assert!(running.contains(&"tts"), "{st}");
}

// ── Local Engines (D156) through the host's handler ─────────────────────────────────────────

async fn call(m: &Arc<Machine>, method: Method, path: &str, body: Value) -> (u16, Value) {
    let req = Request::builder()
        .method(method)
        .uri(format!("/api/machine/{path}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let r = m.handle(req).await;
    let s = r.status().as_u16();
    let b = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
    (s, serde_json::from_slice(&b).unwrap_or(Value::Null))
}

#[tokio::test]
async fn local_engines_add_check_route_and_remove_an_unmanaged_engine() {
    use axum::routing::{get, post};
    // a stand-in for LM Studio: lists one model, echoes the key it got
    let app = axum::Router::new()
        .route(
            "/v1/models",
            get(|| async { axum::Json(json!({"data": [{"id": "local-llm"}]})) }),
        )
        .route(
            "/v1/chat/completions",
            post(|h: HeaderMap, b: Bytes| async move {
                let auth = h
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                let v: Value = serde_json::from_slice(&b).unwrap_or(Value::Null);
                axum::Json(json!({"auth": auth, "model": v["model"]}))
            }),
        );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });

    let d = tempfile::tempdir().unwrap();
    let home = d.path();
    set_desktop(home, json!({"bridgeMode": false}));
    let m = Machine::open(home, desktop_from_file).await.unwrap();

    let (s, v) = call(&m, Method::GET, "local-engines", Value::Null).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["endpoints"], json!([]));
    let rev = v["rev"].as_str().unwrap().to_string();

    let ep = json!({"title": "LM Studio", "kind": "llm", "url": format!("http://{addr}/v1/"), "models": ["local-llm"]});
    // Check, before it is saved
    let (s, v) = call(&m, Method::POST, "endpoints/check", json!({"endpoint": ep})).await;
    assert_eq!((s, &v["ok"]), (200, &json!(true)), "{v}");
    assert_eq!(v["models"], json!(["local-llm"]));
    // a bad one is refused with the reason
    let (s, v) = call(
        &m,
        Method::PUT,
        "endpoints/x",
        json!({"endpoint": {"kind": "llm", "url": "nope"}}),
    )
    .await;
    assert_eq!(s, 422, "{v}");
    // saved with a key; the key is never shown
    let (s, v) = call(
        &m,
        Method::PUT,
        "endpoints/lm-studio",
        json!({"endpoint": ep, "rev": rev, "key": "placeholder-key"}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    // a stale revision is refused
    let (s, _) = call(
        &m,
        Method::PUT,
        "endpoints/lm-studio",
        json!({"endpoint": ep, "rev": rev}),
    )
    .await;
    assert_eq!(s, 412);
    let (_, v) = call(&m, Method::GET, "local-engines", Value::Null).await;
    assert!(!v.to_string().contains("placeholder-key"), "{v}");
    assert_eq!(v["endpoints"][0]["id"], "lm-studio");
    assert_eq!(v["endpoints"][0]["url"], format!("http://{addr}/v1"));
    assert_eq!(v["endpoints"][0]["keySet"], true);

    // /api/machine/v1 lists its models and sends its requests to it, with its key
    let (_, v) = call(&m, Method::GET, "v1/models", Value::Null).await;
    assert!(
        v["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["id"] == "local-llm" && x["owned_by"] == "lm-studio"),
        "{v}"
    );
    let (s, v) = call(
        &m,
        Method::POST,
        "v1/chat/completions",
        json!({"model": "local-llm", "messages": []}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(
        v,
        json!({"auth": "Bearer placeholder-key", "model": "local-llm"})
    );

    // saved again without a key: the stored one is kept; "" removes it
    let rev = call(&m, Method::GET, "local-engines", Value::Null).await.1["rev"]
        .as_str()
        .unwrap()
        .to_string();
    let (s, v) = call(
        &m,
        Method::PUT,
        "endpoints/lm-studio",
        json!({"endpoint": ep, "rev": rev}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let (_, v) = call(&m, Method::GET, "local-engines", Value::Null).await;
    assert_eq!(v["endpoints"][0]["keySet"], true);

    // tools: an image endpoint exposed becomes the image default
    let img = json!({"title": "Images", "kind": "image", "url": "http://127.0.0.1:9/"});
    let (s, v) = call(
        &m,
        Method::PUT,
        "endpoints/images",
        json!({"endpoint": img}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = call(
        &m,
        Method::PUT,
        "tools",
        json!({"exposed": {"images": true}}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["defaults"]["image"], "images");
    // sharing an endpoint is kept in share.json beside the engines
    let (s, _) = call(
        &m,
        Method::PUT,
        "share",
        json!({"endpoints": {"lm-studio": true}}),
    )
    .await;
    assert_eq!(s, 200);
    let (_, v) = call(&m, Method::GET, "local-engines", Value::Null).await;
    assert_eq!(v["endpoints"][1]["shared"], true, "{v}");

    // removed: its key, sharing and tool default go too
    let (s, _) = call(&m, Method::DELETE, "endpoints/images", Value::Null).await;
    assert_eq!(s, 200);
    let t = crate::local_engines::Tools::load(&m.paths.tools);
    assert!(!t.defaults.contains_key("image"));
    let (s, _) = call(&m, Method::DELETE, "endpoints/lm-studio", Value::Null).await;
    assert_eq!(s, 200);
    assert!(!m.paths.dir.join("secrets").join("lm-studio").exists());
    assert!(Share::load(&m.paths.share).endpoints.is_empty());
    let (s, _) = call(&m, Method::DELETE, "endpoints/lm-studio", Value::Null).await;
    assert_eq!(s, 404);
}
