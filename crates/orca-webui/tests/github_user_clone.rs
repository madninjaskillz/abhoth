//! "Clone from GitHub" with each user's own credentials (D146), against a fake github.com on
//! 127.0.0.1 (API, device flow, git smart HTTP through `git http-backend`) and fake `gh` /
//! credential helpers. No real GitHub calls and no real machine credentials are read.
//!
//! - the machine's sources in order (gh, then `git credential fill`, then anonymous), and a
//!   refused token falls through;
//! - machine credentials only where allowed (never on a non-local server; admins only);
//! - pasted / saved / device-flow tokens are per Abhoth user;
//! - tokens never appear in responses, the audit log, git's config, the remote URL or the
//!   project token file; a saved token is sealed at rest on Windows.

mod common;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::Engine;
use common::{FakeSession, Host, TOKEN};
use orca_webui::WebUiOptions;
use orca_webui::git::github_user::MachineCredentials;
use reqwest::Method as M;
use serde_json::{Value, json};

const GH_TOKEN: &str = "gho_TESTONLYfromGhCli000000000000001";
const GCM_TOKEN: &str = "gho_TESTONLYfromGcm00000000000000002";
const PASTED: &str = "ghp_TESTONLYpasted0000000000000000003";
const BOB_PASTED: &str = "ghp_TESTONLYbobpasted00000000000004";
const DEVICE_TOKEN: &str = "gho_TESTONLYdevice00000000000000005";
const REFUSED: &str = "ghp_TESTONLYrefused0000000000000006";
const CLIENT_ID: &str = "Ov23liTESTONLY";

fn all_tokens() -> [&'static str; 6] {
    [
        GH_TOKEN,
        GCM_TOKEN,
        PASTED,
        BOB_PASTED,
        DEVICE_TOKEN,
        REFUSED,
    ]
}

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
    /// token -> login
    valid: Mutex<HashMap<String, String>>,
    polls: AtomicUsize,
    scope: Mutex<Option<String>>,
    /// Every git request: (path and query, the Basic password it carried).
    git_requests: Mutex<Vec<(String, Option<String>)>>,
    repos_root: PathBuf,
    backend: PathBuf,
}

impl Fake {
    fn login_of(&self, h: &HeaderMap) -> Option<String> {
        let auth = h.get("authorization")?.to_str().ok()?;
        let t = auth.strip_prefix("Bearer ")?;
        self.valid.lock().unwrap().get(t).cloned()
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

async fn device_code(State(f): State<Arc<Fake>>, body: Bytes) -> Response {
    let base = f.base.lock().unwrap().clone();
    let form = form(&body);
    if form.get("client_id").map(String::as_str) != Some(CLIENT_ID) {
        return axum::Json(json!({"error": "incorrect_client_credentials"})).into_response();
    }
    *f.scope.lock().unwrap() = form.get("scope").cloned();
    axum::Json(json!({"device_code": "dc-u1", "user_code": "WXYZ-9876",
        "verification_uri": format!("{base}/login/device"), "expires_in": 900, "interval": 1}))
    .into_response()
}

async fn access_token(State(f): State<Arc<Fake>>, body: Bytes) -> Response {
    let form = form(&body);
    if form.get("device_code").map(String::as_str) != Some("dc-u1") {
        return axum::Json(json!({"error": "incorrect_device_code"})).into_response();
    }
    if f.polls.fetch_add(1, Ordering::SeqCst) == 0 {
        return axum::Json(json!({"error": "authorization_pending"})).into_response();
    }
    axum::Json(
        json!({"access_token": DEVICE_TOKEN, "token_type": "bearer", "scope": "repo,read:org"}),
    )
    .into_response()
}

async fn api(State(f): State<Arc<Fake>>, uri: Uri, headers: HeaderMap) -> Response {
    let login = f.login_of(&headers);
    if headers.get("authorization").is_some() && login.is_none() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let base = f.base.lock().unwrap().clone();
    let repo = |full: &str, private: bool| {
        let (o, n) = full.split_once('/').unwrap();
        json!({"full_name": full, "name": n, "owner": {"login": o}, "private": private,
            "description": format!("{n} repo"), "pushed_at": "2026-11-01T00:00:00Z",
            "default_branch": "main", "html_url": format!("{base}/{full}")})
    };
    let v = match (uri.path(), login) {
        ("/api/user", Some(l)) => {
            json!({"login": l, "avatar_url": format!("https://avatars.test/{l}")})
        }
        ("/api/user/repos", Some(_)) => {
            assert!(uri.query().unwrap_or("").contains("affiliation=owner"));
            json!([repo("acme/secret", true), repo("acme/demo", false)])
        }
        ("/api/repos/acme/demo", _) | ("/api/repos/acme/secret", Some(_)) => {
            json!({"full_name": "acme/x", "default_branch": "main"})
        }
        ("/api/repos/acme/demo/branches", _) | ("/api/repos/acme/secret/branches", Some(_)) => {
            json!([{"name": "main"}, {"name": "dev"}])
        }
        (_, None) => return StatusCode::NOT_FOUND.into_response(),
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    axum::Json(v).into_response()
}

/// `git http-backend` as CGI. acme/demo is public; acme/secret needs a valid token as the
/// Basic password.
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
    let password = h("authorization")
        .strip_prefix("Basic ")
        .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok())
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .and_then(|s| s.split_once(':').map(|(_, p)| p.to_string()));
    f.git_requests.lock().unwrap().push((
        format!("{}?{}", uri.path(), uri.query().unwrap_or("")),
        password.clone(),
    ));
    let private = uri.path().starts_with("/acme/secret");
    let ok = !private
        || password
            .as_ref()
            .is_some_and(|p| f.valid.lock().unwrap().contains_key(p));
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
    {
        let mut v = fake.valid.lock().unwrap();
        for (t, l) in [
            (GH_TOKEN, "octo-gh"),
            (GCM_TOKEN, "octo-gcm"),
            (PASTED, "octo-pasted"),
            (BOB_PASTED, "bob-gh"),
            (DEVICE_TOKEN, "octo-device"),
        ] {
            v.insert(t.into(), l.into());
        }
    }
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

/// What the fake machine has.
#[derive(Clone, Copy)]
struct Machine {
    gh: Option<&'static str>,
    helper: Option<&'static str>,
}

struct World {
    temp: tempfile::TempDir,
    fake: Arc<Fake>,
    host: Host,
    base: String,
    gh_marker: PathBuf,
    helper_marker: PathBuf,
}

/// A fake `gh` (writes a marker, prints the token or fails) and a private git config whose
/// only credential helper is a fake (writes a marker, answers the token or nothing).
fn fake_machine(dir: &Path, m: Machine) -> (MachineCredentials, PathBuf, PathBuf) {
    std::fs::create_dir_all(dir).unwrap();
    let gh_marker = dir.join("gh-ran");
    let helper_marker = dir.join("helper-ran");
    let gh = if cfg!(windows) {
        let p = dir.join("gh.cmd");
        let body = match m.gh {
            Some(t) => format!(
                "@echo off\r\necho x>>\"{}\"\r\necho {t}\r\n",
                gh_marker.display()
            ),
            None => format!(
                "@echo off\r\necho x>>\"{}\"\r\nexit /b 1\r\n",
                gh_marker.display()
            ),
        };
        std::fs::write(&p, body).unwrap();
        p
    } else {
        let p = dir.join("gh");
        let body = match m.gh {
            Some(t) => format!("#!/bin/sh\necho x >> '{}'\necho {t}\n", gh_marker.display()),
            None => format!("#!/bin/sh\necho x >> '{}'\nexit 1\n", gh_marker.display()),
        };
        std::fs::write(&p, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        p
    };
    let marker = helper_marker.to_string_lossy().replace('\\', "/");
    let answer = match m.helper {
        Some(t) => format!("echo username=x-access-token; echo password={t}; "),
        None => String::new(),
    };
    let config = dir.join("gitconfig");
    std::fs::write(
        &config,
        format!(
            "[credential]\n\thelper = \"!f() {{ test \\\"$1\\\" = get || exit 0; echo x >> '{marker}'; {answer}}}; f\"\n"
        ),
    )
    .unwrap();
    let machine = MachineCredentials {
        gh: gh.into_os_string(),
        git: "git".into(),
        env: vec![
            (
                "GIT_CONFIG_GLOBAL".into(),
                config.to_string_lossy().into_owned(),
            ),
            ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
        ],
        timeout: Duration::from_secs(20),
    };
    (machine, gh_marker, helper_marker)
}

async fn world(m: Machine, tweak: impl FnOnce(&mut WebUiOptions) + Send + 'static) -> World {
    let temp = tempfile::tempdir().unwrap();
    let t = temp.path();
    let repos = t.join("repos");
    std::fs::create_dir_all(repos.join("acme")).unwrap();
    let seed = t.join("seed");
    std::fs::create_dir_all(&seed).unwrap();
    git(&seed, &["init", "-q", "-b", "main"]);
    std::fs::write(seed.join("README.md"), "hello\n").unwrap();
    git(&seed, &["add", "."]);
    git(
        &seed,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "-m",
            "first",
        ],
    );
    for name in ["demo.git", "secret.git"] {
        git(
            &repos.join("acme"),
            &["clone", "-q", "--bare", &seed.to_string_lossy(), name],
        );
        git(
            &repos.join("acme").join(name),
            &["config", "http.receivepack", "true"],
        );
    }
    let fake = start_github(&repos).await;
    let base = fake.base.lock().unwrap().clone();
    let (machine, gh_marker, helper_marker) = fake_machine(&t.join("machine"), m);
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
        o.github_machine = Some(machine);
        o.github_oauth_client_id = Some(Arc::new(|| None));
        o.session_factory = Some(Arc::new(|folder: &str| {
            FakeSession::new(Path::new(folder)) as Arc<dyn orca_webui::LiveSession>
        }));
        tweak(o);
    })
    .await;
    World {
        temp,
        fake,
        host,
        base,
        gh_marker,
        helper_marker,
    }
}

/// Every answer the browser got, for the leak check.
#[derive(Default)]
struct Bodies(Mutex<String>);

impl Bodies {
    async fn send_as(
        &self,
        w: &World,
        key: Option<&str>,
        m: M,
        path: &str,
        body: Option<Value>,
    ) -> (u16, Value) {
        let mut req = match key {
            None => w.host.api(m, path),
            Some(k) => w
                .host
                .http()
                .request(m, w.host.url(path))
                .header("Authorization", format!("Bearer {k}")),
        };
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

    async fn send(&self, w: &World, m: M, path: &str, body: Option<Value>) -> (u16, Value) {
        self.send_as(w, None, m, path, body).await
    }

    fn check_clean(&self, w: &World, extra: &[String]) {
        let bodies = self.0.lock().unwrap().clone();
        let audit =
            std::fs::read_to_string(w.host.state_dir.path().join("audit.log")).unwrap_or_default();
        for t in all_tokens() {
            assert!(!bodies.contains(t), "a response carried {t}");
            assert!(!audit.contains(t), "the audit log carried {t}");
            for (i, e) in extra.iter().enumerate() {
                assert!(!e.contains(t), "text #{i} carried {t}");
            }
        }
        for (path, _) in w.fake.git_requests.lock().unwrap().iter() {
            for t in all_tokens() {
                assert!(!path.contains(t), "a git URL carried {t}");
            }
        }
    }
}

async fn clone_and_wait(
    b: &Bodies,
    w: &World,
    key: Option<&str>,
    repo: &str,
    parent: &Path,
    name: &str,
) -> Value {
    std::fs::create_dir_all(parent).unwrap();
    let (cs, job) = b
        .send_as(
            w,
            key,
            M::POST,
            "/api/github/user/clone",
            Some(json!({"repo": repo, "parent": parent.to_string_lossy(), "name": name})),
        )
        .await;
    assert_eq!(cs, 200, "{job}");
    assert_eq!(job["url"], format!("{}/{repo}.git", w.base));
    let id = job["id"].as_str().unwrap().to_string();
    let mut done = Value::Null;
    for _ in 0..300 {
        done = b
            .send_as(w, key, M::GET, &format!("/api/clone/{id}"), None)
            .await
            .1;
        if done["state"] != "running" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    done
}

fn read_tree(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if e.metadata().unwrap().len() < 1_000_000 {
                out.push(String::from_utf8_lossy(&std::fs::read(&p).unwrap()).into_owned());
            }
        }
    }
    out
}

#[tokio::test]
async fn gh_cli_first_then_list_and_clone_a_private_repo_and_fetch_without_storing_or_leaking_the_token()
 {
    let w = world(
        Machine {
            gh: Some(GH_TOKEN),
            helper: Some(GCM_TOKEN),
        },
        |o| o.github_machine_allowed = Some(true),
    )
    .await;
    let b = Bodies::default();

    let (_, st) = b.send(&w, M::GET, "/api/github/user", None).await;
    assert_eq!(st["source"], "gh-cli", "{st}");
    assert_eq!(st["sourceText"], "Using your GitHub CLI sign-in");
    assert_eq!(st["login"], "octo-gh");
    assert_eq!(st["machineAllowed"], true);
    assert_eq!(st["oauthConfigured"], false);
    assert_eq!(st["saved"], false);

    let (_, repos) = b
        .send(&w, M::GET, "/api/github/user/repos?q=acme", None)
        .await;
    assert_eq!(repos["total"], 2, "{repos}");
    assert_eq!(repos["items"][0]["fullName"], "acme/secret");
    assert_eq!(repos["items"][0]["private"], true);
    let (_, br) = b
        .send(
            &w,
            M::GET,
            "/api/github/user/branches?repo=acme/secret",
            None,
        )
        .await;
    assert_eq!(br["branches"], json!(["main", "dev"]));

    let parent = w.temp.path().join("clones");
    let done = clone_and_wait(&b, &w, None, "acme/secret", &parent, "secret").await;
    assert_eq!(done["state"], "done", "{done}");
    let pid = done["projectId"].as_str().unwrap().to_string();
    let clone = parent.join("secret");
    assert!(clone.join("README.md").is_file());
    assert_eq!(
        git(&clone, &["remote", "get-url", "origin"]).trim(),
        format!("{}/acme/secret.git", w.base)
    );
    // git got the gh token as the Basic password through the helper, never in the URL.
    assert!(
        w.fake
            .git_requests
            .lock()
            .unwrap()
            .iter()
            .any(|(_, p)| p.as_deref() == Some(GH_TOKEN))
    );
    // The project is linked to its owner's credentials, not given a copy.
    let project_tokens =
        std::fs::read_to_string(w.host.state_dir.path().join("project-tokens.json")).unwrap();
    assert!(project_tokens.contains("@github-user:"), "{project_tokens}");
    let (_, shown) = b
        .send(&w, M::GET, &format!("/api/projects/{pid}/github"), None)
        .await;
    assert_eq!(shown["githubUser"], true);
    // Fetch resolves the link to the machine's token again.
    let (fs, fetch) = b
        .send(&w, M::POST, &format!("/api/projects/{pid}/git/fetch"), None)
        .await;
    assert_eq!(fs, 200, "{fetch}");
    // Nothing was saved: machine tokens stay where they are.
    assert!(
        !w.host
            .state_dir
            .path()
            .join("github-user-tokens.json")
            .exists()
    );
    let mut extra = read_tree(&clone.join(".git"));
    extra.push(project_tokens);
    extra.push(git(&clone, &["config", "--list", "--show-origin"]));
    b.check_clean(&w, &extra);
}

#[tokio::test]
async fn git_credential_fill_when_gh_is_missing_and_refused_tokens_fall_through_to_anonymous() {
    // No gh sign-in; the helper has a token: git credential fill is used.
    let w = world(
        Machine {
            gh: None,
            helper: Some(GCM_TOKEN),
        },
        |o| o.github_machine_allowed = Some(true),
    )
    .await;
    let b = Bodies::default();
    let (_, st) = b.send(&w, M::GET, "/api/github/user", None).await;
    assert_eq!(st["source"], "git-credential", "{st}");
    assert_eq!(st["login"], "octo-gcm");
    assert!(w.gh_marker.is_file() && w.helper_marker.is_file());
    b.check_clean(&w, &[]);

    // gh's token is refused and the helper has nothing: anonymous, with a note.
    let w = world(
        Machine {
            gh: Some(REFUSED),
            helper: None,
        },
        |o| o.github_machine_allowed = Some(true),
    )
    .await;
    let b = Bodies::default();
    let (_, st) = b.send(&w, M::GET, "/api/github/user", None).await;
    assert_eq!(st["source"], "anonymous", "{st}");
    assert_eq!(st["signedIn"], false);
    assert!(st["notes"][0].as_str().unwrap().contains("GitHub CLI"));
    assert_eq!(
        b.send(&w, M::GET, "/api/github/user/repos", None).await.0,
        409
    );
    // A public repository clones anonymously; git sent no credentials.
    let parent = w.temp.path().join("clones");
    let done = clone_and_wait(&b, &w, None, "acme/demo", &parent, "demo").await;
    assert_eq!(done["state"], "done", "{done}");
    assert!(
        w.fake
            .git_requests
            .lock()
            .unwrap()
            .iter()
            .all(|(_, p)| p.is_none())
    );
    // Not linked (nothing to resolve later).
    let pid = done["projectId"].as_str().unwrap();
    let project_tokens =
        std::fs::read_to_string(w.host.state_dir.path().join("project-tokens.json"))
            .unwrap_or_default();
    assert!(!project_tokens.contains(pid));
    b.check_clean(&w, &[]);
}

#[tokio::test]
async fn a_server_reachable_from_other_machines_never_reads_this_machines_credentials() {
    // Behind a reverse proxy (so reachable from elsewhere; not overridden): machine
    // credentials are off. (`machine_credentials_only_for_local_servers` covers 0.0.0.0 and
    // --hostui-login.)
    let w = world(
        Machine {
            gh: Some(GH_TOKEN),
            helper: Some(GCM_TOKEN),
        },
        |o| o.login.trusted_proxies = vec!["10.9.9.9".parse().unwrap()],
    )
    .await;
    let b = Bodies::default();
    let (_, st) = b.send(&w, M::GET, "/api/github/user", None).await;
    assert_eq!(st["source"], "anonymous", "{st}");
    assert_eq!(st["machineAllowed"], false);
    assert_eq!(st["machineServer"], false);
    let _ = b.send(&w, M::GET, "/api/github/user/repos", None).await;
    // The clone is isolated from the server's own helpers: it fails, and git sent nothing.
    let parent = w.temp.path().join("clones");
    let failed = clone_and_wait(&b, &w, None, "acme/secret", &parent, "secret").await;
    assert_eq!(failed["state"], "failed", "{failed}");
    assert!(
        w.fake
            .git_requests
            .lock()
            .unwrap()
            .iter()
            .all(|(_, p)| p.is_none())
    );
    assert!(!w.gh_marker.exists(), "gh was run");
    assert!(!w.helper_marker.exists(), "a credential helper was run");
    b.check_clean(&w, &[]);
}

#[tokio::test]
async fn pasted_saved_and_device_flow_tokens_belong_to_one_user_each() {
    let w = world(
        Machine {
            gh: Some(GH_TOKEN),
            helper: None,
        },
        |o| {
            o.github_machine_allowed = Some(true);
            o.github_oauth_client_id = Some(Arc::new(|| Some(CLIENT_ID.to_string())));
        },
    )
    .await;
    let b = Bodies::default();
    // Bob: not an admin, with an API key.
    let db = &w.host.server.state().login.db;
    let bob = db.create_user("bob", "x", false, false, 0).unwrap();
    let bob_key = "abk_TESTONLYbobkey0000000000000000";
    db.create_api_key(bob.id, "t", bob_key, "abk_TEST", 0)
        .unwrap();
    let bob_key = Some(bob_key);

    // Machine credentials are the admin's only.
    let (_, st) = b
        .send_as(&w, bob_key, M::GET, "/api/github/user", None)
        .await;
    assert_eq!(st["source"], "anonymous", "{st}");
    assert_eq!(st["machineAllowed"], false);
    assert_eq!(st["oauthConfigured"], true);

    // A refused token is refused, and the message doesn't echo it.
    let (rs, refused) = b
        .send(
            &w,
            M::POST,
            "/api/github/user/token",
            Some(json!({"token": REFUSED, "save": true})),
        )
        .await;
    assert_eq!(rs, 400, "{refused}");

    // The admin saves a token: theirs only, and sealed at rest on Windows.
    let (s1, _) = b
        .send(
            &w,
            M::POST,
            "/api/github/user/token",
            Some(json!({"token": PASTED, "save": true})),
        )
        .await;
    assert_eq!(s1, 200);
    let (_, st) = b.send(&w, M::GET, "/api/github/user", None).await;
    assert_eq!(st["source"], "saved");
    assert_eq!(st["login"], "octo-pasted");
    let stored =
        std::fs::read_to_string(w.host.state_dir.path().join("github-user-tokens.json")).unwrap();
    if cfg!(windows) {
        assert!(stored.contains("dpapi:"), "{stored}");
    }
    let (_, st) = b
        .send_as(&w, bob_key, M::GET, "/api/github/user", None)
        .await;
    assert_eq!(st["source"], "anonymous");

    // Bob pastes his own for the session: only he uses it; nothing is written for him.
    let (s2, _) = b
        .send_as(
            &w,
            bob_key,
            M::POST,
            "/api/github/user/token",
            Some(json!({"token": BOB_PASTED})),
        )
        .await;
    assert_eq!(s2, 200);
    let (_, st) = b
        .send_as(&w, bob_key, M::GET, "/api/github/user", None)
        .await;
    assert_eq!(st["source"], "session");
    assert_eq!(st["login"], "bob-gh");
    let (_, st) = b.send(&w, M::GET, "/api/github/user", None).await;
    assert_eq!(st["login"], "octo-pasted");
    let stored =
        std::fs::read_to_string(w.host.state_dir.path().join("github-user-tokens.json")).unwrap();
    let ids: HashSet<String> = serde_json::from_str::<HashMap<String, Value>>(&stored)
        .unwrap()
        .into_keys()
        .collect();
    assert!(!ids.contains(&bob.id.to_string()));

    // Device flow: the admin starts it; Bob can't poll it; the admin gets signed in.
    let (_, start) = b.send(&w, M::POST, "/api/github/user/device", None).await;
    assert_eq!(start["userCode"], "WXYZ-9876", "{start}");
    assert_eq!(
        w.fake.scope.lock().unwrap().as_deref(),
        Some("repo read:org")
    );
    let flow = start["flowId"].as_str().unwrap().to_string();
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let (_, p) = b
        .send_as(
            &w,
            bob_key,
            M::POST,
            &format!("/api/github/user/device/{flow}"),
            None,
        )
        .await;
    assert_eq!(p["state"], "expired");
    let mut state = String::new();
    for _ in 0..10 {
        let (_, p) = b
            .send(
                &w,
                M::POST,
                &format!("/api/github/user/device/{flow}"),
                None,
            )
            .await;
        state = p["state"].as_str().unwrap().to_string();
        if state != "pending" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1100)).await;
    }
    assert_eq!(state, "done");
    let (_, st) = b.send(&w, M::GET, "/api/github/user", None).await;
    assert_eq!(st["source"], "oauth");
    assert_eq!(st["login"], "octo-device");
    let (_, st) = b
        .send_as(&w, bob_key, M::GET, "/api/github/user", None)
        .await;
    assert_eq!(st["login"], "bob-gh");

    // Forget: the admin falls back to the machine's gh sign-in; Bob is untouched.
    let (_, f) = b.send(&w, M::DELETE, "/api/github/user/token", None).await;
    assert_eq!(f["removed"], true);
    let (_, st) = b.send(&w, M::GET, "/api/github/user", None).await;
    assert_eq!(st["source"], "gh-cli");
    let (_, st) = b
        .send_as(&w, bob_key, M::GET, "/api/github/user", None)
        .await;
    assert_eq!(st["source"], "session");
    let stored =
        std::fs::read_to_string(w.host.state_dir.path().join("github-user-tokens.json")).unwrap();
    b.check_clean(&w, &[stored]);
    let _ = TOKEN;
}

#[tokio::test]
async fn device_flow_needs_the_oauth_client_id() {
    let w = world(
        Machine {
            gh: None,
            helper: None,
        },
        |o| o.github_machine_allowed = Some(false),
    )
    .await;
    let b = Bodies::default();
    let (_, st) = b.send(&w, M::GET, "/api/github/user", None).await;
    assert_eq!(st["oauthConfigured"], false);
    let (s, body) = b.send(&w, M::POST, "/api/github/user/device", None).await;
    assert_eq!(s, 400);
    assert!(body["error"].as_str().unwrap().contains("oauthClientId"));
    for repo in ["acme", "../etc/passwd", "-x/y"] {
        let (s, _) = b
            .send(
                &w,
                M::POST,
                "/api/github/user/clone",
                Some(json!({"repo": repo, "parent": w.temp.path().to_string_lossy()})),
            )
            .await;
        assert_eq!(s, 400, "{repo}");
    }
}
