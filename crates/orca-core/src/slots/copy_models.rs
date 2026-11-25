//! The model ids each slot copy serves (`CopyModels`), asked of the copy itself
//! (`GET {baseUrl}/models`), and the copy's chat client that sends the id the copy serves now
//! (`CopyModelChatClient`): ninfer refuses any id but its own (404 `model_not_found`), and a
//! card can be switched to another model at any time. A list is kept until invalidated (a 404
//! model_not_found, a transport failure); a failed lookup is asked again after 30 s.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures_util::future::BoxFuture;

use super::allocator::Clock;
use crate::client::{
    ChatBackend, ChatError, ChatMessage, ChatOptions, ChatResponse, StreamControl, StreamEvent,
    is_transport_failure, server_identity,
};

/// How long "the copy did not list its models" stands before the next request asks again.
pub const RETRY_FAILED_AFTER: Duration = Duration::from_secs(30);
/// One lookup may take this long.
pub const LOOKUP_TIMEOUT: Duration = Duration::from_secs(3);

/// (base URL, key) -> the ids the server lists; `None` when it did not answer.
pub type ModelLister =
    Arc<dyn Fn(String, Option<String>) -> BoxFuture<'static, Option<Vec<String>>> + Send + Sync>;

struct Entry {
    cell: Arc<tokio::sync::OnceCell<Option<Vec<String>>>>,
    at: Instant,
}

pub struct CopyModels {
    list: ModelLister,
    clock: Clock,
    lists: Mutex<HashMap<String, Entry>>,
}

impl Default for CopyModels {
    fn default() -> Self {
        Self::new(None, None)
    }
}

impl CopyModels {
    pub fn new(list: Option<ModelLister>, clock: Option<Clock>) -> Self {
        let list = list.unwrap_or_else(|| {
            let http = reqwest::Client::builder()
                .timeout(LOOKUP_TIMEOUT)
                .build()
                .expect("reqwest client builds with static settings");
            Arc::new(move |url, key| {
                let http = http.clone();
                Box::pin(async move { list_models(&http, &url, key.as_deref()).await })
            })
        });
        Self {
            list,
            clock: clock.unwrap_or_else(|| Arc::new(Instant::now)),
            lists: Mutex::new(HashMap::new()),
        }
    }

    /// The id to send to a copy: `preferred` when the copy lists it (in its spelling), else the
    /// copy's only model; `None` when the copy could not be asked or lists several others.
    pub async fn choose_for(
        &self,
        copy: &str,
        base_url: &str,
        key: Option<&str>,
        preferred: Option<&str>,
    ) -> Option<String> {
        choose(
            self.served(copy, base_url, key).await.as_deref(),
            &[preferred],
        )
    }

    /// The ids the copy lists (asked once, kept); `None` when it could not be asked.
    pub async fn served(
        &self,
        copy: &str,
        base_url: &str,
        key: Option<&str>,
    ) -> Option<Vec<String>> {
        let now = (self.clock)();
        let cell = {
            let mut lists = self.lists.lock().unwrap();
            let k = copy.to_lowercase();
            if let Some(e) = lists.get(&k)
                && matches!(e.cell.get(), Some(None))
                && now.duration_since(e.at) >= RETRY_FAILED_AFTER
            {
                lists.remove(&k);
            }
            lists
                .entry(k)
                .or_insert_with(|| Entry {
                    cell: Arc::new(tokio::sync::OnceCell::new()),
                    at: now,
                })
                .cell
                .clone()
        };
        let list = self.list.clone();
        let (url, key) = (base_url.to_string(), key.map(str::to_string));
        cell.get_or_init(|| async move { list(url, key).await })
            .await
            .clone()
    }

    /// What [`Self::choose_for`] would answer without asking; `None` while unknown.
    pub fn known(&self, copy: &str, preferred: Option<&str>) -> Option<String> {
        let lists = self.lists.lock().unwrap();
        let served = lists.get(&copy.to_lowercase())?.cell.get()?.clone();
        choose(served.as_deref(), &[preferred])
    }

    /// Forget what a copy serves; the next request asks again.
    pub fn invalidate(&self, copy: &str) {
        self.lists.lock().unwrap().remove(&copy.to_lowercase());
    }
}

/// The first of `preferences` the copy lists (in its spelling), else the copy's only model;
/// `None` when it lists nothing (or several, none of them preferred).
pub fn choose(served: Option<&[String]>, preferences: &[Option<&str>]) -> Option<String> {
    let served = served.filter(|s| !s.is_empty())?;
    for p in preferences.iter().flatten() {
        if p.trim().is_empty() {
            continue;
        }
        if let Some(listed) = served.iter().find(|m| m.eq_ignore_ascii_case(p)) {
            return Some(listed.clone());
        }
    }
    (served.len() == 1).then(|| served[0].clone())
}

/// `GET {baseUrl}/models` -> its model ids; `None` when it did not answer with a list.
pub async fn list_models(
    http: &reqwest::Client,
    base_url: &str,
    key: Option<&str>,
) -> Option<Vec<String>> {
    let mut v1 = base_url.trim().trim_end_matches('/').to_string();
    if !v1.to_lowercase().ends_with("/v1") {
        v1.push_str("/v1");
    }
    let mut req = http.get(format!("{v1}/models"));
    if let Some(k) = key.filter(|k| !k.trim().is_empty()) {
        req = req.bearer_auth(k);
    }
    let r = req.send().await.ok()?;
    if !r.status().is_success() {
        return None;
    }
    let ids = server_identity::model_ids(&r.text().await.ok()?);
    (!ids.is_empty()).then_some(ids)
}

/// The server refused the request's model id: 404 with `model_not_found` (ninfer, OpenAI) or
/// "model ... not found" (llama.cpp, LiteLLM).
pub fn is_model_not_found(e: &ChatError) -> bool {
    let ChatError::Http { status, body, .. } = e else {
        return false;
    };
    let message = body.to_lowercase();
    if message.contains("model_not_found") {
        return true;
    }
    *status == 404 && message.contains("model") && message.contains("not found")
}

/// Builds the copy's OpenAI client for one model id.
pub type ModelClientBuilder = Arc<dyn Fn(&str) -> Arc<dyn ChatBackend> + Send + Sync>;

/// The chat client of one slot copy: each request goes out with the id the copy serves; a 404
/// model_not_found (the card was switched since) asks the list again and retries once, before
/// anything has streamed.
pub struct CopyModelClient {
    copy: String,
    base_url: String,
    key: Option<String>,
    preferred: String,
    models: Arc<CopyModels>,
    build: ModelClientBuilder,
    by_model: Mutex<HashMap<String, Arc<dyn ChatBackend>>>,
    last_logged: Mutex<Option<String>>,
}

impl CopyModelClient {
    pub fn new(
        copy: &str,
        base_url: &str,
        key: Option<String>,
        preferred: &str,
        models: Arc<CopyModels>,
        build: ModelClientBuilder,
    ) -> Self {
        Self {
            copy: copy.into(),
            base_url: base_url.into(),
            key,
            preferred: preferred.into(),
            models,
            build,
            by_model: Mutex::new(HashMap::new()),
            last_logged: Mutex::new(None),
        }
    }

    /// The id requests to this copy send now (or would, once asked).
    pub fn known_model(&self) -> Option<String> {
        self.models.known(&self.copy, Some(&self.preferred))
    }

    fn client_for(&self, model: &str) -> Arc<dyn ChatBackend> {
        self.by_model
            .lock()
            .unwrap()
            .entry(model.to_string())
            .or_insert_with(|| (self.build)(model))
            .clone()
    }

    /// The id this request goes out with: the configured one when the copy lists it, else the
    /// request's own id when listed, else the copy's only model; as asked when unknown.
    async fn model(&self, options: &ChatOptions) -> String {
        let asked = options.model.as_deref();
        let served = self
            .models
            .served(&self.copy, &self.base_url, self.key.as_deref())
            .await;
        let model = choose(served.as_deref(), &[Some(&self.preferred), asked])
            .or_else(|| asked.map(str::to_string))
            .unwrap_or_else(|| self.preferred.clone());
        let mut last = self.last_logged.lock().unwrap();
        if last.as_deref() != Some(model.as_str()) {
            *last = Some(model.clone());
            let wanted = asked.unwrap_or(&self.preferred);
            if model != wanted {
                tracing::info!(
                    "Copy {} serves {model}, not {wanted}: its requests use {model}",
                    self.copy
                );
            }
        }
        model
    }

    fn with_model(options: &ChatOptions, model: &str) -> ChatOptions {
        ChatOptions {
            model: Some(model.to_string()),
            ..options.clone()
        }
    }

    /// After a 404 model_not_found for `sent`: the copy's model now, or `None` when that
    /// changes nothing.
    async fn rediscover(&self, sent: &str, options: &ChatOptions, e: &ChatError) -> Option<String> {
        self.models.invalidate(&self.copy);
        let now = self.model(options).await;
        if now == sent {
            tracing::warn!(
                "Copy {} refused model {sent} and lists nothing else to use: {e}",
                self.copy
            );
            return None;
        }
        tracing::info!(
            "Copy {} no longer serves {sent} (404 model_not_found); retrying with {now}",
            self.copy
        );
        Some(now)
    }

    fn note_failure(&self, e: &ChatError) {
        if is_transport_failure(e) {
            self.models.invalidate(&self.copy);
        }
    }
}

#[async_trait]
impl ChatBackend for CopyModelClient {
    async fn complete(
        &self,
        actor: &str,
        messages: &[ChatMessage],
        options: &ChatOptions,
    ) -> Result<ChatResponse, ChatError> {
        let model = self.model(options).await;
        match self
            .client_for(&model)
            .complete(actor, messages, &Self::with_model(options, &model))
            .await
        {
            Ok(r) => Ok(r),
            Err(e) => {
                self.note_failure(&e);
                if !is_model_not_found(&e) {
                    return Err(e);
                }
                let Some(again) = self.rediscover(&model, options, &e).await else {
                    return Err(e);
                };
                self.client_for(&again)
                    .complete(actor, messages, &Self::with_model(options, &again))
                    .await
            }
        }
    }

    async fn stream_with_control(
        &self,
        actor: &str,
        messages: &[ChatMessage],
        options: &ChatOptions,
        on_event: &mut (dyn FnMut(StreamEvent) -> StreamControl + Send),
    ) -> Result<ChatResponse, ChatError> {
        let model = self.model(options).await;
        let mut started = false;
        let result = {
            let mut tracked = |e: StreamEvent| {
                started = true;
                on_event(e)
            };
            self.client_for(&model)
                .stream_with_control(
                    actor,
                    messages,
                    &Self::with_model(options, &model),
                    &mut tracked,
                )
                .await
        };
        match result {
            Ok(r) => Ok(r),
            Err(e) => {
                self.note_failure(&e);
                if started || !is_model_not_found(&e) {
                    return Err(e);
                }
                let Some(again) = self.rediscover(&model, options, &e).await else {
                    return Err(e);
                };
                self.client_for(&again)
                    .stream_with_control(
                        actor,
                        messages,
                        &Self::with_model(options, &again),
                        on_event,
                    )
                    .await
            }
        }
    }
}
