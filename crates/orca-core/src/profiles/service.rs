//! Client of the inference server's profiles API (`ProfileService`): the shared profile list
//! (cached on disk for offline display), the live layout, switch plans and switch jobs.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

use super::catalog::{LiveLayout, ProfileCatalog, SwitchJob, SwitchPlan, SwitchStep};
use crate::config::OrcaConfig;

#[derive(Debug, thiserror::Error)]
pub enum ProfileError {
    /// The API answered with an error (its `error` text, or "<what> failed (<status>)").
    #[error("{0}")]
    Api(String),
    #[error("{0}")]
    Transport(String),
}

pub struct ProfileService {
    http: reqwest::Client,
    base_url: Option<String>,
    cache_file: Option<PathBuf>,
}

/// .NET's `HttpRequestException` text for a non-success status from `GetStringAsync`.
fn status_message(status: reqwest::StatusCode) -> String {
    format!(
        "Response status code does not indicate success: {} ({}).",
        status.as_u16(),
        status.canonical_reason().unwrap_or("")
    )
}

fn transport(e: reqwest::Error) -> ProfileError {
    if e.is_timeout() {
        // HttpClient.Timeout's TaskCanceledException.
        return ProfileError::Transport(
            "The request was canceled due to the configured HttpClient.Timeout elapsing."
                .to_string(),
        );
    }
    ProfileError::Transport(error_chain(&e.without_url()))
}

fn error_chain(e: &dyn std::error::Error) -> String {
    let mut s = e.to_string();
    let mut cur = e.source();
    while let Some(c) = cur {
        let t = c.to_string();
        if !s.contains(&t) {
            s.push_str(": ");
            s.push_str(&t);
        }
        cur = c.source();
    }
    s
}

fn error_of(body: &str) -> Option<String> {
    serde_json::from_str::<Value>(body)
        .ok()?
        .get("error")?
        .as_str()
        .map(str::to_string)
}

impl ProfileService {
    pub fn new(base_url: Option<&str>, cache_file: Option<PathBuf>, timeout: Duration) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(timeout)
                .build()
                .expect("reqwest client builds with static settings"),
            base_url: base_url.map(|u| u.trim_end_matches('/').to_string()),
            cache_file,
        }
    }

    /// The service the config describes (profiles.serverUrl or the registry's host :8098),
    /// cache in the config dir.
    pub fn from_config(config: &OrcaConfig, config_dir: &Path) -> Self {
        Self::new(
            resolve_server_url(config).as_deref(),
            Some(config_dir.join("profiles-cache.json")),
            Duration::from_millis(config.profiles.timeout_ms.max(500) as u64),
        )
    }

    pub fn base_url(&self) -> Option<&str> {
        self.base_url.as_deref()
    }

    pub fn enabled(&self) -> bool {
        self.base_url.is_some()
    }

    /// The profile list: from the API (and saved to the cache), else the cached copy
    /// (`from_cache`), else `None`, with the error when the API could not be used.
    pub async fn get_catalog(&self) -> (Option<ProfileCatalog>, Option<String>) {
        let error = match &self.base_url {
            Some(base) => match self.get_string(&format!("{base}/profiles")).await {
                Ok(json) => match ProfileCatalog::parse(&json) {
                    Ok(mut catalog) => {
                        catalog.fetched_at = Some(std::time::SystemTime::now());
                        self.save_cache(&json);
                        return (Some(catalog), None);
                    }
                    Err(e) => format!("profiles API {base} unreachable: {e}"),
                },
                Err(e) => format!("profiles API {base} unreachable: {e}"),
            },
            None => "no profiles API configured (profiles.serverUrl, or slots.registryUrl's host)"
                .to_string(),
        };
        (self.load_cache(), Some(error))
    }

    async fn get_string(&self, url: &str) -> Result<String, ProfileError> {
        let r = self.http.get(url).send().await.map_err(transport)?;
        if !r.status().is_success() {
            return Err(ProfileError::Transport(status_message(r.status())));
        }
        r.text().await.map_err(transport)
    }

    pub async fn get_live(&self) -> Result<Option<LiveLayout>, ProfileError> {
        let Some(base) = &self.base_url else {
            return Ok(None);
        };
        let text = self.get_string(&format!("{base}/live")).await?;
        serde_json::from_str(&text).map_err(|e| ProfileError::Api(e.to_string()))
    }

    fn base(&self) -> &str {
        self.base_url.as_deref().unwrap_or("")
    }

    pub async fn plan(&self, profile: &str) -> Result<SwitchPlan, ProfileError> {
        let url = format!("{}/plan?profile={}", self.base(), escape_data(profile));
        let r = self.http.get(url).send().await.map_err(transport)?;
        let status = r.status();
        let body = r.text().await.map_err(transport)?;
        if !status.is_success() {
            return Err(ProfileError::Api(
                error_of(&body).unwrap_or_else(|| format!("plan failed ({})", status.as_u16())),
            ));
        }
        serde_json::from_str(&body).map_err(|e| ProfileError::Api(e.to_string()))
    }

    async fn post_switch(&self, body: Value) -> Result<(u16, String), ProfileError> {
        let r = self
            .http
            .post(format!("{}/switch", self.base()))
            .header("content-type", "application/json; charset=utf-8")
            .body(body.to_string())
            .send()
            .await
            .map_err(transport)?;
        let status = r.status().as_u16();
        Ok((status, r.text().await.map_err(transport)?))
    }

    fn switch_job(status: u16, text: &str) -> Result<Option<SwitchJob>, ProfileError> {
        if status == 409 {
            return Ok(None);
        }
        if !(200..300).contains(&status) {
            return Err(ProfileError::Api(
                error_of(text).unwrap_or_else(|| format!("switch request failed ({status})")),
            ));
        }
        serde_json::from_str(text)
            .map(Some)
            .map_err(|e| ProfileError::Api(e.to_string()))
    }

    /// Ask the server to switch to the profile's layout: the queued job, a finished one with
    /// `already`, or a refusal (state "refused").
    pub async fn request_switch(
        &self,
        profile: &str,
        requested_by: &str,
        reason: Option<&str>,
    ) -> Result<SwitchJob, ProfileError> {
        let (status, text) = self
            .post_switch(json!({"profile": profile, "requestedBy": requested_by, "reason": reason.unwrap_or("")}))
            .await?;
        Ok(match Self::switch_job(status, &text)? {
            None => SwitchJob {
                state: Some("refused".into()),
                error: error_of(&text),
                profile: Some(profile.into()),
                ..SwitchJob::default()
            },
            Some(mut job) => {
                job.profile.get_or_insert_with(|| profile.to_string());
                job
            }
        })
    }

    /// Switch the server to a layout by its id.
    pub async fn request_layout_switch(
        &self,
        layout: &str,
        requested_by: &str,
        reason: Option<&str>,
    ) -> Result<SwitchJob, ProfileError> {
        let (status, text) = self
            .post_switch(json!({"layout": layout, "requestedBy": requested_by, "reason": reason.unwrap_or("")}))
            .await?;
        Ok(match Self::switch_job(status, &text)? {
            None => SwitchJob {
                state: Some("refused".into()),
                error: error_of(&text),
                layout: Some(layout.into()),
                ..SwitchJob::default()
            },
            Some(mut job) => {
                job.layout.get_or_insert_with(|| layout.to_string());
                job
            }
        })
    }

    /// The profile name for per-card picks: a saved profile with exactly those cards, else a
    /// composed one.
    pub async fn compose(&self, cards: &Value) -> Result<String, ProfileError> {
        let r = self.post("/compose", json!({"cards": cards})).await?;
        r.get("profile")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| ProfileError::Api("compose: no profile in the answer".into()))
    }

    /// Save per-card picks + mode as a named profile.
    #[allow(clippy::too_many_arguments)]
    pub async fn save_profile(
        &self,
        name: &str,
        cards: &Value,
        mode: Option<&str>,
        title: Option<&str>,
        description: Option<&str>,
        overwrite: bool,
    ) -> Result<String, ProfileError> {
        let r = self
            .post(
                "/profiles/save",
                json!({"name": name, "cards": cards, "mode": mode, "title": title, "description": description, "overwrite": overwrite}),
            )
            .await?;
        Ok(r.get("profile")
            .and_then(Value::as_str)
            .unwrap_or(name)
            .to_string())
    }

    pub async fn delete_profile(&self, name: &str) -> Result<(), ProfileError> {
        self.post("/profiles/delete", json!({"name": name}))
            .await
            .map(|_| ())
    }

    async fn post(&self, path: &str, body: Value) -> Result<Value, ProfileError> {
        let Some(base) = &self.base_url else {
            return Err(ProfileError::Api("no profiles API configured".into()));
        };
        let r = self
            .http
            .post(format!("{base}{path}"))
            .header("content-type", "application/json; charset=utf-8")
            .body(body.to_string())
            .send()
            .await
            .map_err(transport)?;
        let status = r.status();
        let text = r.text().await.map_err(transport)?;
        if !status.is_success() {
            return Err(ProfileError::Api(
                error_of(&text).unwrap_or_else(|| format!("{path} failed ({})", status.as_u16())),
            ));
        }
        serde_json::from_str(&text).map_err(|e| ProfileError::Api(e.to_string()))
    }

    pub async fn get_job(&self, id: &str) -> Result<Option<SwitchJob>, ProfileError> {
        let r = self
            .http
            .get(format!("{}/jobs/{}", self.base(), escape_data(id)))
            .send()
            .await
            .map_err(transport)?;
        if r.status().as_u16() == 404 {
            return Ok(None);
        }
        if !r.status().is_success() {
            return Err(ProfileError::Transport(status_message(r.status())));
        }
        let text = r.text().await.map_err(transport)?;
        serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| ProfileError::Transport(e.to_string()))
    }

    /// Follow a job until it finishes; `on_step` gets each new step. Transient API errors are
    /// retried. Gives up after `limit`.
    pub async fn wait_for_job(
        &self,
        job: SwitchJob,
        mut on_step: impl FnMut(&SwitchStep),
        limit: Duration,
        poll: Option<Duration>,
    ) -> SwitchJob {
        let Some(id) = job.id.clone().filter(|_| !job.finished()) else {
            return job;
        };
        let mut seen = 0;
        let end = std::time::Instant::now() + limit;
        let mut current = job;
        while std::time::Instant::now() < end {
            tokio::time::sleep(poll.unwrap_or(Duration::from_secs(2))).await;
            if let Ok(Some(j)) = self.get_job(&id).await {
                while seen < j.steps.len() {
                    on_step(&j.steps[seen]);
                    seen += 1;
                }
                let finished = j.finished();
                current = j;
                if finished {
                    return current;
                }
            }
        }
        if current.error.is_none() {
            current.error = Some(format!(
                "still {} after {:.0} min (the server carries on; /profile shows the live layout)",
                current.state.as_deref().unwrap_or(""),
                limit.as_secs_f64() / 60.0
            ));
        }
        current
    }

    fn save_cache(&self, json: &str) {
        let Some(file) = &self.cache_file else {
            return;
        };
        if let Some(dir) = file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = PathBuf::from(format!("{}.tmp", file.display()));
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(&tmp, file);
        }
    }

    fn load_cache(&self) -> Option<ProfileCatalog> {
        let file = self.cache_file.as_ref()?;
        let text = std::fs::read_to_string(file).ok()?;
        let mut c = ProfileCatalog::parse(text.trim_start_matches('\u{feff}')).ok()?;
        c.from_cache = true;
        c.fetched_at = std::fs::metadata(file).and_then(|m| m.modified()).ok();
        Some(c)
    }
}

/// `ProfilesConfig.ResolveServerUrl`: profiles.serverUrl, else the slots registry's host on
/// port 8098, else `None`.
pub fn resolve_server_url(config: &OrcaConfig) -> Option<String> {
    if let Some(url) = config
        .profiles
        .server_url
        .as_deref()
        .filter(|u| !u.trim().is_empty())
    {
        return Some(url.trim_end_matches('/').to_string());
    }
    let reg = config.slots.as_ref()?.registry_url.as_deref()?;
    if reg.is_empty() {
        return None;
    }
    let u = reqwest::Url::parse(reg).ok()?;
    Some(format!("{}://{}:8098", u.scheme(), u.host_str()?))
}

/// `Uri.EscapeDataString`: everything but RFC 3986 unreserved characters is percent-encoded.
pub fn escape_data(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}
