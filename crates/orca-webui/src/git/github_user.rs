//! "Clone from GitHub" with each Abhoth user's own GitHub credentials (D147), no GitHub App
//! needed. The token comes from, in order:
//!
//! 1. a token the user pasted for this session (memory only);
//! 2. a token the user saved, or got by signing in with GitHub (OAuth device flow, when
//!    `github.oauthClientId` is set): `github-user-tokens.json`, one entry per user, sealed
//!    with DPAPI on Windows, mode 600 elsewhere (`secret_box`);
//! 3. only in the desktop app or a local, non-exposed `--hostui`, and only for admins: this
//!    machine's own sign-ins, `gh auth token`, then `git credential fill` (Git Credential
//!    Manager or any helper);
//! 4. nothing: public repositories only.
//!
//! A token never leaves the server: responses carry the source and the GitHub login, git gets
//! the token through the credential helper (`TokenFile`), never the URL or arguments, and
//! error text is scrubbed of it.

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Digest;
use tokio::io::AsyncWriteExt;

use crate::git::github_app::{
    GitHubAppError, GitHubBranches, GitHubDevicePoll, GitHubDeviceStart, GitHubRepo,
    GitHubRepoPage, check_repo, error_text, form_escape, int_of, parse_offset, str_of,
};
use crate::git::{secret_box, tokens};

/// A project's stored value when it was cloned with its owner's GitHub credentials:
/// `@github-user:<user id>` (no real token contains '@' or ':').
pub const USER_LINK_PREFIX: &str = "@github-user:";

/// The OAuth app's client ID shipped with the build ("" = none; `github.oauthClientId` in
/// config.json or `ABHOTH_GITHUB_OAUTH_CLIENT_ID` set one).
pub const DEFAULT_OAUTH_CLIENT_ID: &str = "";

/// The scopes the device-flow sign-in asks for: private repositories (clone, fetch, push) and
/// the user's organizations (to list their repositories).
pub const OAUTH_SCOPES: &str = "repo read:org";

/// Where a user's GitHub token came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Source {
    /// Pasted, for this server process only.
    Session,
    /// Pasted and saved for this user.
    Saved,
    /// Signed in with GitHub (device flow) and saved for this user.
    Oauth,
    /// `gh auth token` on this machine.
    GhCli,
    /// `git credential fill` on this machine (Git Credential Manager, any helper).
    GitCredential,
    /// No credentials: public repositories only.
    Anonymous,
}

impl Source {
    pub fn text(self) -> &'static str {
        match self {
            Source::Session => "Using the token you pasted (until the server restarts)",
            Source::Saved => "Using your saved token",
            Source::Oauth => "Using your GitHub sign-in",
            Source::GhCli => "Using your GitHub CLI sign-in",
            Source::GitCredential => "Using your Git credential manager sign-in",
            Source::Anonymous => "No GitHub sign-in: public repositories only",
        }
    }

    pub fn is_machine(self) -> bool {
        matches!(self, Source::GhCli | Source::GitCredential)
    }
}

// ---- this machine's credentials ----

/// How `gh` and `git` are run to read this machine's GitHub sign-ins (tests use fakes).
#[derive(Clone)]
pub struct MachineCredentials {
    pub gh: OsString,
    pub git: OsString,
    /// Extra environment for both (tests: a private git config with a fake helper).
    pub env: Vec<(String, String)>,
    pub timeout: Duration,
}

impl MachineCredentials {
    pub fn system() -> Self {
        Self {
            gh: "gh".into(),
            git: "git".into(),
            env: Vec::new(),
            timeout: Duration::from_secs(10),
        }
    }

    fn command(&self, program: &OsString) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new(program);
        // Never prompt: no terminal, no GCM window, no askpass program.
        cmd.env("GH_PROMPT_DISABLED", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GCM_INTERACTIVE", "never")
            .env("GIT_ASKPASS", "")
            .env("SSH_ASKPASS", "")
            .env("NO_COLOR", "1");
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        #[cfg(windows)]
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        cmd
    }

    async fn output(
        &self,
        mut cmd: tokio::process::Command,
        stdin: Option<String>,
    ) -> Option<String> {
        cmd.stdin(if stdin.is_some() {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        });
        let mut child = cmd.spawn().ok()?;
        if let (Some(text), Some(mut si)) = (stdin, child.stdin.take()) {
            let _ = si.write_all(text.as_bytes()).await;
            drop(si);
        }
        let out = tokio::time::timeout(self.timeout, child.wait_with_output())
            .await
            .ok()?
            .ok()?;
        if !out.status.success() {
            return None;
        }
        String::from_utf8(out.stdout).ok()
    }

    /// `gh auth token --hostname <host>`: the GitHub CLI's token, when it's installed and
    /// signed in.
    pub async fn gh_token(&self, host: &str) -> Option<String> {
        let mut cmd = self.command(&self.gh);
        cmd.args(["auth", "token", "--hostname", host]);
        let out = self.output(cmd, None).await?;
        tokens::validate(Some(out.trim())).ok()
    }

    /// `git credential fill` for `<protocol>://<host>`: what Git Credential Manager (or any
    /// configured helper) has for GitHub. Only the password is used, and only if it looks
    /// like a token.
    pub async fn git_credential(&self, protocol: &str, host: &str) -> Option<String> {
        let mut cmd = self.command(&self.git);
        cmd.args(["credential", "fill"]);
        let out = self
            .output(cmd, Some(format!("protocol={protocol}\nhost={host}\n\n")))
            .await?;
        let mut password = None;
        for line in out.lines() {
            if let Some(p) = line.strip_prefix("password=") {
                password = Some(p.to_string());
            }
        }
        tokens::validate(password.as_deref()).ok()
    }
}

// ---- saved tokens, per user ----

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredToken {
    /// `secret_box::seal` of the token.
    secret: String,
    /// "saved" or "oauth".
    kind: String,
    #[serde(default)]
    login: Option<String>,
    saved_at: String,
}

/// Saved tokens, one per Abhoth user, in `<state>/github-user-tokens.json` (mode 600, written
/// atomically; each token sealed with DPAPI on Windows).
pub struct UserTokenStore {
    path: PathBuf,
    lock: Mutex<()>,
}

impl UserTokenStore {
    pub const FILE_NAME: &'static str = "github-user-tokens.json";

    pub fn new(directory: &Path) -> Self {
        Self {
            path: directory.join(Self::FILE_NAME),
            lock: Mutex::new(()),
        }
    }

    pub fn file_path(&self) -> &Path {
        &self.path
    }

    fn load(&self) -> BTreeMap<String, StoredToken> {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|t| serde_json::from_str(t.trim_start_matches('\u{feff}')).ok())
            .unwrap_or_default()
    }

    /// The user's saved token and how it was got.
    pub fn get(&self, uid: i64) -> Option<(String, Source)> {
        let _g = self.lock.lock().unwrap();
        let entry = self.load().remove(&uid.to_string())?;
        let token = secret_box::open(&entry.secret)?;
        let source = if entry.kind == "oauth" {
            Source::Oauth
        } else {
            Source::Saved
        };
        Some((token, source))
    }

    pub fn set(
        &self,
        uid: i64,
        token: &str,
        source: Source,
        login: Option<&str>,
    ) -> Result<(), String> {
        let sealed = secret_box::seal(token)?;
        let _g = self.lock.lock().unwrap();
        let mut all = self.load();
        all.insert(
            uid.to_string(),
            StoredToken {
                secret: sealed,
                kind: if source == Source::Oauth {
                    "oauth"
                } else {
                    "saved"
                }
                .into(),
                login: login.map(str::to_string),
                saved_at: chrono::Utc::now().to_rfc3339(),
            },
        );
        tokens::write_secret(&self.path, &serde_json::to_string(&all).unwrap_or_default())
            .map_err(|e| format!("Couldn't save the token: {e}"))
    }

    pub fn remove(&self, uid: i64) -> bool {
        let _g = self.lock.lock().unwrap();
        let mut all = self.load();
        if all.remove(&uid.to_string()).is_none() {
            return false;
        }
        let _ = tokens::write_secret(&self.path, &serde_json::to_string(&all).unwrap_or_default());
        true
    }
}

// ---- the service ----

/// Gives the OAuth app's client ID, asked on each use (a config.json change applies at once).
pub type ClientIdFn = Arc<dyn Fn() -> Option<String> + Send + Sync>;

/// Whether a user id is an admin (machine credentials are an admin's only).
pub type IsAdminFn = Arc<dyn Fn(i64) -> bool + Send + Sync>;

/// `github.oauthClientId` from `<config_home>/config.json`, else `ABHOTH_GITHUB_OAUTH_CLIENT_ID`,
/// else [`DEFAULT_OAUTH_CLIENT_ID`]. Only GitHub's id characters are accepted.
pub fn load_client_id(config_home: Option<&Path>) -> Option<String> {
    let from_file = config_home.and_then(|home| {
        let text = std::fs::read_to_string(home.join("config.json")).ok()?;
        let doc: Value = serde_json::from_str(text.trim_start_matches('\u{feff}')).ok()?;
        let gh = doc.get("github").or_else(|| doc.get("GitHub"))?;
        gh.get("oauthClientId")
            .or_else(|| gh.get("OAuthClientId"))
            .and_then(Value::as_str)
            .map(str::to_string)
    });
    let id = from_file
        .filter(|v| !v.trim().is_empty())
        .or_else(|| orca_core::brand::env_var("GITHUB_OAUTH_CLIENT_ID"))
        .unwrap_or_else(|| DEFAULT_OAUTH_CLIENT_ID.to_string());
    let id = id.trim().to_string();
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"^[A-Za-z0-9._-]{1,100}$").unwrap())
        .is_match(&id)
        .then_some(id)
}

pub struct GitHubUserOptions {
    /// github.com (device flow, clone URLs); tests point it at a fake.
    pub web: String,
    /// api.github.com.
    pub api: String,
    pub state_dir: PathBuf,
    pub machine: MachineCredentials,
    /// The desktop app or a local, non-exposed `--hostui`: this machine's sign-ins may be used
    /// (by admins).
    pub machine_allowed: bool,
    pub client_id: ClientIdFn,
}

/// Who the user is on GitHub, for a token.
#[derive(Debug, Clone, Default)]
struct Identity {
    login: Option<String>,
    avatar_url: Option<String>,
}

/// The credentials a request would use.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub source: Source,
    pub token: Option<String>,
    pub login: Option<String>,
    pub avatar_url: Option<String>,
    /// Sources that were found but refused by GitHub ("GitHub refused your saved token").
    pub notes: Vec<String>,
}

struct UserFlow {
    owner: i64,
    device_code: String,
    expires_at: Instant,
    state: tokio::sync::Mutex<UserFlowState>,
}

struct UserFlowState {
    interval: i64,
    next_poll: Instant,
    result: Option<GitHubDevicePoll>,
}

/// This machine's tokens, as read at a time.
type MachineCache = Option<(Instant, Vec<(Source, String)>)>;
/// A user's repository list: when, the token's key, the list.
type RepoCacheEntry = (Instant, String, Arc<Vec<GitHubRepo>>);

const IDENTITY_SECS: u64 = 300;
const MACHINE_SECS: u64 = 60;
const REPO_CACHE_SECS: u64 = 60;
const MAX_REPO_PAGES: usize = 10;

pub struct GitHubUserService {
    web: String,
    api: String,
    http: reqwest::Client,
    machine: MachineCredentials,
    machine_allowed: bool,
    client_id: ClientIdFn,
    store: UserTokenStore,
    session: Mutex<HashMap<i64, String>>,
    identities: Mutex<HashMap<String, (Instant, Identity)>>,
    machine_cache: tokio::sync::Mutex<MachineCache>,
    flows: Mutex<HashMap<String, Arc<UserFlow>>>,
    repo_cache: Mutex<HashMap<i64, RepoCacheEntry>>,
    is_admin: OnceLock<IsAdminFn>,
}

/// A token's cache key (its SHA-256, so the caches never hold the token itself twice).
fn key_of(token: &str) -> String {
    crate::util::hex_lower(&sha2::Sha256::digest(token.as_bytes()))
}

impl GitHubUserService {
    pub fn new(o: GitHubUserOptions) -> Self {
        Self {
            web: o.web.trim_end_matches('/').to_string(),
            api: o.api.trim_end_matches('/').to_string(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_default(),
            machine: o.machine,
            machine_allowed: o.machine_allowed,
            client_id: o.client_id,
            store: UserTokenStore::new(&o.state_dir),
            session: Mutex::new(HashMap::new()),
            identities: Mutex::new(HashMap::new()),
            machine_cache: tokio::sync::Mutex::new(None),
            flows: Mutex::new(HashMap::new()),
            repo_cache: Mutex::new(HashMap::new()),
            is_admin: OnceLock::new(),
        }
    }

    pub fn set_is_admin(&self, f: IsAdminFn) {
        let _ = self.is_admin.set(f);
    }

    pub fn machine_allowed(&self) -> bool {
        self.machine_allowed
    }

    pub fn client_id(&self) -> Option<String> {
        (self.client_id)()
    }

    pub fn store(&self) -> &UserTokenStore {
        &self.store
    }

    pub fn web_base(&self) -> &str {
        &self.web
    }

    /// The HTTPS clone URL of "owner/name" (no credentials in it).
    pub fn clone_url(&self, full_name: &str) -> Result<String, GitHubAppError> {
        Ok(format!("{}/{}.git", self.web, check_repo(full_name)?))
    }

    /// The link value a project cloned by `uid` with their credentials stores.
    pub fn link_for(uid: i64) -> String {
        format!("{USER_LINK_PREFIX}{uid}")
    }

    fn host(&self) -> (String, String) {
        let (protocol, rest) = self.web.split_once("://").unwrap_or(("https", &self.web));
        let host = rest.split('/').next().unwrap_or("github.com");
        (protocol.to_string(), host.to_string())
    }

    /// This machine's GitHub tokens (gh, then git's helpers), read at most once a minute.
    async fn machine_tokens(&self) -> Vec<(Source, String)> {
        if !self.machine_allowed {
            return Vec::new();
        }
        let mut cache = self.machine_cache.lock().await;
        if let Some((at, list)) = cache.as_ref()
            && at.elapsed() < Duration::from_secs(MACHINE_SECS)
        {
            return list.clone();
        }
        let (protocol, host) = self.host();
        let mut list = Vec::new();
        if let Some(t) = self.machine.gh_token(&host).await {
            list.push((Source::GhCli, t));
        }
        if let Some(t) = self.machine.git_credential(&protocol, &host).await
            && !list.iter().any(|(_, x)| *x == t)
        {
            list.push((Source::GitCredential, t));
        }
        *cache = Some((Instant::now(), list.clone()));
        list
    }

    /// Forget the machine tokens read (the next use reads them again).
    pub async fn recheck_machine(&self) {
        *self.machine_cache.lock().await = None;
    }

    /// Who the token belongs to. `Err(true)`: GitHub refused it (401); `Err(false)`: GitHub
    /// couldn't be reached.
    async fn identify(&self, token: &str) -> Result<Identity, bool> {
        let key = key_of(token);
        if let Some((at, id)) = self.identities.lock().unwrap().get(&key)
            && at.elapsed() < Duration::from_secs(IDENTITY_SECS)
        {
            return Ok(id.clone());
        }
        match self.get_api(Some(token), "/user").await {
            Ok((200, body)) => {
                let id = Identity {
                    login: str_of(&body, "login"),
                    avatar_url: str_of(&body, "avatar_url"),
                };
                self.identities
                    .lock()
                    .unwrap()
                    .insert(key, (Instant::now(), id.clone()));
                Ok(id)
            }
            Ok((401, _)) => Err(true),
            Ok(_) | Err(_) => Err(false),
        }
    }

    /// The credentials `uid` would use now (see the module docs for the order). `admin`: may
    /// use this machine's sign-ins (when the server allows them at all).
    pub async fn resolve(&self, uid: i64, admin: bool) -> Resolved {
        let mut candidates: Vec<(Source, String)> = Vec::new();
        if let Some(t) = self.session.lock().unwrap().get(&uid).cloned() {
            candidates.push((Source::Session, t));
        }
        if let Some((t, s)) = self.store.get(uid) {
            candidates.push((s, t));
        }
        if admin {
            candidates.extend(self.machine_tokens().await);
        }
        let mut notes = Vec::new();
        for (source, token) in candidates {
            match self.identify(&token).await {
                Ok(id) => {
                    return Resolved {
                        source,
                        token: Some(token),
                        login: id.login,
                        avatar_url: id.avatar_url,
                        notes,
                    };
                }
                Err(true) => notes.push(format!(
                    "GitHub refused {}.",
                    match source {
                        Source::Session => "the token you pasted",
                        Source::Saved => "your saved token",
                        Source::Oauth => "your GitHub sign-in (sign in again)",
                        Source::GhCli => "the GitHub CLI's token (run gh auth login)",
                        Source::GitCredential => "the Git credential manager's token",
                        Source::Anonymous => "the request",
                    }
                )),
                // GitHub unreachable: use it anyway (git may still get through).
                Err(false) => {
                    return Resolved {
                        source,
                        token: Some(token),
                        login: None,
                        avatar_url: None,
                        notes,
                    };
                }
            }
        }
        Resolved {
            source: Source::Anonymous,
            token: None,
            login: None,
            avatar_url: None,
            notes,
        }
    }

    /// The token for a project linked with `@github-user:<uid>` (fetch, pull, push).
    pub async fn token_for_link(&self, link: &str) -> Option<String> {
        let uid: i64 = link.strip_prefix(USER_LINK_PREFIX)?.parse().ok()?;
        let admin = self.is_admin.get().is_some_and(|f| f(uid));
        self.resolve(uid, admin).await.token
    }

    /// Use a pasted token: checked with GitHub first; `save` keeps it (sealed) for this user,
    /// else it lasts until the server restarts.
    pub async fn set_token(
        &self,
        uid: i64,
        token: &str,
        save: bool,
    ) -> Result<Resolved, GitHubAppError> {
        let token = tokens::validate(Some(token)).map_err(GitHubAppError::Argument)?;
        let id = match self.identify(&token).await {
            Ok(id) => id,
            Err(true) => {
                return Err(GitHubAppError::Argument(
                    "GitHub refused that token (expired, revoked or mistyped?).".into(),
                ));
            }
            Err(false) => {
                return Err(GitHubAppError::Http(
                    "couldn't check the token with GitHub".into(),
                ));
            }
        };
        if save {
            self.store
                .set(uid, &token, Source::Saved, id.login.as_deref())
                .map_err(GitHubAppError::Invalid)?;
            self.session.lock().unwrap().remove(&uid);
        } else {
            self.session.lock().unwrap().insert(uid, token.clone());
        }
        self.repo_cache.lock().unwrap().remove(&uid);
        Ok(Resolved {
            source: if save { Source::Saved } else { Source::Session },
            token: Some(token),
            login: id.login,
            avatar_url: id.avatar_url,
            notes: Vec::new(),
        })
    }

    /// Forget the user's pasted and saved tokens (and their GitHub sign-in). Machine sign-ins
    /// are not touched.
    pub fn forget(&self, uid: i64) -> bool {
        let a = self.session.lock().unwrap().remove(&uid).is_some();
        let b = self.store.remove(uid);
        self.repo_cache.lock().unwrap().remove(&uid);
        a || b
    }

    // ---- device flow (an OAuth app's, `github.oauthClientId`) ----

    fn require_client_id(&self) -> Result<String, GitHubAppError> {
        self.client_id().ok_or_else(|| {
            GitHubAppError::Invalid(
                "Sign in with GitHub isn't set up (github.oauthClientId is empty). Paste a token instead."
                    .into(),
            )
        })
    }

    pub async fn start_device(&self, uid: i64) -> Result<GitHubDeviceStart, GitHubAppError> {
        let id = self.require_client_id()?;
        let (status, body) = self
            .post_form(
                "/login/device/code",
                &[("client_id", &id), ("scope", OAUTH_SCOPES)],
            )
            .await?;
        if let Some(err) = str_of(&body, "error") {
            return Err(GitHubAppError::Invalid(oauth_error_text(
                &err,
                str_of(&body, "error_description").as_deref(),
            )));
        }
        let (Some(device_code), Some(user_code), 200) = (
            str_of(&body, "device_code"),
            str_of(&body, "user_code"),
            status,
        ) else {
            return Err(GitHubAppError::Invalid(format!(
                "GitHub didn't start the sign-in (HTTP {status}). Check github.oauthClientId."
            )));
        };
        let interval = int_of(&body, "interval").unwrap_or(5).max(1);
        let expires_in = int_of(&body, "expires_in").unwrap_or(900).max(1);
        let now = Instant::now();
        let flow_id = crate::util::hex_lower(&crate::util::random_bytes(16));
        let flow = Arc::new(UserFlow {
            owner: uid,
            device_code,
            expires_at: now + Duration::from_secs(expires_in as u64),
            state: tokio::sync::Mutex::new(UserFlowState {
                interval,
                next_poll: now + Duration::from_secs(interval as u64),
                result: None,
            }),
        });
        {
            let mut flows = self.flows.lock().unwrap();
            flows.retain(|_, f| f.expires_at > now);
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

    /// One poll of `uid`'s device flow (another user's flow id is "expired" to them).
    pub async fn poll_device(&self, uid: i64, flow_id: &str) -> GitHubDevicePoll {
        let ended = |interval| GitHubDevicePoll {
            state: "expired".into(),
            interval,
            login: None,
            error: Some("That sign-in has ended. Start again.".into()),
        };
        let flow = self.flows.lock().unwrap().get(flow_id).cloned();
        let Some(flow) = flow.filter(|f| f.owner == uid) else {
            return ended(5);
        };
        let mut st = flow.state.lock().await;
        if let Some(done) = &st.result {
            return done.clone();
        }
        let poll = |state: &str, interval: i64, error: Option<String>, login: Option<String>| {
            GitHubDevicePoll {
                state: state.into(),
                interval,
                login,
                error,
            }
        };
        if Instant::now() >= flow.expires_at {
            self.flows.lock().unwrap().remove(flow_id);
            return poll(
                "expired",
                st.interval,
                Some("The code expired before it was entered. Start again.".into()),
                None,
            );
        }
        if Instant::now() < st.next_poll {
            return poll("pending", st.interval, None, None);
        }
        let id = match self.require_client_id() {
            Ok(id) => id,
            Err(e) => return poll("error", st.interval, Some(e.to_string()), None),
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
        st.next_poll = Instant::now() + Duration::from_secs(st.interval as u64);
        let Ok((status, body)) = answer else {
            return poll("pending", st.interval, None, None);
        };
        let interval = st.interval;
        let result = if let Some(token) = str_of(&body, "access_token").filter(|t| !t.is_empty()) {
            let identity = self.identify(&token).await.unwrap_or_default();
            if let Err(e) = self
                .store
                .set(uid, &token, Source::Oauth, identity.login.as_deref())
            {
                poll("error", interval, Some(e), None)
            } else {
                self.session.lock().unwrap().remove(&uid);
                self.repo_cache.lock().unwrap().remove(&uid);
                poll("done", interval, None, identity.login)
            }
        } else {
            match str_of(&body, "error").as_deref() {
                Some("authorization_pending") => return poll("pending", interval, None, None),
                Some("slow_down") => {
                    st.interval = int_of(&body, "interval")
                        .unwrap_or(interval + 5)
                        .max(interval + 1);
                    st.next_poll = Instant::now() + Duration::from_secs(st.interval as u64);
                    return poll("pending", st.interval, None, None);
                }
                Some("expired_token" | "token_expired" | "incorrect_device_code") => poll(
                    "expired",
                    interval,
                    Some("The code expired before it was entered. Start again.".into()),
                    None,
                ),
                Some("access_denied") => poll(
                    "denied",
                    interval,
                    Some("The sign-in was cancelled on GitHub.".into()),
                    None,
                ),
                Some(other) => poll(
                    "error",
                    interval,
                    Some(oauth_error_text(
                        other,
                        str_of(&body, "error_description").as_deref(),
                    )),
                    None,
                ),
                None if status >= 500 => return poll("pending", interval, None, None),
                None => poll(
                    "error",
                    interval,
                    Some(format!("Unexpected answer from GitHub (HTTP {status}).")),
                    None,
                ),
            }
        };
        st.result = Some(result.clone());
        result
    }

    // ---- repositories ----

    /// The repositories the user's token reaches (their own, collaborations, organizations),
    /// most recently pushed first, filtered and cut into pages. `Auth` error without a token.
    pub async fn repos(
        &self,
        uid: i64,
        admin: bool,
        query: Option<&str>,
        page: i64,
        per_page: i64,
        reload: bool,
    ) -> Result<GitHubRepoPage, GitHubAppError> {
        let per_page = per_page.clamp(1, 100) as usize;
        let page = page.max(1) as usize;
        let r = self.resolve(uid, admin).await;
        let Some(token) = r.token else {
            return Err(GitHubAppError::Auth(
                "No GitHub sign-in: paste a token or sign in to list your repositories.".into(),
            ));
        };
        let key = key_of(&token);
        let cached = self.repo_cache.lock().unwrap().get(&uid).cloned();
        let repos = match cached {
            Some((at, k, list))
                if !reload && k == key && at.elapsed() < Duration::from_secs(REPO_CACHE_SECS) =>
            {
                list
            }
            _ => {
                let mut list: Vec<GitHubRepo> = Vec::new();
                for p in 1..=MAX_REPO_PAGES {
                    let (status, body) = self
                        .get_api(
                            Some(&token),
                            &format!(
                                "/user/repos?per_page=100&page={p}&sort=pushed&affiliation=owner,collaborator,organization_member"
                            ),
                        )
                        .await?;
                    if status == 401 {
                        return Err(GitHubAppError::Auth(
                            "GitHub refused the token. Paste a new one or sign in again.".into(),
                        ));
                    }
                    if status != 200 {
                        return Err(GitHubAppError::Invalid(format!(
                            "GitHub answered {status} for your repositories."
                        )));
                    }
                    let Some(Value::Array(arr)) = body else { break };
                    let n = arr.len();
                    list.extend(arr.iter().filter_map(repo_of));
                    if n < 100 {
                        break;
                    }
                }
                list.sort_by_key(|r| std::cmp::Reverse(r.updated_at));
                let list = Arc::new(list);
                self.repo_cache
                    .lock()
                    .unwrap()
                    .insert(uid, (Instant::now(), key, list.clone()));
                list
            }
        };
        let words: Vec<String> = query
            .unwrap_or("")
            .split(' ')
            .filter(|w| !w.is_empty())
            .map(str::to_lowercase)
            .collect();
        let hits: Vec<&GitHubRepo> = repos
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
            total: hits.len(),
            page,
            per_page,
            items: hits
                .into_iter()
                .skip((page - 1).saturating_mul(per_page))
                .take(per_page)
                .cloned()
                .collect(),
            installations: Vec::new(),
        })
    }

    /// A repository's branches (up to 500) and default branch, with the user's token, or
    /// anonymously for a public repository.
    pub async fn branches(
        &self,
        uid: i64,
        admin: bool,
        full_name: &str,
    ) -> Result<GitHubBranches, GitHubAppError> {
        let full_name = check_repo(full_name)?;
        let token = self.resolve(uid, admin).await.token;
        let (status, repo) = self
            .get_api(token.as_deref(), &format!("/repos/{full_name}"))
            .await?;
        if status == 404 || (status == 401 && token.is_none()) {
            return Err(GitHubAppError::Argument(if token.is_some() {
                "GitHub can't find that repository, or your token can't see it.".into()
            } else {
                "GitHub can't find that repository. If it's private, paste a token or sign in first."
                    .into()
            }));
        }
        if status != 200 {
            return Err(GitHubAppError::Invalid(format!(
                "GitHub answered {status} for {full_name}."
            )));
        }
        let mut names: Vec<String> = Vec::new();
        let mut truncated = false;
        for page in 1..=5 {
            let (s, body) = self
                .get_api(
                    token.as_deref(),
                    &format!("/repos/{full_name}/branches?per_page=100&page={page}"),
                )
                .await?;
            let Some(Value::Array(arr)) = body.filter(|_| s == 200) else {
                break;
            };
            names.extend(
                arr.iter()
                    .filter_map(|b| b.get("name").and_then(Value::as_str).map(str::to_string)),
            );
            if arr.len() < 100 {
                break;
            }
            truncated = page == 5;
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

    // ---- HTTP ----

    async fn get_api(
        &self,
        token: Option<&str>,
        path: &str,
    ) -> Result<(u16, Option<Value>), GitHubAppError> {
        let mut req = self
            .http
            .get(format!("{}{path}", self.api))
            .header("User-Agent", "Abhoth-WebUI")
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        answer(req.send().await).await
    }

    async fn post_form(
        &self,
        path: &str,
        form: &[(&str, &str)],
    ) -> Result<(u16, Option<Value>), GitHubAppError> {
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
            .header("User-Agent", "Abhoth-WebUI")
            .body(body);
        answer(req.send().await).await
    }
}

async fn answer(
    res: Result<reqwest::Response, reqwest::Error>,
) -> Result<(u16, Option<Value>), GitHubAppError> {
    // without_url: a URL never carries a token here, but keep error text free of query strings.
    let res = res.map_err(|e| GitHubAppError::Http(e.without_url().to_string()))?;
    let status = res.status().as_u16();
    let text = res
        .text()
        .await
        .map_err(|e| GitHubAppError::Http(e.without_url().to_string()))?;
    Ok((status, serde_json::from_str(&text).ok()))
}

fn repo_of(el: &Value) -> Option<GitHubRepo> {
    let e = Some(el.clone());
    let full = str_of(&e, "full_name")?;
    let owner = el
        .get("owner")
        .and_then(|o| str_of(&Some(o.clone()), "login"))
        .unwrap_or_else(|| full.split('/').next().unwrap_or("").into());
    Some(GitHubRepo {
        owner,
        name: str_of(&e, "name").unwrap_or_else(|| full.rsplit('/').next().unwrap_or("").into()),
        private: el.get("private") == Some(&Value::Bool(true)),
        description: str_of(&e, "description"),
        updated_at: str_of(&e, "pushed_at")
            .or_else(|| str_of(&e, "updated_at"))
            .and_then(|s| parse_offset(&s)),
        default_branch: str_of(&e, "default_branch"),
        html_url: str_of(&e, "html_url"),
        full_name: full,
    })
}

fn oauth_error_text(error: &str, description: Option<&str>) -> String {
    match error {
        "device_flow_disabled" => {
            "Device flow isn't enabled for the OAuth app (tick \"Enable Device Flow\" in its settings).".into()
        }
        "incorrect_client_credentials" => {
            "GitHub doesn't know this client ID. Check github.oauthClientId.".into()
        }
        _ => error_text(error, description),
    }
}

/// Is the web UI reachable only from this machine (so this machine's own GitHub sign-ins may
/// be offered)? The desktop window always is; a `--hostui` is when it binds a loopback address,
/// isn't exposed (`--hostui-login`) and trusts no reverse proxy.
pub fn machine_credentials_allowed(options: &crate::server::WebUiOptions) -> bool {
    if options.desktop.is_some() {
        return true;
    }
    let loopback = crate::server::parse_address(&options.host).is_ok_and(|a| a.is_loopback());
    loopback && !options.login.exposed && options.login.trusted_proxies.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_id_comes_from_config_json() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.json"),
            r#"{"github": {"oauthClientId": "Ov23liTESTONLY"}}"#,
        )
        .unwrap();
        assert_eq!(
            load_client_id(Some(dir.path())).as_deref(),
            Some("Ov23liTESTONLY")
        );
        std::fs::write(
            dir.path().join("config.json"),
            r#"{"github": {"oauthClientId": "bad id/../"}}"#,
        )
        .unwrap();
        assert_eq!(load_client_id(Some(dir.path())), None);
    }

    #[test]
    fn store_keeps_one_sealed_token_per_user() {
        let dir = tempfile::tempdir().unwrap();
        let s = UserTokenStore::new(dir.path());
        let t1 = "ghp_TESTONLYaaaaaaaaaaaaaaaaaaaaaaaa";
        let t2 = "gho_TESTONLYbbbbbbbbbbbbbbbbbbbbbbbb";
        s.set(1, t1, Source::Saved, Some("one")).unwrap();
        s.set(2, t2, Source::Oauth, None).unwrap();
        assert_eq!(s.get(1), Some((t1.to_string(), Source::Saved)));
        assert_eq!(s.get(2), Some((t2.to_string(), Source::Oauth)));
        assert_eq!(s.get(3), None);
        let raw = std::fs::read_to_string(s.file_path()).unwrap();
        if cfg!(windows) {
            assert!(!raw.contains(t1) && !raw.contains(t2), "sealed at rest");
        }
        assert!(s.remove(1));
        assert_eq!(s.get(1), None);
        assert!(s.get(2).is_some());
    }

    #[test]
    fn machine_credentials_only_for_local_servers() {
        let mut o = crate::server::WebUiOptions::new("t");
        assert!(machine_credentials_allowed(&o));
        o.host = "0.0.0.0".into();
        assert!(!machine_credentials_allowed(&o));
        o.host = "127.0.0.1".into();
        o.login.exposed = true;
        assert!(!machine_credentials_allowed(&o));
        o.login.exposed = false;
        o.login.trusted_proxies = vec!["10.0.0.1".parse().unwrap()];
        assert!(!machine_credentials_allowed(&o));
    }
}
