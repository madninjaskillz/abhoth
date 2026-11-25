//! `GitHubAppServiceTests`: the GitHub App sign-in against a fake GitHub on 127.0.0.1 (device
//! flow: pending, slow_down, success, expired, denied; token refresh and rotation; repository
//! paging; branches; sign-out). No real GitHub calls.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::IntoResponse;
use chrono::{DateTime, TimeZone, Utc};
use orca_webui::git::github_app::{
    Clock, GitHubAccount, GitHubAccountStore, GitHubAppConfig, GitHubAppError, GitHubAppService,
    ManualClock,
};
use serde_json::{Value, json};

const CLIENT_ID: &str = "Iv23liTESTONLY";

#[derive(Debug, Clone)]
pub struct Seen {
    pub method: String,
    /// Path and query, as sent.
    pub url: String,
    pub body: String,
    pub bearer: Option<String>,
    pub accept: Option<String>,
}

type Handler = Arc<dyn Fn(&Seen) -> (u16, Value) + Send + Sync>;

/// Answers by method and path+query; records every request.
#[derive(Clone, Default)]
pub struct FakeGitHub {
    routes: Arc<Mutex<HashMap<String, Handler>>>,
    pub requests: Arc<Mutex<Vec<Seen>>>,
}

impl FakeGitHub {
    pub fn on(
        &self,
        method: &str,
        url: &str,
        f: impl Fn(&Seen) -> (u16, Value) + Send + Sync + 'static,
    ) {
        self.routes
            .lock()
            .unwrap()
            .insert(format!("{method} {url}"), Arc::new(f));
    }

    pub fn count(&self, part: &str) -> usize {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.url.contains(part))
            .count()
    }

    pub fn seen(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    pub fn last(&self) -> Seen {
        self.requests.lock().unwrap().last().unwrap().clone()
    }

    /// Start on 127.0.0.1:0; the base URL ("http://127.0.0.1:port").
    pub async fn start(&self) -> String {
        let me = self.clone();
        let app = Router::new().fallback(
            move |method: Method, uri: Uri, headers: HeaderMap, body: Bytes| {
                let me = me.clone();
                async move {
                    let h = |n: &str| {
                        headers
                            .get(n)
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_string)
                    };
                    let seen = Seen {
                        method: method.to_string(),
                        url: uri
                            .path_and_query()
                            .map(|p| p.to_string())
                            .unwrap_or_default(),
                        body: String::from_utf8_lossy(&body).into_owned(),
                        bearer: h("authorization")
                            .and_then(|a| a.strip_prefix("Bearer ").map(str::to_string)),
                        accept: h("accept"),
                    };
                    me.requests.lock().unwrap().push(seen.clone());
                    let f = me
                        .routes
                        .lock()
                        .unwrap()
                        .get(&format!("{} {}", seen.method, seen.url))
                        .cloned();
                    let (status, v) = match f {
                        Some(f) => f(&seen),
                        None => (404, json!({})),
                    };
                    (StatusCode::from_u16(status).unwrap(), axum::Json(v)).into_response()
                }
            },
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://127.0.0.1:{port}")
    }
}

struct Fixture {
    dir: tempfile::TempDir,
    clock: Arc<ManualClock>,
    gh: FakeGitHub,
    base: String,
    svc: GitHubAppService,
}

fn start_time() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 24, 9, 0, 0).unwrap()
}

async fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(ManualClock::new(start_time()));
    let gh = FakeGitHub::default();
    let base = gh.start().await;
    let svc = service(
        &dir,
        &clock,
        &base,
        GitHubAppConfig::new(Some(CLIENT_ID), Some("openorca-test")),
    );
    Fixture {
        dir,
        clock,
        gh,
        base,
        svc,
    }
}

fn service(
    dir: &tempfile::TempDir,
    clock: &Arc<ManualClock>,
    base: &str,
    config: GitHubAppConfig,
) -> GitHubAppService {
    let c: Arc<dyn Clock> = clock.clone();
    GitHubAppService::new(
        Arc::new(move || config.clone()),
        base,
        &format!("{base}/api"),
        GitHubAccountStore::new(dir.path()),
        Some(c),
    )
}

impl Fixture {
    fn account_file(&self) -> std::path::PathBuf {
        self.dir.path().join("github-account.json")
    }

    fn device_code_answer(&self, interval: i64) {
        let base = self.base.clone();
        self.gh.on("POST", "/login/device/code", move |_| {
            (200, json!({"device_code": "dc-123", "user_code": "WDJB-MJHT", "verification_uri": format!("{base}/login/device"), "expires_in": 900, "interval": interval}))
        });
    }

    fn user_answer(&self) {
        self.gh.on("GET", "/api/user", |r| match &r.bearer {
            Some(t) if t.starts_with("ghu_") => (
                200,
                json!({"login": "octo", "avatar_url": "https://avatars.test/octo"}),
            ),
            _ => (401, json!({"message": "Bad credentials"})),
        });
    }

    fn sign_in(&self, access: &str, refresh: &str, expires_in: i64) {
        let now = self.clock.now();
        GitHubAccountStore::new(self.dir.path())
            .save(&GitHubAccount {
                access_token: access.into(),
                access_expires: Some(now + chrono::Duration::seconds(expires_in)),
                refresh_token: Some(refresh.into()),
                refresh_expires: Some(now + chrono::Duration::days(180)),
                login: Some("octo".into()),
                avatar_url: None,
                signed_in_at: now,
            })
            .unwrap();
    }

    fn sign_in_default(&self) {
        self.sign_in("ghu_test-old-access-token", "ghr_OLDrefresh", 28800);
    }
}

fn token_body(access: &str, refresh: &str, expires_in: i64) -> Value {
    json!({"access_token": access, "expires_in": expires_in, "refresh_token": refresh,
        "refresh_token_expires_in": 15897600, "scope": "", "token_type": "bearer"})
}

#[tokio::test]
async fn device_start_sends_the_client_id_and_keeps_the_device_code_on_the_server() {
    let f = fixture().await;
    f.device_code_answer(5);
    let s = f.svc.start_device().await.unwrap();
    assert_eq!(s.user_code, "WDJB-MJHT");
    assert_eq!(s.verification_uri, format!("{}/login/device", f.base));
    assert_eq!(s.interval, 5);
    assert!(!serde_json::to_string(&s).unwrap().contains("dc-123"));
    assert_eq!(f.gh.seen(), 1);
    let r = f.gh.last();
    assert!(r.body.contains(&format!("client_id={CLIENT_ID}")));
    assert_eq!(r.accept.as_deref(), Some("application/json"));
}

#[tokio::test]
async fn poll_pending_slow_down_then_success_saves_tokens_privately() {
    let f = fixture().await;
    f.device_code_answer(5);
    f.user_answer();
    let answers = Arc::new(Mutex::new(std::collections::VecDeque::from(vec![
        json!({"error": "authorization_pending"}),
        json!({"error": "slow_down", "interval": 10}),
        json!({"error": "slow_down"}), // no interval given: +5
        token_body(
            "ghu_test-first-access-token",
            "ghr_test-first-refresh",
            28800,
        ),
    ])));
    f.gh.on("POST", "/login/oauth/access_token", move |_| {
        (200, answers.lock().unwrap().pop_front().unwrap())
    });
    let s = f.svc.start_device().await.unwrap();

    // Too early: GitHub isn't asked.
    assert_eq!(f.svc.poll(&s.flow_id).await.state, "pending");
    assert_eq!(f.gh.count("/login/oauth/access_token"), 0);

    f.clock.advance(5);
    assert_eq!(f.svc.poll(&s.flow_id).await.state, "pending");
    let form = f.gh.last().body;
    assert!(form.contains("device_code=dc-123"));
    assert!(form.contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code"));

    f.clock.advance(5);
    let slowed = f.svc.poll(&s.flow_id).await;
    assert_eq!((slowed.state.as_str(), slowed.interval), ("pending", 10));
    f.clock.advance(5); // under the new interval: not asked
    assert_eq!(f.svc.poll(&s.flow_id).await.state, "pending");
    assert_eq!(f.gh.count("/login/oauth/access_token"), 2);

    f.clock.advance(5);
    assert_eq!(f.svc.poll(&s.flow_id).await.interval, 15);

    f.clock.advance(15);
    let done = f.svc.poll(&s.flow_id).await;
    assert_eq!(done.state, "done");
    assert_eq!(done.login.as_deref(), Some("octo"));
    assert_eq!(f.svc.poll(&s.flow_id).await.state, "done"); // a repeat poll doesn't ask again
    assert_eq!(f.gh.count("/login/oauth/access_token"), 4);

    let account = f.svc.account().unwrap();
    assert_eq!(account.access_token, "ghu_test-first-access-token");
    assert_eq!(
        account.access_expires,
        Some(f.clock.now() + chrono::Duration::seconds(28800))
    );
    assert_eq!(account.login.as_deref(), Some("octo"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(f.account_file())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    assert!(f.account_file().is_file());
    assert_eq!(
        f.svc.access_token(false).await.unwrap(),
        "ghu_test-first-access-token"
    ); // fresh: no refresh
}

#[tokio::test]
async fn poll_end_states() {
    for (error, state) in [
        ("expired_token", "expired"),
        ("token_expired", "expired"),
        ("access_denied", "denied"),
        ("device_flow_disabled", "error"),
    ] {
        let f = fixture().await;
        f.device_code_answer(1);
        f.gh.on("POST", "/login/oauth/access_token", move |_| {
            (200, json!({"error": error}))
        });
        let s = f.svc.start_device().await.unwrap();
        f.clock.advance(1);
        let p = f.svc.poll(&s.flow_id).await;
        assert_eq!(p.state, state, "{error}");
        assert!(p.error.is_some());
        assert!(f.svc.account().is_none());
        f.clock.advance(5);
        assert_eq!(f.svc.poll(&s.flow_id).await.state, state);
        assert_eq!(f.gh.count("/login/oauth/access_token"), 1);
    }
}

#[tokio::test]
async fn poll_after_the_code_expires_is_expired_without_asking_github() {
    let f = fixture().await;
    f.device_code_answer(5);
    let s = f.svc.start_device().await.unwrap();
    f.clock.advance(901);
    assert_eq!(f.svc.poll(&s.flow_id).await.state, "expired");
    assert_eq!(f.svc.poll("no-such-flow").await.state, "expired");
    assert_eq!(f.gh.count("/login/oauth/access_token"), 0);
}

#[tokio::test]
async fn refresh_near_expiry_rotates_both_tokens_once_even_with_concurrent_callers() {
    let f = fixture().await;
    f.sign_in_default();
    let refreshes = Arc::new(Mutex::new(0));
    let r2 = refreshes.clone();
    f.gh.on("POST", "/login/oauth/access_token", move |r| {
        *r2.lock().unwrap() += 1;
        assert!(r.body.contains("grant_type=refresh_token"));
        assert!(r.body.contains("refresh_token=ghr_OLDrefresh"));
        assert!(r.body.contains(&format!("client_id={CLIENT_ID}")));
        assert!(!r.body.contains("client_secret")); // device-flow tokens refresh without one
        (
            200,
            token_body("ghu_test-new-access-token", "ghr_NEWrefresh", 28800),
        )
    });
    assert_eq!(
        f.svc.access_token(false).await.unwrap(),
        "ghu_test-old-access-token"
    );
    f.clock.advance(28800 - 200); // within 5 minutes of expiry
    let tokens = futures_util::future::join_all((0..5).map(|_| f.svc.access_token(false))).await;
    for t in tokens {
        assert_eq!(t.unwrap(), "ghu_test-new-access-token");
    }
    assert_eq!(*refreshes.lock().unwrap(), 1);
    let a = f.svc.account().unwrap();
    assert_eq!(a.refresh_token.as_deref(), Some("ghr_NEWrefresh"));
    assert_eq!(a.login.as_deref(), Some("octo")); // kept across the refresh
    assert_eq!(
        a.access_expires,
        Some(f.clock.now() + chrono::Duration::seconds(28800))
    );
}

#[tokio::test]
async fn refresh_with_a_bad_refresh_token_signs_out() {
    let f = fixture().await;
    f.sign_in("ghu_test-old-access-token", "ghr_OLDrefresh", 60);
    f.gh.on("POST", "/login/oauth/access_token", |_| {
        (200, json!({"error": "bad_refresh_token"}))
    });
    match f.svc.access_token(false).await {
        Err(GitHubAppError::Auth(m)) => assert!(m.contains("Sign in again"), "{m}"),
        other => panic!("{other:?}"),
    }
    assert!(!f.account_file().exists());
    assert!(matches!(
        f.svc.access_token(false).await,
        Err(GitHubAppError::Auth(_))
    ));
}

#[tokio::test]
async fn api_401_refreshes_once_and_retries() {
    let f = fixture().await;
    f.sign_in_default();
    f.gh.on("POST", "/login/oauth/access_token", |_| {
        (
            200,
            token_body("ghu_test-new-access-token", "ghr_NEWrefresh", 28800),
        )
    });
    f.gh.on("GET", "/api/user/installations?per_page=100&page=1", |r| {
        if r.bearer.as_deref() == Some("ghu_test-new-access-token") {
            (200, json!({"total_count": 0, "installations": []}))
        } else {
            (401, json!({"message": "Bad credentials"}))
        }
    });
    let page = f.svc.repos(None, 1, 30, false).await.unwrap();
    assert!(page.installations.is_empty());
    assert_eq!(
        f.svc.account().unwrap().access_token,
        "ghu_test-new-access-token"
    );
}

fn repo(base: &str, owner: &str, i: i64) -> Value {
    let pushed = chrono::NaiveDate::from_ymd_opt(2026, 1, 1)
        .unwrap()
        .and_hms_opt(0, 0, 0)
        .unwrap()
        + chrono::Duration::hours(i);
    json!({
        "full_name": format!("{owner}/repo-{i:03}"), "name": format!("repo-{i:03}"), "owner": {"login": owner},
        "private": i % 2 == 0, "description": if i == 7 { Some("The special one") } else { None },
        // DateTime.ToString("o") of an unspecified-kind time: no offset.
        "pushed_at": format!("{}0", pushed.format("%Y-%m-%dT%H:%M:%S%.6f")),
        "default_branch": "main", "html_url": format!("{base}/{owner}/repo-{i:03}"),
    })
}

#[tokio::test]
async fn repos_follow_githubs_pages_then_search_and_page_locally() {
    let f = fixture().await;
    f.sign_in_default();
    f.gh.on("GET", "/api/user/installations?per_page=100&page=1", |_| {
        (
            200,
            json!({"total_count": 2, "installations": [
                {"id": 11, "account": {"login": "octo", "type": "User"}},
                {"id": 22, "account": {"login": "acme", "type": "Organization"}},
            ]}),
        )
    });
    let b = f.base.clone();
    f.gh.on("GET", "/api/user/installations/11/repositories?per_page=100&page=1", move |_| {
        (200, json!({"total_count": 130, "repositories": (0..100).map(|i| repo(&b, "octo", i)).collect::<Vec<_>>()}))
    });
    let b = f.base.clone();
    f.gh.on("GET", "/api/user/installations/11/repositories?per_page=100&page=2", move |_| {
        (200, json!({"total_count": 130, "repositories": (100..130).map(|i| repo(&b, "octo", i)).collect::<Vec<_>>()}))
    });
    let b = f.base.clone();
    f.gh.on(
        "GET",
        "/api/user/installations/22/repositories?per_page=100&page=1",
        move |_| {
            (
                200,
                json!({"total_count": 1, "repositories": [repo(&b, "acme", 500)]}),
            )
        },
    );

    let first = f.svc.repos(None, 1, 30, false).await.unwrap();
    assert_eq!(first.total, 131);
    assert_eq!(first.items.len(), 30);
    assert_eq!(first.items[0].full_name, "acme/repo-500"); // newest first
    assert_eq!(
        first
            .installations
            .iter()
            .map(|i| i.account.as_str())
            .collect::<Vec<_>>(),
        ["octo", "acme"]
    );
    let last = f.svc.repos(None, 5, 30, false).await.unwrap();
    assert_eq!(last.items.len(), 11);
    let l = last.items.last().unwrap();
    assert_eq!(l.full_name, "octo/repo-000");
    assert!(l.private);
    assert_eq!(l.default_branch.as_deref(), Some("main"));

    let found = f.svc.repos(Some("special"), 1, 30, false).await.unwrap();
    assert_eq!(found.items.len(), 1);
    assert_eq!(found.items[0].full_name, "octo/repo-007");
    assert_eq!(
        f.svc
            .repos(Some("octo 12"), 1, 30, false)
            .await
            .unwrap()
            .total,
        12
    ); // 012, 112, 120-129

    // Cached for a minute (no new GitHub calls), then read again; reload forces it.
    let calls = f.gh.seen();
    f.svc.repos(None, 2, 30, false).await.unwrap();
    assert_eq!(f.gh.seen(), calls);
    f.svc.repos(None, 1, 30, true).await.unwrap();
    assert_eq!(f.gh.seen(), calls + 4);
    f.clock.advance(61);
    f.svc.repos(None, 1, 30, false).await.unwrap();
    assert_eq!(f.gh.seen(), calls + 8);
    for r in f.gh.requests.lock().unwrap().iter() {
        if r.url.starts_with("/api") {
            assert_eq!(r.bearer.as_deref(), Some("ghu_test-old-access-token"));
        }
    }
    // The JSON the client reads.
    let v = serde_json::to_value(&last.items[0]).unwrap();
    for key in [
        "fullName",
        "owner",
        "name",
        "private",
        "description",
        "updatedAt",
        "defaultBranch",
        "htmlUrl",
    ] {
        assert!(v.get(key).is_some(), "{key}");
    }
    assert_eq!(
        serde_json::to_value(&first.installations[1]).unwrap(),
        json!({"id": 22, "account": "acme", "type": "Organization"})
    );
}

#[tokio::test]
async fn branches_default_first_and_repo_names_are_checked() {
    let f = fixture().await;
    f.sign_in_default();
    f.gh.on("GET", "/api/repos/acme/demo", |_| {
        (
            200,
            json!({"full_name": "acme/demo", "default_branch": "trunk"}),
        )
    });
    f.gh.on(
        "GET",
        "/api/repos/acme/demo/branches?per_page=100&page=1",
        |_| {
            (
                200,
                json!([{"name": "zeta"}, {"name": "alpha"}, {"name": "trunk"}]),
            )
        },
    );
    let b = f.svc.branches("acme/demo").await.unwrap();
    assert_eq!(b.default_branch.as_deref(), Some("trunk"));
    assert_eq!(b.branches, ["trunk", "alpha", "zeta"]);
    for bad in [
        "acme",
        "../x/y",
        "acme/demo/extra",
        "-x/y",
        "acme/..",
        "acme/de mo",
    ] {
        assert!(matches!(
            f.svc.branches(bad).await,
            Err(GitHubAppError::Argument(_))
        ));
    }
    assert_eq!(
        f.svc.clone_url("acme/demo").unwrap(),
        format!("{}/acme/demo.git", f.base)
    );
}

#[tokio::test]
async fn sign_out_deletes_the_tokens() {
    let f = fixture().await;
    f.sign_in_default();
    assert!(f.account_file().is_file());
    assert!(f.svc.sign_out());
    assert!(!f.account_file().exists());
    assert!(f.svc.account().is_none());
    assert!(!f.svc.sign_out());
    assert!(matches!(
        f.svc.access_token(false).await,
        Err(GitHubAppError::Auth(_))
    ));
}

#[tokio::test]
async fn not_configured_says_so() {
    let f = fixture().await;
    let svc = service(&f.dir, &f.clock, &f.base, GitHubAppConfig::default());
    assert!(svc.install_url().is_none());
    match svc.start_device().await {
        Err(GitHubAppError::Invalid(m)) => assert!(m.contains("isn't configured")),
        other => panic!("{other:?}"),
    }
    assert_eq!(f.gh.seen(), 0);
}

#[test]
fn config_comes_from_the_environment_then_the_file_and_is_checked() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(GitHubAppConfig::FILE_NAME),
        "{\"clientId\": \"Iv23liFILE\", \"slug\": \"from-file\"}",
    )
    .unwrap();
    // The environment's values are passed in (tests never set process variables).
    assert_eq!(
        GitHubAppConfig::load_from(None, None, dir.path()),
        GitHubAppConfig::new(Some("Iv23liFILE"), Some("from-file"))
    );
    assert_eq!(
        GitHubAppConfig::load_from(Some("Iv23liENV".into()), None, dir.path()),
        GitHubAppConfig::new(Some("Iv23liENV"), Some("from-file"))
    );
    assert_eq!(
        GitHubAppConfig::load_from(
            Some("Iv23liENV".into()),
            Some("bad/slug?x=1".into()),
            dir.path()
        )
        .slug,
        None
    );
    assert_eq!(
        GitHubAppConfig::load_from(Some("Iv23liENV".into()), None, &dir.path().join("none"))
            .client_id
            .as_deref(),
        Some("Iv23liENV")
    );
}
