//! The HTTP side (ports of WebUiServerTests / ProjectTests): token auth, the client build with
//! its SPA fallback, git, files and the folder wizard.

mod common;

use std::path::Path;
use std::process::Command;

use common::{FakeSession, Host, TOKEN};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn brand_images_are_public_nothing_else_is() {
    let work = tempfile::tempdir().unwrap();
    let host = Host::start(FakeSession::new(work.path())).await;
    let c = host.http();
    let mascot = c
        .get(host.url("/brand/mascot-256.webp"))
        .send()
        .await
        .unwrap();
    assert_eq!(mascot.status(), StatusCode::OK);
    assert_eq!(mascot.headers()["content-type"], "image/webp");
    let page = c.get(host.url("/")).send().await.unwrap();
    assert_eq!(page.status(), StatusCode::UNAUTHORIZED);
    assert!(
        page.text()
            .await
            .unwrap()
            .contains("/brand/mascot-256.webp")
    );
    assert_eq!(
        c.post(host.url("/brand/x")).send().await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        c.get(host.url("/branding")).send().await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        c.get(host.url("/api/info")).send().await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        c.get(host.url("/?token=wrong-token-xyz"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    // The hub needs the token too.
    let ws =
        tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{}/hub/ws", host.server.port()))
            .await;
    assert!(ws.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_token_in_the_query_sets_the_cookie_and_redirects_without_it() {
    let work = tempfile::tempdir().unwrap();
    let host = Host::start(FakeSession::new(work.path())).await;
    let c = host.http();
    let first = c
        .get(host.url(&format!("/?token={TOKEN}")))
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::FOUND);
    assert_eq!(first.headers()["location"], "/");
    let cookie = first.headers()["set-cookie"].to_str().unwrap().to_string();
    let name = format!("openorca_token_{}", host.server.port());
    assert!(cookie.starts_with(&format!("{name}={TOKEN}")), "{cookie}");
    assert!(cookie.to_lowercase().contains("httponly"));

    // The cookie alone is enough afterwards: the page, a client route, the API.
    let jar = format!("{name}={TOKEN}");
    for path in ["/", "/projects/abc"] {
        let page = c
            .get(host.url(path))
            .header("Cookie", &jar)
            .send()
            .await
            .unwrap();
        assert_eq!(page.status(), StatusCode::OK, "{path}");
        assert!(page.text().await.unwrap().contains("<div id=\"root\">"));
    }
    let info: Value = c
        .get(host.url("/api/info"))
        .header("Cookie", &jar)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(info["hub"], "ws");
    assert_eq!(info["mode"], "headless");
    assert_eq!(info["auth"], "token");
    // Unknown API paths are 404, not the page.
    let missing = c
        .get(host.url("/api/nothing"))
        .header("Cookie", &jar)
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_header_token_works_and_cross_origin_posts_are_refused() {
    let work = tempfile::tempdir().unwrap();
    let host = Host::start(FakeSession::new(work.path())).await;
    let pid = host.primary();
    let r = host
        .api(Method::POST, &format!("/api/projects/{pid}/git/commit"))
        .header("Origin", "http://evil.example")
        .json(&json!({"message": "x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    let same = host
        .api(Method::POST, &format!("/api/projects/{pid}/git/commit"))
        .header("Origin", host.url(""))
        .json(&json!({"message": " "}))
        .send()
        .await
        .unwrap();
    assert_ne!(same.status(), StatusCode::FORBIDDEN);
}

#[tokio::test(flavor = "multi_thread")]
async fn git_status_diff_stage_unstage_commit() {
    let work = tempfile::tempdir().unwrap();
    let dir = work.path();
    git(dir, &["init", "-q", "-b", "main"]);
    std::fs::write(dir.join("a.txt"), "one\n").unwrap();
    git(dir, &["add", "-A"]);
    git(
        dir,
        &[
            "-c",
            "user.email=t@example.com",
            "-c",
            "user.name=Test",
            "commit",
            "-qm",
            "init",
        ],
    );
    std::fs::write(dir.join("a.txt"), "one\ntwo\n").unwrap();
    std::fs::create_dir(dir.join("sub")).unwrap();
    std::fs::write(dir.join("sub").join("new file.md"), "new\n").unwrap();

    let host = Host::start(FakeSession::new(dir)).await;
    let pid = host.primary();
    let get = |path: String| {
        let req = host.api(Method::GET, &path);
        async move { req.send().await.unwrap() }
    };
    let files_of = |status: &Value| -> std::collections::HashMap<String, Value> {
        status["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| (f["path"].as_str().unwrap().to_string(), f.clone()))
            .collect()
    };

    let status: Value = get(format!("/api/projects/{pid}/git/status"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(status["isRepo"], true);
    assert_eq!(status["branch"], "main");
    assert_eq!(status["merging"], false);
    let files = files_of(&status);
    assert_eq!(files["a.txt"]["unstaged"], true);
    assert_eq!(files["sub/new file.md"]["untracked"], true);

    let diff: Value = get(format!(
        "/api/projects/{pid}/git/diff?path=a.txt&staged=false"
    ))
    .await
    .json()
    .await
    .unwrap();
    assert_eq!(
        diff["original"].as_str().unwrap().replace("\r\n", "\n"),
        "one\n"
    );
    assert_eq!(
        diff["modified"].as_str().unwrap().replace("\r\n", "\n"),
        "one\ntwo\n"
    );
    assert_eq!(
        get(format!("/api/projects/{pid}/git/diff?path=../outside.txt"))
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );

    let post = |path: String, body: Value| {
        let req = host.api(Method::POST, &path).json(&body);
        async move { req.send().await.unwrap() }
    };
    let staged = post(
        format!("/api/projects/{pid}/git/stage"),
        json!({"paths": ["a.txt", "sub/new file.md"]}),
    )
    .await;
    assert!(
        staged.status().is_success(),
        "{}",
        staged.text().await.unwrap()
    );
    let unstaged = post(
        format!("/api/projects/{pid}/git/unstage"),
        json!({"paths": ["sub/new file.md"]}),
    )
    .await;
    assert!(unstaged.status().is_success());

    let status: Value = get(format!("/api/projects/{pid}/git/status"))
        .await
        .json()
        .await
        .unwrap();
    let files = files_of(&status);
    assert_eq!(files["a.txt"]["staged"], true);
    assert_eq!(files["sub/new file.md"]["untracked"], true);

    assert_eq!(
        post(
            format!("/api/projects/{pid}/git/commit"),
            json!({"message": " "})
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    git(dir, &["config", "user.email", "t@example.com"]);
    git(dir, &["config", "user.name", "Test"]);
    let commit = post(
        format!("/api/projects/{pid}/git/commit"),
        json!({"message": "Second commit\n\nwith \"quotes\" and $dollars"}),
    )
    .await;
    assert!(
        commit.status().is_success(),
        "{}",
        commit.text().await.unwrap()
    );
    assert!(git(dir, &["log", "-1", "--format=%B"]).contains("with \"quotes\" and $dollars"));

    // The branch menu: a new branch, listed as current.
    let created = post(
        format!("/api/projects/{pid}/git/branch"),
        json!({"name": "feature/x"}),
    )
    .await;
    assert!(
        created.status().is_success(),
        "{}",
        created.text().await.unwrap()
    );
    let branches: Value = get(format!("/api/projects/{pid}/git/branches"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(branches["current"], "feature/x");
    let bad = post(
        format!("/api/projects/{pid}/git/branch"),
        json!({"name": "-x"}),
    )
    .await;
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test(flavor = "multi_thread")]
async fn files_tree_read_write_and_traversal() {
    let work = tempfile::tempdir().unwrap();
    let dir = work.path();
    std::fs::create_dir(dir.join("src")).unwrap();
    std::fs::write(dir.join("src").join("main.rs"), "fn main() {}\n").unwrap();
    let host = Host::start(FakeSession::new(dir)).await;
    let pid = host.primary();

    let tree: Value = host
        .api(Method::GET, &format!("/api/projects/{pid}/tree"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(tree["entries"][0]["name"], "src");
    assert_eq!(tree["entries"][0]["dir"], true);

    let file: Value = host
        .api(
            Method::GET,
            &format!("/api/projects/{pid}/file?path=src/main.rs"),
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(file["content"], "fn main() {}\n");
    let etag = file["etag"].as_str().unwrap().to_string();

    let saved = host
        .api(Method::PUT, &format!("/api/projects/{pid}/file"))
        .json(&json!({"path": "src/main.rs", "content": "fn main() { }\n", "etag": etag}))
        .send()
        .await
        .unwrap();
    assert_eq!(saved.status(), StatusCode::OK);
    let stale = host
        .api(Method::PUT, &format!("/api/projects/{pid}/file"))
        .json(&json!({"path": "src/main.rs", "content": "x", "etag": etag}))
        .send()
        .await
        .unwrap();
    assert_eq!(stale.status(), StatusCode::CONFLICT);

    for path in ["../x.txt", "src/../../x"] {
        let r = host
            .api(
                Method::GET,
                &format!("/api/projects/{pid}/file?path={path}"),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN, "{path}");
    }
    let wt = host
        .api(
            Method::GET,
            &format!("/api/projects/{pid}/tree?wt=/elsewhere"),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(wt.status(), StatusCode::NOT_FOUND);
    let none = host
        .api(Method::GET, "/api/projects/nope/tree")
        .send()
        .await
        .unwrap();
    assert_eq!(none.status(), StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread")]
async fn projects_add_list_remove_and_the_folder_wizard() {
    let work = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let host = Host::start_with(FakeSession::new(work.path()), |o| {
        o.session_factory = Some(std::sync::Arc::new(|folder: &str| {
            FakeSession::new(Path::new(folder)) as std::sync::Arc<dyn orca_webui::LiveSession>
        }));
    })
    .await;
    // Without a session factory the server shows a single project.
    let single = Host::start(FakeSession::new(work.path())).await;
    let refused = single
        .api(Method::POST, "/api/projects")
        .json(&json!({"path": other.path().to_string_lossy()}))
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    let added: Value = host
        .api(Method::POST, "/api/projects")
        .json(&json!({"path": other.path().to_string_lossy(), "name": "Other"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = added["id"].as_str().unwrap().to_string();
    assert_eq!(added["name"], "Other");
    let list: Value = host
        .api(Method::GET, "/api/projects")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list["primary"], host.primary());
    assert!(
        list["projects"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["id"] == id.as_str())
    );
    // Saved (camelCase, version 1) next to the test's state.
    let saved: Value = serde_json::from_str(
        &std::fs::read_to_string(host.state_dir.path().join("projects.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(saved["version"], 1);

    let mkdir = host
        .api(Method::POST, "/api/fs/mkdir")
        .json(&json!({"parent": other.path().to_string_lossy(), "name": "made"}))
        .send()
        .await
        .unwrap();
    assert_eq!(mkdir.status(), StatusCode::OK);
    let again = host
        .api(Method::POST, "/api/fs/mkdir")
        .json(&json!({"parent": other.path().to_string_lossy(), "name": "made"}))
        .send()
        .await
        .unwrap();
    assert_eq!(again.status(), StatusCode::CONFLICT);
    let browse: Value = host
        .api(
            Method::GET,
            &format!(
                "/api/fs/browse?path={}",
                orca_webui::util::escape_data(&other.path().to_string_lossy())
            ),
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(browse["folders"][0]["name"], "made");

    let removed = host
        .api(Method::DELETE, &format!("/api/projects/{id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(removed.status(), StatusCode::OK);
    assert!(
        other.path().join("made").is_dir(),
        "closing keeps the folder"
    );
    let primary = host
        .api(Method::DELETE, &format!("/api/projects/{}", host.primary()))
        .send()
        .await
        .unwrap();
    assert_eq!(primary.status(), StatusCode::BAD_REQUEST);
}
