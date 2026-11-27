//! Sign-in with Abhoth, the router's side (D154, `docs/design/ROUTER-SSO.md`): the admin pages'
//! "Sign in with Abhoth" button. The router is an OAuth2 client of the web UI (authorization
//! code with PKCE S256, `orca_core::sso`).
//!
//! - `GET /admin/sso` starts: a random `state`, a PKCE verifier kept here (10 minutes, single
//!   use) and a random binding in a short-lived `SameSite=Lax` cookie (the callback must come
//!   back to the browser that started), then a redirect to the web UI's `/oauth/authorize`.
//! - `GET /admin/sso-callback?code&state` checks the state and the binding, exchanges the code
//!   server to server (client id and secret, the verifier) for `{sub, username, admin}`, and for
//!   an admin makes the usual admin session (`webui:<username>` in the audit).
//!
//! The client's credentials live in `<state_dir>/sso-client.json` (0600; never logged). A linked
//! router (`accounts_url` + link token, as in one container with the web UI) registers itself
//! over the link when it has none, and adds the callback of an address it is opened at when that
//! address is one it may vouch for (see [`SsoClient::may_register`]). Others link once with
//! `abhoth router sso link --webui URL` (an admin approves it in the web UI).

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use orca_core::sso::{
    ClientCredentials, ClientRegistration, REGISTER_INTERNAL_PATH, TOKEN_PATH, TokenRequest,
    TokenResponse, pkce_challenge, random,
};
use sha2::{Digest, Sha256};

/// The flow's binding cookie (Lax: it must ride along on the web UI's redirect back).
pub const FLOW_COOKIE: &str = "abhoth_router_sso";
/// How long a started sign-in waits for its callback.
pub const FLOW_TTL: Duration = Duration::from_secs(600);
/// Sign-ins waiting at once, in all and from one address.
const MAX_FLOWS: usize = 200;
const MAX_FLOWS_PER_IP: usize = 20;
/// The credentials file in the state directory.
pub const CREDENTIALS_FILE: &str = "sso-client.json";

struct Flow {
    verifier: String,
    redirect_uri: String,
    binding: Vec<u8>,
    ip: Option<IpAddr>,
    created: Instant,
}

/// A started sign-in, for the redirect.
pub struct Started {
    pub state: String,
    pub challenge: String,
    /// The cookie value (only its hash is kept).
    pub binding: String,
}

/// A callback's flow, once its state and binding matched.
pub struct Finished {
    pub verifier: String,
    pub redirect_uri: String,
}

/// The router's OAuth client: its credentials and the sign-ins in progress.
pub struct SsoClient {
    path: PathBuf,
    creds: Mutex<Option<ClientCredentials>>,
    flows: Mutex<HashMap<String, Flow>>,
    http: reqwest::Client,
}

fn hash(s: &str) -> Vec<u8> {
    Sha256::digest(s.as_bytes()).to_vec()
}

impl SsoClient {
    /// The client whose credentials are (or will be) in `state_dir`.
    pub fn open(state_dir: &Path) -> Self {
        let path = state_dir.join(CREDENTIALS_FILE);
        let creds = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<ClientCredentials>(&b).ok());
        Self {
            path,
            creds: Mutex::new(creds),
            flows: Mutex::new(HashMap::new()),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("an HTTP client"),
        }
    }

    /// The credentials (read again from the file while there are none: `sso link` may have
    /// written them since the start).
    pub fn credentials(&self) -> Option<ClientCredentials> {
        let mut c = self.creds.lock().unwrap();
        if c.is_none() {
            *c = std::fs::read(&self.path)
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok());
        }
        c.clone()
    }

    /// Keep new credentials (0600, written whole then renamed into place).
    pub fn save(&self, c: &ClientCredentials) -> Result<(), String> {
        save_credentials(&self.path, c)?;
        *self.creds.lock().unwrap() = Some(c.clone());
        Ok(())
    }

    /// Start a sign-in that comes back to `redirect_uri`.
    pub fn begin(&self, redirect_uri: &str, ip: Option<IpAddr>) -> Result<Started, &'static str> {
        let mut flows = self.flows.lock().unwrap();
        flows.retain(|_, f| f.created.elapsed() < FLOW_TTL);
        if flows.len() >= MAX_FLOWS
            || flows.values().filter(|f| f.ip == ip).count() >= MAX_FLOWS_PER_IP
        {
            return Err("too many sign-ins are waiting; try again in a few minutes");
        }
        let verifier = random(32);
        let state = random(24);
        let binding = random(24);
        let challenge = pkce_challenge(&verifier);
        flows.insert(
            state.clone(),
            Flow {
                verifier,
                redirect_uri: redirect_uri.to_string(),
                binding: hash(&binding),
                ip,
                created: Instant::now(),
            },
        );
        Ok(Started {
            state,
            challenge,
            binding,
        })
    }

    /// The flow of a callback: its state must be waiting (it is used up now, whatever follows)
    /// and the browser must carry its binding.
    pub fn finish(&self, state: &str, binding: Option<&str>) -> Result<Finished, &'static str> {
        let f = self
            .flows
            .lock()
            .unwrap()
            .remove(state)
            .filter(|f| f.created.elapsed() < FLOW_TTL)
            .ok_or("this sign-in has expired or was already used")?;
        let ok =
            binding.is_some_and(|b| orca_core::router_link::fixed_time_eq(&hash(b), &f.binding));
        if !ok {
            return Err("this sign-in was started in another browser");
        }
        Ok(Finished {
            verifier: f.verifier,
            redirect_uri: f.redirect_uri,
        })
    }

    /// Exchange a code at `base` (the web UI, server to server).
    pub async fn exchange(
        &self,
        base: &str,
        code: &str,
        f: &Finished,
    ) -> Result<TokenResponse, String> {
        let c = self
            .credentials()
            .ok_or("this router has no Sign-in with Abhoth credentials")?;
        let r = self
            .http
            .post(format!("{}{TOKEN_PATH}", base.trim_end_matches('/')))
            .json(&TokenRequest {
                grant_type: "authorization_code".into(),
                code: code.to_string(),
                redirect_uri: f.redirect_uri.clone(),
                client_id: c.client_id,
                client_secret: c.client_secret,
                code_verifier: f.verifier.clone(),
            })
            .send()
            .await
            .map_err(|e| {
                format!(
                    "the Abhoth server did not answer ({})",
                    super::keys::without_url(&e)
                )
            })?;
        let status = r.status();
        if !status.is_success() {
            let v: serde_json::Value = r.json().await.unwrap_or_default();
            let why = v["error_description"]
                .as_str()
                .or(v["error"].as_str())
                .unwrap_or("refused");
            return Err(format!(
                "the Abhoth server refused the sign-in ({status}: {why})"
            ));
        }
        r.json::<TokenResponse>()
            .await
            .map_err(|_| "the Abhoth server's answer was not understood".to_string())
    }

    /// Register (or re-register, with a new secret) over the link: `url` the accounts server,
    /// `token` the link token. Keeps the credentials.
    pub async fn register(
        &self,
        url: &str,
        token: &[u8],
        name: &str,
        redirect_uris: Vec<String>,
    ) -> Result<ClientCredentials, String> {
        let r = self
            .http
            .post(format!(
                "{}{REGISTER_INTERNAL_PATH}",
                url.trim_end_matches('/')
            ))
            .bearer_auth(String::from_utf8_lossy(token).trim())
            .json(&ClientRegistration {
                name: name.to_string(),
                redirect_uris,
            })
            .send()
            .await
            .map_err(|e| {
                format!(
                    "the Abhoth server did not answer ({})",
                    super::keys::without_url(&e)
                )
            })?;
        let status = r.status();
        if !status.is_success() {
            let v: serde_json::Value = r.json().await.unwrap_or_default();
            return Err(format!(
                "the Abhoth server refused the registration ({status}: {})",
                v["error"].as_str().unwrap_or("?")
            ));
        }
        let c: ClientCredentials = r
            .json()
            .await
            .map_err(|_| "the registration's answer was not understood".to_string())?;
        self.save(&c)?;
        Ok(c)
    }

    /// May a linked router register the callback of `host` (the Host the browser used)
    /// itself? Only addresses a header can't fake into something else: an IP literal or
    /// `localhost`, on a request that no proxy passed on, or a listed public URL's host.
    pub fn may_register(host: &str, forwarded: bool, public_hosts: &[String]) -> bool {
        if public_hosts.iter().any(|h| h.eq_ignore_ascii_case(host)) {
            return true;
        }
        if forwarded {
            return false;
        }
        let bare = host
            .rsplit_once(':')
            .filter(|(_, p)| p.chars().all(|c| c.is_ascii_digit()))
            .map(|(h, _)| h)
            .unwrap_or(host);
        let bare = bare.trim_start_matches('[').trim_end_matches(']');
        bare.eq_ignore_ascii_case("localhost") || bare.parse::<IpAddr>().is_ok()
    }
}

/// Write credentials to `path`, readable by the owner only.
pub fn save_credentials(path: &Path, c: &ClientCredentials) -> Result<(), String> {
    let body = serde_json::to_vec_pretty(c).map_err(|e| e.to_string())?;
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    let _ = std::fs::remove_file(&tmp);
    {
        use std::io::Write;
        let mut o = std::fs::OpenOptions::new();
        o.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            o.mode(0o600);
        }
        let mut f = o
            .open(&tmp)
            .map_err(|e| format!("{}: {e}", tmp.display()))?;
        f.write_all(&body)
            .and_then(|_| f.sync_all())
            .map_err(|e| format!("{}: {e}", tmp.display()))?;
    }
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_flow_works_once_and_only_with_its_binding() {
        let dir = tempfile::tempdir().unwrap();
        let c = SsoClient::open(dir.path());
        let s = c.begin("http://h/admin/sso-callback", None).unwrap();
        assert_eq!(
            s.challenge,
            pkce_challenge(&{
                let f = c.flows.lock().unwrap();
                f[&s.state].verifier.clone()
            })
        );
        assert!(c.finish(&s.state, Some("not-it")).is_err());
        // a wrong binding used the state up too
        assert!(c.finish(&s.state, Some(&s.binding)).is_err());
        let s = c.begin("http://h/admin/sso-callback", None).unwrap();
        let f = c.finish(&s.state, Some(&s.binding)).unwrap();
        assert_eq!(f.redirect_uri, "http://h/admin/sso-callback");
        assert!(c.finish(&s.state, Some(&s.binding)).is_err(), "single use");
        assert!(c.finish("unknown", None).is_err());
    }

    #[test]
    fn flows_are_limited_per_address() {
        let dir = tempfile::tempdir().unwrap();
        let c = SsoClient::open(dir.path());
        let ip: IpAddr = "10.0.0.9".parse().unwrap();
        for _ in 0..MAX_FLOWS_PER_IP {
            c.begin("http://h/admin/sso-callback", Some(ip)).unwrap();
        }
        assert!(c.begin("http://h/admin/sso-callback", Some(ip)).is_err());
        assert!(c.begin("http://h/admin/sso-callback", None).is_ok());
    }

    #[test]
    fn credentials_are_kept_private() {
        let dir = tempfile::tempdir().unwrap();
        let c = SsoClient::open(dir.path());
        assert!(c.credentials().is_none());
        let creds = ClientCredentials {
            client_id: "router-x".into(),
            client_secret: "s".repeat(43),
            redirect_uris: vec!["http://h/admin/sso-callback".into()],
        };
        c.save(&creds).unwrap();
        assert_eq!(SsoClient::open(dir.path()).credentials(), Some(creds));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let m = std::fs::metadata(dir.path().join(CREDENTIALS_FILE)).unwrap();
            assert_eq!(m.permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn only_unfakeable_hosts_register_themselves() {
        let none: Vec<String> = vec![];
        assert!(SsoClient::may_register("192.0.2.7:8010", false, &none));
        assert!(SsoClient::may_register("localhost:8010", false, &none));
        assert!(SsoClient::may_register("[::1]:8010", false, &none));
        assert!(!SsoClient::may_register("evil.example", false, &none));
        assert!(!SsoClient::may_register("192.0.2.7:8010", true, &none));
        let listed = vec!["router.example.org".to_string()];
        assert!(SsoClient::may_register("router.example.org", true, &listed));
    }
}
