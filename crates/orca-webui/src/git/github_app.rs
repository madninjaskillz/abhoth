//! `GitHubApp.cs`: the GitHub App the "Clone from GitHub" wizard signs in with (OAuth device
//! flow), the signed-in account's tokens (`github-account.json`, mode 600, written atomically),
//! token refresh (5-minute margin, one refresh at a time), and the repositories / branches the
//! app's installations reach. Tokens stay on the server: only [`GitHubAppService::access_token`]
//! returns one, and its callers hand it to git through the credential helper.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use chrono::{DateTime, FixedOffset, SecondsFormat, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::git::tokens::write_secret;

/// What the service reads the time from (tests move it by hand).
pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

/// The system clock.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// A clock that only moves when told to (tests).
pub struct ManualClock(Mutex<DateTime<Utc>>);

impl ManualClock {
    pub fn new(start: DateTime<Utc>) -> Self {
        Self(Mutex::new(start))
    }

    pub fn advance(&self, seconds: i64) {
        let mut t = self.0.lock().unwrap();
        *t += chrono::Duration::seconds(seconds);
    }
}

impl Clock for ManualClock {
    fn now(&self) -> DateTime<Utc> {
        *self.0.lock().unwrap()
    }
}

/// The GitHub App's client ID and slug. Neither is a secret: the device flow needs only the
/// client ID. Read from `ABHOTH_GITHUB_APP_CLIENT_ID` / `ABHOTH_GITHUB_APP_SLUG` (then the
/// `OPENORCA_*` names), else from `github-app.json` in the server's state folder
/// (`{"clientId": "Iv23…", "slug": "my-app"}`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GitHubAppConfig {
    pub client_id: Option<String>,
    pub slug: Option<String>,
}

/// Gives the app config, asked on each use (a changed github-app.json applies without a restart).
pub type GitHubAppConfigFn = Arc<dyn Fn() -> GitHubAppConfig + Send + Sync>;

impl GitHubAppConfig {
    pub const CLIENT_ID_VARIABLE: &'static str = "ABHOTH_GITHUB_APP_CLIENT_ID";
    pub const SLUG_VARIABLE: &'static str = "ABHOTH_GITHUB_APP_SLUG";
    pub const FILE_NAME: &'static str = "github-app.json";

    pub fn new(client_id: Option<&str>, slug: Option<&str>) -> Self {
        Self {
            client_id: client_id.map(str::to_string),
            slug: slug.map(str::to_string),
        }
    }

    pub fn configured(&self) -> bool {
        self.client_id
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty())
    }

    /// The environment first, then `<directory>/github-app.json`.
    pub fn load(directory: &Path) -> Self {
        Self::load_from(
            orca_core::brand::env_var("GITHUB_APP_CLIENT_ID"),
            orca_core::brand::env_var("GITHUB_APP_SLUG"),
            directory,
        )
    }

    /// [`Self::load`] with the environment's values given.
    pub fn load_from(id: Option<String>, slug: Option<String>, directory: &Path) -> Self {
        let blank = |s: &Option<String>| s.as_deref().is_none_or(|v| v.trim().is_empty());
        let (mut id, mut slug) = (id, slug);
        let file = directory.join(Self::FILE_NAME);
        if (blank(&id) || blank(&slug))
            && file.is_file()
            && let Ok(text) = std::fs::read_to_string(&file)
            && let Ok(doc) = serde_json::from_str::<Value>(text.trim_start_matches('\u{feff}'))
        {
            let s = |name: &str| doc.get(name).and_then(Value::as_str).map(str::to_string);
            if blank(&id) {
                id = s("clientId");
            }
            if blank(&slug) {
                slug = s("slug");
            }
        }
        static ID: OnceLock<Regex> = OnceLock::new();
        static SLUG: OnceLock<Regex> = OnceLock::new();
        // Both end up in URLs: keep them to the characters GitHub uses.
        let id = id
            .filter(|v| !v.trim().is_empty())
            .map(|v| v.trim().to_string())
            .filter(|v| {
                ID.get_or_init(|| Regex::new(r"^[A-Za-z0-9._-]{1,100}$").unwrap())
                    .is_match(v)
            });
        let slug = slug
            .filter(|v| !v.trim().is_empty())
            .map(|v| v.trim().to_string())
            .filter(|v| {
                SLUG.get_or_init(|| Regex::new(r"^[A-Za-z0-9-]{1,100}$").unwrap())
                    .is_match(v)
            });
        Self {
            client_id: id,
            slug,
        }
    }
}

mod iso {
    //! DateTimeOffset as System.Text.Json writes it ("2026-11-24T09:00:00+00:00").
    use super::*;
    use serde::{Deserializer, Serializer};

    pub fn text(t: &DateTime<Utc>) -> String {
        t.to_rfc3339_opts(SecondsFormat::AutoSi, false)
    }

    pub fn parse(s: &str) -> Option<DateTime<Utc>> {
        DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|d| d.with_timezone(&Utc))
    }

    pub fn ser<S: Serializer>(t: &DateTime<Utc>, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&text(t))
    }

    pub fn de<'de, D: Deserializer<'de>>(d: D) -> Result<DateTime<Utc>, D::Error> {
        let s = String::deserialize(d)?;
        parse(&s).ok_or_else(|| serde::de::Error::custom("not a date"))
    }

    pub fn ser_opt<S: Serializer>(t: &Option<DateTime<Utc>>, s: S) -> Result<S::Ok, S::Error> {
        match t {
            Some(t) => s.serialize_str(&text(t)),
            None => s.serialize_none(),
        }
    }

    pub fn de_opt<'de, D: Deserializer<'de>>(d: D) -> Result<Option<DateTime<Utc>>, D::Error> {
        Ok(Option::<String>::deserialize(d)?.and_then(|s| parse(&s)))
    }
}

/// The signed-in GitHub user: tokens (server only), their expiry, and who it is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitHubAccount {
    #[serde(default)]
    pub access_token: String,
    #[serde(
        default,
        serialize_with = "iso::ser_opt",
        deserialize_with = "iso::de_opt"
    )]
    pub access_expires: Option<DateTime<Utc>>,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(
        default,
        serialize_with = "iso::ser_opt",
        deserialize_with = "iso::de_opt"
    )]
    pub refresh_expires: Option<DateTime<Utc>>,
    #[serde(default)]
    pub login: Option<String>,
    #[serde(default)]
    pub avatar_url: Option<String>,
    #[serde(serialize_with = "iso::ser", deserialize_with = "iso::de")]
    pub signed_in_at: DateTime<Utc>,
}

/// The GitHub sign-in's tokens, in `<state>/github-account.json` (mode 600, written atomically).
pub struct GitHubAccountStore {
    path: PathBuf,
    lock: Mutex<()>,
}

impl GitHubAccountStore {
    pub fn new(directory: &Path) -> Self {
        Self {
            path: directory.join("github-account.json"),
            lock: Mutex::new(()),
        }
    }

    pub fn file_path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> Option<GitHubAccount> {
        let _g = self.lock.lock().unwrap();
        let text = std::fs::read_to_string(&self.path).ok()?;
        let a: GitHubAccount = serde_json::from_str(text.trim_start_matches('\u{feff}')).ok()?;
        (!a.access_token.is_empty()).then_some(a)
    }

    pub fn save(&self, account: &GitHubAccount) -> std::io::Result<()> {
        let _g = self.lock.lock().unwrap();
        write_secret(
            &self.path,
            &serde_json::to_string(account).unwrap_or_default(),
        )
    }

    /// Whether there was a file to delete.
    pub fn delete(&self) -> bool {
        let _g = self.lock.lock().unwrap();
        let existed = self.path.is_file();
        let _ = std::fs::remove_file(&self.path);
        existed
    }
}

/// Why a GitHub App call failed; each message is for the user.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GitHubAppError {
    /// Not signed in, or the sign-in can't be refreshed any more (C# `GitHubAuthException`).
    #[error("{0}")]
    Auth(String),
    /// A bad argument, e.g. a repository name (C# `ArgumentException`).
    #[error("{0}")]
    Argument(String),
    /// Not configured, or GitHub refused (C# `InvalidOperationException`).
    #[error("{0}")]
    Invalid(String),
    /// GitHub could not be reached (C# `HttpRequestException`).
    #[error("{0}")]
    Http(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitHubDeviceStart {
    pub flow_id: String,
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in: i64,
    pub interval: i64,
}

/// A poll of a device flow: pending, done, expired, denied or error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitHubDevicePoll {
    pub state: String,
    pub interval: i64,
    pub login: Option<String>,
    pub error: Option<String>,
}

impl GitHubDevicePoll {
    fn new(state: &str, interval: i64) -> Self {
        Self {
            state: state.into(),
            interval,
            login: None,
            error: None,
        }
    }

    fn failed(state: &str, interval: i64, error: impl Into<String>) -> Self {
        Self {
            error: Some(error.into()),
            ..Self::new(state, interval)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitHubRepo {
    pub full_name: String,
    pub owner: String,
    pub name: String,
    pub private: bool,
    pub description: Option<String>,
    #[serde(serialize_with = "ser_offset")]
    pub updated_at: Option<DateTime<FixedOffset>>,
    pub default_branch: Option<String>,
    pub html_url: Option<String>,
}

fn ser_offset<S: serde::Serializer>(
    t: &Option<DateTime<FixedOffset>>,
    s: S,
) -> Result<S::Ok, S::Error> {
    match t {
        Some(t) => s.serialize_str(&t.to_rfc3339_opts(SecondsFormat::AutoSi, false)),
        None => s.serialize_none(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GitHubInstallation {
    pub id: i64,
    pub account: String,
    #[serde(rename = "type")]
    pub kind: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitHubRepoPage {
    pub total: usize,
    pub page: usize,
    pub per_page: usize,
    pub items: Vec<GitHubRepo>,
    pub installations: Vec<GitHubInstallation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitHubBranches {
    pub default_branch: Option<String>,
    pub branches: Vec<String>,
    pub truncated: bool,
}

struct FlowState {
    interval: i64,
    next_poll: DateTime<Utc>,
    result: Option<GitHubDevicePoll>,
    finished_at: Option<DateTime<Utc>>,
}

struct Flow {
    device_code: String,
    expires_at: DateTime<Utc>,
    state: tokio::sync::Mutex<FlowState>,
}

struct RepoCache {
    at: DateTime<Utc>,
    repos: Vec<GitHubRepo>,
    installations: Vec<GitHubInstallation>,
}

const MAX_PAGES: usize = 20;
const REFRESH_MARGIN_SECS: i64 = 5 * 60;
const REPO_CACHE_SECS: i64 = 60;
/// A finished flow answers repeat polls for this long, then is forgotten (C#: one minute).
const FINISHED_FLOW_SECS: i64 = 60;

type Answer = (u16, Option<Value>);

/// Sign in with a GitHub App through the OAuth device flow, keep the user's token fresh (GitHub
/// App user tokens last about 8 hours; the refresh token is rotated on each use), list the
/// repositories the app's installations can reach and their branches.
pub struct GitHubAppService {
    config: GitHubAppConfigFn,
    web: String,
    api: String,
    store: GitHubAccountStore,
    http: reqwest::Client,
    clock: Arc<dyn Clock>,
    refresh: tokio::sync::Mutex<()>,
    flows: Mutex<HashMap<String, Arc<Flow>>>,
    repo_cache: Mutex<Option<Arc<RepoCache>>>,
}

impl GitHubAppService {
    /// `web_base` is github.com (device flow, token endpoint, clone URLs), `api_base`
    /// api.github.com; tests point both at a fake.
    pub fn new(
        config: GitHubAppConfigFn,
        web_base: &str,
        api_base: &str,
        store: GitHubAccountStore,
        clock: Option<Arc<dyn Clock>>,
    ) -> Self {
        Self {
            config,
            web: web_base.trim_end_matches('/').to_string(),
            api: api_base.trim_end_matches('/').to_string(),
            store,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_default(),
            clock: clock.unwrap_or_else(|| Arc::new(SystemClock)),
            refresh: tokio::sync::Mutex::new(()),
            flows: Mutex::new(HashMap::new()),
            repo_cache: Mutex::new(None),
        }
    }

    pub fn config(&self) -> GitHubAppConfig {
        (self.config)()
    }

    pub fn web_base(&self) -> &str {
        &self.web
    }

    pub fn store(&self) -> &GitHubAccountStore {
        &self.store
    }

    /// Where the user installs the app on an account or organization (`None` without a slug).
    pub fn install_url(&self) -> Option<String> {
        self.config()
            .slug
            .map(|s| format!("{}/apps/{s}/installations/new", self.web))
    }

    /// The HTTPS clone URL of "owner/name" (no credentials: git gets them from the helper).
    pub fn clone_url(&self, full_name: &str) -> Result<String, GitHubAppError> {
        Ok(format!("{}/{}.git", self.web, check_repo(full_name)?))
    }

    pub fn account(&self) -> Option<GitHubAccount> {
        self.store.load()
    }

    fn now(&self) -> DateTime<Utc> {
        self.clock.now()
    }

    // ---- device flow ----

    /// Asks GitHub for a device code; the user enters the user code at the verification URI.
    pub async fn start_device(&self) -> Result<GitHubDeviceStart, GitHubAppError> {
        let id = self.require_client_id()?;
        let (status, body) = self
            .post_form("/login/device/code", &[("client_id", &id)])
            .await?;
        if let Some(err) = str_of(&body, "error") {
            return Err(GitHubAppError::Invalid(error_text(
                &err,
                str_of(&body, "error_description").as_deref(),
            )));
        }
        let (Some(device_code), Some(user_code)) =
            (str_of(&body, "device_code"), str_of(&body, "user_code"))
        else {
            return Err(self.not_started(status));
        };
        if status != 200 {
            return Err(self.not_started(status));
        }
        let interval = int_of(&body, "interval").unwrap_or(5).max(1);
        let expires_in = int_of(&body, "expires_in").unwrap_or(900);
        let now = self.now();
        let flow_id = crate::util::hex_lower(&crate::util::random_bytes(16));
        let flow = Arc::new(Flow {
            device_code,
            expires_at: now + chrono::Duration::seconds(expires_in),
            state: tokio::sync::Mutex::new(FlowState {
                interval,
                next_poll: now + chrono::Duration::seconds(interval),
                result: None,
                finished_at: None,
            }),
        });
        {
            let mut flows = self.flows.lock().unwrap();
            flows.retain(|_, f| f.expires_at >= now);
            flows.insert(flow_id.clone(), flow);
        }
        Ok(GitHubDeviceStart {
            flow_id,
            user_code,
            verification_uri: str_of(&body, "verification_uri")
                .unwrap_or_else(|| format!("{}/login/device", self.web)),
            expires_in,
            interval,
        })
    }

    fn not_started(&self, status: u16) -> GitHubAppError {
        GitHubAppError::Invalid(format!(
            "GitHub didn't start the sign-in (HTTP {status}). Check the app's client ID."
        ))
    }

    /// One poll of a device flow. GitHub is asked at most once per interval (an early poll just
    /// answers "pending"); slow_down raises the interval as GitHub says (or by 5 s).
    pub async fn poll(&self, flow_id: &str) -> GitHubDevicePoll {
        let now = self.now();
        let flow = {
            let mut flows = self.flows.lock().unwrap();
            // Finished flows answer repeat polls for a minute, then are forgotten.
            flows.retain(|_, f| {
                f.state.try_lock().map_or(true, |s| {
                    s.finished_at
                        .is_none_or(|t| now - t < chrono::Duration::seconds(FINISHED_FLOW_SECS))
                })
            });
            flows.get(flow_id).cloned()
        };
        let Some(flow) = flow else {
            return GitHubDevicePoll::failed("expired", 5, "That sign-in has ended. Start again.");
        };
        let mut st = flow.state.lock().await;
        if now >= flow.expires_at {
            self.flows.lock().unwrap().remove(flow_id);
            return GitHubDevicePoll::failed(
                "expired",
                st.interval,
                "The code expired before it was entered. Start again.",
            );
        }
        if let Some(done) = &st.result {
            return done.clone();
        }
        if now < st.next_poll {
            return GitHubDevicePoll::new("pending", st.interval);
        }
        let id = match self.require_client_id() {
            Ok(id) => id,
            Err(e) => return GitHubDevicePoll::failed("error", st.interval, e.to_string()),
        };
        let answer = self
            .post_form(
                "/login/oauth/access_token",
                &[
                    ("client_id", &id),
                    ("device_code", &flow.device_code),
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ],
            )
            .await;
        st.next_poll = self.now() + chrono::Duration::seconds(st.interval);
        let (status, body) = match answer {
            Ok(a) => a,
            // A network blip: try again next interval.
            Err(_) => return GitHubDevicePoll::new("pending", st.interval),
        };
        let finish = |st: &mut FlowState, result: GitHubDevicePoll| {
            st.result = Some(result.clone());
            st.finished_at = Some(self.now());
            result
        };
        if str_of(&body, "access_token").is_some_and(|t| !t.is_empty()) {
            let mut account = self.to_account(body.as_ref().unwrap(), None);
            self.identify(&mut account).await;
            let _ = self.store.save(&account);
            *self.repo_cache.lock().unwrap() = None;
            let interval = st.interval;
            return finish(
                &mut st,
                GitHubDevicePoll {
                    login: account.login.clone(),
                    ..GitHubDevicePoll::new("done", interval)
                },
            );
        }
        let interval = st.interval;
        match str_of(&body, "error").as_deref() {
            Some("authorization_pending") => GitHubDevicePoll::new("pending", interval),
            Some("slow_down") => {
                st.interval = int_of(&body, "interval")
                    .unwrap_or(interval + 5)
                    .max(interval + 1);
                st.next_poll = self.now() + chrono::Duration::seconds(st.interval);
                GitHubDevicePoll::new("pending", st.interval)
            }
            Some(
                "expired_token"
                | "token_expired"
                | "incorrect_device_code"
                | "bad_verification_code",
            ) => finish(
                &mut st,
                GitHubDevicePoll::failed(
                    "expired",
                    interval,
                    "The code expired before it was entered. Start again.",
                ),
            ),
            Some("access_denied") => finish(
                &mut st,
                GitHubDevicePoll::failed(
                    "denied",
                    interval,
                    "The sign-in was cancelled on GitHub.",
                ),
            ),
            Some(other) => {
                let text = error_text(other, str_of(&body, "error_description").as_deref());
                finish(&mut st, GitHubDevicePoll::failed("error", interval, text))
            }
            None if status >= 500 => GitHubDevicePoll::new("pending", interval),
            None => GitHubDevicePoll::failed(
                "error",
                interval,
                format!("Unexpected answer from GitHub (HTTP {status})."),
            ),
        }
    }

    // ---- tokens ----

    /// The signed-in user's access token, refreshed first when it expires within 5 minutes (or
    /// `force`). [`GitHubAppError::Auth`] when not signed in or the refresh token is no longer
    /// good (the account is then signed out).
    pub async fn access_token(&self, force: bool) -> Result<String, GitHubAppError> {
        let account = self.store.load().ok_or_else(|| {
            GitHubAppError::Auth(
                "Not signed in to GitHub. Sign in from the \"Clone from GitHub\" wizard.".into(),
            )
        })?;
        let fresh = |exp: Option<DateTime<Utc>>| {
            exp.is_none_or(|e| e - self.now() > chrono::Duration::seconds(REFRESH_MARGIN_SECS))
        };
        if !force && fresh(account.access_expires) {
            return Ok(account.access_token);
        }
        let _one = self.refresh.lock().await;
        // Another caller may have refreshed while this one waited (refresh tokens are single-use).
        let current = self
            .store
            .load()
            .ok_or_else(|| GitHubAppError::Auth("Not signed in to GitHub.".into()))?;
        if current.access_token != account.access_token {
            return Ok(current.access_token);
        }
        let still_valid = |a: &GitHubAccount| a.access_expires.is_some_and(|e| e > self.now());
        let refresh = match current.refresh_token.as_deref() {
            Some(r) if !r.is_empty() && current.refresh_expires.is_none_or(|e| e > self.now()) => {
                r.to_string()
            }
            _ => {
                if still_valid(&current) && !force {
                    return Ok(current.access_token);
                }
                self.store.delete();
                return Err(GitHubAppError::Auth(
                    "The GitHub sign-in has expired. Sign in again.".into(),
                ));
            }
        };
        let id = self.require_client_id()?;
        let (status, body) = match self
            .post_form(
                "/login/oauth/access_token",
                &[
                    ("client_id", &id),
                    ("grant_type", "refresh_token"),
                    ("refresh_token", &refresh),
                ],
            )
            .await
        {
            Ok(a) => a,
            // GitHub unreachable, but the token still has a few minutes.
            Err(_) if still_valid(&current) => return Ok(current.access_token),
            Err(e) => return Err(e),
        };
        if str_of(&body, "access_token").is_some_and(|t| !t.is_empty()) {
            let renewed = self.to_account(body.as_ref().unwrap(), Some(&current));
            let _ = self.store.save(&renewed);
            return Ok(renewed.access_token);
        }
        let error = str_of(&body, "error");
        if matches!(
            error.as_deref(),
            Some("bad_refresh_token" | "unauthorized_client" | "incorrect_client_credentials")
        ) {
            self.store.delete();
            return Err(GitHubAppError::Auth(
                "The GitHub sign-in has expired or was revoked. Sign in again.".into(),
            ));
        }
        if still_valid(&current) {
            return Ok(current.access_token);
        }
        Err(GitHubAppError::Auth(format!(
            "Couldn't refresh the GitHub sign-in: {}",
            error_text(
                &error.unwrap_or_else(|| format!("HTTP {status}")),
                str_of(&body, "error_description").as_deref()
            )
        )))
    }

    /// Deletes the stored tokens (revoking is done on GitHub, under Settings -> Applications).
    pub fn sign_out(&self) -> bool {
        *self.repo_cache.lock().unwrap() = None;
        self.store.delete()
    }

    fn to_account(&self, body: &Value, previous: Option<&GitHubAccount>) -> GitHubAccount {
        let now = self.now();
        let b = Some(body.clone());
        let expires_in = int_of(&b, "expires_in");
        GitHubAccount {
            access_token: str_of(&b, "access_token").unwrap_or_default(),
            access_expires: expires_in.map(|e| now + chrono::Duration::seconds(e)),
            // A refresh answer always carries a new refresh token (the old one is spent).
            refresh_token: str_of(&b, "refresh_token").or_else(|| {
                expires_in.and_then(|_| previous.and_then(|p| p.refresh_token.clone()))
            }),
            refresh_expires: int_of(&b, "refresh_token_expires_in")
                .map(|r| now + chrono::Duration::seconds(r))
                .or_else(|| previous.and_then(|p| p.refresh_expires)),
            login: previous.and_then(|p| p.login.clone()),
            avatar_url: previous.and_then(|p| p.avatar_url.clone()),
            signed_in_at: previous.map_or(now, |p| p.signed_in_at),
        }
    }

    async fn identify(&self, account: &mut GitHubAccount) {
        if let Ok((200, user)) = self.send_api(&account.access_token, "/user").await {
            account.login = str_of(&user, "login");
            account.avatar_url = str_of(&user, "avatar_url");
        }
    }

    // ---- repositories ----

    /// The repositories the user's installations of the app can reach, newest first, filtered
    /// by `query` (owner/name or description) and cut into pages. GitHub's own pages are all
    /// read (up to 20 per installation) and kept for a minute; `reload` reads them again.
    pub async fn repos(
        &self,
        query: Option<&str>,
        page: i64,
        per_page: i64,
        reload: bool,
    ) -> Result<GitHubRepoPage, GitHubAppError> {
        let per_page = per_page.clamp(1, 100) as usize;
        let page = page.max(1) as usize;
        let cached = self.repo_cache.lock().unwrap().clone();
        let cache = match cached {
            Some(c)
                if !reload && self.now() - c.at <= chrono::Duration::seconds(REPO_CACHE_SECS) =>
            {
                c
            }
            _ => {
                let mut installations = Vec::new();
                self.pages("/user/installations", "installations", |el| {
                    if let Some(id) = el.get("id").and_then(Value::as_i64) {
                        let acc = el.get("account").map(|a| Some(a.clone()));
                        installations.push(GitHubInstallation {
                            id,
                            account: acc
                                .as_ref()
                                .and_then(|a| str_of(a, "login"))
                                .unwrap_or_else(|| "?".into()),
                            kind: acc.as_ref().and_then(|a| str_of(a, "type")),
                        });
                    }
                })
                .await?;
                let mut repos: Vec<GitHubRepo> = Vec::new();
                let mut index: HashMap<String, usize> = HashMap::new();
                for inst in &installations {
                    let path = format!("/user/installations/{}/repositories", inst.id);
                    self.pages(&path, "repositories", |el| {
                        let e = Some(el.clone());
                        let Some(full) = str_of(&e, "full_name") else {
                            return;
                        };
                        let owner = el
                            .get("owner")
                            .and_then(|o| str_of(&Some(o.clone()), "login"));
                        let updated = str_of(&e, "pushed_at")
                            .or_else(|| str_of(&e, "updated_at"))
                            .and_then(|s| parse_offset(&s));
                        let repo = GitHubRepo {
                            owner: owner
                                .unwrap_or_else(|| full.split('/').next().unwrap_or("").into()),
                            name: str_of(&e, "name")
                                .unwrap_or_else(|| full.rsplit('/').next().unwrap_or("").into()),
                            private: el.get("private") == Some(&Value::Bool(true)),
                            description: str_of(&e, "description"),
                            updated_at: updated,
                            default_branch: str_of(&e, "default_branch"),
                            html_url: str_of(&e, "html_url"),
                            full_name: full.clone(),
                        };
                        match index.get(&full.to_lowercase()) {
                            Some(&i) => repos[i] = repo,
                            None => {
                                index.insert(full.to_lowercase(), repos.len());
                                repos.push(repo);
                            }
                        }
                    })
                    .await?;
                }
                // Newest first; a stable sort keeps GitHub's order among equals.
                repos.sort_by_key(|r| std::cmp::Reverse(r.updated_at));
                let c = Arc::new(RepoCache {
                    at: self.now(),
                    repos,
                    installations,
                });
                *self.repo_cache.lock().unwrap() = Some(c.clone());
                c
            }
        };
        let words: Vec<String> = query
            .unwrap_or("")
            .split(' ')
            .filter(|w| !w.is_empty())
            .map(str::to_lowercase)
            .collect();
        let list: Vec<&GitHubRepo> = cache
            .repos
            .iter()
            .filter(|r| {
                words.iter().all(|w| {
                    r.full_name.to_lowercase().contains(w)
                        || r.description
                            .as_deref()
                            .is_some_and(|d| d.to_lowercase().contains(w))
                })
            })
            .collect();
        Ok(GitHubRepoPage {
            total: list.len(),
            page,
            per_page,
            items: list
                .into_iter()
                .skip((page - 1).saturating_mul(per_page))
                .take(per_page)
                .cloned()
                .collect(),
            installations: cache.installations.clone(),
        })
    }

    /// A repository's branches (up to 500) and its default branch.
    pub async fn branches(&self, full_name: &str) -> Result<GitHubBranches, GitHubAppError> {
        let full_name = check_repo(full_name)?;
        let (status, repo) = self.api_get(&format!("/repos/{full_name}")).await?;
        if status == 404 {
            return Err(GitHubAppError::Argument(
                "GitHub can't find that repository, or the app isn't installed on it.".into(),
            ));
        }
        let mut names: Vec<String> = Vec::new();
        let mut truncated = false;
        for page in 1.. {
            let (s, body) = self
                .api_get(&format!(
                    "/repos/{full_name}/branches?per_page=100&page={page}"
                ))
                .await?;
            let Some(Value::Array(arr)) = body.filter(|_| s == 200) else {
                break;
            };
            for b in &arr {
                if let Some(name) = b.get("name").and_then(Value::as_str) {
                    names.push(name.to_string());
                }
            }
            if arr.len() < 100 {
                break;
            }
            if page == 5 {
                truncated = true;
                break;
            }
        }
        let def = str_of(&repo, "default_branch");
        if let Some(d) = &def
            && !names.contains(d)
        {
            names.insert(0, d.clone());
        }
        names.sort_by(|a, b| {
            let (ad, bd) = (Some(a) != def.as_ref(), Some(b) != def.as_ref());
            ad.cmp(&bd)
                .then_with(|| a.to_uppercase().cmp(&b.to_uppercase()))
        });
        Ok(GitHubBranches {
            default_branch: def,
            branches: names,
            truncated,
        })
    }

    /// Reads GitHub's pages of a list ("total_count" plus an array under `key`).
    async fn pages(
        &self,
        path: &str,
        key: &str,
        mut each: impl FnMut(&Value),
    ) -> Result<(), GitHubAppError> {
        let mut seen = 0i64;
        for page in 1..=MAX_PAGES {
            let (status, body) = self
                .api_get(&format!("{path}?per_page=100&page={page}"))
                .await?;
            if status != 200 {
                return Err(GitHubAppError::Invalid(format!(
                    "GitHub answered {status} for {path}."
                )));
            }
            let Some(arr) = body
                .as_ref()
                .and_then(|b| b.get(key))
                .and_then(Value::as_array)
            else {
                return Ok(());
            };
            for el in arr {
                each(el);
            }
            let n = arr.len() as i64;
            seen += n;
            let done = match int_of(&body, "total_count") {
                Some(t) => seen >= t,
                None => n < 100,
            };
            if n == 0 || done {
                return Ok(());
            }
        }
        Ok(())
    }

    /// An API GET with the user's token; on 401 the token is refreshed once and the call retried.
    async fn api_get(&self, path: &str) -> Result<Answer, GitHubAppError> {
        let token = self.access_token(false).await?;
        let r = self.send_api(&token, path).await?;
        if r.0 != 401 {
            return Ok(r);
        }
        let renewed = self.access_token(true).await?;
        let r = self.send_api(&renewed, path).await?;
        if r.0 == 401 {
            self.store.delete();
            return Err(GitHubAppError::Auth(
                "GitHub no longer accepts the sign-in (revoked?). Sign in again.".into(),
            ));
        }
        Ok(r)
    }

    async fn send_api(&self, token: &str, path: &str) -> Result<Answer, GitHubAppError> {
        let req = self
            .http
            .get(format!("{}{path}", self.api))
            .bearer_auth(token)
            .header("User-Agent", "OpenOrca-WebUI")
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");
        Self::answer(req.send().await).await
    }

    async fn post_form(&self, path: &str, form: &[(&str, &str)]) -> Result<Answer, GitHubAppError> {
        let body = form
            .iter()
            .map(|(k, v)| format!("{}={}", form_escape(k), form_escape(v)))
            .collect::<Vec<_>>()
            .join("&");
        let req = self
            .http
            .post(format!("{}{path}", self.web))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Accept", "application/json")
            .header("User-Agent", "OpenOrca-WebUI")
            .body(body);
        Self::answer(req.send().await).await
    }

    async fn answer(
        res: Result<reqwest::Response, reqwest::Error>,
    ) -> Result<Answer, GitHubAppError> {
        let res = res.map_err(|e| GitHubAppError::Http(e.without_url().to_string()))?;
        let status = res.status().as_u16();
        let text = res
            .text()
            .await
            .map_err(|e| GitHubAppError::Http(e.without_url().to_string()))?;
        let body = if text.is_empty() {
            None
        } else {
            serde_json::from_str(&text).ok()
        };
        Ok((status, body))
    }

    fn require_client_id(&self) -> Result<String, GitHubAppError> {
        self.config().client_id.ok_or_else(|| {
            GitHubAppError::Invalid(
                "GitHub sign-in isn't configured on this server (no GitHub App client ID).".into(),
            )
        })
    }
}

/// "owner/name", checked (it goes into URLs).
pub fn check_repo(full_name: &str) -> Result<String, GitHubAppError> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let f = full_name.trim();
    let re = RE.get_or_init(|| {
        Regex::new(r"^[A-Za-z0-9][A-Za-z0-9-]{0,38}/[A-Za-z0-9._-]{1,100}$").unwrap()
    });
    if !re.is_match(f) || f.contains("..") {
        return Err(GitHubAppError::Argument(format!(
            "Not a GitHub repository name (owner/name): {f}"
        )));
    }
    Ok(f.to_string())
}

pub(crate) fn error_text(error: &str, description: Option<&str>) -> String {
    match error {
        "device_flow_disabled" => "Device flow isn't enabled for this GitHub App (tick \"Enable Device Flow\" in the app's settings).".into(),
        "incorrect_client_credentials" => "GitHub doesn't know this client ID. Check the GitHub App's client ID (not its app ID).".into(),
        "unsupported_grant_type" => "GitHub refused the request (unsupported grant type).".into(),
        _ => match description.filter(|d| !d.trim().is_empty()) {
            None => error.to_string(),
            Some(d) => format!("{d} ({error})"),
        },
    }
}

/// FormUrlEncodedContent: Uri.EscapeDataString, spaces as '+'.
pub(crate) fn form_escape(s: &str) -> String {
    crate::util::escape_data(s).replace("%20", "+")
}

pub(crate) fn str_of(el: &Option<Value>, name: &str) -> Option<String> {
    el.as_ref()?.get(name)?.as_str().map(str::to_string)
}

pub(crate) fn int_of(el: &Option<Value>, name: &str) -> Option<i64> {
    el.as_ref()?
        .get(name)?
        .as_i64()
        .filter(|i| i32::try_from(*i).is_ok())
}

/// A GitHub timestamp; one without an offset is read as UTC.
pub(crate) fn parse_offset(s: &str) -> Option<DateTime<FixedOffset>> {
    DateTime::parse_from_rfc3339(s).ok().or_else(|| {
        chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f")
            .ok()
            .map(|n| n.and_utc().fixed_offset())
    })
}

/// The account's refresh expiry as the status endpoint shows it.
pub fn expires_text(account: &GitHubAccount) -> Option<String> {
    account.refresh_expires.as_ref().map(iso::text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_escape_matches_form_url_encoded_content() {
        assert_eq!(
            form_escape("urn:ietf:params:oauth:grant-type:device_code"),
            "urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code"
        );
        assert_eq!(form_escape("a b"), "a+b");
    }

    #[test]
    fn account_json_round_trips_in_the_csharp_shape() {
        let t = DateTime::parse_from_rfc3339("2026-11-24T09:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let a = GitHubAccount {
            access_token: "ghu_x".into(),
            access_expires: Some(t),
            refresh_token: None,
            refresh_expires: None,
            login: Some("octo".into()),
            avatar_url: None,
            signed_in_at: t,
        };
        let text = serde_json::to_string(&a).unwrap();
        assert_eq!(
            text,
            r#"{"accessToken":"ghu_x","accessExpires":"2026-11-24T09:00:00+00:00","refreshToken":null,"refreshExpires":null,"login":"octo","avatarUrl":null,"signedInAt":"2026-11-24T09:00:00+00:00"}"#
        );
        assert_eq!(serde_json::from_str::<GitHubAccount>(&text).unwrap(), a);
        // C# writes fractional seconds with seven digits.
        let cs = r#"{"accessToken":"ghu_x","accessExpires":"2026-11-24T09:00:00.1234567+00:00","signedInAt":"2026-11-24T09:00:00+00:00"}"#;
        assert!(serde_json::from_str::<GitHubAccount>(cs).is_ok());
    }

    #[test]
    fn check_repo_refuses_odd_names() {
        assert_eq!(check_repo(" acme/demo ").unwrap(), "acme/demo");
        for bad in [
            "acme",
            "../x/y",
            "acme/demo/extra",
            "-x/y",
            "acme/..",
            "acme/de mo",
        ] {
            assert!(check_repo(bad).is_err(), "{bad}");
        }
    }
}
