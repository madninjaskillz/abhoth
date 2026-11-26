//! Mocked Hugging Face answers only: no live calls.
use super::*;

pub(crate) fn gguf_repo() -> Value {
    json!({
        "id": "example/Tiny-Chat-GGUF",
        "sha": "abc123",
        "pipeline_tag": "text-generation",
        "tags": ["gguf", "license:apache-2.0", "conversational"],
        "gated": false,
        "downloads": 1234,
        "gguf": {"total": 1, "architecture": "qwen3", "context_length": 40960},
        "siblings": [
            {"rfilename": "README.md", "size": 2000},
            {"rfilename": "Tiny-Chat-Q4_K_M.gguf", "size": 5_000_000_000u64, "lfs": {"sha256": "aa", "size": 5_000_000_000u64}},
            {"rfilename": "Tiny-Chat-Q8_0-00001-of-00002.gguf", "size": 4_000_000_000u64},
            {"rfilename": "Tiny-Chat-Q8_0-00002-of-00002.gguf", "size": 4_000_000_000u64},
            {"rfilename": "mmproj-F16.gguf", "size": 800_000_000u64},
            {"rfilename": "../evil.gguf", "size": 1}
        ]
    })
}

pub(crate) fn st_repo() -> Value {
    json!({
        "id": "example/voice-1",
        "pipeline_tag": "text-to-speech",
        "tags": ["safetensors"],
        "cardData": {"license": "other"},
        "gated": "manual",
        "siblings": [
            {"rfilename": "config.json", "size": 1000},
            {"rfilename": "tokenizer.json", "size": 2000},
            {"rfilename": "model-00001-of-00002.safetensors", "size": 3_000_000_000u64},
            {"rfilename": "model-00002-of-00002.safetensors", "size": 1_000_000_000u64},
            {"rfilename": "samples/a.wav", "size": 1000}
        ]
    })
}

#[test]
fn gguf_quants_are_choices_with_shards_and_mmproj() {
    let d = details("example/Tiny-Chat-GGUF", &gguf_repo(), None);
    assert_eq!(d["kind"], "llm");
    assert_eq!(d["arch"], "qwen3");
    assert_eq!(d["context"], 40960);
    assert_eq!(d["vision"], true);
    assert_eq!(d["licence"], "apache-2.0");
    assert_eq!(d["gated"], false);
    let ch = d["choices"].as_array().unwrap();
    assert_eq!(ch.len(), 2);
    let q4 = ch.iter().find(|c| c["quant"] == "Q4_K_M").unwrap();
    assert_eq!(
        q4["files"].as_array().unwrap().len(),
        2,
        "the quant plus the mmproj"
    );
    assert_eq!(q4["files"][0]["sha256"], "aa");
    let q8 = ch.iter().find(|c| c["quant"] == "Q8_0").unwrap();
    assert_eq!(q8["files"].as_array().unwrap().len(), 3);
    assert_eq!(q8["sizeGib"], 8.2);
    // a path that climbs out is never offered
    assert!(!d.to_string().contains("evil"));
}

#[test]
fn safetensors_sets_types_gated_and_licence() {
    let cfg = json!({"model_type": "maya", "max_position_embeddings": 8192});
    let d = details("example/voice-1", &st_repo(), Some(&cfg));
    assert_eq!(d["kind"], "tts");
    assert_eq!(d["arch"], "maya");
    assert_eq!(d["context"], 8192);
    assert_eq!(d["gated"], true);
    assert_eq!(d["gatedHow"], "manual");
    assert_eq!(d["licence"], "other");
    let ch = d["choices"].as_array().unwrap();
    assert_eq!(ch.len(), 1);
    assert_eq!(ch[0]["format"], "safetensors");
    let files: Vec<&str> = ch[0]["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    assert!(files.contains(&"config.json") && files.contains(&"tokenizer.json"));
    assert!(!files.contains(&"samples/a.wav"));
}

#[test]
fn pipeline_tags_map_to_types() {
    assert_eq!(
        kind_of(Some("automatic-speech-recognition"), &[]),
        Some(ModelKind::Stt)
    );
    assert_eq!(kind_of(Some("text-to-image"), &[]), Some(ModelKind::Image));
    assert_eq!(
        kind_of(Some("text-to-audio"), &["music".into()]),
        Some(ModelKind::Music)
    );
    assert_eq!(kind_of(Some("text-to-audio"), &[]), Some(ModelKind::Tts));
    assert_eq!(
        kind_of(Some("sentence-similarity"), &[]),
        Some(ModelKind::Embedding)
    );
    assert_eq!(kind_of(None, &[]), None);
}

#[test]
fn repo_and_file_names_are_checked() {
    assert!(valid_repo("Qwen/Qwen3-8B-GGUF"));
    assert!(!valid_repo("../x"));
    assert!(!valid_repo("a/b/c"));
    assert!(!valid_repo("a b/c"));
    assert!(valid_file("sub/dir/model.gguf"));
    assert!(!valid_file("../x"));
    assert!(!valid_file("/etc/passwd"));
    let h = Hf::new(Some("http://127.0.0.1:1"), None);
    assert_eq!(
        h.file_url("o/m", "main", "a b/c.gguf"),
        "http://127.0.0.1:1/o/m/resolve/main/a%20b/c.gguf"
    );
}

#[test]
fn the_token_never_shows() {
    let h = Hf::new(None, Some("hf_secretsecretsecret".into()));
    assert!(h.has_token());
    assert!(!format!("{h:?}").contains("secret"));
}

/// A mock Hugging Face API on loopback.
pub(crate) async fn mock() -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    use axum::{Router, extract::Path, routing::get};
    let auth = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let a2 = auth.clone();
    let app = Router::new()
        .route(
            "/api/models",
            get(
                move |h: axum::http::HeaderMap, q: axum::extract::RawQuery| async move {
                    if let Some(v) = h.get("authorization") {
                        a2.lock()
                            .unwrap()
                            .push(v.to_str().unwrap_or("").to_string());
                    }
                    let q = q.0.unwrap_or_default();
                    if q.contains("pipeline_tag=text-to-speech") {
                        axum::Json(json!([st_repo()]))
                    } else {
                        axum::Json(json!([gguf_repo(), st_repo()]))
                    }
                },
            ),
        )
        .route(
            "/api/models/{owner}/{name}",
            get(|Path((o, n)): Path<(String, String)>| async move {
                match format!("{o}/{n}").as_str() {
                    "example/Tiny-Chat-GGUF" => Ok(axum::Json(gguf_repo())),
                    "example/voice-1" => Ok(axum::Json(st_repo())),
                    "example/locked" => Err(axum::http::StatusCode::FORBIDDEN),
                    _ => Err(axum::http::StatusCode::NOT_FOUND),
                }
            }),
        )
        .route(
            "/{owner}/{name}/resolve/{rev}/{*file}",
            get(
                |Path((_o, n, _r, f)): Path<(String, String, String, String)>,
                 h: axum::http::HeaderMap| async move {
                    if f == "config.json" {
                        return (
                            axum::http::StatusCode::OK,
                            axum::http::HeaderMap::new(),
                            br#"{"model_type":"maya"}"#.to_vec(),
                        );
                    }
                    let body: Vec<u8> = (0..100u8).collect();
                    let _ = n;
                    match h
                        .get("range")
                        .and_then(|r| r.to_str().ok())
                        .and_then(|r| r.strip_prefix("bytes="))
                        .and_then(|r| r.trim_end_matches('-').parse::<usize>().ok())
                    {
                        Some(from) => (
                            axum::http::StatusCode::PARTIAL_CONTENT,
                            axum::http::HeaderMap::new(),
                            body[from.min(100)..].to_vec(),
                        ),
                        None => (
                            axum::http::StatusCode::OK,
                            axum::http::HeaderMap::new(),
                            body,
                        ),
                    }
                },
            ),
        );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.ok() });
    (format!("http://{addr}"), auth)
}

#[tokio::test]
async fn search_and_details_against_a_mock() {
    let (url, auth) = mock().await;
    let h = Hf::new(Some(&url), Some("tok".into()));
    let s = h.search("tiny", None).await.unwrap();
    assert_eq!(s.as_array().unwrap().len(), 2);
    assert_eq!(s[0]["repo"], "example/Tiny-Chat-GGUF");
    assert_eq!(s[0]["gguf"], true);
    assert_eq!(s[1]["gated"], true);
    let s = h.search("voice", Some(ModelKind::Tts)).await.unwrap();
    assert_eq!(s.as_array().unwrap().len(), 1);
    assert_eq!(auth.lock().unwrap()[0], "Bearer tok");
    let d = h.model("example/voice-1").await.unwrap();
    assert_eq!(d["arch"], "maya", "from config.json");
    let e = h.model("example/locked").await.unwrap_err();
    assert!(e.contains("gated"), "{e}");
    assert!(h.model("bad repo").await.is_err());
}

#[tokio::test]
async fn the_download_builtin_resumes_and_checks() {
    use crate::control::exec::Cancel;
    use crate::control::fetch::{DownloadArgs, download, status};
    let (url, _) = mock().await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir
        .path()
        .join("m/model.gguf")
        .to_string_lossy()
        .to_string();
    // a part from an earlier, interrupted run
    std::fs::create_dir_all(dir.path().join("m")).unwrap();
    std::fs::write(format!("{dest}.part"), (0..40u8).collect::<Vec<_>>()).unwrap();
    assert_eq!(status(&dest)["bytes"], 40);
    let a = DownloadArgs {
        url: format!("{url}/example/Tiny-Chat-GGUF/resolve/main/model.gguf"),
        dest: dest.clone(),
        size: Some(100),
        sha256: None,
    };
    let o = download(
        &a,
        Some(b"tok"),
        std::time::Duration::from_secs(10),
        &Cancel::new(),
    )
    .await;
    assert!(o.ok(), "{o:?}");
    assert_eq!(
        std::fs::read(&dest).unwrap(),
        (0..100u8).collect::<Vec<_>>()
    );
    assert_eq!(status(&dest)["done"], true);
    // done already: nothing fetched
    let o = download(&a, None, std::time::Duration::from_secs(10), &Cancel::new()).await;
    assert!(o.ok());
    // a wrong checksum removes the part
    let dest2 = dir.path().join("x.gguf").to_string_lossy().to_string();
    let b = DownloadArgs {
        dest: dest2.clone(),
        sha256: Some("00".into()),
        ..a
    };
    let o = download(&b, None, std::time::Duration::from_secs(10), &Cancel::new()).await;
    assert_eq!(o.exit_code, Some(6));
    assert!(!std::path::Path::new(&format!("{dest2}.part")).exists());
    // cancelled before it starts: the part (none) is kept, not an error
    let c = Cancel::new();
    c.cancel();
    let d = DownloadArgs {
        url: format!("{url}/example/x/resolve/main/y.gguf"),
        dest: dir.path().join("y.gguf").to_string_lossy().to_string(),
        size: None,
        sha256: None,
    };
    let o = download(&d, None, std::time::Duration::from_secs(10), &c).await;
    assert!(o.cancelled);
}

#[test]
fn hostctl_allows_only_hugging_face_downloads() {
    use crate::control::exec::ExecRequest;
    use crate::control::hostctl::Allowlist;
    let al = Allowlist {
        version: 1,
        max_timeout_seconds: 3600,
        paths: crate::control::hostctl::PathRules {
            read: vec![],
            write: vec!["/srv/models".into()],
        },
        ..Default::default()
    };
    let req = |url: &str, dest: &str| {
        ExecRequest::builtin(
            "gpu",
            "download",
            json!({"url": url, "dest": dest}),
            std::time::Duration::from_secs(60),
        )
    };
    assert!(
        al.check(&req(
            "https://huggingface.co/o/m/resolve/main/a.gguf",
            "/srv/models/o/a.gguf"
        ))
        .is_ok()
    );
    assert!(
        al.check(&req("https://evil.example/a.gguf", "/srv/models/a.gguf"))
            .is_err()
    );
    assert!(
        al.check(&req(
            "https://huggingface.co/o/m/resolve/main/a.gguf",
            "/etc/a.gguf"
        ))
        .is_err()
    );
    assert!(
        al.check(&req(
            "https://huggingface.co/o/m/resolve/main/a.gguf",
            "/srv/models/../etc/a"
        ))
        .is_err()
    );
}
