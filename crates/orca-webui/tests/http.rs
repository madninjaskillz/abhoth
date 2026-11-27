//! The HTTP side (ports of WebUiServerTests / ProjectTests): token auth, the client build with
//! its SPA fallback, git, files and the folder wizard.

mod common;

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

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
    // The brand images have content-hashed names (D150), public like every hashed asset: the
    // favicon the page links to is an Abhoth file, cached for good.
    let page = c.get(host.url("/")).send().await.unwrap();
    assert_eq!(page.status(), StatusCode::OK);
    let html = page.text().await.unwrap();
    // Pages without a session are the sign-in page (the app shows the form; D140).
    assert!(html.contains("<div id=\"root\">"));
    let icon = html
        .split("href=\"")
        .skip(1)
        .map(|s| s.split('"').next().unwrap())
        .find(|h| h.starts_with("/assets/abhoth-favicon-32-") && h.ends_with(".png"))
        .expect("a hashed Abhoth favicon");
    let image = c.get(host.url(icon)).send().await.unwrap();
    assert_eq!(image.status(), StatusCode::OK);
    assert_eq!(image.headers()["content-type"], "image/png");
    assert!(
        image.headers()["cache-control"]
            .to_str()
            .unwrap()
            .contains("immutable")
    );
    // branding.json stays at its path, public and never cached (a host can serve its own).
    let branding = c
        .get(host.url("/brand/branding.json"))
        .send()
        .await
        .unwrap();
    assert_eq!(branding.status(), StatusCode::OK);
    assert_eq!(branding.headers()["cache-control"], "no-cache");
    // The old paths (the OpenOrca build's orca was served there) are no image any more.
    let old = c
        .get(host.url("/brand/mascot-256.webp"))
        .send()
        .await
        .unwrap();
    assert_ne!(old.headers()["content-type"], "image/webp");
    assert_eq!(
        c.post(host.url("/brand/x")).send().await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    // Any other page path is the app (the sign-in form): no data behind it.
    let other = c.get(host.url("/branding")).send().await.unwrap();
    assert_eq!(other.status(), StatusCode::OK);
    assert!(other.text().await.unwrap().contains("<div id=\"root\">"));
    assert_eq!(
        c.get(host.url("/api/info")).send().await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    // A wrong launch token signs nobody in: just the sign-in page, no cookie.
    let wrong = c
        .get(host.url("/?token=wrong-token-xyz"))
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), StatusCode::OK);
    assert!(wrong.headers().get("set-cookie").is_none());
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
    // The launch token stands for the first admin (D140): a session cookie, not the token.
    let cookie = first.headers()["set-cookie"].to_str().unwrap().to_string();
    assert!(cookie.starts_with("abhoth_session="), "{cookie}");
    assert!(!cookie.contains(TOKEN), "{cookie}");
    assert!(cookie.to_lowercase().contains("httponly"));

    // The cookie alone is enough afterwards: the page, a client route, the API.
    let jar = cookie.split(';').next().unwrap().to_string();
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
    assert_eq!(info["auth"], "login");
    assert_eq!(info["user"], common::ADMIN);
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

#[tokio::test(flavor = "multi_thread")]
async fn the_terminal_echoes_a_command_and_refuses_cross_origin() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let work = tempfile::tempdir().unwrap();
    let host = Host::start_with(FakeSession::new(work.path()), |o| {
        o.shell = Some(if cfg!(windows) {
            "cmd.exe".into()
        } else {
            "/bin/sh".into()
        });
    })
    .await;
    let pid = host.primary();
    let url = format!(
        "ws://127.0.0.1:{}/api/projects/{pid}/pty?cols=100&rows=30",
        host.server.port()
    );
    let mut req = url.clone().into_client_request().unwrap();
    req.headers_mut()
        .insert("X-OpenOrca-Token", TOKEN.parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    // The typed line echoes the expression; only the command's output reads "pty-marker-42".
    let line = if cfg!(windows) {
        "echo pty-marker-%COMPUTERNAME:~0,0%42\r"
    } else {
        "echo pty-marker-$((40+2))\r"
    };
    ws.send(Message::Text(
        json!({"t": "i", "d": line}).to_string().into(),
    ))
    .await
    .unwrap();
    let mut output = String::new();
    let found = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        while let Some(Ok(msg)) = ws.next().await {
            if let Message::Binary(b) = msg {
                let text = String::from_utf8_lossy(&b).into_owned();
                // ConPTY asks where the cursor is before it starts (xterm.js answers in the
                // browser): answer as a terminal would.
                if text.contains("[6n") {
                    ws.send(Message::Binary(b"[1;1R".to_vec().into()))
                        .await
                        .unwrap();
                }
                output.push_str(&text);
                if output.contains("pty-marker-42") {
                    return true;
                }
            }
        }
        false
    })
    .await
    .unwrap_or(false);
    assert!(found, "{output}");
    assert_eq!(host.server.state().ptys.count(), 1);
    let _ = ws.close(None).await;

    let mut evil = url.into_client_request().unwrap();
    evil.headers_mut()
        .insert("X-OpenOrca-Token", TOKEN.parse().unwrap());
    evil.headers_mut()
        .insert("Origin", "http://evil.example".parse().unwrap());
    assert!(tokio_tungstenite::connect_async(evil).await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn previews_are_signed_scoped_and_sandboxed() {
    let work = tempfile::tempdir().unwrap();
    let site = work.path().join("site");
    std::fs::create_dir_all(site.join("css")).unwrap();
    std::fs::write(
        site.join("index.html"),
        "<!doctype html><html><head><title>t</title></head><body>hi</body></html>",
    )
    .unwrap();
    std::fs::write(site.join("css").join("a.css"), "body{}").unwrap();
    std::fs::write(site.join(".env"), "SECRET=1").unwrap();
    std::fs::write(work.path().join("outside.txt"), "no").unwrap();
    let host = Host::start(FakeSession::new(work.path())).await;
    let pid = host.primary();

    let bad = host
        .api(Method::POST, &format!("/api/projects/{pid}/preview"))
        .json(&json!({"path": "outside.txt"}))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
    let minted: Value = host
        .api(Method::POST, &format!("/api/projects/{pid}/preview"))
        .json(&json!({"path": "site/index.html"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let url = minted["url"].as_str().unwrap();
    let base = minted["base"].as_str().unwrap();
    assert!(url.starts_with("/preview/") && url.ends_with("/index.html"));

    // No token needed: the signed path is the grant.
    let c = host.http();
    let page = c.get(host.url(url)).send().await.unwrap();
    assert_eq!(page.status(), StatusCode::OK);
    assert!(
        page.headers()["content-security-policy"]
            .to_str()
            .unwrap()
            .starts_with("sandbox")
    );
    let body = page.text().await.unwrap();
    assert!(body.starts_with("<!doctype html><html><head><script>/* openorca preview */"));
    let css = c
        .get(host.url(&format!("{base}css/a.css")))
        .send()
        .await
        .unwrap();
    assert_eq!(css.headers()["content-type"], "text/css; charset=utf-8");
    for (path, status) in [
        (format!("{base}.env"), StatusCode::NOT_FOUND),
        // ".." is a dot segment: not found, as in C#.
        (format!("{base}..%2Foutside.txt"), StatusCode::NOT_FOUND),
        (
            "/preview/forged.token/index.html".to_string(),
            StatusCode::FORBIDDEN,
        ),
    ] {
        assert_eq!(
            c.get(host.url(&path)).send().await.unwrap().status(),
            status,
            "{path}"
        );
    }
    let dir = c.get(host.url(&format!("{base}css"))).send().await.unwrap();
    assert_eq!(dir.status(), StatusCode::FOUND);
}

#[tokio::test(flavor = "multi_thread")]
async fn attachments_upload_and_raw_files_with_ranges() {
    let work = tempfile::tempdir().unwrap();
    std::fs::write(work.path().join("clip.mp4"), b"0123456789").unwrap();
    let host = Host::start(FakeSession::new(work.path())).await;
    let pid = host.primary();
    let boundary = "XyZ";
    let body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"files\"; filename=\"notes.md\"\r\nContent-Type: text/markdown\r\n\r\n# hi\r\n--{boundary}--\r\n"
    );
    let saved: Value = host
        .api(Method::POST, &format!("/api/projects/{pid}/attachments"))
        .header(
            "Content-Type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let path = saved[0]["path"].as_str().unwrap();
    assert!(
        path.starts_with(".orca/attachments/web-") && path.ends_with("-notes.md"),
        "{path}"
    );
    assert_eq!(saved[0]["kind"], "file");
    assert!(work.path().join(".orca/attachments/.gitignore").exists());

    let exe = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"files\"; filename=\"x.exe\"\r\n\r\nMZ\r\n--{boundary}--\r\n"
    );
    let refused = host
        .api(Method::POST, &format!("/api/projects/{pid}/attachments"))
        .header(
            "Content-Type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(exe)
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);

    let part = host
        .api(
            Method::GET,
            &format!("/api/projects/{pid}/raw?path=clip.mp4"),
        )
        .header("Range", "bytes=2-4")
        .send()
        .await
        .unwrap();
    assert_eq!(part.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(part.headers()["content-range"], "bytes 2-4/10");
    assert_eq!(part.headers()["content-type"], "video/mp4");
    assert_eq!(part.bytes().await.unwrap().as_ref(), b"234");
    let outside = host
        .api(Method::GET, &format!("/api/projects/{pid}/raw?path=../x"))
        .send()
        .await
        .unwrap();
    assert_eq!(outside.status(), StatusCode::FORBIDDEN);
}

/// A stand-in for the inference server's profiles API (never a live server).
async fn fake_profiles_api() -> String {
    use axum::routing::{get, post};
    let app = axum::Router::new()
        .route(
            "/profiles",
            get(|| async {
                axum::Json(json!({
                    "version": 3,
                    "layouts": {"solo": {"title": "Solo"}},
                    "profiles": {
                        "coder": {"title": "Coder", "layout": "solo", "mode": "orchestrator"},
                    },
                    "aliases": {"old-coder": "coder"},
                }))
            }),
        )
        .route(
            "/compose",
            post(|axum::Json(b): axum::Json<Value>| async move {
                let n = b["cards"].as_object().map_or(0, |m| m.len());
                axum::Json(json!({"profile": format!("composed-{n}")}))
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://127.0.0.1:{port}")
}

#[tokio::test(flavor = "multi_thread")]
async fn profiles_catalog_compose_and_the_project_pickers() {
    let work = tempfile::tempdir().unwrap();
    let api = fake_profiles_api().await;
    let host = Host::start_with(FakeSession::new(work.path()), |o| {
        o.profiles_url = Some(api.clone());
    })
    .await;
    let pid = host.primary();
    let cat: Value = host
        .api(Method::GET, "/api/profiles")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(cat["available"], true, "{cat}");
    assert_eq!(cat["profiles"]["coder"]["title"], "Coder");
    assert_eq!(cat["profiles"]["coder"]["bridge"], false);
    assert!(cat["allModes"].as_array().unwrap().len() == 4);
    assert!(cat["projects"].as_object().unwrap().contains_key(&pid));

    let composed: Value = host
        .api(Method::POST, "/api/profiles/compose")
        .json(&json!({"cards": {"0": {}, "1": {}}}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(composed["profile"], "composed-2");
    let no_cards = host
        .api(Method::POST, "/api/profiles/compose")
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(no_cards.status(), StatusCode::BAD_REQUEST);

    // Unknown names are refused; a known one reaches the session (which, here, says no).
    let unknown = host
        .api(Method::POST, &format!("/api/projects/{pid}/profile"))
        .json(&json!({"profile": "nope"}))
        .send()
        .await
        .unwrap();
    assert_eq!(unknown.status(), StatusCode::BAD_REQUEST);
    let r = host
        .api(Method::POST, &format!("/api/projects/{pid}/profile"))
        .json(&json!({"profile": "old-coder"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::CONFLICT);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["profile"], "coder");
    let bad_mode = host
        .api(Method::POST, &format!("/api/projects/{pid}/mode"))
        .json(&json!({"mode": "wizard"}))
        .send()
        .await
        .unwrap();
    assert_eq!(bad_mode.status(), StatusCode::BAD_REQUEST);
    let err: Value = bad_mode.json().await.unwrap();
    assert!(
        err["error"]
            .as_str()
            .unwrap()
            .starts_with("Unknown mode wizard")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn profiles_are_not_mapped_without_an_api() {
    let work = tempfile::tempdir().unwrap();
    let host = Host::start(FakeSession::new(work.path())).await;
    let info: Value = host
        .api(Method::GET, "/api/info")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(info["profiles"], false);
    let r = host.api(Method::GET, "/api/profiles").send().await.unwrap();
    assert_ne!(r.status(), StatusCode::OK);
}

/// A settings page with one setting, `temperature`, kept in memory.
struct FakeSchema(std::sync::Mutex<f64>);

#[async_trait::async_trait]
impl orca_webui::settings::SettingsSchema for FakeSchema {
    async fn describe_file(&self, home: Option<&Path>) -> Result<Value, String> {
        Ok(json!({
            "configFile": home.map(|h| h.join("config.json").display().to_string()),
            "settings": [{"key": "temperature", "value": *self.0.lock().unwrap()}],
        }))
    }

    async fn apply_file(
        &self,
        _home: Option<&Path>,
        key: &str,
        value: Option<Value>,
        reset: bool,
    ) -> orca_webui::live::SettingChange {
        let Some(v) = value.and_then(|v| v.as_f64()).or(reset.then_some(0.7)) else {
            return orca_webui::live::SettingChange::failed(key, "Not a number.");
        };
        if key != "temperature" {
            return orca_webui::live::SettingChange::failed(key, "No such setting.");
        }
        let old = std::mem::replace(&mut *self.0.lock().unwrap(), v);
        orca_webui::live::SettingChange {
            ok: true,
            key: key.into(),
            error: None,
            old: Some(old.to_string()),
            new: Some(v.to_string()),
            live: false,
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn settings_page_reads_and_changes_the_config() {
    let work = tempfile::tempdir().unwrap();
    // Without a schema of its own the host uses the core ConfigSchema on its home's config.json.
    let home = tempfile::tempdir().unwrap();
    let builtin = Host::start_with(FakeSession::new(work.path()), |o| {
        o.config_home = Some(home.path().to_path_buf());
    })
    .await;
    let page: Value = builtin
        .api(Method::GET, "/api/settings")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(page["sections"].as_array().unwrap().len() > 3);
    assert!(
        page["configFile"]
            .as_str()
            .unwrap()
            .ends_with("config.json")
    );
    assert!(page["profile"].is_null());
    let ok: Value = builtin
        .api(Method::PUT, "/api/settings")
        .json(&json!({"key": "lmStudio.temperature", "value": 0.35}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(ok["ok"], true, "{ok}");
    assert_eq!(ok["new"], "0.35");
    let saved = std::fs::read_to_string(home.path().join("config.json")).unwrap();
    assert!(saved.contains("0.35"), "{saved}");
    let bad = builtin
        .api(Method::PUT, "/api/settings")
        .json(&json!({"key": "lmStudio.temperature", "value": "hot"}))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);

    let host = Host::start_with(FakeSession::new(work.path()), |o| {
        o.settings = Some(Arc::new(FakeSchema(std::sync::Mutex::new(0.2))));
        o.config_home = Some(work.path().to_path_buf());
    })
    .await;
    let page: Value = host
        .api(Method::GET, "/api/settings")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(page["settings"][0]["value"], 0.2);
    assert!(
        page["configFile"]
            .as_str()
            .unwrap()
            .ends_with("config.json")
    );
    let ok: Value = host
        .api(Method::PUT, "/api/settings")
        .json(&json!({"key": "temperature", "value": 0.5}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        ok,
        json!({"ok": true, "key": "temperature", "old": "0.2", "new": "0.5", "live": false})
    );
    let bad = host
        .api(Method::PUT, "/api/settings")
        .json(&json!({"key": "nope", "value": 1}))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
    let pid = host.primary();
    let proj = host
        .api(Method::GET, &format!("/api/projects/{pid}/settings"))
        .send()
        .await
        .unwrap();
    assert!(proj.status() == StatusCode::OK || proj.status() == StatusCode::CONFLICT);
}

#[tokio::test(flavor = "multi_thread")]
async fn raw_serves_a_generated_image_from_the_image_folder_by_its_absolute_path() {
    let work = tempfile::tempdir().unwrap();
    let images = tempfile::tempdir().unwrap();
    let png = images.path().join("gen-1.png");
    let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    bytes.extend_from_slice(&[0; 8]);
    std::fs::write(&png, &bytes).unwrap();
    std::fs::write(images.path().join("notes.txt"), "x").unwrap();
    let dir = images.path().to_path_buf();
    let host = Host::start_with(FakeSession::new(work.path()), move |o| {
        o.image_directory = Some(Arc::new(move || Some(dir.clone())));
    })
    .await;
    let pid = host.primary();
    let get = |p: String| {
        let mut url = reqwest::Url::parse(&host.url(&format!("/api/projects/{pid}/raw"))).unwrap();
        url.query_pairs_mut().append_pair("path", &p);
        host.http()
            .get(url)
            .header("X-OpenOrca-Token", TOKEN)
            .send()
    };
    let ok = get(png.to_string_lossy().to_string()).await.unwrap();
    assert_eq!(ok.status(), StatusCode::OK);
    assert_eq!(ok.headers()["content-type"], "image/png");
    assert_eq!(ok.bytes().await.unwrap().as_ref(), bytes.as_slice());
    // Only images, and only inside the folder.
    let txt = get(images
        .path()
        .join("notes.txt")
        .to_string_lossy()
        .to_string())
    .await
    .unwrap();
    assert_eq!(txt.status(), StatusCode::FORBIDDEN);
    let other = tempfile::tempdir().unwrap();
    let elsewhere = other.path().join("x.png");
    std::fs::write(&elsewhere, &bytes).unwrap();
    let out = get(elsewhere.to_string_lossy().to_string()).await.unwrap();
    assert_eq!(out.status(), StatusCode::FORBIDDEN);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_desktop_folder_dialog_is_only_there_for_the_desktop_window() {
    let work = tempfile::tempdir().unwrap();
    // A plain host: no desktop flag, no endpoint.
    let host = Host::start(FakeSession::new(work.path())).await;
    let info: Value = host
        .api(Method::GET, "/api/info")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(info.get("desktop").is_none());
    let r = host
        .api(Method::POST, "/api/desktop/pick-folder")
        .header("Origin", host.url(""))
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);

    // The desktop window's host: the hook answers (here a fake dialog), cancel gives null.
    let picked = work.path().join("picked");
    let answer = Arc::new(std::sync::Mutex::new(Some(picked.clone())));
    let seen_start = Arc::new(std::sync::Mutex::new(None));
    let (a, s) = (answer.clone(), seen_start.clone());
    let host = Host::start_with(FakeSession::new(work.path()), move |o| {
        o.desktop = Some(orca_webui::server::DesktopHooks {
            pick_folder: Arc::new(move |start| {
                *s.lock().unwrap() = start;
                let r = a.lock().unwrap().take();
                Box::pin(async move { r })
            }),
            // This machine (D155): the app's handler sees the request as sent.
            machine: Some(Arc::new(|req: axum::http::Request<axum::body::Body>| {
                let path = req.uri().path().to_string();
                Box::pin(async move {
                    axum::response::IntoResponse::into_response(axum::Json(json!({"seen": path})))
                })
            })),
            // Settings -> Updates: the app's update service sees the request as sent.
            updates: Some(Arc::new(|req: axum::http::Request<axum::body::Body>| {
                let seen = format!("{} {}", req.method(), req.uri().path());
                Box::pin(async move {
                    axum::response::IntoResponse::into_response(axum::Json(json!({"seen": seen})))
                })
            })),
        });
    })
    .await;
    let r: Value = host
        .api(Method::GET, "/api/machine/status")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r["seen"], "/api/machine/status");
    let r: Value = host
        .api(Method::GET, "/api/admin/machine/inventory")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r["seen"], "/api/admin/machine/inventory");
    for (m, p) in [
        (Method::GET, "/api/desktop/updates"),
        (Method::POST, "/api/desktop/updates/check"),
        (Method::POST, "/api/desktop/updates/apply"),
    ] {
        let r: Value = host
            .api(m.clone(), p)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(r["seen"], format!("{m} {p}"));
    }
    let info: Value = host
        .api(Method::GET, "/api/info")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(info["desktop"], true);
    let start = work.path().to_string_lossy().to_string();
    let r: Value = host
        .api(Method::POST, "/api/desktop/pick-folder")
        .header("Origin", host.url(""))
        .json(&json!({"start": start}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r["path"], picked.to_string_lossy().as_ref());
    assert_eq!(seen_start.lock().unwrap().as_deref(), Some(work.path()));
    let r: Value = host
        .api(Method::POST, "/api/desktop/pick-folder")
        .header("Origin", host.url(""))
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(r["path"].is_null());
    // Without the token: refused like any other API call.
    let r = host
        .http()
        .post(host.url("/api/desktop/pick-folder"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
}
