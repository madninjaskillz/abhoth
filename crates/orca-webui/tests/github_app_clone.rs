//! `GitHubAppCloneTests`: "Clone from GitHub" end to end against a fake github.com on 127.0.0.1
//! (device flow, API, git smart HTTP through `git http-backend`): sign in, list repositories
//! and branches, clone a branch, fetch and push with the sign-in's token (refreshed when it
//! expires), sign out. The token is never in the clone URL, .git/config, API responses, the
//! audit log or the project token file. No real GitHub calls.

mod common;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::Engine;
use common::{FakeSession, Host};
use orca_webui::git::github_app::GitHubAppConfig;
use reqwest::Method as M;
use serde_json::{Value, json};

fn git(cwd: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "git {}: {text}", args.join(" "));
    text
}

#[derive(Default)]
struct Fake {
    base: Mutex<String>,
    issued: Mutex<Vec<String>>,
    current: Mutex<String>,
    polls: AtomicUsize,
    refreshes: AtomicUsize,
    repos_root: PathBuf,
    backend: PathBuf,
}

impl Fake {
    /// A new user token (each refresh rotates it; only the latest is accepted, like GitHub).
    fn issue(&self, expires_in: i64) -> Value {
        let mut issued = self.issued.lock().unwrap();
        let access = format!(
            "ghu_TESTONLY{:02}{}",
            issued.len(),
            orca_webui::util::short_id(32)
        );
        *self.current.lock().unwrap() = access.clone();
        issued.push(access.clone());
        let refresh = format!(
            "ghr_TESTONLY{:02}{}",
            issued.len(),
            orca_webui::util::short_id(32)
        );
        issued.push(refresh.clone());
        json!({"access_token": access, "expires_in": expires_in, "refresh_token": refresh,
            "refresh_token_expires_in": 15897600, "scope": "", "token_type": "bearer"})
    }

    fn bearer(&self, h: &HeaderMap) -> bool {
        h.get("authorization").and_then(|v| v.to_str().ok())
            == Some(&format!("Bearer {}", self.current.lock().unwrap()))
    }
}

fn form(body: &Bytes) -> HashMap<String, String> {
    String::from_utf8_lossy(body)
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| {
            let v = percent_encoding::percent_decode_str(&v.replace('+', " "))
                .decode_utf8_lossy()
                .into_owned();
            (k.to_string(), v)
        })
        .collect()
}

fn unauthorized() -> Response {
    StatusCode::UNAUTHORIZED.into_response()
}

async fn device_code(State(f): State<Arc<Fake>>, body: Bytes) -> Response {
    let base = f.base.lock().unwrap().clone();
    if form(&body).get("client_id").map(String::as_str) == Some("Iv23liTESTONLY") {
        axum::Json(json!({"device_code": "dc-1", "user_code": "ABCD-1234", "verification_uri": format!("{base}/login/device"), "expires_in": 900, "interval": 1})).into_response()
    } else {
        axum::Json(json!({"error": "incorrect_client_credentials"})).into_response()
    }
}

async fn access_token(State(f): State<Arc<Fake>>, body: Bytes) -> Response {
    let form = form(&body);
    if form.get("grant_type").map(String::as_str) == Some("refresh_token") {
        if form.get("refresh_token") != f.issued.lock().unwrap().last() {
            return axum::Json(json!({"error": "bad_refresh_token"})).into_response();
        }
        f.refreshes.fetch_add(1, Ordering::SeqCst);
        return axum::Json(f.issue(60)).into_response(); // still "about to expire": every use refreshes
    }
    if form.get("device_code").map(String::as_str) != Some("dc-1") {
        return axum::Json(json!({"error": "incorrect_device_code"})).into_response();
    }
    // First poll: not entered yet; then signed in, with a token inside the refresh margin.
    if f.polls.fetch_add(1, Ordering::SeqCst) == 0 {
        axum::Json(json!({"error": "authorization_pending"})).into_response()
    } else {
        axum::Json(f.issue(60)).into_response()
    }
}

async fn api(
    State(f): State<Arc<Fake>>,
    uri: Uri,
    headers: HeaderMap,
    Query(_q): Query<HashMap<String, String>>,
) -> Response {
    if !f.bearer(&headers) {
        return unauthorized();
    }
    let base = f.base.lock().unwrap().clone();
    let v = match uri.path() {
        "/api/user" => json!({"login": "octo", "avatar_url": "https://avatars.test/octo"}),
        "/api/user/installations" => {
            json!({"total_count": 1, "installations": [{"id": 7, "account": {"login": "acme", "type": "Organization"}}]})
        }
        "/api/user/installations/7/repositories" => json!({"total_count": 1, "repositories": [
            {"full_name": "acme/demo", "name": "demo", "owner": {"login": "acme"}, "private": true, "description": "Demo",
             "pushed_at": "2026-11-01T00:00:00Z", "default_branch": "main", "html_url": format!("{base}/acme/demo")}]}),
        "/api/repos/acme/demo" => json!({"full_name": "acme/demo", "default_branch": "main"}),
        "/api/repos/acme/demo/branches" => json!([{"name": "main"}, {"name": "dev"}]),
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    axum::Json(v).into_response()
}

/// git http-backend as CGI (smart HTTP) behind a Basic "x-access-token:<current token>" check.
async fn git_backend(
    State(f): State<Arc<Fake>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let h = |n: &str| {
        headers
            .get(n)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    };
    let auth = h("authorization");
    let ok = auth
        .strip_prefix("Basic ")
        .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok())
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        == Some(format!("x-access-token:{}", f.current.lock().unwrap()));
    if !ok {
        return Response::builder()
            .status(401)
            .header("WWW-Authenticate", "Basic realm=\"git\"")
            .body(Body::empty())
            .unwrap();
    }
    let mut cmd = tokio::process::Command::new(&f.backend);
    cmd.env("GIT_PROJECT_ROOT", &f.repos_root)
        .env("GIT_HTTP_EXPORT_ALL", "1")
        .env("PATH_INFO", uri.path())
        .env("REQUEST_METHOD", method.as_str())
        .env("QUERY_STRING", uri.query().unwrap_or(""))
        .env("CONTENT_TYPE", h("content-type"))
        .env("REMOTE_USER", "x-access-token")
        .env("REMOTE_ADDR", "127.0.0.1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped());
    if !h("content-encoding").is_empty() {
        cmd.env("HTTP_CONTENT_ENCODING", h("content-encoding"));
    }
    if !h("git-protocol").is_empty() {
        cmd.env("GIT_PROTOCOL", h("git-protocol"));
    }
    let mut child = cmd.spawn().unwrap();
    {
        use tokio::io::AsyncWriteExt;
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(&body).await.unwrap();
    }
    let out = child.wait_with_output().await.unwrap().stdout;
    let (split, sep) = match out.windows(4).position(|w| w == b"\r\n\r\n") {
        Some(i) => (i, 4),
        None => (out.windows(2).position(|w| w == b"\n\n").unwrap(), 2),
    };
    let mut res = Response::builder();
    for line in String::from_utf8_lossy(&out[..split]).split('\n') {
        let Some((k, v)) = line.trim().split_once(':') else {
            continue;
        };
        let v = v.trim();
        if k.eq_ignore_ascii_case("Status") {
            res = res.status(v.split(' ').next().unwrap().parse::<u16>().unwrap());
        } else {
            res = res.header(k, v);
        }
    }
    res.body(Body::from(out[split + sep..].to_vec())).unwrap()
}

async fn start_github(root: &Path) -> Arc<Fake> {
    let exec = git(root, &["--exec-path"]).trim().to_string();
    let backend = Path::new(&exec).join(if cfg!(windows) {
        "git-http-backend.exe"
    } else {
        "git-http-backend"
    });
    let fake = Arc::new(Fake {
        repos_root: root.to_path_buf(),
        backend,
        ..Default::default()
    });
    let app = Router::new()
        .route("/login/device/code", post(device_code))
        .route("/login/oauth/access_token", post(access_token))
        .route("/api/{*rest}", get(api))
        .fallback(git_backend)
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    *fake.base.lock().unwrap() = format!("http://127.0.0.1:{port}");
    fake
}

struct World {
    temp: tempfile::TempDir,
    fake: Arc<Fake>,
    host: Host,
    base: String,
}

async fn world() -> World {
    let temp = tempfile::tempdir().unwrap();
    let t = temp.path();
    let repos = t.join("repos");
    std::fs::create_dir_all(repos.join("acme")).unwrap();
    let seed = t.join("seed");
    std::fs::create_dir_all(&seed).unwrap();
    git(&seed, &["init", "-q", "-b", "main"]);
    std::fs::write(seed.join("README.md"), "hello\n").unwrap();
    git(&seed, &["add", "."]);
    let commit = [
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t",
        "commit",
        "-q",
        "-m",
    ];
    git(&seed, &[&commit[..], &["first"]].concat());
    git(&seed, &["checkout", "-q", "-b", "dev"]);
    std::fs::write(seed.join("DEV.md"), "dev\n").unwrap();
    git(&seed, &["add", "."]);
    git(&seed, &[&commit[..], &["on dev"]].concat());
    git(
        &repos.join("acme"),
        &["clone", "-q", "--bare", &seed.to_string_lossy(), "demo.git"],
    );
    git(
        &repos.join("acme").join("demo.git"),
        &["config", "http.receivepack", "true"],
    );
    let fake = start_github(&repos).await;
    let base = fake.base.lock().unwrap().clone();
    let work = t.join("work");
    std::fs::create_dir_all(&work).unwrap();
    let b = base.clone();
    let host = Host::start_with(FakeSession::new(&work), move |o| {
        o.audit_file = o
            .projects_file
            .as_ref()
            .map(|p| p.with_file_name("audit.log"));
        o.github_web = b.clone();
        o.github_api = format!("{b}/api");
        o.github_app = Some(Arc::new(|| {
            GitHubAppConfig::new(Some("Iv23liTESTONLY"), Some("openorca-test"))
        }));
        o.session_factory = Some(Arc::new(|folder: &str| {
            FakeSession::new(Path::new(folder)) as Arc<dyn orca_webui::LiveSession>
        }));
    })
    .await;
    World {
        temp,
        fake,
        host,
        base,
    }
}

impl World {
    fn state(&self) -> &Path {
        self.host.state_dir.path()
    }
}

/// Every answer the browser got, for the leak check.
#[derive(Default)]
struct Bodies(Mutex<String>);

impl Bodies {
    async fn send(&self, w: &World, m: M, path: &str, body: Option<Value>) -> (u16, Value) {
        let mut req = w.host.api(m, path);
        if let Some(b) = body {
            req = req.json(&b);
        }
        let r = req.send().await.unwrap();
        let status = r.status().as_u16();
        let text = r.text().await.unwrap();
        self.0.lock().unwrap().push_str(&text);
        self.0.lock().unwrap().push('\n');
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }
}

#[tokio::test]
async fn sign_in_pick_a_repo_clone_a_branch_fetch_and_push_with_refreshed_tokens_then_sign_out_without_leaking_tokens()
 {
    let w = world().await;
    let b = Bodies::default();

    let (_, status) = b.send(&w, M::GET, "/api/github/app", None).await;
    assert_eq!(status["configured"], true);
    assert_eq!(status["signedIn"], false);
    assert_eq!(
        status["installUrl"],
        format!("{}/apps/openorca-test/installations/new", w.base)
    );
    assert_eq!(
        b.send(&w, M::GET, "/api/github/app/repos", None).await.0,
        409
    ); // not signed in

    // Device flow: the code is shown; polling goes pending, then done.
    let (_, start) = b.send(&w, M::POST, "/api/github/app/device", None).await;
    assert_eq!(start["userCode"], "ABCD-1234");
    assert!(start.get("deviceCode").is_none());
    let flow = start["flowId"].as_str().unwrap().to_string();
    let mut state;
    let mut tries = 0;
    loop {
        tokio::time::sleep(Duration::from_millis(1100)).await;
        let (_, p) = b
            .send(&w, M::POST, &format!("/api/github/app/device/{flow}"), None)
            .await;
        state = p["state"].as_str().unwrap().to_string();
        tries += 1;
        if state != "pending" || tries >= 10 {
            break;
        }
    }
    assert_eq!(state, "done");
    assert_eq!(w.fake.polls.load(Ordering::SeqCst), 2);
    let (_, status) = b.send(&w, M::GET, "/api/github/app", None).await;
    assert_eq!(status["signedIn"], true);
    assert_eq!(status["login"], "octo");
    let account_file = w.state().join("github-account.json");
    assert!(account_file.is_file());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&account_file)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    // Repositories and branches.
    let (_, repos) = b
        .send(&w, M::GET, "/api/github/app/repos?q=demo", None)
        .await;
    assert_eq!(repos["total"], 1);
    assert_eq!(repos["items"][0]["fullName"], "acme/demo");
    assert_eq!(repos["items"][0]["private"], true);
    assert_eq!(repos["items"][0]["updatedAt"], "2026-11-01T00:00:00+00:00");
    assert_eq!(
        repos["installations"][0],
        json!({"id": 7, "account": "acme", "type": "Organization"})
    );
    let (_, branches) = b
        .send(&w, M::GET, "/api/github/app/branches?repo=acme/demo", None)
        .await;
    assert_eq!(branches["defaultBranch"], "main");
    assert_eq!(branches["branches"], json!(["main", "dev"]));
    assert_eq!(branches["truncated"], false);

    // Clone the dev branch.
    let parent = w.temp.path().join("clones");
    std::fs::create_dir_all(&parent).unwrap();
    let (cs, job) = b
        .send(
            &w,
            M::POST,
            "/api/github/app/clone",
            Some(json!({"repo": "acme/demo", "branch": "dev", "parent": parent.to_string_lossy()})),
        )
        .await;
    assert_eq!(cs, 200, "{job}");
    assert_eq!(job["url"], format!("{}/acme/demo.git", w.base));
    let id = job["id"].as_str().unwrap().to_string();
    let mut done = Value::Null;
    for _ in 0..300 {
        done = b
            .send(&w, M::GET, &format!("/api/clone/{id}"), None)
            .await
            .1;
        if done["state"] != "running" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(done["state"], "done", "{done}");
    let pid = done["projectId"].as_str().unwrap().to_string();
    let clone = parent.join("demo");
    assert_eq!(
        git(&clone, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
        "dev"
    );
    assert!(clone.join("DEV.md").is_file());
    assert_eq!(
        git(&clone, &["remote", "get-url", "origin"]).trim(),
        format!("{}/acme/demo.git", w.base)
    ); // no credentials in the URL
    assert_eq!(
        b.send(&w, M::GET, &format!("/api/projects/{pid}/github"), None)
            .await
            .1["token"],
        "GitHub sign-in (@octo)"
    );

    // fetch and push: each refreshes the about-to-expire token first and gives git the new one.
    let before = w.fake.refreshes.load(Ordering::SeqCst);
    let (fs, fetch) = b
        .send(&w, M::POST, &format!("/api/projects/{pid}/git/fetch"), None)
        .await;
    assert_eq!(fs, 200, "{fetch}");
    assert!(w.fake.refreshes.load(Ordering::SeqCst) > before);
    std::fs::write(clone.join("new.txt"), "x\n").unwrap();
    git(&clone, &["add", "."]);
    git(
        &clone,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "-m",
            "pushed via app",
        ],
    );
    let (ps, push) = b
        .send(&w, M::POST, &format!("/api/projects/{pid}/git/push"), None)
        .await;
    assert_eq!(ps, 200, "{push}");
    assert!(
        git(
            &w.temp.path().join("repos").join("acme").join("demo.git"),
            &["log", "--oneline", "-1", "dev"]
        )
        .contains("pushed via app")
    );
    let stored: Value =
        serde_json::from_str(&std::fs::read_to_string(&account_file).unwrap()).unwrap();
    assert_eq!(
        stored["accessToken"].as_str().unwrap(),
        *w.fake.current.lock().unwrap()
    ); // the rotated tokens were saved

    // No token anywhere it shouldn't be.
    let tokens = w.fake.issued.lock().unwrap().clone();
    assert!(tokens.len() >= 4);
    let git_config = git(&clone, &["config", "--list", "--show-origin"]);
    let mut cloned_files = Vec::new();
    let mut stack = vec![clone.join(".git")];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if e.metadata().unwrap().len() < 1_000_000 {
                cloned_files
                    .push(String::from_utf8_lossy(&std::fs::read(&p).unwrap()).into_owned());
            }
        }
    }
    let audit = std::fs::read_to_string(w.state().join("audit.log")).unwrap();
    let project_tokens = std::fs::read_to_string(w.state().join("project-tokens.json")).unwrap();
    let bodies = b.0.lock().unwrap().clone();
    for t in &tokens {
        assert!(!git_config.contains(t));
        assert!(cloned_files.iter().all(|f| !f.contains(t)));
        assert!(!bodies.contains(t));
        assert!(!audit.contains(t));
        assert!(!project_tokens.contains(t)); // linked to the sign-in, not given a copy
    }
    assert!(audit.contains("\"github.signin\""));
    assert!(audit.contains("github-app"));

    // Sign out: tokens gone; the linked project's fetch says to sign in again.
    let (_, signout) = b.send(&w, M::POST, "/api/github/app/signout", None).await;
    assert_eq!(signout["removed"], true);
    assert!(
        signout["revokeUrl"]
            .as_str()
            .unwrap()
            .ends_with("/settings/apps/authorizations")
    );
    assert!(!account_file.exists());
    assert_eq!(
        b.send(&w, M::GET, "/api/github/app", None).await.1["signedIn"],
        false
    );
    let (after, body) = b
        .send(&w, M::POST, &format!("/api/projects/{pid}/git/fetch"), None)
        .await;
    assert_eq!(after, 400);
    assert!(
        body.to_string().contains("Not signed in to GitHub"),
        "{body}"
    );
    assert!(
        std::fs::read_to_string(w.state().join("audit.log"))
            .unwrap()
            .contains("\"github.signout\"")
    );
}

#[tokio::test]
async fn clone_requests_are_checked() {
    let w = world().await;
    let parent = w.temp.path().join("clones2");
    std::fs::create_dir_all(&parent).unwrap();
    let p = parent.to_string_lossy().to_string();
    // Not signed in: refused before anything runs (409, not 401, which would end the web session).
    let r = w
        .host
        .api(M::POST, "/api/github/app/clone")
        .json(&json!({"repo": "acme/demo", "parent": p}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 409);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["signedOut"], true);
    for repo in ["acme", "../etc/passwd", "acme/demo/../../x", "-x/y"] {
        let r = w
            .host
            .api(M::POST, "/api/github/app/clone")
            .json(&json!({"repo": repo, "parent": p}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 400, "{repo}");
    }
}

#[tokio::test]
async fn status_says_how_to_set_it_up_when_not_configured() {
    let temp = tempfile::tempdir().unwrap();
    let host = Host::start_with(FakeSession::new(temp.path()), |o| {
        o.github_app = Some(Arc::new(GitHubAppConfig::default));
    })
    .await;
    let v: Value = host
        .api(M::GET, "/api/github/app")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["configured"], false);
    assert_eq!(v["signedIn"], false);
    assert!(v["installUrl"].is_null());
    assert!(
        v["setup"]
            .as_str()
            .unwrap()
            .contains("ABHOTH_GITHUB_APP_CLIENT_ID")
    );
    let r = host
        .api(M::POST, "/api/github/app/device")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 400);
}
