//! `UncensoredGate`: decides, per server, whether the `/uncensored` body field goes on a request
//! (the client asks it only while the switch is on). The field goes to a server whose kind is in
//! `agent.uncensored.serverKinds` (default: Strata), to one that could not be identified and to
//! one that is neither Strata nor ninfer (a gateway in front of them); not to a server identified
//! as ninfer (or Strata) when that kind is not listed: ninfer answers the field with HTTP 400
//! `speed_projection_not_supported`. One gate per client, so in a split layout the field goes
//! only on the Strata copy's requests.
//!
//! Safety net: a server that answers 400 naming the field as unsupported, whatever it was
//! identified as, is remembered for the process ([`UncensoredGate::mark_refused`]); that request
//! is sent once more without the field and the user is told once ([`set_on_field_refused`]).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use super::openai::ChatError;
use super::server_identity::{self, ServerIdentity, ServerKind};
use crate::config::AgentUncensoredConfig;

/// ninfer's error code for the field.
pub const REFUSAL_CODE: &str = "speed_projection_not_supported";

/// Servers (roots, lower case) that refused the field with a 400, and the first line of that
/// answer.
static REFUSED: LazyLock<Mutex<HashMap<String, String>>> = LazyLock::new(Default::default);

type Notice = Arc<dyn Fn(&str) + Send + Sync>;

static ON_FIELD_REFUSED: Mutex<Option<Notice>> = Mutex::new(None);

/// `OnFieldRefused`: called once per server when it refuses the field, with a line for the user
/// (plain text). The REPL prints it; `None` = say nothing.
pub fn set_on_field_refused(notice: Option<Notice>) {
    *ON_FIELD_REFUSED.lock().unwrap() = notice;
}

fn key(server_url: &str) -> String {
    server_identity::root(server_url).to_lowercase()
}

/// `HasRefused`: whether `server_url` refused the field earlier in this process.
pub fn has_refused(server_url: &str) -> bool {
    REFUSED.lock().unwrap().contains_key(&key(server_url))
}

/// Test hook (`ResetRefusals`): forgets every refusal.
pub fn reset_refusals() {
    REFUSED.lock().unwrap().clear();
}

/// `IsFieldRefusal`: a 400 (or 422) whose answer names [`REFUSAL_CODE`], or an error whose
/// message does.
pub fn is_field_refusal(error: &ChatError) -> bool {
    match error {
        ChatError::Http { status, body, .. } => {
            matches!(status, 400 | 422) && contains_ci(body, REFUSAL_CODE)
        }
        other => contains_ci(&other.to_string(), REFUSAL_CODE),
    }
}

/// [`is_field_refusal`] for an error message (what C# checks on exceptions of other types).
pub fn is_field_refusal_message(message: &str) -> bool {
    contains_ci(message, REFUSAL_CODE)
}

fn contains_ci(text: &str, needle: &str) -> bool {
    text.to_lowercase().contains(&needle.to_lowercase())
}

/// The first line of the error as the OpenAI .NET SDK words it: `HTTP 400 (type: code)`.
pub fn first_line(error: &ChatError) -> String {
    let text = match error {
        ChatError::Http { status, body, .. } => {
            let parsed: Option<serde_json::Value> = serde_json::from_str(body).ok();
            let err = parsed.as_ref().map(|v| &v["error"]);
            let kind = err.and_then(|e| e["type"].as_str());
            let code = err.and_then(|e| e["code"].as_str());
            match (kind, code) {
                (Some(t), Some(c)) => format!("HTTP {status} ({t}: {c})"),
                (Some(t), None) => format!("HTTP {status} ({t})"),
                (None, Some(c)) => format!("HTTP {status} ({c})"),
                (None, None) => format!("HTTP {status}"),
            }
        }
        other => other.to_string(),
    };
    text.split('\n')
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("HTTP 400")
        .to_string()
}

/// One client's gate (`UncensoredGate`).
pub struct UncensoredGate {
    uncensored: AgentUncensoredConfig,
    base_url: String,
    api_key: Option<String>,
    /// `None` (demo mode): the field always goes.
    identity: Option<ServerIdentity>,
    awaited: AtomicBool,
}

impl std::fmt::Debug for UncensoredGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UncensoredGate")
            .field("base_url", &self.base_url)
            .field("identity", &self.identity.is_some())
            .finish()
    }
}

impl UncensoredGate {
    pub fn new(
        uncensored: AgentUncensoredConfig,
        base_url: impl Into<String>,
        api_key: Option<String>,
        identity: Option<ServerIdentity>,
    ) -> Self {
        Self {
            uncensored,
            base_url: base_url.into(),
            api_key,
            identity,
            awaited: AtomicBool::new(false),
        }
    }

    /// The gate C# builds for a client of `config` (`config.DemoMode ? null :
    /// ServerIdentity.Shared`).
    pub fn for_config(
        config: &crate::config::OrcaConfig,
        base_url: &str,
        api_key: Option<String>,
    ) -> Arc<Self> {
        Arc::new(Self::new(
            config.agent.uncensored.clone(),
            base_url,
            api_key,
            (!config.demo_mode).then(ServerIdentity::shared),
        ))
    }

    /// `AllowsAsync`: whether the field goes on a request to this gate's server right now. The
    /// first call waits for the server's identity (normally known from startup); later calls use
    /// what is known without waiting.
    pub async fn allows(&self) -> bool {
        if has_refused(&self.base_url) {
            return false;
        }
        let Some(identity) = &self.identity else {
            return true;
        };
        let info = if !self.awaited.swap(true, Ordering::SeqCst) {
            Some(identity.get(&self.base_url, self.api_key.as_deref()).await)
        } else {
            // Asks again in the background once "no answer" is old.
            identity.warm(&self.base_url, self.api_key.as_deref());
            identity.known(&self.base_url)
        };
        self.uncensored
            .is_sent_to(info.map_or(ServerKind::Unknown, |i| i.kind))
    }

    /// `MarkRefused`: remembers that this gate's server refused the field and tells the user
    /// once.
    pub fn mark_refused(&self, error: &ChatError) {
        let first = first_line(error);
        {
            let mut refused = REFUSED.lock().unwrap();
            let k = key(&self.base_url);
            if refused.contains_key(&k) {
                return;
            }
            refused.insert(k, first.clone());
        }
        let field = if self.uncensored.body_field.trim().is_empty() {
            "the /uncensored field".to_string()
        } else {
            self.uncensored.body_field.trim().to_string()
        };
        let notice = ON_FIELD_REFUSED.lock().unwrap().clone();
        if let Some(notice) = notice {
            notice(&format!(
                "Uncensored: the model server at {} does not take {field} ({first}). The request was sent again without it, and requests there stay normal for the rest of this session.",
                self.base_url
            ));
        }
    }
}
