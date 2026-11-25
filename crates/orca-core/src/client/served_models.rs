//! The model that actually answered each actor's last request (`ServedModels`). A gateway
//! (LiteLLM) echoes the alias in the body but names the real model in `x-litellm-model-name`;
//! a server reached directly may answer with its own model id.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::SystemTime;

/// Response headers that name the model behind an alias; first match wins.
pub const HEADERS: [&str; 2] = ["x-litellm-model-name", "x-served-model"];

/// What served an actor's last request.
#[derive(Debug, Clone, PartialEq)]
pub struct ServedModel {
    /// The model id sent (often a gateway alias).
    pub requested: Option<String>,
    /// The model that answered, when the server said (`None` = it only echoed the alias).
    pub served: Option<String>,
    pub at: SystemTime,
}

#[derive(Default)]
pub struct ServedModels {
    last: Mutex<HashMap<String, ServedModel>>,
    pending_header: Mutex<HashMap<String, String>>,
}

impl ServedModels {
    pub fn new() -> Self {
        Self::default()
    }

    /// What served `actor`'s last request; `None` before its first reply.
    pub fn for_actor(&self, actor: &str) -> Option<ServedModel> {
        self.last.lock().unwrap().get(actor).cloned()
    }

    /// A request is about to be sent: forget a header left over from an earlier one.
    pub fn begin(&self, actor: &str) {
        self.pending_header.lock().unwrap().remove(actor);
    }

    /// The response headers named a served model.
    pub fn note_header(&self, actor: &str, served: &str) {
        self.pending_header
            .lock()
            .unwrap()
            .insert(actor.to_string(), served.to_string());
    }

    /// The reply came: the header's model, else the body's model id when it differs from the
    /// one sent (an echo of the alias says nothing), else unknown. With `direct` (a request
    /// straight to a model server, not a gateway) the id it answers with, or else the id sent,
    /// is the model that served.
    pub fn complete(
        &self,
        actor: &str,
        requested: Option<&str>,
        body_model: Option<&str>,
        direct: bool,
    ) -> ServedModel {
        let header = self.pending_header.lock().unwrap().remove(actor);
        let requested_clean = clean(requested);
        let served = clean(header.as_deref())
            .or_else(|| {
                clean(body_model).filter(|b| {
                    direct
                        || !requested_clean
                            .as_deref()
                            .is_some_and(|r| r.eq_ignore_ascii_case(b))
                })
            })
            .or_else(|| {
                if direct {
                    requested_clean.clone()
                } else {
                    None
                }
            });
        let entry = ServedModel {
            requested: requested.map(str::to_string),
            served,
            at: SystemTime::now(),
        };
        self.last
            .lock()
            .unwrap()
            .insert(actor.to_string(), entry.clone());
        entry
    }
}

/// A model name as shown: LiteLLM's provider prefix ("openai/inkling-2-mini") dropped.
pub fn clean(model: Option<&str>) -> Option<String> {
    let m = model?.trim();
    if m.is_empty() {
        return None;
    }
    match m.rfind('/') {
        Some(slash) if slash < m.len() - 1 => Some(m[slash + 1..].to_string()),
        _ => Some(m.to_string()),
    }
}
