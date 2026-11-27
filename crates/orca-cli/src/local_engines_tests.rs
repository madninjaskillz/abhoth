use super::*;

fn ep(kind: &str, url: &str) -> Endpoint {
    Endpoint {
        kind: kind.into(),
        url: url.into(),
        ..Default::default()
    }
}

/// This machine's inventory (as a new machine writes it: one GPU and the CPU) with an LLM
/// engine serving one model and an image engine.
fn inventory() -> ControlConfig {
    let d = tempfile::tempdir().unwrap();
    let p = crate::machine::Paths::of(d.path());
    let gpu = orca_bridge::detect::Gpu {
        index: 0,
        uuid: "GPU-abc".into(),
        name: "RTX 4090".into(),
        vram_gib: 24.0,
    };
    let cpu = orca_bridge::detect::Cpu {
        cores: 8,
        ram_gib: 31.9,
        simd: vec![],
    };
    let text = crate::machine::render_main(&p, "pc1", &[gpu], &cpu, &d.path().join("models"), None)
        + r#"
[engines.ninfer]
driver = "external"
port = { GPU0 = 8094 }

[engines.comfyui]
driver = "external"
port = { GPU0 = 8188 }

[models.qwen3-8b]
title = "Qwen3 8B"
[models.qwen3-8b.artifacts.ninfer]
file = "qwen3-8b.gguf"

[models.flux]
kind = "image"
[models.flux.artifacts.comfyui]
file = "flux.safetensors"
"#;
    let f = d.path().join("router.toml");
    std::fs::write(&f, &text).unwrap();
    orca_router::control::load::load(&f)
        .map(|l| l.config)
        .unwrap_or_else(|i| {
            panic!(
                "{i:?}
{text}"
            )
        })
}

#[test]
fn ids_are_checked_and_made_from_titles() {
    assert!(valid_id("lm-studio"));
    assert!(valid_id("a1_b"));
    assert!(!valid_id(""));
    assert!(!valid_id("-x"));
    assert!(!valid_id("LM"));
    assert!(!valid_id(&"x".repeat(41)));
    let taken: BTreeSet<String> = ["lm-studio".to_string()].into();
    assert_eq!(id_from("LM Studio", &taken), "lm-studio-2");
    assert_eq!(id_from("  ComfyUI (NAS)! ", &taken), "comfyui-nas");
    assert_eq!(id_from("!!!", &taken), "engine");
}

#[test]
fn an_endpoint_is_cleaned_and_gets_the_kinds_default_api() {
    let mut e = ep("LLM", " http://127.0.0.1:1234/v1/ ");
    e.models = vec![" a ".into(), "".into(), "a".into(), "b".into()];
    e.timeout_seconds = Some(0);
    e.uncensored_model = Some("  ".into());
    let e = check("lm-studio", &e, &BTreeSet::new()).unwrap();
    assert_eq!(e.kind, "llm");
    assert_eq!(e.api, "openai");
    assert_eq!(e.group, "unmanaged");
    assert_eq!(e.title, "lm-studio");
    assert_eq!(e.url, "http://127.0.0.1:1234/v1");
    assert_eq!(e.models, vec!["a", "b"]);
    assert_eq!(e.timeout_seconds, None);
    assert_eq!(e.uncensored_model, None);
}

#[test]
fn bad_endpoints_are_refused_with_a_reason() {
    let none = BTreeSet::new();
    let err =
        |id: &str, e: Endpoint, managed: &BTreeSet<String>| check(id, &e, managed).unwrap_err();
    assert!(err("x", ep("speech", "http://h"), &none).contains("Pick a kind"));
    assert!(err("x", ep("llm", "ftp://h"), &none).contains("http://"));
    assert!(err("x", ep("llm", "http://"), &none).contains("http://"));
    let mut e = ep("image", "http://h:8188");
    e.api = "anthropic".into();
    assert!(err("x", e, &none).contains("openai-images or comfyui"));
    let mut e = ep("llm", "http://h");
    e.group = "elsewhere".into();
    assert!(err("x", e, &none).contains("Unknown group"));
    let managed: BTreeSet<String> = ["ninfer".to_string()].into();
    assert!(err("ninfer", ep("llm", "http://h"), &managed).contains("managed engine"));
    assert!(err("Bad Id", ep("llm", "http://h"), &none).contains("a-z"));
}

#[test]
fn ollama_is_kept_by_its_root_and_spoken_to_at_v1() {
    let mut e = ep("llm", "http://127.0.0.1:11434/v1/");
    e.api = "ollama".into();
    let e = check("ollama", &e, &BTreeSet::new()).unwrap();
    assert_eq!(e.url, "http://127.0.0.1:11434");
    assert_eq!(openai_base(&e), "http://127.0.0.1:11434/v1");
    let o = check("o", &ep("llm", "http://h/v1"), &BTreeSet::new()).unwrap();
    assert_eq!(openai_base(&o), "http://h/v1");
}

#[test]
fn wyoming_endpoints_may_use_tcp_and_others_may_not() {
    let mut e = ep("stt", "tcp://nas:10300");
    e.api = "wyoming".into();
    assert!(check("whisper", &e, &BTreeSet::new()).is_ok());
    assert!(check("whisper", &ep("stt", "tcp://nas:10300"), &BTreeSet::new()).is_err());
}

#[test]
fn a_model_name_served_twice_is_a_clash() {
    let c = inventory();
    let mut eps = Endpoints::default();
    let mut a = ep("llm", "http://a/v1");
    a.models = vec!["qwen3-8b".into(), "solo".into()];
    eps.endpoints.insert("a".into(), a);
    let mut b = ep("llm", "http://b/v1");
    b.models = vec!["solo".into()];
    eps.endpoints.insert("b".into(), b);
    let x = clashes(&c, &eps);
    assert_eq!(x.len(), 2, "{x:?}");
    assert!(x[0].contains("`qwen3-8b` is served by this machine's models and `a`"));
    assert!(x[1].contains("`solo` is served by `a` and `b`"));
}

#[test]
fn managed_engines_list_their_kinds_cards_and_models() {
    let c = inventory();
    let k = managed_kinds(&c);
    assert_eq!(k["ninfer"], BTreeSet::from(["llm".to_string()]));
    assert_eq!(k["comfyui"], BTreeSet::from(["image".to_string()]));
    assert_eq!(managed_models(&c, "ninfer"), vec!["qwen3-8b"]);
    assert_eq!(managed_models(&c, "comfyui"), vec!["flux"]);
}

#[test]
fn keys_are_stored_apart_and_never_in_the_view() {
    let d = tempfile::tempdir().unwrap();
    let mut e = check(
        "cloud-a",
        &ep("llm", "https://api.example.com/v1"),
        &BTreeSet::new(),
    )
    .unwrap();
    e.key = store_key(d.path(), "cloud-a", Some(" placeholder-key ")).unwrap();
    assert_eq!(e.key.as_deref(), Some("secret:cloud-a"));
    assert_eq!(read_key(d.path(), &e).as_deref(), Some("placeholder-key"));
    let mut eps = Endpoints::default();
    eps.endpoints.insert("cloud-a".into(), e.clone());
    let v = view(
        None,
        &eps,
        d.path(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &Tools::default(),
        &BTreeMap::new(),
    );
    let text = v.to_string();
    assert!(!text.contains("placeholder-key"), "{text}");
    assert!(!text.contains("secret:"), "{text}");
    assert_eq!(v["endpoints"][0]["keySet"], json!(true));
    assert_eq!(v["endpoints"][0]["shared"], json!(false));
    assert_eq!(store_key(d.path(), "cloud-a", None).unwrap(), None);
    assert_eq!(read_key(d.path(), &e), None);
}

#[test]
fn the_view_shows_managed_engines_with_state_sharing_and_tools() {
    let d = tempfile::tempdir().unwrap();
    let c = inventory();
    let tools = Tools {
        exposed: [("comfyui".to_string(), true)].into(),
        defaults: [("image".to_string(), "comfyui".to_string())].into(),
    };
    let states: BTreeMap<String, Vec<Value>> = [(
        "ninfer".to_string(),
        vec![json!({"card": "GPU0", "state": "running"})],
    )]
    .into();
    let v = view(
        Some(&c),
        &Endpoints::default(),
        d.path(),
        &BTreeMap::new(),
        &[("ninfer".to_string(), true)].into(),
        &tools,
        &states,
    );
    let m = v["managed"].as_array().unwrap();
    let ninfer = m.iter().find(|e| e["id"] == "ninfer").unwrap();
    assert_eq!(ninfer["shared"], json!(true));
    assert_eq!(ninfer["cards"], json!(["GPU0"]));
    assert_eq!(ninfer["states"][0]["state"], json!("running"));
    let comfy = m.iter().find(|e| e["id"] == "comfyui").unwrap();
    assert_eq!(comfy["tool"], json!(true));
    assert_eq!(v["defaults"]["image"], json!("comfyui"));
    assert_eq!(v["tools"]["image"], json!("generate_image"));
    assert_eq!(v["tools"]["tts"], json!("speak"));
    assert!(v["tools"].get("llm").is_none());
}

#[test]
fn a_kinds_default_tool_engine_follows_what_is_exposed() {
    let kinds: BTreeMap<String, BTreeSet<String>> = [
        ("comfy".to_string(), BTreeSet::from(["image".to_string()])),
        ("dalle".to_string(), BTreeSet::from(["image".to_string()])),
        ("kokoro".to_string(), BTreeSet::from(["tts".to_string()])),
    ]
    .into();
    let mut t = Tools::default();
    t.exposed.insert("dalle".into(), true);
    t.exposed.insert("gone".into(), true);
    settle_defaults(&mut t, &kinds);
    assert_eq!(t.defaults.get("image").map(String::as_str), Some("dalle"));
    assert!(!t.exposed.contains_key("gone"));
    assert!(!t.defaults.contains_key("tts"));
    // a chosen default that is exposed stays
    t.exposed.insert("comfy".into(), true);
    t.defaults.insert("image".into(), "comfy".into());
    settle_defaults(&mut t, &kinds);
    assert_eq!(t.defaults["image"], "comfy");
    // not exposed any more: the next exposed one
    t.exposed.insert("comfy".into(), false);
    settle_defaults(&mut t, &kinds);
    assert_eq!(t.defaults["image"], "dalle");
    // nothing exposed: no default
    t.exposed.insert("dalle".into(), false);
    settle_defaults(&mut t, &kinds);
    assert!(!t.defaults.contains_key("image"));
}

#[test]
fn the_files_load_when_missing_and_old_share_files_still_parse() {
    let d = tempfile::tempdir().unwrap();
    assert_eq!(
        Endpoints::load(&d.path().join("endpoints.json")).unwrap(),
        Endpoints::default()
    );
    assert_eq!(Tools::load(&d.path().join("tools.json")), Tools::default());
    let bad = d.path().join("bad.json");
    std::fs::write(&bad, "{not json").unwrap();
    assert!(Endpoints::load(&bad).is_err());
    // share.json written by 0.16.36 (no `endpoints`)
    let s: crate::machine::Share =
        serde_json::from_str(r#"{"engines":{"ninfer":true},"models":{}}"#).unwrap();
    assert!(s.endpoints.is_empty());
    assert!(s.engine("ninfer"));
    // and a share.json without endpoints is written as before
    assert!(!serde_json::to_string(&s).unwrap().contains("endpoints"));
}

#[tokio::test]
async fn check_lists_an_llm_servers_models_and_reports_a_dead_one() {
    use axum::routing::get;
    let app = axum::Router::new().route(
        "/v1/models",
        get(|h: axum::http::HeaderMap| async move {
            let auth = h
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            axum::Json(json!({"data": [{"id": "m1"}, {"id": auth}]}))
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    let http = reqwest::Client::new();
    let e = check(
        "x",
        &ep("llm", &format!("http://{addr}/v1")),
        &BTreeSet::new(),
    )
    .unwrap();
    let v = probe(&http, &e, Some("k1")).await;
    assert_eq!(v["ok"], json!(true), "{v}");
    assert_eq!(v["models"], json!(["m1", "Bearer k1"]));
    // nothing listening
    let dead = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead_addr = dead.local_addr().unwrap();
    drop(dead);
    let e = check(
        "y",
        &ep("llm", &format!("http://{dead_addr}/v1")),
        &BTreeSet::new(),
    )
    .unwrap();
    let v = probe(&http, &e, None).await;
    assert_eq!(v["ok"], json!(false), "{v}");
    assert!(v["error"].as_str().is_some_and(|s| !s.is_empty()));
}
