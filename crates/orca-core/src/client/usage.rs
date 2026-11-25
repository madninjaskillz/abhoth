//! Per-request usage: what one request produced and the "Slot usage" log line the bench reads
//! (`RequestStats`, `ServerTimings`), the per-provider totals (`ProviderUsageLedger`) and the
//! backend that logs every request it passes on (`UsageLoggingChatClient`).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use async_trait::async_trait;
use serde_json::Value;

use super::backend::ChatBackend;
use super::{ChatError, ChatMessage, ChatOptions, ChatResponse, StreamControl, StreamEvent, Usage};
use crate::config::ProviderPricing;
use crate::logging::{Level, LogSink};

/// The log category of the provider route (`OpenOrca.Usage`).
pub const USAGE_CATEGORY: &str = "OpenOrca.Usage";
/// The log category of the slot clients (`OpenOrca.Slots`).
pub const SLOTS_CATEGORY: &str = "OpenOrca.Slots";

/// llama.cpp / Strata per-request `timings` (sent next to `usage` on the last chunk): tokens
/// prefilled (`prompt_n`) and reused from the cache (`cache_n`), tokens generated
/// (`predicted_n`), and the server's own prefill and decode times.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ServerTimings {
    pub prompt_n: i32,
    pub cache_n: i32,
    pub prompt_ms: f64,
    pub predicted_n: i32,
    pub predicted_ms: f64,
}

impl ServerTimings {
    /// `ServerTimings.Parse`: `None` unless the text is a JSON object.
    pub fn parse(json: &str) -> Option<Self> {
        Self::from_value(&serde_json::from_str::<Value>(json).ok()?)
    }

    /// The timings in a parsed `timings` field; `None` unless it is an object.
    pub fn from_value(t: &Value) -> Option<Self> {
        if !t.is_object() {
            return None;
        }
        let num = |name: &str| t.get(name).and_then(Value::as_f64).unwrap_or(0.0);
        // `(int)GetDouble()`: truncation, saturating.
        let int = |name: &str| num(name) as i32;
        Some(Self {
            prompt_n: int("prompt_n"),
            cache_n: int("cache_n"),
            prompt_ms: num("prompt_ms"),
            predicted_n: int("predicted_n"),
            predicted_ms: num("predicted_ms"),
        })
    }
}

/// What one LLM request produced, gathered while it streams: hidden reasoning vs visible
/// content vs tool-call output, when the first reasoning and first answer token arrived, which
/// tools it called, and the server's usage and timings. Servers that don't split reasoning out
/// of `completion_tokens` (llama.cpp, Strata) stream one token per chunk, so reasoning and
/// content are counted in chunks; tool-call tokens are the rest of the output.
#[derive(Debug, Clone)]
pub struct RequestStats {
    started: Instant,
    calls: Vec<String>,
    pub usage: Option<Usage>,
    pub timings: Option<ServerTimings>,
    pub reasoning_chunks: i64,
    pub content_chunks: i64,
    pub first_reasoning_ms: Option<i64>,
    pub first_answer_ms: Option<i64>,
    /// The request's reasoning_effort, when it set one (logged as effort=).
    pub effort: Option<String>,
    /// The named provider the request went to (agent.providerSchedule, e.g. "haiku"); `None` =
    /// the default (local) model. Logged as provider= and model=, and the key of its usage in
    /// the ledger.
    pub provider: Option<String>,
    /// The model of `provider` (logged as model=).
    pub model: Option<String>,
    /// Where this request's usage is added up; `None` = [`ProviderUsageLedger::shared`].
    pub ledger: Option<Arc<ProviderUsageLedger>>,
}

impl RequestStats {
    /// Empty stats started now (`new RequestStats(Stopwatch.GetTimestamp())`).
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
            calls: Vec::new(),
            usage: None,
            timings: None,
            reasoning_chunks: 0,
            content_chunks: 0,
            first_reasoning_ms: None,
            first_answer_ms: None,
            effort: None,
            provider: None,
            model: None,
            ledger: None,
        }
    }

    /// `RequestStats.Start`.
    pub fn start(
        options: Option<&ChatOptions>,
        provider: Option<&str>,
        model: Option<&str>,
        ledger: Option<Arc<ProviderUsageLedger>>,
    ) -> Self {
        Self {
            provider: provider.map(str::to_string),
            model: model.map(str::to_string),
            ledger,
            effort: options.and_then(effort_of),
            ..Self::new()
        }
    }

    pub fn calls(&self) -> &[String] {
        &self.calls
    }

    fn elapsed_ms(&self) -> i64 {
        self.started.elapsed().as_millis() as i64
    }

    /// One streamed event (`Observe(ChatResponseUpdate)`).
    pub fn observe_event(&mut self, event: &StreamEvent) {
        match event {
            StreamEvent::Reasoning(r) if !r.is_empty() => {
                self.reasoning_chunks += 1;
                if self.first_reasoning_ms.is_none() {
                    self.first_reasoning_ms = Some(self.elapsed_ms());
                }
            }
            StreamEvent::Text(t) if !t.is_empty() => {
                self.content_chunks += 1;
                if self.first_answer_ms.is_none() {
                    self.first_answer_ms = Some(self.elapsed_ms());
                }
            }
            StreamEvent::Usage(u) => self.usage = Some(*u),
            // The call itself is only surfaced when the stream ends.
            StreamEvent::UpdateEnd {
                tool_call_delta: true,
            } if self.first_answer_ms.is_none() => {
                self.first_answer_ms = Some(self.elapsed_ms());
            }
            _ => {}
        }
    }

    /// The end of a stream (complete, stopped or broken off): its finished tool calls (the C#
    /// client surfaces them at the end) and the last timings.
    pub fn observe_stream_end(&mut self, response: &ChatResponse) {
        if !response.tool_calls.is_empty() && self.first_answer_ms.is_none() {
            self.first_answer_ms = Some(self.elapsed_ms());
        }
        self.calls
            .extend(response.tool_calls.iter().map(|c| c.name.clone()));
        if response.timings.is_some() {
            self.timings = response.timings;
        }
    }

    /// A non-streaming response (`Observe(ChatResponse)`).
    pub fn observe_response(&mut self, response: &ChatResponse) {
        self.usage = response.usage_reported.then_some(response.usage);
        self.calls
            .extend(response.tool_calls.iter().map(|c| c.name.clone()));
        self.timings = response.timings;
    }

    /// (reasoning, content, tool-call) output tokens. The server's own reasoning count wins
    /// when it reports one; chunk counts are scaled down if a server packs several tokens per
    /// chunk.
    pub fn split(&self) -> (i64, i64, i64) {
        let output = self
            .usage
            .and_then(|u| u.completion_tokens)
            .or(self.timings.map(|t| i64::from(t.predicted_n)))
            .unwrap_or(0);
        let mut reasoning = match self.usage.and_then(|u| u.reasoning_tokens) {
            Some(r) if r > 0 => r,
            _ => self.reasoning_chunks,
        };
        let mut content = self.content_chunks;
        if output <= 0 {
            return (reasoning, content, 0);
        }
        if reasoning + content > output {
            let scale = output as f64 / (reasoning + content) as f64;
            reasoning = round_half_even(reasoning as f64 * scale);
            content = output - reasoning;
        }
        (reasoning, content, (output - reasoning - content).max(0))
    }

    /// The "Slot usage ..." message, or `None` when nothing streamed at all.
    pub fn message(&self, route: &str) -> Option<String> {
        if self.usage.is_none()
            && self.reasoning_chunks + self.content_chunks == 0
            && self.calls.is_empty()
        {
            return None;
        }
        let prompt = self.usage.and_then(|u| u.prompt_tokens).unwrap_or(0);
        let cached = self.usage.and_then(|u| u.cached_tokens).unwrap_or(0);
        let (reasoning, content, tool) = self.split();
        let output = self
            .usage
            .and_then(|u| u.completion_tokens)
            .unwrap_or(reasoning + content + tool);
        Some(format!(
            "Slot usage {route}: prompt {prompt}, cached {cached}, re-read {}, output {output}, reasoning {reasoning}, {} ms | {}",
            (prompt - cached).max(0),
            self.elapsed_ms(),
            self.detail(content, tool)
        ))
    }

    /// `RequestStats.Log`: "Slot usage {route}: prompt P, cached C, re-read R, output O,
    /// reasoning X, N ms | content=.. tool=.. first_think_ms=.. first_answer_ms=.. calls=a,b
    /// effort=.. prompt_n=.. cache_n=.. prompt_ms=.. predicted_n=.. predicted_ms=..". A stream
    /// stopped before the server's usage arrived (cancelled, runaway cap) is still logged, from
    /// the chunks seen, with aborted=1. The usage is added to the ledger too.
    pub fn log(&self, sink: Option<&dyn LogSink>, category: &str, route: &str) {
        let Some(message) = self.message(route) else {
            return;
        };
        let prompt = self.usage.and_then(|u| u.prompt_tokens).unwrap_or(0);
        let cached = self.usage.and_then(|u| u.cached_tokens).unwrap_or(0);
        let (reasoning, content, tool) = self.split();
        let output = self
            .usage
            .and_then(|u| u.completion_tokens)
            .unwrap_or(reasoning + content + tool);
        // Cache writes are Anthropic's (CacheCreationInputTokens); no Rust client reports them.
        self.ledger
            .clone()
            .unwrap_or_else(ProviderUsageLedger::shared)
            .record(
                self.provider.as_deref(),
                self.model.as_deref(),
                prompt,
                cached,
                0,
                output,
            );
        if let Some(sink) = sink {
            sink.log(Level::Info, category, &message);
        }
    }

    fn detail(&self, content: i64, tool: i64) -> String {
        let ms = |v: Option<i64>| v.map_or_else(|| "-".to_string(), |v| v.to_string());
        let mut s = format!(
            "content={content} tool={tool} first_think_ms={} first_answer_ms={} calls={}",
            ms(self.first_reasoning_ms),
            ms(self.first_answer_ms),
            if self.calls.is_empty() {
                "-".to_string()
            } else {
                self.calls.join(",")
            }
        );
        if let Some(e) = self.effort.as_deref().filter(|e| !e.is_empty()) {
            s.push_str(" effort=");
            s.push_str(e);
        }
        if let Some(p) = self.provider.as_deref().filter(|p| !p.is_empty()) {
            s.push_str(" provider=");
            s.push_str(p);
        }
        if let Some(m) = self.model.as_deref().filter(|m| !m.is_empty()) {
            s.push_str(" model=");
            s.push_str(m);
        }
        if let Some(t) = self.timings {
            s.push_str(&format!(
                " prompt_n={} cache_n={} prompt_ms={} predicted_n={} predicted_ms={}",
                t.prompt_n,
                t.cache_n,
                format_whole(t.prompt_ms),
                t.predicted_n,
                format_whole(t.predicted_ms)
            ));
        }
        if self.usage.is_none() {
            s.push_str(" aborted=1");
        }
        s
    }
}

impl Default for RequestStats {
    fn default() -> Self {
        Self::new()
    }
}

/// `Math.Round(double)`: to even.
fn round_half_even(v: f64) -> i64 {
    let r = v.round();
    if (v - v.trunc()).abs() == 0.5 && r as i64 % 2 != 0 {
        (r - v.signum()) as i64
    } else {
        r as i64
    }
}

/// .NET's `{0:0}`: rounded half away from zero, no fraction.
fn format_whole(v: f64) -> String {
    let r = v.round();
    if r == 0.0 {
        "0".into()
    } else {
        format!("{r:.0}")
    }
}

/// The request's effort as logged: "off" when it turns thinking off
/// (`chat_template_kwargs.enable_thinking = false`), else its `reasoning_effort`.
fn effort_of(options: &ChatOptions) -> Option<String> {
    let off = options
        .additional
        .get("chat_template_kwargs")
        .and_then(|k| k.get("enable_thinking"))
        .is_some_and(|v| v == &Value::Bool(false));
    if off {
        return Some("off".into());
    }
    match options.additional.get("reasoning_effort")? {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

/// Token totals for one provider (and its cost when it has prices).
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderUsage {
    pub provider: String,
    pub model: Option<String>,
    pub requests: i32,
    pub prompt: i64,
    pub cached: i64,
    pub cache_write: i64,
    pub output: i64,
    pub cost_usd: Option<f64>,
}

impl std::fmt::Display for ProviderUsage {
    /// "haiku (claude-haiku-5-5): 12 requests, prompt 340,000 (cached 300,000, written 20,000),
    /// output 9,000, $0.0412".
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let model = match self.model.as_deref() {
            Some(m) if !m.is_empty() => format!(" ({m})"),
            _ => String::new(),
        };
        let cost = self
            .cost_usd
            .map_or_else(String::new, |c| format!(", ${c:.4}"));
        write!(
            f,
            "{}{model}: {} requests, prompt {} (cached {}, written {}), output {}{cost}",
            self.provider,
            self.requests,
            group(self.prompt),
            group(self.cached),
            group(self.cache_write),
            group(self.output)
        )
    }
}

/// `{0:N0}` in the invariant culture.
fn group(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 { format!("-{out}") } else { out }
}

#[derive(Debug, Default)]
struct Row {
    key: String,
    model: Option<String>,
    requests: i32,
    prompt: i64,
    cached: i64,
    cache_write: i64,
    output: i64,
    cost: f64,
    costed: bool,
}

/// Usage per provider for the session: every logged request adds to its provider's row
/// ("local" = the default model and its slots). A provider with pricing is costed per request
/// (long-context rate included). Keys are case-insensitive.
#[derive(Debug, Default)]
pub struct ProviderUsageLedger {
    rows: Mutex<BTreeMap<String, Row>>,
    prices: Mutex<BTreeMap<String, ProviderPricing>>,
}

/// `AgentConfig.LocalProvider`.
pub const LOCAL_PROVIDER: &str = "local";

impl ProviderUsageLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// The process-wide ledger.
    pub fn shared() -> Arc<Self> {
        static SHARED: OnceLock<Arc<ProviderUsageLedger>> = OnceLock::new();
        SHARED.get_or_init(|| Arc::new(Self::new())).clone()
    }

    /// Prices for a provider's requests from now on (`None` = stop costing it).
    pub fn set_pricing(&self, provider: &str, pricing: Option<ProviderPricing>) {
        let mut prices = self.prices.lock().unwrap();
        let key = provider.to_lowercase();
        match pricing {
            Some(p) => {
                prices.insert(key, p);
            }
            None => {
                prices.remove(&key);
            }
        }
    }

    /// Add one request. `prompt` is the whole prompt (cache reads and writes included).
    pub fn record(
        &self,
        provider: Option<&str>,
        model: Option<&str>,
        prompt: i64,
        cached: i64,
        cache_write: i64,
        output: i64,
    ) {
        let key = match provider {
            Some(p) if !p.trim().is_empty() => p,
            _ => LOCAL_PROVIDER,
        };
        let price = self
            .prices
            .lock()
            .unwrap()
            .get(&key.to_lowercase())
            .cloned();
        let mut rows = self.rows.lock().unwrap();
        let row = rows.entry(key.to_lowercase()).or_insert_with(|| Row {
            key: key.to_string(),
            ..Row::default()
        });
        if row.model.is_none() {
            row.model = model.map(str::to_string);
        }
        row.requests += 1;
        row.prompt += prompt;
        row.cached += cached;
        row.cache_write += cache_write;
        row.output += output;
        if let Some(p) = price {
            row.cost += p.cost(
                (prompt - cached - cache_write).max(0),
                cached,
                cache_write,
                output,
            );
            row.costed = true;
        }
    }

    /// One row per provider that served a request, local first.
    pub fn snapshot(&self) -> Vec<ProviderUsage> {
        let rows = self.rows.lock().unwrap();
        let mut out: Vec<(&String, &Row)> = rows.iter().collect();
        out.sort_by_key(|(k, _)| (k.as_str() != LOCAL_PROVIDER, (*k).clone()));
        out.into_iter()
            .map(|(_, r)| ProviderUsage {
                provider: r.key.clone(),
                model: r.model.clone(),
                requests: r.requests,
                prompt: r.prompt,
                cached: r.cached,
                cache_write: r.cache_write,
                output: r.output,
                cost_usd: r.costed.then(|| (r.cost * 1e6).round() / 1e6),
            })
            .collect()
    }

    /// Total cost of the costed providers (USD).
    pub fn total_cost(&self) -> f64 {
        self.snapshot().iter().filter_map(|u| u.cost_usd).sum()
    }

    pub fn clear(&self) {
        self.rows.lock().unwrap().clear();
        self.prices.lock().unwrap().clear();
    }
}

/// `UsageLoggingChatClient`: logs each request's server-reported usage (see
/// [`RequestStats`]) so prefix-cache behaviour and thinking cost are measurable on any path,
/// and passes everything through unchanged.
pub struct UsageLoggingBackend {
    inner: Arc<dyn ChatBackend>,
    sink: Option<Arc<dyn LogSink>>,
    category: String,
    route: String,
    provider: Option<String>,
    model: Option<String>,
    ledger: Option<Arc<ProviderUsageLedger>>,
}

impl UsageLoggingBackend {
    /// `route`: "provider" (shared by every actor: logged as "{actor} on provider") or a
    /// fixed one such as "lead on CPU".
    pub fn new(
        inner: Arc<dyn ChatBackend>,
        sink: Option<Arc<dyn LogSink>>,
        category: &str,
        route: &str,
    ) -> Self {
        Self {
            inner,
            sink,
            category: category.to_string(),
            route: route.to_string(),
            provider: None,
            model: None,
            ledger: None,
        }
    }

    /// A named provider and its model (mixed swarm), and the ledger to add up in.
    pub fn with_provider(
        mut self,
        provider: Option<&str>,
        model: Option<&str>,
        ledger: Option<Arc<ProviderUsageLedger>>,
    ) -> Self {
        self.provider = provider.map(str::to_string);
        self.model = model.map(str::to_string);
        self.ledger = ledger;
        self
    }

    /// The route as logged: shared routes (no " on ") get the actor in front ("lead" for the
    /// Lead, whatever case the caller used).
    pub fn route(&self, actor: &str) -> String {
        if self.route.contains(" on ") {
            return self.route.clone();
        }
        let actor = if actor.is_empty() || actor.eq_ignore_ascii_case("lead") {
            "lead"
        } else {
            actor
        };
        format!("{actor} on {}", self.route)
    }

    fn stats(&self, options: &ChatOptions) -> RequestStats {
        RequestStats::start(
            Some(options),
            self.provider.as_deref(),
            self.model.as_deref(),
            self.ledger.clone(),
        )
    }

    fn log(&self, stats: &RequestStats, actor: &str) {
        stats.log(self.sink.as_deref(), &self.category, &self.route(actor));
    }
}

#[async_trait]
impl ChatBackend for UsageLoggingBackend {
    async fn complete(
        &self,
        actor: &str,
        messages: &[ChatMessage],
        options: &ChatOptions,
    ) -> Result<ChatResponse, ChatError> {
        let mut stats = self.stats(options);
        let r = self.inner.complete(actor, messages, options).await?;
        stats.observe_response(&r);
        self.log(&stats, actor);
        Ok(r)
    }

    async fn stream_with_control(
        &self,
        actor: &str,
        messages: &[ChatMessage],
        options: &ChatOptions,
        on_event: &mut (dyn FnMut(StreamEvent) -> StreamControl + Send),
    ) -> Result<ChatResponse, ChatError> {
        let mut stats = self.stats(options);
        let guard = StreamLog {
            owner: self,
            actor,
            stats: &mut stats,
        };
        let result = {
            let stats = &mut *guard.stats;
            let mut observe = |e: StreamEvent| {
                stats.observe_event(&e);
                on_event(e)
            };
            self.inner
                .stream_with_control(actor, messages, options, &mut observe)
                .await
        };
        match &result {
            Ok(r) => guard.stats.observe_stream_end(r),
            Err(ChatError::Interrupted { partial, .. }) => guard.stats.observe_stream_end(partial),
            Err(_) => {}
        }
        drop(guard);
        result
    }
}

/// Logs the stream's stats when dropped: also when the caller drops the request (cancel).
struct StreamLog<'a> {
    owner: &'a UsageLoggingBackend,
    actor: &'a str,
    stats: &'a mut RequestStats,
}

impl Drop for StreamLog<'_> {
    fn drop(&mut self) {
        self.owner.log(self.stats, self.actor);
    }
}

#[cfg(test)]
#[path = "usage_tests.rs"]
mod tests;
