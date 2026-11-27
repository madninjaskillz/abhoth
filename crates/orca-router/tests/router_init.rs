//! `abhoth router init` end to end: the files it writes start a real router (the admin-only
//! plane, since `[control] enabled = false`), `/admin/` answers, and its key signs in.

use orca_router::config::Snapshot;
use orca_router::server::{self, Mode};
use orca_router::state::AppState;
use serde_json::{Value, json};

#[tokio::test(flavor = "multi_thread")]
async fn a_router_from_init_starts_and_its_key_signs_in() {
    let t = tempfile::tempdir().unwrap();
    let dir = t.path().join("router");
    let code = orca_router::cli::run(
        [
            "init",
            &dir.display().to_string(),
            "--yes",
            "--no-local",
            "--listen",
            "127.0.0.1:0",
        ]
        .map(String::from)
        .to_vec(),
    )
    .await
    .unwrap();
    assert_eq!(code, 0);
    let toml = dir.join("sothoth.toml");
    let json_path = dir.join("sothoth.json");
    let key = std::fs::read_to_string(dir.join("secrets/router-admin-key"))
        .unwrap()
        .trim()
        .to_string();

    // The router's own checks take both files.
    let check = orca_router::cli::run(
        ["config-check", "--config", &toml.display().to_string()]
            .map(String::from)
            .to_vec(),
    )
    .await
    .unwrap();
    assert_eq!(check, 0);

    let st = AppState::new(Snapshot::load(&json_path).unwrap(), Some(json_path.clone()));
    let started = orca_router::control::startup::start(&st, &toml)
        .await
        .unwrap();
    assert!(
        started.message.contains("admin pages on"),
        "{}",
        started.message
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(server::serve(l, st, Mode::Gateway, std::future::pending()));

    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    // As if from another machine (through a proxy), so loopback does not make it an admin.
    let login = |k: &str| {
        http.post(format!("{url}/admin/login"))
            .header("x-forwarded-for", "10.0.0.5")
            .json(&json!({"key": k}))
            .send()
    };
    // A well-formed key the router never made.
    let wrong = format!(
        "{}{}",
        orca_core::router_link::API_KEY_PREFIX,
        "0".repeat(48)
    );
    let bad = login(&wrong).await.unwrap();
    assert_eq!(bad.status(), 401);
    let ok = login(&key).await.unwrap();
    assert_eq!(ok.status(), 200);
    let v: Value = ok.json().await.unwrap();
    assert_eq!(v["csrfHeader"], "x-router-csrf");

    // Nothing was migrated or rewritten at start-up: the file is as init wrote it.
    let text = std::fs::read_to_string(&toml).unwrap();
    assert!(text.contains("[inventory]\nversion = 1"));
    assert!(
        !dir.join("state/backups").exists() || {
            std::fs::read_dir(dir.join("state/backups"))
                .unwrap()
                .all(|e| {
                    !e.unwrap()
                        .file_name()
                        .to_string_lossy()
                        .contains("pre-inventory")
                })
        }
    );
}
