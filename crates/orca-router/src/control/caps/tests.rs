use super::*;
use crate::control::schema::{CapsDelta, EngineCaps};
use crate::control::validate::tests::SAMPLE;

fn cfg(extra: &str) -> ControlConfig {
    toml::from_str(&format!("{SAMPLE}\n{extra}")).expect("parses")
}

const GGUF_MODELS: &str = r#"
[engines.llamacpp]
title = "llama.cpp"
driver = "docker"
image = "local/llama:b1"
port = { A = 8081, B = 8097 }
[engines.llamacpp.profiles.x]
lanes = 2
vram_gib = 2.0

[models.qwen-gguf]
title = "Qwen GGUF"
arch = "qwen3"
[models.qwen-gguf.artifacts.llamacpp]
file = "Qwen3-8B-Q4_K_M.gguf"
vram_gib = 6.0

[models.exl]
title = "An EXL2 quant"
arch = "llama"
[models.exl.artifacts.exllama]
file = "llama-8b-exl2-4.0bpw"
vram_gib = 6.0
"#;

#[test]
fn file_names_say_format_and_quant() {
    assert_eq!(
        format_of_name("a/b/Qwen3-8B-Q4_K_M.gguf").as_deref(),
        Some("gguf")
    );
    assert_eq!(
        format_of_name("model-00001-of-00004.safetensors").as_deref(),
        Some("safetensors")
    );
    assert_eq!(
        format_of_name("qwen3_8_27b_nvfp4.ninfer").as_deref(),
        Some("ninfer")
    );
    assert_eq!(
        format_of_name("llama-8b-exl2-4.0bpw").as_deref(),
        Some("exl2")
    );
    assert_eq!(format_of_name("ggml-large-v3.bin").as_deref(), Some("ggml"));
    assert_eq!(format_of_name(""), None);
    assert_eq!(
        quant_of_name("Qwen3-8B-Q4_K_M.gguf").as_deref(),
        Some("Q4_K_M")
    );
    assert_eq!(
        quant_of_name("Inkling-2-Mini-UD-Q4_K_XL.gguf").as_deref(),
        Some("Q4_K_XL")
    );
    assert_eq!(
        quant_of_name("QwenPi-IQ4_XS.gguf").as_deref(),
        Some("IQ4_XS")
    );
    assert_eq!(
        quant_of_name("qwen3_8_27b_nvfp4.ninfer").as_deref(),
        Some("nvfp4")
    );
    assert_eq!(quant_of_name("model.F16.gguf").as_deref(), Some("F16"));
    assert_eq!(norm("Qwen3_MoE"), "qwen3moe");
}

#[test]
fn matching_with_reasons() {
    let c = cfg(GGUF_MODELS);
    let m = &c.models["qwen-gguf"];
    let s = supported(&c, m);
    let l = s.iter().find(|x| x.engine == "llamacpp").unwrap();
    assert!(l.ok);
    assert_eq!(l.reason, "gguf + qwen3 ✓");
    assert_eq!(l.via, "capability");
    // ninfer runs .ninfer files, not GGUF; strata loads its own build
    let n = s.iter().find(|x| x.engine == "ninfer").unwrap();
    assert!(!n.ok && n.reason == "no gguf support", "{n:?}");
    let st = s.iter().find(|x| x.engine == "strata").unwrap();
    assert!(!st.ok);
    // an EXL2 model: no engine set up runs it
    let e = supported(&c, &c.models["exl"]);
    assert!(e.iter().all(|x| !x.ok));
    assert!(
        e.iter()
            .any(|x| x.engine == "llamacpp" && x.reason == "no exl2 support")
    );
    // an architecture an engine lists none of
    let caps_w = caps(&c, "llamacpp");
    let d = FileDesc {
        format: Some("gguf".into()),
        arch: Some("whisper".into()),
        quant: None,
    };
    assert!(
        matches(&caps_w, ModelKind::Llm, &d).is_ok(),
        "llama.cpp: any architecture"
    );
    let wc = from_catalog("whisper");
    let d = FileDesc {
        format: Some("gguf".into()),
        arch: Some("llama".into()),
        quant: None,
    };
    assert_eq!(
        matches(&wc, ModelKind::Stt, &d).unwrap_err(),
        "no llama support"
    );
    assert_eq!(
        matches(&wc, ModelKind::Llm, &d).unwrap_err(),
        "does not run language models"
    );
}

#[test]
fn a_fork_runs_every_matching_model_with_no_model_edits() {
    let before = cfg(GGUF_MODELS);
    assert_eq!(
        engine_order(&before, &before.models["qwen-gguf"]),
        vec!["llamacpp"]
    );
    let c = cfg(&format!(
        "{GGUF_MODELS}\n[engines.llamacpp-fork]\ntitle = \"llama.cpp fork\"\ndriver = \"docker\"\nimage = \"local/fork:1\"\nport = {{ A = 8181 }}\ncaps = {{ inherits = \"llamacpp\" }}\n[engines.llamacpp-fork.profiles.x]\nlanes = 1\nvram_gib = 2.0\n"
    ));
    let m = &c.models["qwen-gguf"];
    assert_eq!(engine_order(&c, m), vec!["llamacpp", "llamacpp-fork"]);
    let f = supported(&c, m)
        .into_iter()
        .find(|x| x.engine == "llamacpp-fork")
        .unwrap();
    assert!(f.ok);
    assert_eq!(f.file_of.as_deref(), Some("llamacpp"));
    // it loads the llama.cpp file, and the router can place it on its card
    assert_eq!(
        artifact_for(&c, m, "llamacpp-fork").unwrap().file,
        "Qwen3-8B-Q4_K_M.gguf"
    );
    let p = crate::control::inventory::place(&c, "qwen-gguf", &Default::default());
    assert!(
        p.iter()
            .any(|o| o.card == "A" && o.ok && o.engine == "llamacpp")
    );
    // making it primary moves it first, with no other edit
    let mut c2 = c.clone();
    c2.models["qwen-gguf"].primary = Some("llamacpp-fork".into());
    let p = crate::control::inventory::place(&c2, "qwen-gguf", &Default::default());
    let a = p.iter().find(|o| o.card == "A").unwrap();
    assert_eq!((a.engine.as_str(), a.rank), ("llamacpp-fork", 0));
    // B has no fork port: llama.cpp there, as the fallback
    let b = p.iter().find(|o| o.card == "B").unwrap();
    assert_eq!((b.engine.as_str(), b.rank), ("llamacpp", 1));
    // and the switch can resolve it
    let spec = crate::control::schema::CardSpec {
        engine: "llamacpp-fork".into(),
        profile: "x".into(),
        model: "qwen-gguf".into(),
    };
    assert!(
        crate::control::resolve::resolve(&c2, "A", &spec).is_ok()
            || crate::control::resolve::resolve(&c2, "A", &spec)
                .unwrap_err()
                .contains("run")
    );
}

#[test]
fn inheritance_overrides_and_deltas() {
    let mut c = cfg(GGUF_MODELS);
    c.engines.insert(
        "fork".into(),
        Engine {
            caps: Some(EngineCaps {
                inherits: Some("llamacpp".into()),
                add: CapsDelta {
                    formats: vec!["exl2".into()],
                    kinds: vec![ModelKind::Tts],
                    ..Default::default()
                },
                remove: CapsDelta {
                    kinds: vec![ModelKind::Embedding],
                    ..Default::default()
                },
                ..Default::default()
            }),
            ..Default::default()
        },
    );
    let f = caps(&c, "fork");
    assert_eq!(f.formats, vec!["gguf", "exl2"]);
    assert_eq!(f.kinds, vec![ModelKind::Llm, ModelKind::Tts]);
    assert_eq!(f.base.as_deref(), Some("llamacpp"));
    assert!(f.sources.contains(&"engine:llamacpp".to_string()));
    assert!(f.sources.contains(&"override".to_string()));
    // the exl2 model now has an engine (its file named for exllama, run by the fork)
    assert_eq!(engine_order(&c, &c.models["exl"]), vec!["fork"]);
    // a grandchild inherits the fork; a replaced list wins over the inherited one
    c.engines.insert(
        "grand".into(),
        Engine {
            caps: Some(EngineCaps {
                inherits: Some("fork".into()),
                formats: Some(vec!["gguf".into()]),
                ..Default::default()
            }),
            ..Default::default()
        },
    );
    assert_eq!(caps(&c, "grand").formats, vec!["gguf"]);
    assert_eq!(
        caps(&c, "grand").kinds,
        vec![ModelKind::Llm, ModelKind::Tts]
    );
    // a cycle stops, it does not loop
    c.engines["fork"].caps.as_mut().unwrap().inherits = Some("grand".into());
    let _ = caps(&c, "grand");
    // an unknown engine kind with no caps runs nothing by capability
    c.engines.insert("mystery".into(), Engine::default());
    assert!(caps(&c, "mystery").kinds.is_empty());
}

#[test]
fn allow_and_deny_are_the_exceptions() {
    let mut c = cfg(GGUF_MODELS);
    let m = c.models.get_mut("qwen-gguf").unwrap();
    m.deny = vec!["llamacpp".into()];
    m.allow = vec!["strata".into()];
    let s = supported(&c, &c.models["qwen-gguf"]);
    let l = s.iter().find(|x| x.engine == "llamacpp").unwrap();
    assert!(!l.ok && l.via == "deny");
    let st = s.iter().find(|x| x.engine == "strata").unwrap();
    assert!(st.ok && st.via == "allow");
    assert_eq!(engine_order(&c, &c.models["qwen-gguf"]), vec!["strata"]);
}

#[test]
fn preference_order_then_capability() {
    let c = cfg(&format!(
        "{GGUF_MODELS}\n[engines.fork-a]\nport = 1\ncaps = {{ inherits = \"llamacpp\" }}\n[engines.fork-b]\nport = 2\ncaps = {{ inherits = \"llamacpp\" }}\n"
    ));
    let mut m = c.models["qwen-gguf"].clone();
    assert_eq!(engine_order(&c, &m), vec!["llamacpp", "fork-a", "fork-b"]);
    m.prefer = vec!["fork-b".into()];
    assert_eq!(engine_order(&c, &m), vec!["fork-b", "llamacpp", "fork-a"]);
    m.primary = Some("fork-a".into());
    assert_eq!(engine_order(&c, &m), vec!["fork-a", "fork-b", "llamacpp"]);
    // a primary that can't run it is skipped, the rest follow
    m.primary = Some("ninfer".into());
    assert_eq!(engine_order(&c, &m), vec!["fork-b", "llamacpp", "fork-a"]);
}

#[test]
fn probes_are_cached_per_build_and_overrides_still_win() {
    let mut c = cfg(&format!(
        "{GGUF_MODELS}\n[engines.llamacpp.caps]\nremove = {{ kinds = [\"embedding\"] }}\n"
    ));
    let e = c.engines["llamacpp"].clone();
    // catalog archs are "any" for llama.cpp: use whisper's to see a probe add one
    c.engines.insert(
        "wcpp".into(),
        Engine {
            image: Some("local/whisper:1".into()),
            caps: Some(EngineCaps {
                inherits: Some("whisper".into()),
                ..Default::default()
            }),
            ..Default::default()
        },
    );
    let now = 1_000_000;
    assert!(needs_probe(&c, "llamacpp", now, 3600));
    let props = json!({"build_info": "b12620-abc", "default_generation_settings": {"n_ctx": 32768}, "model_meta": {"general.architecture": "qwen3"}});
    record_probe("llamacpp", parse_probe(stamp(&e), Some(&props), None, now));
    assert!(!needs_probe(&c, "llamacpp", now + 10, 3600));
    assert!(
        needs_probe(&c, "llamacpp", now + 7200, 3600),
        "old probes are asked again"
    );
    let cp = caps(&c, "llamacpp");
    assert_eq!(cp.version.as_deref(), Some("b12620-abc"));
    assert_eq!(cp.max_context, Some(32768));
    assert!(cp.sources.contains(&"probe".to_string()));
    assert!(
        !cp.kinds.contains(&ModelKind::Embedding),
        "the override still wins over the probe"
    );
    // a new image = a new build: the cached probe no longer counts
    c.engines["llamacpp"].image = Some("local/llama:b2".into());
    assert!(probe_of(&c, "llamacpp").is_none());
    assert!(needs_probe(&c, "llamacpp", now + 10, 3600));
    // a probe that saw an architecture adds it to a list that names them
    let w = c.engines["wcpp"].clone();
    let models = json!({"data": [{"id": "x", "meta": {"general.architecture": "whisper-turbo"}}]});
    record_probe("wcpp", parse_probe(stamp(&w), None, Some(&models), now));
    assert!(caps(&c, "wcpp").archs.contains(&"whisperturbo".to_string()));
    // probes off for an engine
    c.engines["wcpp"].caps.as_mut().unwrap().probe = Some(false);
    assert_eq!(probe_how(&c, "wcpp"), "none");
    assert!(!needs_probe(&c, "wcpp", now + 99999, 3600));
}

#[tokio::test]
async fn a_probe_reads_static_endpoints_only() {
    use axum::{Router, routing::get};
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<(String, bool)>::new()));
    let s2 = seen.clone();
    let s3 = seen.clone();
    let app = Router::new()
        .route(
            "/props",
            get(move |h: axum::http::HeaderMap| async move {
                s2.lock()
                    .unwrap()
                    .push(("/props".into(), h.contains_key("x-abhoth-no-switch")));
                axum::Json(json!({"build_info": "b1"}))
            }),
        )
        .route(
            "/v1/models",
            get(move |h: axum::http::HeaderMap| async move {
                s3.lock()
                    .unwrap()
                    .push(("/v1/models".into(), h.contains_key("x-abhoth-no-switch")));
                axum::Json(json!({"data": [{"id": "m", "meta": {"n_ctx_train": 4096}}]}))
            }),
        );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.ok() });
    let http = reqwest::Client::new();
    let p = probe_engine(
        &http,
        &format!("http://{addr}/v1"),
        Some("k"),
        "props",
        "s".into(),
    )
    .await
    .unwrap();
    assert_eq!(p.version.as_deref(), Some("b1"));
    assert_eq!(p.ctx, Some(4096));
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2);
    assert!(seen.iter().all(|(_, h)| *h));
}

fn gguf_bytes(kvs: &[(&str, u32, Vec<u8>)]) -> Vec<u8> {
    let mut b = b"GGUF".to_vec();
    b.extend(3u32.to_le_bytes());
    b.extend(0u64.to_le_bytes());
    b.extend((kvs.len() as u64).to_le_bytes());
    for (k, t, v) in kvs {
        b.extend((k.len() as u64).to_le_bytes());
        b.extend(k.as_bytes());
        b.extend(t.to_le_bytes());
        b.extend(v);
    }
    b
}

fn gstr(s: &str) -> Vec<u8> {
    let mut v = (s.len() as u64).to_le_bytes().to_vec();
    v.extend(s.as_bytes());
    v
}

#[test]
fn gguf_metadata_gives_the_architecture() {
    let mut arr = 8u32.to_le_bytes().to_vec(); // array of strings
    arr.extend(2u64.to_le_bytes());
    arr.extend(gstr("a"));
    arr.extend(gstr("b"));
    let b = gguf_bytes(&[
        ("tokenizer.ggml.tokens", 9, arr),
        ("general.architecture", 8, gstr("qwen3moe")),
        ("general.file_type", 4, 15u32.to_le_bytes().to_vec()),
        (
            "qwen3moe.context_length",
            4,
            262144u32.to_le_bytes().to_vec(),
        ),
    ]);
    let m = gguf_meta(&b).unwrap();
    assert_eq!(m["general.architecture"], "qwen3moe");
    assert_eq!(m["qwen3moe.context_length"], "262144");
    assert!(gguf_meta(b"not gguf").is_none());
    // cut short: no answer rather than a panic
    assert!(gguf_meta(&b[..30]).is_none());
}
