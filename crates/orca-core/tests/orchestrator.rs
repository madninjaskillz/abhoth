//! Ports of the C# tests that run the agent orchestrator:
//! `AgentOrchestratorTests`, `AgentSwarmTests` and the orchestrator-dependent tests of
//! `AgentSupportTests`, `AgentFooterTests`, `PeerReplyGuardTests` and `AgentUserMessageTests`.
//! (Their pure-function tests live in `orchestration_support.rs`.)

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, SystemTime};

use orca_abstractions::ToolResult;
use orca_core::chat::{CompactionReport, Conversation, ConversationCompactor};
use orca_core::client::{
    ChatError, ChatMessage, ChatOptions, ChatResponse, Role, StreamControl, StreamEvent, ToolCall,
    ToolDefinition, Usage,
};
use orca_core::config::{ContextConfig, OrcaConfig, ProviderConfig};
use orca_core::orchestration::palette::MODEL_HOLD;
use orca_core::orchestration::step_budget::StepBudget;
use orca_core::orchestration::transcript::AgentTranscriptKind;
use orca_core::orchestration::types::{
    AgentOrigin, AgentStatus, AgentTypeDefinition, AgentTypeRegistry,
};
use orca_core::orchestration::*;
use serde_json::{Value, json};

// ── test doubles ─────────────────────────────────────────────────────────────

type BoxFut<T> = Pin<Box<dyn Future<Output = T> + Send>>;
type Responder = Box<
    dyn Fn(Vec<ChatMessage>, ChatOptions, usize) -> BoxFut<Result<ChatResponse, ChatError>>
        + Send
        + Sync,
>;

/// A chat client driven by a function (C#'s `FuncChatClient` / `SequenceChatClient`): records
/// every request; streaming replays the response as one update.
struct FuncClient {
    respond: Responder,
    calls: AtomicUsize,
    streaming_calls: AtomicUsize,
    requests: Mutex<Vec<Vec<ChatMessage>>>,
    options: Mutex<Vec<ChatOptions>>,
}

impl FuncClient {
    fn new(
        f: impl Fn(Vec<ChatMessage>, ChatOptions, usize) -> BoxFut<Result<ChatResponse, ChatError>>
        + Send
        + Sync
        + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            respond: Box::new(f),
            calls: AtomicUsize::new(0),
            streaming_calls: AtomicUsize::new(0),
            requests: Mutex::new(Vec::new()),
            options: Mutex::new(Vec::new()),
        })
    }

    /// A synchronous responder: (messages of this request, options, call index).
    fn seq(
        f: impl Fn(&[ChatMessage], &ChatOptions, usize) -> ChatResponse + Send + Sync + 'static,
    ) -> Arc<Self> {
        Self::new(move |m, o, n| {
            let r = f(&m, &o, n);
            Box::pin(async move { Ok(r) })
        })
    }

    /// Always answers `text`.
    fn stub(answer: &str) -> Arc<Self> {
        let answer = answer.to_string();
        Self::seq(move |_, _, _| text(&answer))
    }

    fn requests(&self) -> Vec<Vec<ChatMessage>> {
        self.requests.lock().unwrap().clone()
    }

    fn options(&self) -> Vec<ChatOptions> {
        self.options.lock().unwrap().clone()
    }

    fn start(
        &self,
        messages: &[ChatMessage],
        options: &ChatOptions,
    ) -> BoxFut<Result<ChatResponse, ChatError>> {
        self.requests.lock().unwrap().push(messages.to_vec());
        self.options.lock().unwrap().push(options.clone());
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        (self.respond)(messages.to_vec(), options.clone(), n)
    }
}

#[async_trait::async_trait]
impl AgentChatClient for FuncClient {
    async fn complete(
        &self,
        _actor: &str,
        messages: &[ChatMessage],
        options: &ChatOptions,
    ) -> Result<ChatResponse, ChatError> {
        self.start(messages, options).await
    }

    async fn stream(
        &self,
        _actor: &str,
        messages: &[ChatMessage],
        options: &ChatOptions,
        on_event: &mut (dyn FnMut(StreamEvent) -> StreamControl + Send),
    ) -> Result<ChatResponse, ChatError> {
        self.streaming_calls.fetch_add(1, Ordering::SeqCst);
        let mut r = self.start(messages, options).await?;
        if !r.text.is_empty() {
            on_event(StreamEvent::Text(r.text.clone()));
        }
        if on_event(StreamEvent::UpdateEnd {
            tool_call_delta: !r.tool_calls.is_empty(),
        }) == StreamControl::Stop
        {
            r.stopped = true;
        }
        Ok(r)
    }
}

fn text(t: &str) -> ChatResponse {
    ChatResponse {
        text: t.to_string(),
        content_items: 1,
        text_parts: 1,
        ..ChatResponse::default()
    }
}

static CALL_IDS: AtomicUsize = AtomicUsize::new(0);

fn call(tool: &str, args: Value) -> ChatResponse {
    let id = format!("c{:06}", CALL_IDS.fetch_add(1, Ordering::SeqCst));
    call_id(&id, tool, args, "")
}

fn call_id(id: &str, tool: &str, args: Value, narration: &str) -> ChatResponse {
    ChatResponse {
        text: narration.to_string(),
        tool_calls: vec![ToolCall {
            id: id.to_string(),
            name: tool.to_string(),
            arguments: args.to_string(),
        }],
        content_items: 2,
        text_parts: 1,
        ..ChatResponse::default()
    }
}

fn tool(name: &str) -> ToolDefinition {
    ToolDefinition {
        name: name.to_string(),
        description: String::new(),
        parameters: json!({"type": "object", "properties": {}}),
    }
}

fn tools(names: &[&str]) -> Vec<ToolDefinition> {
    names.iter().map(|n| tool(n)).collect()
}

fn task_of(messages: &[ChatMessage]) -> String {
    messages
        .iter()
        .find(|m| m.role == Role::User)
        .and_then(|m| m.content.clone())
        .unwrap_or_default()
}

fn text_of(m: &ChatMessage) -> &str {
    m.content.as_deref().unwrap_or("")
}

fn has_tool_message(messages: &[ChatMessage]) -> bool {
    messages.iter().any(|m| m.role == Role::Tool)
}

/// A tool pipeline driven by an async function of (tool name, JSON arguments).
struct FnExecutor(Box<dyn Fn(String, String) -> BoxFut<String> + Send + Sync>);

#[async_trait::async_trait]
impl SubAgentToolExecutor for FnExecutor {
    async fn execute(&self, tool_name: &str, args_json: &str) -> ToolResult {
        ToolResult::success((self.0)(tool_name.to_string(), args_json.to_string()).await)
    }
}

fn executor_async(
    f: impl Fn(String, String) -> BoxFut<String> + Send + Sync + 'static,
) -> Option<Arc<dyn SubAgentToolExecutor>> {
    Some(Arc::new(FnExecutor(Box::new(f))))
}

fn executor(
    f: impl Fn(&str, &str) -> String + Send + Sync + 'static,
) -> Option<Arc<dyn SubAgentToolExecutor>> {
    executor_async(move |n, a| {
        let r = f(&n, &a);
        Box::pin(async move { r })
    })
}

/// A one-shot gate (`TaskCompletionSource`).
#[derive(Clone)]
struct Gate(Arc<tokio::sync::watch::Sender<bool>>);

impl Gate {
    fn new() -> Self {
        Self(Arc::new(tokio::sync::watch::channel(false).0))
    }

    fn open(&self) {
        self.0.send_replace(true);
    }

    async fn wait(&self) {
        let mut rx = self.0.subscribe();
        let _ = rx.wait_for(|v| *v).await;
    }
}

async fn wait_until(cond: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !cond() {
        assert!(std::time::Instant::now() < deadline, "condition not met");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn orchestrator(client: Arc<dyn AgentChatClient>, config: OrcaConfig) -> Arc<AgentOrchestrator> {
    AgentOrchestrator::new(
        Arc::new(DefaultClientProvider::new(client)),
        config,
        AgentTypeRegistry::new(),
        std::env::temp_dir(),
    )
}

/// `AgentSupportTests.Create`: native tool calling on.
fn native(client: Arc<dyn AgentChatClient>, config: Option<OrcaConfig>) -> Arc<AgentOrchestrator> {
    let mut config = config.unwrap_or_default();
    config.lm_studio.native_tool_calling = true;
    orchestrator(client, config)
}

fn default_orch() -> Arc<AgentOrchestrator> {
    orchestrator(FuncClient::stub("Done."), OrcaConfig::default())
}

fn none() -> CancelToken {
    CancelToken::new()
}

async fn spawn_at(
    o: &Arc<AgentOrchestrator>,
    task: &str,
    ty: &str,
    depth: i32,
) -> Arc<AgentContext> {
    o.spawn_agent_with(task, ty, depth, &none(), SpawnOptions::default())
        .await
}

async fn spawn_opts(
    o: &Arc<AgentOrchestrator>,
    task: &str,
    options: SpawnOptions,
) -> Arc<AgentContext> {
    o.spawn_agent_with(task, "general", 0, &none(), options)
        .await
}

fn retain() -> SpawnOptions {
    SpawnOptions {
        retain_conversation: true,
        ..SpawnOptions::default()
    }
}

fn specs(tasks: &[&str]) -> Vec<ParallelAgentSpec> {
    tasks
        .iter()
        .map(|t| ParallelAgentSpec::new(*t, "general"))
        .collect()
}

fn spec(
    task: &str,
    label: Option<&str>,
    size: Option<i32>,
    after: Option<Vec<usize>>,
) -> ParallelAgentSpec {
    ParallelAgentSpec {
        task: task.into(),
        agent_type: "general".into(),
        label: label.map(str::to_string),
        size,
        after,
    }
}

fn result(c: &AgentContext) -> String {
    c.result().unwrap_or_default()
}

fn kinds_of(c: &AgentContext) -> Vec<AgentTranscriptKind> {
    c.transcript.snapshot().iter().map(|e| e.kind).collect()
}

/// `RecordingCompactor`: counts calls and replaces all but the last message with a summary.
#[derive(Default)]
struct RecordingCompactor {
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl ConversationCompactor for RecordingCompactor {
    async fn compact_conversation(
        &self,
        conversation: &mut Conversation,
        _extra_instructions: Option<&str>,
        _context: Option<&ContextConfig>,
    ) -> CompactionReport {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let count = conversation.messages().len();
        if count > 1 {
            conversation.apply_compaction("summary", count - 1, 0);
        }
        CompactionReport::default()
    }
}

// ── AgentOrchestratorTests ───────────────────────────────────────────────────

#[tokio::test]
async fn spawn_agent_with_type_sets_agent_type() {
    let o = default_orch();
    let c = o.spawn_agent("test task", "explore").await;
    assert_eq!(c.agent_type, "explore");
}

#[tokio::test]
async fn spawn_agent_default_type_is_general() {
    let o = default_orch();
    let c = o.spawn_agent("test task", "general").await;
    assert_eq!(c.agent_type, "general");
}

#[tokio::test]
async fn spawn_agent_unknown_type_falls_back_to_general() {
    let o = default_orch();
    let c = o.spawn_agent("test task", "nonexistent").await;
    assert_eq!(c.agent_type, "general");
}

#[test]
fn filter_tools_for_type_unrestricted_returns_all_tools() {
    let o = default_orch();
    o.set_tools(tools(&["read_file", "bash", "write_file"]));
    let general = o.type_registry().get_default();
    assert_eq!(o.filter_tools_for_type(&general).len(), 3);
}

#[test]
fn filter_tools_for_type_restricted_filters_correctly() {
    let o = default_orch();
    o.set_tools(tools(&["read_file", "bash", "glob", "write_file"]));
    let explore = o.type_registry().resolve("explore").unwrap();
    let filtered = o.filter_tools_for_type(&explore);
    assert_eq!(filtered.len(), 2);
    assert!(
        filtered
            .iter()
            .all(|t| t.name == "read_file" || t.name == "glob")
    );
}

#[test]
fn filter_tools_for_type_null_tools_returns_empty() {
    let o = default_orch();
    o.set_tools(Vec::new());
    let explore = o.type_registry().resolve("explore").unwrap();
    assert!(o.filter_tools_for_type(&explore).is_empty());
}

#[test]
fn is_tool_allowed_unrestricted_type_allows_anything() {
    let general = AgentTypeRegistry::new().get_default();
    assert!(is_tool_allowed(&general, "bash"));
    assert!(is_tool_allowed(&general, "write_file"));
    assert!(is_tool_allowed(&general, "anything"));
}

#[test]
fn is_tool_allowed_restricted_type_allows_only_listed() {
    let explore = AgentTypeRegistry::new().resolve("explore").unwrap();
    assert!(is_tool_allowed(&explore, "read_file"));
    assert!(is_tool_allowed(&explore, "glob"));
    assert!(!is_tool_allowed(&explore, "bash"));
    assert!(!is_tool_allowed(&explore, "write_file"));
}

#[test]
fn is_tool_allowed_is_case_insensitive() {
    let explore = AgentTypeRegistry::new().resolve("explore").unwrap();
    assert!(is_tool_allowed(&explore, "READ_FILE"));
    assert!(is_tool_allowed(&explore, "Glob"));
}

#[tokio::test]
async fn spawn_parallel_two_agents_both_complete() {
    let o = default_orch();
    let results = o
        .spawn_parallel(&specs(&["task 1", "task 2"]), 0, &none())
        .await
        .unwrap();
    assert_eq!(results.len(), 2);
    assert!(results.iter().all(|r| r.status() == AgentStatus::Completed));
}

#[test]
fn start_order_biggest_size_first_stable_and_unsized_last_else_as_listed() {
    assert_eq!(
        start_order(&[
            spec("a", None, Some(50), None),
            spec("b", None, None, None),
            spec("c", None, Some(300), None),
            spec("d", None, Some(50), None),
        ]),
        vec![2, 0, 3, 1]
    );
    assert_eq!(start_order(&specs(&["a", "b", "c"])), vec![0, 1, 2]);
}

#[tokio::test]
async fn spawn_parallel_starts_the_biggest_first_results_in_batch_order() {
    let mut config = OrcaConfig::default();
    config.agent.max_concurrent = 1; // one at a time, so the start order is observable
    let o = orchestrator(FuncClient::stub("Done."), config);
    let results = o
        .spawn_parallel(
            &[
                spec("small", None, Some(10), None),
                spec("big", None, Some(400), None),
            ],
            0,
            &none(),
        )
        .await
        .unwrap();
    assert_eq!(
        results.iter().map(|r| r.task.as_str()).collect::<Vec<_>>(),
        ["small", "big"]
    );
    assert!(
        results[1].number < results[0].number,
        "the big task was spawned first"
    );
    assert!(results.iter().all(|r| !r.heavy)); // no sticky slots: no heavy placement
}

#[tokio::test]
async fn spawn_parallel_agents_have_isolated_conversations() {
    let o = default_orch();
    let results = o
        .spawn_parallel(&specs(&["task A", "task B"]), 0, &none())
        .await
        .unwrap();
    assert_eq!(results.len(), 2);
    assert!(!Arc::ptr_eq(&results[0], &results[1]));
    assert_ne!(results[0].id, results[1].id);
}

#[tokio::test]
async fn spawn_agent_at_max_depth_returns_failed_context() {
    let mut config = OrcaConfig::default();
    config.agent.max_spawn_depth = 2;
    let o = orchestrator(FuncClient::stub("Done."), config);
    let c = spawn_at(&o, "test task", "general", 2).await;
    assert_eq!(c.status(), AgentStatus::Failed);
    assert!(c.error().unwrap().contains("Maximum agent spawn depth"));
    assert!(c.result().is_some());
}

#[tokio::test]
async fn spawn_agent_below_max_depth_succeeds() {
    let mut config = OrcaConfig::default();
    config.agent.max_spawn_depth = 3;
    let o = orchestrator(FuncClient::stub("Done."), config);
    let c = spawn_at(&o, "test task", "general", 1).await;
    assert_eq!(c.status(), AgentStatus::Completed);
}

#[tokio::test]
async fn spawn_agent_default_depth_zero_succeeds() {
    let o = default_orch();
    let c = o.spawn_agent("test task", "general").await;
    assert_eq!(c.status(), AgentStatus::Completed);
}

fn slow_client(delay: Duration) -> Arc<FuncClient> {
    FuncClient::new(move |_, _, _| {
        Box::pin(async move {
            tokio::time::sleep(delay).await;
            Ok(text("Done."))
        })
    })
}

#[tokio::test]
async fn spawn_agent_timed_out_reports_timeout() {
    let mut config = OrcaConfig::default();
    config.agent.timeout_seconds = 1;
    let o = orchestrator(slow_client(Duration::from_secs(10)), config);
    *o.timeout_report_grace.lock().unwrap() = Duration::from_millis(200); // the report request is slow too

    let c = spawn_at(&o, "test task", "general", 0).await;

    // Out of time is INCOMPLETE, not failed: the fallback report says how far it got.
    assert_eq!(c.status(), AgentStatus::CompletedPartial);
    assert_eq!(c.state().time_limit_seconds, Some(1));
    assert!(
        result(&c).starts_with("Agent timed out after 1 seconds"),
        "{}",
        result(&c)
    );
    assert!(c.completed_at().is_some());
}

#[tokio::test]
async fn spawn_agent_time_limit_closes_the_cut_off_step_asks_for_the_report_and_a_follow_up_continues()
 {
    let mut config = OrcaConfig::default();
    config.agent.timeout_seconds = 1;
    config.lm_studio.native_tool_calling = true;
    let report_saw_closed_call = Arc::new(Mutex::new(None::<bool>));
    let seen = report_saw_closed_call.clone();
    let client = FuncClient::seq(move |messages, _, n| {
        let last = text_of(messages.last().unwrap());
        if last.contains("You have reached your time limit (1 s)") {
            // The cut-off write got a result, so this request is valid.
            let closed = messages.iter().any(|m| {
                m.role == Role::Tool
                    && m.tool_call_id.as_deref() == Some("w0")
                    && text_of(m).contains("time limit (1 s) was reached")
            });
            *seen.lock().unwrap() = Some(closed);
            return text("## Completed\nInput.cs\n## Remaining\nGridView.cs");
        }
        if last.starts_with("Continue") {
            return text("Wrote GridView.cs.");
        }
        call_id(&format!("w{n}"), "write", json!({"path": "Input.cs"}), "")
    });
    let o = orchestrator(client, config);
    o.set_tools(tools(&["write"]));
    // The first write outlasts the agent's time limit.
    o.set_executor(executor_async(|_, _| {
        Box::pin(async {
            tokio::time::sleep(Duration::from_secs(10)).await;
            "ok".to_string()
        })
    }));

    let c = spawn_opts(&o, "build the app engine", retain()).await;

    assert_eq!(*report_saw_closed_call.lock().unwrap(), Some(true));
    assert_eq!(c.status(), AgentStatus::CompletedPartial);
    assert!(c.stopped_at_time_limit());
    assert_eq!(c.limit_description(), "time limit (1 s, 1 steps)");
    assert!(
        result(&c).starts_with("[Partial result \u{2014} time limit reached]"),
        "{}",
        result(&c)
    );
    assert!(result(&c).contains("## Remaining\nGridView.cs"));

    // C# also raises agent.timeoutSeconds to 30 here; the Rust orchestrator owns its config, and
    // the follow-up answers at once, well inside the 1 s limit.
    o.set_executor(executor(|_, _| "ok".into()));
    let resumed = o
        .follow_up(&c.letter().to_string(), "Continue: GridView.cs", &none())
        .await
        .unwrap();
    assert_eq!(resumed.status(), AgentStatus::Completed);
    assert!(!resumed.stopped_at_time_limit());
    assert_eq!(result(&resumed).trim(), "Wrote GridView.cs.");
}

async fn follow_up_compaction(long_pause: bool) {
    let mut config = OrcaConfig::default();
    config.lm_studio.native_tool_calling = true;
    config.context.context_window_size = 131072;
    config.context.auto_detect_context_window = false;
    config.context.keep_recent_tokens = 2000;
    let client = FuncClient::seq(|_, _, n| {
        if n < 8 {
            let mut r = call_id(
                &format!("b{n}"),
                "bash",
                json!({"command": format!("build {n}")}),
                "",
            );
            r.reasoning = "r".repeat(4000);
            return r;
        }
        text("Done.")
    });
    let o = orchestrator(client, config);
    *o.resume_compaction_idle.lock().unwrap() = if long_pause {
        Duration::ZERO
    } else {
        Duration::from_secs(3600)
    };
    o.set_tools(tools(&["bash"]));
    o.set_executor(executor(|_, _| "o".repeat(3000)));

    let c = spawn_opts(&o, "build the UI", retain()).await;
    let reasoning = |c: &AgentContext| {
        c.messages()
            .iter()
            .filter(|m| m.reasoning.as_deref().is_some_and(|r| !r.is_empty()))
            .count()
    };
    let before = reasoning(&c);

    let resumed = o
        .follow_up(&c.letter().to_string(), "Fix the resize", &none())
        .await
        .unwrap();

    let after = reasoning(&resumed);
    if long_pause {
        assert!(
            after < before,
            "old reasoning should be dropped before the follow-up ({after} vs {before})"
        );
    } else {
        assert_eq!(before, after);
    }
}

#[tokio::test]
async fn follow_up_compacts_old_reasoning_only_after_a_long_pause_long() {
    follow_up_compaction(true).await;
}

#[tokio::test]
async fn follow_up_compacts_old_reasoning_only_after_a_long_pause_short() {
    follow_up_compaction(false).await;
}

#[tokio::test]
async fn spawn_agent_cancelled_reports_cancelled() {
    let o = orchestrator(slow_client(Duration::from_secs(30)), OrcaConfig::default());
    let ct = CancelToken::new();
    let cancel = ct.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel.cancel();
    });
    let c = o
        .spawn_agent_with("test task", "general", 0, &ct, SpawnOptions::default())
        .await;
    assert_eq!(c.status(), AgentStatus::Cancelled);
    assert!(c.completed_at().is_some());
}

#[tokio::test]
async fn spawn_agent_sets_completed_at_on_success() {
    let o = default_orch();
    let c = o.spawn_agent("test task", "general").await;
    assert_eq!(c.status(), AgentStatus::Completed);
    assert!(c.completed_at().is_some());
}

#[tokio::test]
async fn get_all_agents_returns_snapshot_of_all_agents() {
    let o = default_orch();
    o.spawn_agent("task 1", "general").await;
    o.spawn_agent("task 2", "general").await;
    assert_eq!(o.get_all_agents().len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spawn_parallel_concurrent_access_does_not_corrupt() {
    let o = default_orch();
    let tasks: Vec<String> = (0..20).map(|i| format!("task {i}")).collect();
    let specs: Vec<ParallelAgentSpec> = tasks
        .iter()
        .map(|t| ParallelAgentSpec::new(t.as_str(), "general"))
        .collect();
    let results = o.spawn_parallel(&specs, 0, &none()).await.unwrap();
    assert_eq!(results.len(), 20);
    assert!(results.iter().all(|r| r.status() == AgentStatus::Completed));
    assert_eq!(o.get_all_agents().len(), 20);
}

#[tokio::test]
async fn spawn_agent_multi_message_response_executes_tool_call_after_text_message() {
    // One response with narration and a tool call (C#: two messages in one response).
    let client = FuncClient::seq(|_, _, n| {
        if n == 0 {
            return call_id(
                "c1",
                "read_file",
                json!({"path": "x"}),
                "Let me check something.",
            );
        }
        text("Final answer.")
    });
    let o = orchestrator(client, OrcaConfig::default());
    o.set_tools(tools(&["read_file"]));
    let c = o.spawn_agent("task", "general").await;
    assert_eq!(c.status(), AgentStatus::Completed);
    assert_eq!(result(&c), "Final answer.");
    assert_eq!(c.iteration_count(), 2);
}

/// A tool pipeline that answers every call with a PNG (a sub-agent reading a screenshot).
struct ImageExecutor;

const SHOT_PNG: [u8; 6] = [0x89, 0x50, 0x4E, 0x47, 1, 2];

#[async_trait::async_trait]
impl SubAgentToolExecutor for ImageExecutor {
    async fn execute(&self, _: &str, _: &str) -> ToolResult {
        ToolResult {
            parts: Some(vec![orca_abstractions::ToolResultPart::Image {
                data: SHOT_PNG.to_vec(),
                mime_type: "image/png".into(),
            }]),
            ..ToolResult::success("Read image file [image/png]")
        }
    }
}

#[tokio::test]
async fn spawn_agent_tool_result_images_reach_the_model_unless_vision_is_off() {
    for (provider, vision, expect_image) in [
        (Some("anthropic"), None, true),
        (None, None, true),
        (None, Some(false), false),
    ] {
        // The image goes back next to the tool result (the OpenAI-compatible client moves it
        // into a user message; see tool_images tests).
        let client = FuncClient::seq(|_, _, n| {
            if n == 0 {
                call_id("c1", "read", json!({"path": "shot.png"}), "")
            } else {
                text("Seen.")
            }
        });
        let mut config = OrcaConfig::default();
        config.lm_studio.native_tool_calling = true;
        config.lm_studio.provider = provider.map(str::to_string);
        config.lm_studio.vision = vision;
        let o = orchestrator(client.clone(), config);
        o.set_tools(tools(&["read"]));
        o.set_executor(Some(Arc::new(ImageExecutor)));

        let c = o.spawn_agent("task", "general").await;

        assert_eq!(c.status(), AgentStatus::Completed);
        let requests = client.requests.lock().unwrap().clone();
        let tool_msgs: Vec<_> = requests[1]
            .iter()
            .filter(|m| m.role == Role::Tool)
            .collect();
        assert_eq!(tool_msgs.len(), 1);
        assert_eq!(tool_msgs[0].tool_call_id.as_deref(), Some("c1"));
        let images: Vec<_> = tool_msgs[0].images().collect();
        if expect_image {
            assert_eq!(images.len(), 1, "{provider:?} {vision:?}");
            assert_eq!(images[0].data, SHOT_PNG.to_vec());
        } else {
            assert!(images.is_empty(), "{provider:?} {vision:?}");
        }
    }
}

#[tokio::test]
async fn spawn_agent_iteration_exhaustion_returns_partial_summary() {
    let mut config = OrcaConfig::default();
    config.agent.max_iterations = 2;
    config.lm_studio.native_tool_calling = true;
    let client = FuncClient::seq(|_, _, n| {
        // The salvage pass is the one carrying the iteration-limit message.
        if n == 2 {
            return text("I inspected two files.");
        }
        call_id(&format!("c{n}"), "read_file", json!({"path": "x"}), "")
    });
    let o = orchestrator(client.clone(), config);
    o.set_tools(tools(&["read_file"]));

    let c = o.spawn_agent("task", "general").await;

    assert_eq!(c.status(), AgentStatus::CompletedPartial);
    assert!(result(&c).contains("Partial result"));
    assert!(result(&c).contains("I inspected two files."));
    // The salvage request keeps the agent's tools (same prompt cache).
    let options = client.options();
    assert_eq!(options.len(), 3);
    assert_eq!(options[2].tools.len(), 1);
    assert_eq!(options[2].tools[0].name, "read_file");
    assert_eq!(options[0].tools, options[2].tools);
    // ...and only appends to what the previous request sent.
    let requests = client.requests();
    let (before, summary) = (&requests[1], &requests[2]);
    assert!(summary.len() > before.len());
    for (a, b) in before.iter().zip(summary.iter()) {
        assert_eq!(a, b);
    }
}

#[tokio::test]
async fn spawn_agent_text_mode_parses_executes_and_feeds_back_text_tool_calls() {
    let mut config = OrcaConfig::default();
    config.lm_studio.native_tool_calling = false;
    let client = FuncClient::seq(|_, _, n| {
        if n == 0 {
            return text(
                r#"<tool_call>{"name": "read_file", "arguments": {"path": "x"}}</tool_call>"#,
            );
        }
        text("Final answer.")
    });
    let o = orchestrator(client.clone(), config);
    o.set_tools(vec![ToolDefinition {
        name: "read_file".into(),
        description: "Read a file".into(),
        parameters: json!({"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}),
    }]);
    let executed = Arc::new(Mutex::new(Vec::<String>::new()));
    let e = executed.clone();
    o.set_executor(executor(move |name, _| {
        e.lock().unwrap().push(name.to_string());
        "file contents here".into()
    }));
    // C# plugs in a fake ToolListFormatter and TextToolCallParser; the Rust orchestrator uses the
    // real signature formatter and tool_call_parser.

    let c = o.spawn_agent("task", "general").await;

    assert_eq!(c.status(), AgentStatus::Completed);
    assert_eq!(result(&c), "Final answer.");
    assert_eq!(*executed.lock().unwrap(), ["read_file"]);
    // Text mode never sends native tool definitions.
    assert!(client.options().iter().all(|o| o.tools.is_empty()));

    let requests = client.requests();
    let system = requests[0].iter().find(|m| m.role == Role::System).unwrap();
    assert!(text_of(system).contains("TOOLS AVAILABLE"));
    assert!(
        text_of(system).contains("read_file(path: string)"),
        "{}",
        text_of(system)
    );
    assert!(text_of(system).contains("<tool_call>"));
    // The tool result went back as a USER message in the main loop's text format.
    let feedback = requests[1]
        .iter()
        .rev()
        .find(|m| m.role == Role::User)
        .unwrap();
    assert!(text_of(feedback).contains("Here are the results of the tool calls you made:"));
    assert!(text_of(feedback).contains("[Tool result for read_file]"));
    assert!(text_of(feedback).contains("file contents here"));
}

fn record_events(o: &AgentOrchestrator) -> Arc<Mutex<Vec<AgentEvent>>> {
    let events = Arc::new(Mutex::new(Vec::new()));
    let e = events.clone();
    o.subscribe(move |ev| e.lock().unwrap().push(ev.clone()));
    events
}

#[tokio::test]
async fn spawn_agent_raises_lifecycle_events() {
    let mut config = OrcaConfig::default();
    config.lm_studio.native_tool_calling = true;
    let client = FuncClient::seq(|_, _, n| {
        if n == 0 {
            return call_id("c1", "read_file", json!({"path": "x"}), "");
        }
        text("Done.")
    });
    let o = orchestrator(client, config);
    o.set_tools(tools(&["read_file"]));
    o.set_executor(executor(|_, _| "ok".into()));
    let events = record_events(&o);

    let c = o.spawn_agent("task", "general").await;

    assert_eq!(c.status(), AgentStatus::Completed);
    let events = events.lock().unwrap().clone();
    assert!(events.iter().all(|e| e.agent_id() == c.id));
    match &events[0] {
        AgentEvent::Started { context, .. } => assert!(Arc::ptr_eq(context, &c)),
        e => panic!("first event {e:?}"),
    }
    match &events[1] {
        AgentEvent::IterationStarted {
            iteration,
            max_iterations,
            ..
        } => {
            assert_eq!(*iteration, 1);
            assert_eq!(*max_iterations, OrcaConfig::default().agent.max_iterations);
        }
        e => panic!("second event {e:?}"),
    }
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, AgentEvent::IterationStarted { .. }))
            .count(),
        2
    );
    let started: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolCallStarted { tool_name, .. } => Some(tool_name.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(started, ["read_file"]);
    let completed: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolCallCompleted {
                tool_name,
                is_error,
                ..
            } => Some((tool_name.clone(), *is_error)),
            _ => None,
        })
        .collect();
    assert_eq!(completed, [("read_file".to_string(), false)]);
    match events.last().unwrap() {
        AgentEvent::Completed {
            status, iterations, ..
        } => {
            assert_eq!(*status, AgentStatus::Completed);
            assert_eq!(*iterations, 2);
        }
        e => panic!("last event {e:?}"),
    }
}

#[tokio::test]
async fn spawn_agent_throwing_event_handler_does_not_kill_agent() {
    let o = default_orch();
    o.subscribe(|_| panic!("renderer bug"));
    let c = o.spawn_agent("task", "general").await;
    assert_eq!(c.status(), AgentStatus::Completed);
}

async fn step_budget_run(distinct: bool, expected_steps: i32, expected_extensions: usize) {
    let mut config = OrcaConfig::default();
    config.agent.max_iterations = 6;
    config.lm_studio.native_tool_calling = true;
    let summary_call = Arc::new(Mutex::new(None::<usize>));
    let sc = summary_call.clone();
    let client = FuncClient::seq(move |messages, _, n| {
        let last = messages.last().unwrap();
        if last.role == Role::User
            && text_of(last).contains("step limit. Do not call any more tools")
        {
            *sc.lock().unwrap() = Some(n);
            return text("## Completed\nmost\n## Remaining\nthe tests");
        }
        let path = if distinct {
            format!("f{n}.cs")
        } else {
            "same.cs".into()
        };
        call_id(
            &format!("c{n}"),
            "write",
            json!({"path": path, "content": "x"}),
            "",
        )
    });
    let o = orchestrator(client.clone(), config);
    o.set_tools(tools(&["write"]));
    o.set_executor(executor(|_, _| "ok".into()));

    let c = o.spawn_agent("build it", "general").await;

    assert_eq!(c.status(), AgentStatus::CompletedPartial);
    assert_eq!(c.iteration_count(), expected_steps);
    let summary_call = summary_call.lock().unwrap().expect("a summary request");
    assert_eq!(summary_call, expected_steps as usize);
    let requests = client.requests();
    let user_texts: Vec<&str> = requests[summary_call]
        .iter()
        .filter(|m| m.role == Role::User)
        .map(text_of)
        .collect();
    // Warned once per limit, 5 steps before it (before step 2 of 6).
    assert!(
        user_texts
            .iter()
            .any(|t| t.starts_with("[Step budget] 5 steps left (this is step 2 of 6)")),
        "{user_texts:?}"
    );
    assert_eq!(
        user_texts
            .iter()
            .filter(|t| t.contains("more steps (new limit"))
            .count(),
        expected_extensions
    );
    assert!(result(&c).contains("## Remaining"));
}

#[tokio::test]
async fn spawn_agent_step_budget_extends_while_making_progress() {
    // distinct writes: two extensions of half the budget (6 -> 9 -> 12)
    step_budget_run(true, 12, 2).await;
}

#[tokio::test]
async fn spawn_agent_step_budget_no_extension_when_looping() {
    // the same call over and over: looping, no extension
    step_budget_run(false, 6, 0).await;
}

async fn cut_off_run(finish_reason: bool) {
    let mut config = OrcaConfig::default();
    config.agent.max_iterations = 10;
    config.lm_studio.max_tokens = Some(32000);
    config.lm_studio.native_tool_calling = true;
    let saw_notice = Arc::new(Mutex::new(false));
    let sn = saw_notice.clone();
    let client = FuncClient::seq(move |messages, _, n| {
        if n == 0 {
            return ChatResponse {
                finish_reason: finish_reason.then(|| "length".to_string()),
                usage: Usage {
                    completion_tokens: Some(32000),
                    ..Usage::default()
                },
                ..text("")
            };
        }
        if n == 1 {
            *sn.lock().unwrap() = text_of(messages.last().unwrap()).contains(
                "[Output limit] Your last reply was cut off at the output limit (32000 tokens)",
            );
            return call_id("c1", "write", json!({"path": "a.cs", "content": "x"}), "");
        }
        ChatResponse {
            finish_reason: Some("stop".into()),
            ..text("Wrote a.cs.")
        }
    });
    let o = orchestrator(client.clone(), config);
    o.set_tools(tools(&["write"]));
    o.set_executor(executor(|_, _| "ok".into()));

    let c = o.spawn_agent("build the UI", "general").await;

    assert!(
        client
            .options()
            .iter()
            .all(|o| o.max_output_tokens == Some(32000))
    );
    assert!(*saw_notice.lock().unwrap());
    assert_eq!(c.status(), AgentStatus::Completed);
    assert_eq!(c.iteration_count(), 3);
    assert_eq!(result(&c).trim(), "Wrote a.cs.");
}

#[tokio::test]
async fn spawn_agent_reply_cut_off_at_the_output_limit_finish_reason_length() {
    cut_off_run(true).await;
}

#[tokio::test]
async fn spawn_agent_reply_cut_off_at_the_output_limit_usage_only() {
    cut_off_run(false).await;
}

#[test]
fn written_path_only_for_write_and_edit_calls() {
    let cases = [
        ("write", "path", "src/A.cs", Some("src/A.cs")),
        ("edit", "path", "src/B.cs", Some("src/B.cs")),
        ("write_file", "file_path", "c.py", Some("c.py")),
        ("read", "path", "src/A.cs", None),
        ("bash", "command", "echo x > y", None),
    ];
    for (tool, key, value, expected) in cases {
        let args = json!({ key: value }).to_string();
        assert_eq!(written_path(tool, &args).as_deref(), expected, "{tool}");
    }
}

#[test]
fn was_cut_off_only_at_the_limit() {
    let options = ChatOptions {
        max_output_tokens: Some(1000),
        ..ChatOptions::default()
    };
    let with = |finish: Option<&str>, out: Option<i64>| ChatResponse {
        finish_reason: finish.map(str::to_string),
        usage: Usage {
            completion_tokens: out,
            ..Usage::default()
        },
        ..ChatResponse::default()
    };
    assert!(!was_cut_off(&with(Some("stop"), Some(999)), &options));
    assert!(was_cut_off(&with(None, Some(1000)), &options));
    assert!(was_cut_off(
        &with(Some("length"), None),
        &ChatOptions::default()
    ));
    assert!(!was_cut_off(&with(None, None), &ChatOptions::default()));
}

#[test]
fn step_budget_progress_needs_work_calls_and_extensions_are_bounded() {
    let mut budget = StepBudget::new(40, 1);
    for i in 0..6 {
        budget.record("read", &json!({"path": format!("f{i}")}).to_string());
    }
    assert!(!budget.making_progress()); // reading only
    budget.record("bash", &json!({"command": "dotnet test"}).to_string());
    assert!(budget.making_progress());
    assert!(budget.try_extend().is_some());
    assert_eq!(budget.max(), 60);
    assert!(budget.try_extend().is_none()); // only one allowed
    assert!(budget.warning_before(50).is_none());
    assert!(budget.warning_before(56).is_some()); // 5 left of 60
    assert!(budget.warning_before(56).is_none()); // once per limit
    assert!(StepBudget::new(40, 0).try_extend().is_none()); // extensions off
}

#[tokio::test]
async fn spawn_agent_type_max_iterations_override_is_honored() {
    let mut config = OrcaConfig::default();
    config.agent.max_iterations = 10;
    config.lm_studio.native_tool_calling = true;
    let client = FuncClient::seq(|_, _, n| {
        // Calls 0-1 use up the type's 2 iterations; call 2 is the salvage pass.
        if n == 2 {
            return text("partial summary");
        }
        call_id(&format!("c{n}"), "read_file", json!({"path": "x"}), "")
    });
    let o = orchestrator(client, config);
    let mut busy = AgentTypeDefinition::new("busy", "test type", None, "general.md");
    busy.max_iterations = Some(2);
    o.type_registry_mut().register(busy);
    o.set_tools(tools(&["read_file"]));
    o.set_executor(executor(|_, _| "ok".into()));

    let c = o.spawn_agent("task", "busy").await;

    assert_eq!(c.status(), AgentStatus::CompletedPartial);
    assert_eq!(c.iteration_count(), 2);
}

#[tokio::test]
async fn spawn_agent_type_model_without_registry_falls_back_to_default_client() {
    let o = orchestrator(
        FuncClient::stub("From default client."),
        OrcaConfig::default(),
    );
    let mut modeled = AgentTypeDefinition::new("modeled", "test type", None, "general.md");
    modeled.model = Some("some-model".into());
    o.type_registry_mut().register(modeled);

    let c = o.spawn_agent("task", "modeled").await;

    assert_eq!(c.status(), AgentStatus::Completed);
    assert_eq!(result(&c), "From default client.");
}

#[tokio::test]
async fn spawn_agent_unknown_model_provider_falls_back_to_default_client() {
    // C# uses a real ChatClientRegistry with no providers; here the provider can't resolve the
    // reference either (client_for_model = None), and the spawn must not fail.
    let o = orchestrator(
        FuncClient::stub("From default client."),
        OrcaConfig::default(),
    );
    let mut modeled = AgentTypeDefinition::new("modeled", "test type", None, "general.md");
    modeled.model = Some("nosuchprovider/some-model".into());
    o.type_registry_mut().register(modeled);

    let c = o.spawn_agent("task", "modeled").await;

    assert_eq!(c.status(), AgentStatus::Completed);
    assert_eq!(result(&c), "From default client.");
}

#[tokio::test]
async fn spawn_agent_over_context_budget_compacts_and_raises_event() {
    let mut config = OrcaConfig::default();
    config.lm_studio.native_tool_calling = true;
    config.context.auto_compact_enabled = true;
    config.context.context_window_size = 200; // the system prompt alone exceeds it
    config.context.auto_compact_threshold = 0.8;
    let client = FuncClient::seq(|_, _, n| {
        if n == 0 {
            return call_id("c1", "read_file", json!({"path": "x"}), "");
        }
        text("Done.")
    });
    let o = orchestrator(client, config);
    let compactor = Arc::new(RecordingCompactor::default());
    o.set_compactor(Some(compactor.clone()));
    o.set_tools(tools(&["read_file"]));
    o.set_executor(executor(|_, _| "x".repeat(20000)));
    let events = record_events(&o);

    let c = o.spawn_agent("task", "general").await;

    assert_eq!(c.status(), AgentStatus::Completed);
    assert!(compactor.calls.load(Ordering::SeqCst) > 0);
    assert!(
        events
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, AgentEvent::Compacted { .. }))
    );
}

async fn reported_prompt_size_run(input: i64, cached: i64) {
    let mut config = OrcaConfig::default();
    config.lm_studio.native_tool_calling = true;
    config.context.context_window_size = 100_000;
    config.context.auto_detect_context_window = false;
    let client = FuncClient::seq(move |_, _, n| {
        if n < 2 {
            return ChatResponse {
                usage: Usage {
                    prompt_tokens: Some(input),
                    cached_tokens: Some(cached),
                    completion_tokens: Some(10),
                    reasoning_tokens: None,
                },
                ..call_id(&format!("c{n}"), "read_file", json!({"path": "x"}), "")
            };
        }
        text("Done.")
    });
    let o = orchestrator(client, config);
    let compactor = Arc::new(RecordingCompactor::default());
    o.set_compactor(Some(compactor.clone()));
    o.set_tools(tools(&["read_file"]));
    o.set_executor(executor(|_, _| "small".into()));

    let c = o.spawn_agent("task", "general").await;

    assert_eq!(c.status(), AgentStatus::Completed);
    assert!(
        compactor.calls.load(Ordering::SeqCst) > 0,
        "the reported 90k-token prompt is past the 83.6k hard limit"
    );
}

#[tokio::test]
async fn spawn_agent_server_reported_prompt_size_triggers_compaction_openai_style() {
    // OpenAI-compatible: prompt_tokens counts the cached part
    reported_prompt_size_run(90_500, 0).await;
}

#[tokio::test]
async fn spawn_agent_server_reported_prompt_size_triggers_compaction_uncached_plus_cache_reads() {
    // input_tokens as the uncached part only, cache reads on top
    reported_prompt_size_run(500, 90_000).await;
}

#[tokio::test]
async fn spawn_agent_auto_compact_disabled_never_compacts() {
    let mut config = OrcaConfig::default();
    config.context.auto_compact_enabled = false;
    config.context.context_window_size = 10;
    let o = orchestrator(FuncClient::stub("Done."), config);
    let compactor = Arc::new(RecordingCompactor::default());
    o.set_compactor(Some(compactor.clone()));

    let c = o.spawn_agent("task", "general").await;

    assert_eq!(c.status(), AgentStatus::Completed);
    assert_eq!(compactor.calls.load(Ordering::SeqCst), 0);
}

/// C#'s `RecordingClient`: answers "Done: <first line>", "slow" tasks take 200 ms, FAIL throws.
fn recording_client(seen: Arc<Mutex<Vec<String>>>) -> Arc<FuncClient> {
    FuncClient::new(move |messages, _, _| {
        let task = task_of(&messages);
        seen.lock().unwrap().push(task.clone());
        Box::pin(async move {
            let ms = if task.contains("slow") { 200 } else { 10 };
            tokio::time::sleep(Duration::from_millis(ms)).await;
            if task.contains("FAIL") {
                return Err(ChatError::Server("boom".into()));
            }
            Ok(text(&format!(
                "Done: {}",
                task.split('\n').next().unwrap_or("")
            )))
        })
    })
}

#[tokio::test]
async fn spawn_parallel_after_starts_when_its_dependency_finishes_with_its_report() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let o = orchestrator(recording_client(seen.clone()), OrcaConfig::default());

    let results = o
        .spawn_parallel(
            &[
                spec("build engine slow", Some("Engine"), None, None),
                spec("build cli", Some("CLI"), None, Some(vec![0])),
                spec("build docs", Some("Docs"), None, None),
            ],
            0,
            &none(),
        )
        .await
        .unwrap();

    assert!(results.iter().all(|r| r.status() == AgentStatus::Completed));
    let seen = seen.lock().unwrap().clone();
    let cli: Vec<_> = seen
        .iter()
        .enumerate()
        .filter(|(_, s)| s.starts_with("build cli"))
        .collect();
    assert_eq!(cli.len(), 1);
    let (cli_index, cli) = cli[0];
    // The CLI started only after the slow engine finished, and its brief carries the report.
    let engine_index = seen
        .iter()
        .position(|s| s.starts_with("build engine"))
        .unwrap();
    assert!(cli_index > engine_index);
    assert!(cli.contains("These tasks finished before you started; their reports:"));
    assert!(cli.contains("(Engine)"));
    assert!(cli.contains("Done: build engine slow"));
    let docs: Vec<_> = seen
        .iter()
        .filter(|s| s.starts_with("build docs"))
        .collect();
    assert_eq!(docs.len(), 1);
    assert!(!docs[0].contains("their reports"));
}

#[tokio::test]
async fn spawn_parallel_after_skips_a_task_whose_dependency_failed() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let o = orchestrator(recording_client(seen.clone()), OrcaConfig::default());

    let results = o
        .spawn_parallel(
            &[
                spec("FAIL engine", Some("Engine"), None, None),
                spec("build cli", Some("CLI"), None, Some(vec![0])),
            ],
            0,
            &none(),
        )
        .await
        .unwrap();

    assert_eq!(results[0].status(), AgentStatus::Failed);
    assert_eq!(results[1].status(), AgentStatus::Failed);
    assert!(
        result(&results[1]).starts_with("Skipped: it waits for"),
        "{}",
        result(&results[1])
    );
    assert!(
        !seen
            .lock()
            .unwrap()
            .iter()
            .any(|s| s.starts_with("build cli"))
    );
}

#[test]
fn dependency_error_rejects_self_unknown_and_cycles() {
    let s = |after: &[usize]| spec("t", None, None, Some(after.to_vec()));
    assert!(dependency_error(&[s(&[]), s(&[0]), s(&[0, 1])]).is_none());
    assert!(dependency_error(&[s(&[0])]).unwrap().contains("itself"));
    assert!(
        dependency_error(&[s(&[]), s(&[5])])
            .unwrap()
            .contains("not in this call")
    );
    assert!(
        dependency_error(&[s(&[1]), s(&[0])])
            .unwrap()
            .contains("cycle")
    );
}

// ── AgentSwarmTests ──────────────────────────────────────────────────────────

const USER_SCHEDULE: [&str; 6] = ["local", "local", "haiku", "local", "local", "haiku"];

fn swarm_config(max_agents: i32, max_tokens: Option<i32>, window: Option<i32>) -> OrcaConfig {
    let mut config = OrcaConfig::default();
    config.agent.provider_schedule = Some(USER_SCHEDULE.iter().map(|s| s.to_string()).collect());
    config.agent.max_agents = max_agents;
    config.agent.max_concurrent = 10;
    config.providers.insert(
        "haiku".into(),
        ProviderConfig {
            kind: "anthropic".into(),
            model: Some("claude-haiku-5-5".into()),
            context_window: window,
            max_tokens,
            pricing: Some(orca_core::config::ProviderPricing {
                input_per_m_tok: 0.10,
                output_per_m_tok: 0.50,
                cache_read_per_m_tok: 0.01,
                cache_write_per_m_tok: 0.125,
                ..Default::default()
            }),
            ..ProviderConfig::default()
        },
    );
    config
}

/// Answers "Done on <name>." and records the task and options of every request.
struct SwarmClient {
    name: String,
    prompt_tokens: i64,
    tasks: Mutex<Vec<String>>,
    options: Mutex<Vec<ChatOptions>>,
}

impl SwarmClient {
    fn new(name: &str, prompt_tokens: i64) -> Arc<Self> {
        Arc::new(Self {
            name: name.into(),
            prompt_tokens,
            tasks: Mutex::new(Vec::new()),
            options: Mutex::new(Vec::new()),
        })
    }

    fn tasks_sorted(&self) -> Vec<String> {
        let mut t = self.tasks.lock().unwrap().clone();
        t.sort();
        t
    }

    fn respond(&self, messages: &[ChatMessage], options: &ChatOptions) -> ChatResponse {
        self.tasks.lock().unwrap().push(task_of(messages));
        self.options.lock().unwrap().push(options.clone());
        ChatResponse {
            usage: Usage {
                prompt_tokens: Some(self.prompt_tokens),
                cached_tokens: Some(self.prompt_tokens / 2),
                completion_tokens: Some(100),
                reasoning_tokens: None,
            },
            usage_reported: true,
            ..text(&format!("Done on {}.", self.name))
        }
    }
}

#[async_trait::async_trait]
impl AgentChatClient for SwarmClient {
    async fn complete(
        &self,
        _: &str,
        messages: &[ChatMessage],
        options: &ChatOptions,
    ) -> Result<ChatResponse, ChatError> {
        Ok(self.respond(messages, options))
    }

    async fn stream(
        &self,
        _: &str,
        messages: &[ChatMessage],
        options: &ChatOptions,
        on_event: &mut (dyn FnMut(StreamEvent) -> StreamControl + Send),
    ) -> Result<ChatResponse, ChatError> {
        let r = self.respond(messages, options);
        on_event(StreamEvent::Text(r.text.clone()));
        on_event(StreamEvent::UpdateEnd {
            tool_call_delta: false,
        });
        Ok(r)
    }
}

/// The local client plus named providers (C#'s `ProviderClientFactory`).
struct SwarmProvider {
    local: Arc<dyn AgentChatClient>,
    config: OrcaConfig,
    routes: HashMap<String, Arc<dyn AgentChatClient>>,
}

#[async_trait::async_trait]
impl AgentClientProvider for SwarmProvider {
    fn default_client(&self) -> Arc<dyn AgentChatClient> {
        self.local.clone()
    }

    fn provider_route(&self, name: &str) -> Option<ProviderRoute> {
        let client = self.routes.get(name)?.clone();
        let p = self.config.providers.get(name)?;
        Some(ProviderRoute {
            name: name.into(),
            model: p.primary_model().unwrap_or_default().to_string(),
            client,
            context_window: p.context_window,
            max_tokens: p.max_tokens,
        })
    }
}

struct Swarm {
    o: Arc<AgentOrchestrator>,
    local: Arc<SwarmClient>,
    haiku: Arc<SwarmClient>,
}

fn swarm_with(config: OrcaConfig, extra: Vec<(&str, Arc<dyn AgentChatClient>)>) -> Swarm {
    let local = SwarmClient::new("local", 1000);
    let haiku = SwarmClient::new("haiku", 4000);
    let mut routes: HashMap<String, Arc<dyn AgentChatClient>> = HashMap::new();
    routes.insert("haiku".into(), haiku.clone());
    for (name, client) in extra {
        routes.insert(name.into(), client);
    }
    let provider = SwarmProvider {
        local: local.clone(),
        config: config.clone(),
        routes,
    };
    let o = AgentOrchestrator::new(
        Arc::new(provider),
        config,
        AgentTypeRegistry::new(),
        std::env::temp_dir(),
    );
    Swarm { o, local, haiku }
}

fn swarm(config: OrcaConfig) -> Swarm {
    swarm_with(config, Vec::new())
}

#[test]
fn provider_at_follows_the_users_order_and_the_last_entry_repeats() {
    let mut config = OrcaConfig::default();
    config.agent.provider_schedule = Some(USER_SCHEDULE.iter().map(|s| s.to_string()).collect());
    let by_position: Vec<String> = (0..10)
        .map(|i| {
            config
                .agent
                .provider_at(i)
                .unwrap_or_else(|| "local".into())
        })
        .collect();
    assert_eq!(
        by_position,
        [
            "local", "local", "haiku", "local", "local", "haiku", "haiku", "haiku", "haiku",
            "haiku"
        ]
    );
    assert!(OrcaConfig::default().agent.provider_at(5).is_none());
    assert!(!is_swarm(&OrcaConfig::default()));
    assert!(is_swarm(&config));
}

#[test]
fn provider_config_provider_and_model_aliases() {
    // The `provider` alias of `type` is applied while loading the config (see the config
    // tests); here the model alias.
    let mut p = ProviderConfig {
        kind: "anthropic".into(),
        model: Some("claude-haiku-5-5".into()),
        ..ProviderConfig::default()
    };
    assert_eq!(p.kind, "anthropic");
    assert_eq!(p.primary_model(), Some("claude-haiku-5-5"));
    p.models = vec!["other".into()];
    assert_eq!(p.primary_model(), Some("other"));
}

#[tokio::test]
async fn sequential_spawns_route_by_creation_order() {
    let s = swarm(swarm_config(10, None, None));
    let mut agents = Vec::new();
    for i in 1..=7 {
        agents.push(spawn_at(&s.o, &format!("task {i}"), "general", 0).await);
    }

    assert!(agents.iter().all(|a| a.status() == AgentStatus::Completed));
    let tags: Vec<String> = agents
        .iter()
        .map(|a| a.model_tag().unwrap_or_default())
        .collect();
    assert_eq!(
        tags,
        [
            "local", "local", "haiku", "local", "local", "haiku", "haiku"
        ]
    );
    assert_eq!(s.haiku.tasks_sorted(), ["task 3", "task 6", "task 7"]);
    assert_eq!(
        s.local.tasks_sorted(),
        ["task 1", "task 2", "task 4", "task 5"]
    );
    assert_eq!(
        agents[2].state().provider_model.as_deref(),
        Some("claude-haiku-5-5")
    );
    assert!(result(&agents[2]).contains("Done on haiku"));
    assert_eq!(s.o.agents_created(), 7);
}

#[tokio::test]
async fn parallel_batch_assigns_providers_in_listed_order_not_start_order() {
    let s = swarm(swarm_config(10, None, None));
    // Sizes make the scheduler start task 6 first and task 1 last.
    let specs: Vec<ParallelAgentSpec> = (1..=6)
        .map(|i| spec(&format!("task {i}"), None, Some(i * 10), None))
        .collect();
    let results = s.o.spawn_parallel(&specs, 0, &none()).await.unwrap();

    let tags: Vec<String> = results
        .iter()
        .map(|a| a.model_tag().unwrap_or_default())
        .collect();
    assert_eq!(tags, ["local", "local", "haiku", "local", "local", "haiku"]);
    assert_eq!(s.haiku.tasks_sorted(), ["task 3", "task 6"]);
    let positions: Vec<i32> = results
        .iter()
        .map(|a| a.schedule_position.unwrap())
        .collect();
    assert_eq!(positions, [0, 1, 2, 3, 4, 5]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_spawns_get_distinct_positions_and_the_schedule_split_holds() {
    let s = swarm(swarm_config(10, None, None));
    let handles: Vec<_> = (1..=10)
        .map(|i| {
            let o = s.o.clone();
            tokio::spawn(async move { spawn_at(&o, &format!("task {i}"), "general", 0).await })
        })
        .collect();
    let mut agents = Vec::new();
    for h in handles {
        agents.push(h.await.unwrap());
    }

    let mut positions: Vec<i32> = agents
        .iter()
        .map(|a| a.schedule_position.unwrap())
        .collect();
    positions.sort();
    assert_eq!(positions, (0..10).collect::<Vec<_>>());
    for a in &agents {
        let p = a.schedule_position.unwrap();
        let expected = if p == 2 || p >= 5 { "haiku" } else { "local" };
        assert_eq!(a.model_tag().as_deref(), Some(expected));
    }
    assert_eq!(s.haiku.tasks.lock().unwrap().len(), 6);
    assert_eq!(s.local.tasks.lock().unwrap().len(), 4);
}

#[tokio::test]
async fn cap_refuses_the_eleventh_agent_with_a_follow_up_hint() {
    let s = swarm(swarm_config(3, None, None));
    for i in 1..=3 {
        spawn_at(&s.o, &format!("task {i}"), "general", 0).await;
    }

    assert_eq!(s.o.agents_left(), Some(0));
    let refused = spawn_at(&s.o, "task 4", "general", 0).await;
    assert_eq!(refused.status(), AgentStatus::Failed);
    let error = refused.error().unwrap();
    assert!(error.contains("Agent cap reached"));
    assert!(error.contains("\"to\""));
    assert!(error.contains("haiku")); // the existing agents are listed with their models
    assert_eq!(s.o.agents_created(), 3);
}

#[tokio::test]
async fn budget_note_counts_down_and_says_when_all_are_used() {
    let s = swarm(swarm_config(2, None, None));
    assert!(
        s.o.budget_note()
            .unwrap()
            .contains("2 of 2 new agents left")
    );
    spawn_at(&s.o, "first", "general", 0).await;
    let note = s.o.budget_note().unwrap();
    assert!(note.contains("1 of 2 new agents left"));
    assert!(note.contains("several new agents"));
    spawn_at(&s.o, "second", "general", 0).await;
    assert!(s.o.budget_note().unwrap().contains("all 2 agents used"));
}

#[tokio::test]
async fn pinned_provider_read_only_runs_off_the_schedule_and_refuses_writes() {
    let mut config = swarm_config(1, None, None);
    config.providers.insert(
        "fable".into(),
        ProviderConfig {
            kind: "anthropic".into(),
            model: Some("claude-fable-5-2".into()),
            max_tokens: Some(64000),
            ..ProviderConfig::default()
        },
    );
    // Asks for a write, then a writing bash command, then a read, then answers with a plan.
    let fable = FuncClient::seq(|_, _, n| {
        let r = match n {
            0 => call_id(
                "c1",
                "write",
                json!({"path": "PLAN.md", "content": "x"}),
                "",
            ),
            1 => call_id("c2", "bash", json!({"command": "echo x > notes.txt"}), ""),
            2 => call_id("c3", "bash", json!({"command": "dir"}), ""),
            _ => text("## Architecture\nThe plan."),
        };
        ChatResponse {
            usage: Usage {
                prompt_tokens: Some(2000),
                completion_tokens: Some(300),
                cached_tokens: None,
                reasoning_tokens: None,
            },
            ..r
        }
    });
    let s = swarm_with(config, vec![("fable", fable.clone())]);
    let executed = Arc::new(Mutex::new(Vec::<String>::new()));
    let e = executed.clone();
    s.o.set_executor(executor(move |name, args| {
        e.lock().unwrap().push(format!("{name} {args}"));
        "ok".into()
    }));

    let plan = spawn_opts(
        &s.o,
        "plan it",
        SpawnOptions {
            provider: Some("fable".into()),
            read_only: true,
            max_iterations: Some(10),
            max_output_tokens: Some(32000),
            effort: Some("high".into()),
            ..SpawnOptions::default()
        },
    )
    .await;

    assert_eq!(plan.status(), AgentStatus::Completed);
    assert!(result(&plan).contains("The plan."));
    assert_eq!(plan.model_tag().as_deref(), Some("fable"));
    assert!(plan.schedule_position.is_none());
    assert!(s.local.tasks.lock().unwrap().is_empty());
    // Only the reading command ran; the write and the redirect came back as errors.
    let executed = executed.lock().unwrap().clone();
    assert_eq!(executed.len(), 1, "{executed:?}");
    assert!(executed[0].contains("dir"));
    let requests = fable.requests();
    let results: Vec<&str> = requests
        .last()
        .unwrap()
        .iter()
        .filter(|m| m.role == Role::Tool)
        .map(text_of)
        .collect();
    assert!(
        results
            .iter()
            .any(|r| r.contains("'write' is not available")),
        "{results:?}"
    );
    assert!(
        results.iter().any(|r| r.contains("redirection")),
        "{results:?}"
    );
    for o in fable.options() {
        assert_eq!(o.max_output_tokens, Some(32000));
        assert_eq!(o.additional.get("reasoning_effort"), Some(&json!("high")));
    }
    // Not counted toward agent.maxAgents: the one scheduled agent can still start.
    assert_eq!(s.o.agents_created(), 0);
    assert_eq!(
        spawn_at(&s.o, "work", "general", 0).await.status(),
        AgentStatus::Completed
    );
}

#[tokio::test]
async fn read_only_agent_has_a_hard_step_limit_and_is_asked_for_its_answer() {
    let mut config = swarm_config(1, None, None);
    config.agent.iteration_extensions = 2;
    config.providers.insert(
        "fable".into(),
        ProviderConfig {
            kind: "anthropic".into(),
            model: Some("claude-fable-5-2".into()),
            ..ProviderConfig::default()
        },
    );
    // Looks with bash on every step until it is told the steps are over, then answers.
    let notes = Arc::new(Mutex::new(Vec::<String>::new()));
    let n2 = notes.clone();
    let fable = FuncClient::seq(move |messages, _, n| {
        let last = messages.last().unwrap();
        if last.role == Role::User && !text_of(last).is_empty() {
            n2.lock().unwrap().push(text_of(last).to_string());
        }
        let r = if last.role == Role::User && text_of(last).contains("[Step limit reached]") {
            text("## Plan\nEvery section.")
        } else {
            call_id(
                &format!("c{}", n + 1),
                "bash",
                json!({"command": "dir"}),
                "",
            )
        };
        ChatResponse {
            usage: Usage {
                prompt_tokens: Some(100),
                completion_tokens: Some(10),
                cached_tokens: None,
                reasoning_tokens: None,
            },
            ..r
        }
    });
    let s = swarm_with(config, vec![("fable", fable.clone())]);
    s.o.set_executor(executor(|_, _| "ok".into()));

    let plan = spawn_opts(
        &s.o,
        "plan it",
        SpawnOptions {
            provider: Some("fable".into()),
            read_only: true,
            max_iterations: Some(8),
            ..SpawnOptions::default()
        },
    )
    .await;

    // 8 steps, no extension although every step "made progress", then one answer request.
    assert_eq!(plan.status(), AgentStatus::CompletedPartial);
    assert_eq!(fable.calls.load(Ordering::SeqCst), 9);
    assert!(result(&plan).contains("## Plan\nEvery section."));
    let notes = notes.lock().unwrap().clone();
    assert!(
        notes
            .iter()
            .any(|n| n.contains("no extensions") && n.contains("the full plan")),
        "{notes:?}"
    );
    assert!(!notes.iter().any(|n| n.contains("run your piece's check")));
}

#[test]
fn budget_note_none_outside_swarm_mode() {
    let s = swarm(OrcaConfig::default());
    assert!(s.o.budget_note().is_none());
}

#[test]
fn worker_hint_asks_for_a_real_sample_before_a_parser() {
    assert!(SWARM_WORKER_HINT.contains("read one real input file"));
    assert!(SWARM_WORKER_HINT.contains("never an invented format"));
}

#[tokio::test]
async fn cap_parallel_batch_over_the_cap_runs_nothing() {
    let s = swarm(swarm_config(4, None, None));
    spawn_at(&s.o, "first", "general", 0).await;

    assert!(s.o.cap_error(3).is_none());
    assert!(s.o.cap_error(4).unwrap().contains("only 3 more can start"));
    let specs: Vec<ParallelAgentSpec> = (1..=4)
        .map(|i| ParallelAgentSpec::new(format!("task {i}"), "general"))
        .collect();
    assert!(s.o.spawn_parallel(&specs, 0, &none()).await.is_err());
    assert_eq!(s.local.tasks.lock().unwrap().len(), 1);
    assert!(s.haiku.tasks.lock().unwrap().is_empty());
    assert_eq!(s.o.agents_created(), 1);
}

#[tokio::test]
async fn follow_up_stays_on_its_provider_and_does_not_count() {
    let s = swarm(swarm_config(3, None, None));
    let mut third = None;
    for i in 1..=3 {
        third = Some(spawn_opts(&s.o, &format!("task {i}"), retain()).await);
    }
    let third = third.unwrap();

    let agent =
        s.o.follow_up(&third.letter().to_string(), "fix it", &none())
            .await
            .unwrap();
    assert_eq!(agent.status(), AgentStatus::Completed);
    assert_eq!(s.haiku.tasks.lock().unwrap().len(), 2); // the spawn and its follow-up
    assert_eq!(s.local.tasks.lock().unwrap().len(), 2);
    assert_eq!(s.o.agents_created(), 3);
}

#[tokio::test]
async fn provider_limits_max_tokens_per_provider_and_no_local_sampler_fields() {
    let mut config = swarm_config(10, Some(8000), None);
    config.lm_studio.max_tokens = Some(32768);
    config.agent.reasoning.worker = Some("low".into());
    let s = swarm(config);
    for i in 1..=3 {
        spawn_at(&s.o, &format!("task {i}"), "general", 0).await;
    }

    let haiku = s.haiku.options.lock().unwrap().clone();
    assert!(!haiku.is_empty());
    for o in &haiku {
        assert_eq!(o.max_output_tokens, Some(8000));
        assert_eq!(o.model.as_deref(), Some("claude-haiku-5-5"));
        assert!(!o.additional.contains_key("reasoning_effort"));
    }
    for o in s.local.options.lock().unwrap().iter() {
        assert_eq!(o.max_output_tokens, Some(32768));
    }
}

#[tokio::test]
async fn provider_usage_is_added_up_per_provider_and_costed() {
    // C#: ProviderClientFactory + UsageLedger; here ProviderRoutes over the local client.
    let config = swarm_config(10, None, None);
    let local = SwarmClient::new("local", 1000);
    let haiku = SwarmClient::new("haiku", 4000);
    let ledger = Arc::new(orca_core::client::usage::ProviderUsageLedger::new());
    let h: Arc<dyn AgentChatClient> = haiku.clone();
    let provider = ProviderRoutes::new(
        Arc::new(DefaultClientProvider::new(local.clone())),
        config.clone(),
        ledger.clone(),
        None,
    )
    .with_factory(Arc::new(move |name| (name == "haiku").then(|| h.clone())));
    let o = AgentOrchestrator::new(
        Arc::new(provider),
        config,
        AgentTypeRegistry::new(),
        std::env::temp_dir(),
    );
    for i in 1..=3 {
        o.spawn_agent(&format!("task {i}"), "general").await;
    }

    // Only the haiku route is wrapped here (the local client is the test's own), so one row.
    let rows = ledger.snapshot();
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.provider, "haiku");
    assert_eq!(row.model.as_deref(), Some("claude-haiku-5-5"));
    assert_eq!(row.requests, 1);
    assert_eq!(row.prompt, 4000);
    assert_eq!(row.cached, 2000);
    // 2000 uncached * 0.10 + 2000 cached * 0.01 + 100 out * 0.50 per MTok
    let expected = (2000.0 * 0.10 + 2000.0 * 0.01 + 100.0 * 0.50) / 1e6;
    assert!((row.cost_usd.unwrap() - expected).abs() < 1e-9);
    assert!(Arc::ptr_eq(&o.usage_ledger(), &ledger));
    // Haiku's agent (the third) asked for haiku's model.
    assert_eq!(
        haiku.options.lock().unwrap()[0].model.as_deref(),
        Some("claude-haiku-5-5")
    );
}

#[tokio::test]
async fn an_unbuildable_provider_runs_its_agent_on_the_local_model() {
    // haiku is "anthropic", which has no Rust client: the agent falls back to local.
    let config = swarm_config(10, None, None);
    let local = SwarmClient::new("local", 1000);
    let provider = ProviderRoutes::new(
        Arc::new(DefaultClientProvider::new(local.clone())),
        config.clone(),
        Arc::new(orca_core::client::usage::ProviderUsageLedger::new()),
        None,
    );
    assert!(provider.provider_route("haiku").is_none());
    assert!(provider.provider_route("nope").is_none());
    let o = AgentOrchestrator::new(
        Arc::new(provider),
        config,
        AgentTypeRegistry::new(),
        std::env::temp_dir(),
    );
    for i in 1..=3 {
        o.spawn_agent(&format!("task {i}"), "general").await;
    }
    assert_eq!(local.tasks.lock().unwrap().len(), 3);
}

#[test]
fn an_openai_compatible_provider_gets_a_logged_route_with_its_limits() {
    let mut config = OrcaConfig::default();
    config.providers.insert(
        "cloud".into(),
        ProviderConfig {
            base_url: "http://127.0.0.1:9/v1".into(),
            models: vec!["big-model".into()],
            context_window: Some(64000),
            max_tokens: Some(4000),
            ..ProviderConfig::default()
        },
    );
    let local = SwarmClient::new("local", 1000);
    let provider = ProviderRoutes::new(
        Arc::new(DefaultClientProvider::new(local)),
        config,
        Arc::new(orca_core::client::usage::ProviderUsageLedger::new()),
        None,
    );
    let route = provider.provider_route("Cloud").unwrap();
    assert_eq!(route.model, "big-model");
    assert_eq!(route.context_window, Some(64000));
    assert_eq!(route.max_tokens, Some(4000));
    // Cached: the same client again.
    let again = provider.provider_route("cloud").unwrap();
    assert!(Arc::ptr_eq(&route.client, &again.client));
}

/// A slot copy's backend: records each request's task.
struct RecordingBackend {
    name: String,
    tasks: Mutex<Vec<String>>,
}

impl RecordingBackend {
    fn new(name: &str) -> Arc<Self> {
        Arc::new(Self {
            name: name.into(),
            tasks: Mutex::new(Vec::new()),
        })
    }

    fn tasks_sorted(&self) -> Vec<String> {
        let mut t = self.tasks.lock().unwrap().clone();
        t.sort();
        t
    }
}

#[async_trait::async_trait]
impl orca_core::client::ChatBackend for RecordingBackend {
    async fn complete(
        &self,
        _: &str,
        messages: &[ChatMessage],
        _: &ChatOptions,
    ) -> Result<ChatResponse, ChatError> {
        self.tasks.lock().unwrap().push(task_of(messages));
        Ok(text(&format!("Done on {}.", self.name)))
    }

    async fn stream_with_control(
        &self,
        actor: &str,
        messages: &[ChatMessage],
        options: &ChatOptions,
        on_event: &mut (dyn FnMut(StreamEvent) -> StreamControl + Send),
    ) -> Result<ChatResponse, ChatError> {
        let r = self.complete(actor, messages, options).await?;
        on_event(StreamEvent::Text(r.text.clone()));
        Ok(r)
    }
}

fn slot_runtime(
    slots: orca_core::config::SlotsConfig,
    config: &OrcaConfig,
    copy: Arc<RecordingBackend>,
    fallback: Arc<RecordingBackend>,
) -> Arc<orca_core::slots::SlotRuntime> {
    use orca_core::slots::testing::{FakeSlotHealth, FakeSlotRegistry};
    use orca_core::slots::{AllocatorOptions, SlotAllocator, SlotClientFactory};
    let allocator = SlotAllocator::new(
        slots,
        Some(Arc::new(FakeSlotRegistry::default())),
        Arc::new(FakeSlotRegistry::default()),
        Arc::new(FakeSlotHealth::default()),
        AllocatorOptions {
            owner: Some("tester@box".into()),
            host: Some("box".into()),
            pid: Some(1),
            ..AllocatorOptions::default()
        },
    );
    let factory = SlotClientFactory::new(
        config.clone(),
        Some(Arc::new(move |_| {
            copy.clone() as Arc<dyn orca_core::client::ChatBackend>
        })),
        None,
        None,
    );
    orca_core::slots::SlotRuntime::new(allocator, Arc::new(factory), fallback, None)
}

fn one_slot_config() -> orca_core::config::SlotsConfig {
    orca_core::config::SlotsConfig {
        enabled: true,
        registry_url: Some("http://fake-registry".into()),
        model: Some("m".into()),
        copies: vec![orca_core::config::SlotCopyConfig {
            name: "A".into(),
            base_url: "http://copy-a/v1".into(),
            slots: 1,
            ..orca_core::config::SlotCopyConfig::default()
        }],
        retry_seconds: 1,
        ..orca_core::config::SlotsConfig::default()
    }
}

#[tokio::test]
async fn haiku_agents_take_no_slot_and_local_agents_reuse_a_freed_slot() {
    let mut config = swarm_config(10, None, None);
    config.slots = Some(one_slot_config());
    let local = SwarmClient::new("local", 1000);
    let haiku = SwarmClient::new("haiku", 4000);
    let mut routes: HashMap<String, Arc<dyn AgentChatClient>> = HashMap::new();
    routes.insert("haiku".into(), haiku.clone());
    let inner = SwarmProvider {
        local: local.clone(),
        config: config.clone(),
        routes,
    };
    let copy_a = RecordingBackend::new("A");
    let fallback = RecordingBackend::new("fallback");
    let runtime = slot_runtime(
        config.slots.clone().unwrap(),
        &config,
        copy_a.clone(),
        fallback.clone(),
    );
    let provider = SlotClientProvider::new(Arc::new(inner), runtime);
    let o = AgentOrchestrator::new(
        Arc::new(provider),
        config,
        AgentTypeRegistry::new(),
        std::env::temp_dir(),
    );

    // One slot, schedule local, local, haiku: the two local agents share the slot in turn
    // (the second waits for the first), the haiku agent runs without one.
    let specs: Vec<ParallelAgentSpec> = (1..=3)
        .map(|i| ParallelAgentSpec::new(format!("task {i}"), "general"))
        .collect();
    let results = o
        .spawn_parallel(&specs, 0, &CancelToken::new())
        .await
        .unwrap();

    assert!(results.iter().all(|a| a.status() == AgentStatus::Completed));
    assert_eq!(copy_a.tasks_sorted(), ["task 1", "task 2"]);
    assert_eq!(haiku.tasks_sorted(), ["task 3"]);
    assert!(fallback.tasks_sorted().is_empty());
    assert!(local.tasks_sorted().is_empty());
    let has =
        |a: &Arc<AgentContext>, t: &str| a.transcript.snapshot().iter().any(|e| e.text.contains(t));
    assert!(has(&results[1], "pinned to slot A1"));
    assert!(!has(&results[2], "slot"));
    assert!(
        results[0].heavy,
        "the first ready task of a slotted batch is heavy"
    );
}

type ReplyFn = Box<dyn Fn(&[ChatMessage]) -> ChatResponse + Send + Sync>;

/// A slot copy whose replies come from a function of the request.
struct ScriptBackend {
    calls: Mutex<Vec<String>>,
    f: ReplyFn,
}

#[async_trait::async_trait]
impl orca_core::client::ChatBackend for ScriptBackend {
    async fn complete(
        &self,
        _: &str,
        messages: &[ChatMessage],
        _: &ChatOptions,
    ) -> Result<ChatResponse, ChatError> {
        self.calls.lock().unwrap().push(task_of(messages));
        Ok((self.f)(messages))
    }

    async fn stream_with_control(
        &self,
        actor: &str,
        messages: &[ChatMessage],
        options: &ChatOptions,
        on_event: &mut (dyn FnMut(StreamEvent) -> StreamControl + Send),
    ) -> Result<ChatResponse, ChatError> {
        let r = self.complete(actor, messages, options).await?;
        if !r.text.is_empty() {
            on_event(StreamEvent::Text(r.text.clone()));
        }
        Ok(r)
    }
}

/// A slot copy that holds the "first" task's request until the gate opens.
struct GateBackend {
    gate: Gate,
}

#[async_trait::async_trait]
impl orca_core::client::ChatBackend for GateBackend {
    async fn complete(
        &self,
        _: &str,
        messages: &[ChatMessage],
        _: &ChatOptions,
    ) -> Result<ChatResponse, ChatError> {
        if task_of(messages) == "first" {
            self.gate.wait().await;
        }
        Ok(text("Done."))
    }

    async fn stream_with_control(
        &self,
        actor: &str,
        messages: &[ChatMessage],
        options: &ChatOptions,
        on_event: &mut (dyn FnMut(StreamEvent) -> StreamControl + Send),
    ) -> Result<ChatResponse, ChatError> {
        let r = self.complete(actor, messages, options).await?;
        on_event(StreamEvent::Text(r.text.clone()));
        Ok(r)
    }
}

#[tokio::test]
async fn an_agent_waits_for_a_free_slot_and_gets_it_when_the_holder_finishes() {
    use orca_core::slots::testing::{FakeSlotHealth, FakeSlotRegistry};
    use orca_core::slots::{AllocatorOptions, SlotAllocator, SlotClientFactory};
    let config = OrcaConfig {
        slots: Some(one_slot_config()),
        ..OrcaConfig::default()
    };
    let gate = Gate::new();
    let backend = Arc::new(GateBackend { gate: gate.clone() });
    let allocator = SlotAllocator::new(
        config.slots.clone().unwrap(),
        Some(Arc::new(FakeSlotRegistry::default())),
        Arc::new(FakeSlotRegistry::default()),
        Arc::new(FakeSlotHealth::default()),
        AllocatorOptions {
            owner: Some("tester@box".into()),
            host: Some("box".into()),
            pid: Some(1),
            ..AllocatorOptions::default()
        },
    );
    let b = backend.clone();
    let factory = SlotClientFactory::new(
        config.clone(),
        Some(Arc::new(move |_| {
            b.clone() as Arc<dyn orca_core::client::ChatBackend>
        })),
        None,
        None,
    );
    let runtime = orca_core::slots::SlotRuntime::new(
        allocator,
        Arc::new(factory),
        RecordingBackend::new("fallback"),
        None,
    );
    let provider = SlotClientProvider::new(
        Arc::new(DefaultClientProvider::new(FuncClient::stub("main"))),
        runtime.clone(),
    );
    let o = AgentOrchestrator::new(
        Arc::new(provider),
        config,
        AgentTypeRegistry::new(),
        std::env::temp_dir(),
    );

    let o1 = o.clone();
    let first = tokio::spawn(async move { o1.spawn_agent("first", "general").await });
    wait_until(|| {
        o.get_all_agents()
            .first()
            .is_some_and(|a| a.state().slot.as_deref() == Some("A1"))
    })
    .await;
    let o2 = o.clone();
    let second = tokio::spawn(async move { o2.spawn_agent("second", "general").await });
    wait_until(|| {
        o.get_all_agents().iter().any(|a| {
            a.task == "second"
                && a.status() == AgentStatus::Pending
                && a.state().current_activity.as_deref() == Some(WAITING_FOR_SLOT)
        })
    })
    .await;

    gate.open();
    let first = first.await.unwrap();
    let second = second.await.unwrap();
    assert_eq!(first.status(), AgentStatus::Completed);
    assert_eq!(second.status(), AgentStatus::Completed);
    let notes: Vec<String> = second
        .transcript
        .snapshot()
        .into_iter()
        .map(|e| e.text)
        .collect();
    assert!(
        notes.contains(&"waiting for a free inference slot".to_string()),
        "{notes:?}"
    );
    assert!(
        notes.contains(&"pinned to slot A1".to_string()),
        "{notes:?}"
    );
    // Leases are released on finish; the last slot is remembered for a follow-up.
    assert!(second.state().slot.is_none());
    assert_eq!(second.state().last_slot.as_deref(), Some("A1"));
    assert!(runtime.allocator.held().iter().all(|l| l.slot().is_none()));
}

#[tokio::test]
async fn nested_spawn_borrows_the_spawners_slot_when_none_is_free() {
    let mut config = OrcaConfig::default();
    config.lm_studio.native_tool_calling = true;
    config.slots = Some(one_slot_config());
    let copy = Arc::new(ScriptBackend {
        calls: Mutex::new(Vec::new()),
        f: Box::new(|m| {
            if task_of(m) == "outer" && !has_tool_message(m) {
                call("spawn", json!({}))
            } else {
                text("Done.")
            }
        }),
    });
    let fallback = RecordingBackend::new("fallback");
    let runtime = {
        use orca_core::slots::testing::{FakeSlotHealth, FakeSlotRegistry};
        use orca_core::slots::{AllocatorOptions, SlotAllocator, SlotClientFactory};
        let allocator = SlotAllocator::new(
            config.slots.clone().unwrap(),
            Some(Arc::new(FakeSlotRegistry::default())),
            Arc::new(FakeSlotRegistry::default()),
            Arc::new(FakeSlotHealth::default()),
            AllocatorOptions {
                owner: Some("tester@box".into()),
                host: Some("box".into()),
                pid: Some(1),
                ..AllocatorOptions::default()
            },
        );
        let c = copy.clone();
        let factory = SlotClientFactory::new(
            config.clone(),
            Some(Arc::new(move |_| {
                c.clone() as Arc<dyn orca_core::client::ChatBackend>
            })),
            None,
            None,
        );
        orca_core::slots::SlotRuntime::new(allocator, Arc::new(factory), fallback.clone(), None)
    };
    let provider = SlotClientProvider::new(
        Arc::new(DefaultClientProvider::new(FuncClient::stub("main"))),
        runtime.clone(),
    );
    let o = AgentOrchestrator::new(
        Arc::new(provider),
        config,
        AgentTypeRegistry::new(),
        std::env::temp_dir(),
    );
    o.set_tools(tools(&["spawn"]));
    let me: Arc<Mutex<Option<Weak<AgentOrchestrator>>>> = Arc::new(Mutex::new(None));
    let inner: Arc<Mutex<Option<Arc<AgentContext>>>> = Arc::new(Mutex::new(None));
    let (me2, inner2) = (me.clone(), inner.clone());
    o.set_executor(executor_async(move |_, _| {
        let o = me2.lock().unwrap().clone().unwrap().upgrade().unwrap();
        let inner = inner2.clone();
        Box::pin(async move {
            let c = o.spawn_agent("inner", "general").await;
            let r = result(&c);
            *inner.lock().unwrap() = Some(c);
            r
        })
    }));
    *me.lock().unwrap() = Some(Arc::downgrade(&o));

    let outer = o.spawn_agent("outer", "general").await;

    assert_eq!(outer.status(), AgentStatus::Completed);
    let inner = inner.lock().unwrap().clone().unwrap();
    assert_eq!(inner.status(), AgentStatus::Completed);
    let notes: Vec<String> = inner
        .transcript
        .snapshot()
        .into_iter()
        .map(|e| e.text)
        .collect();
    assert!(
        notes
            .iter()
            .any(|t| t == "no free slot; sharing Agent A's slot A1"),
        "{notes:?}"
    );
    assert!(copy.calls.lock().unwrap().contains(&"inner".to_string()));
    assert!(fallback.tasks_sorted().is_empty());
    // Every lease is back: the borrowed one was never the inner agent's own.
    assert!(runtime.allocator.held().iter().all(|l| l.slot().is_none()));
    assert!(outer.state().slot.is_none());
}

// ── AgentSupportTests ────────────────────────────────────────────────────────

#[tokio::test]
async fn nested_spawn_through_tool_executor_gets_parent_depth_plus_one_and_stops_at_the_limit() {
    // Every agent asks to spawn a child once, then finishes.
    let client = FuncClient::seq(|m, _, _| {
        if has_tool_message(m) {
            text("done")
        } else {
            call("spawn_agent", json!({}))
        }
    });
    let mut config = OrcaConfig::default();
    config.agent.max_spawn_depth = 3;
    let o = native(client, Some(config));
    o.set_tools(tools(&["spawn_agent"]));
    let depths = Arc::new(Mutex::new(Vec::<i32>::new()));
    let weak: Weak<AgentOrchestrator> = Arc::downgrade(&o);
    let d = depths.clone();
    o.set_executor(executor_async(move |_, _| {
        let weak = weak.clone();
        let d = d.clone();
        Box::pin(async move {
            // The spawn tool calls the orchestrator WITHOUT a depth, like the real tool does.
            let o = weak.upgrade().unwrap();
            let child = o.spawn_agent("child", "general").await;
            d.lock().unwrap().push(child.depth);
            child.result().or(child.error()).unwrap_or_default()
        })
    }));

    let root = o.spawn_agent("root", "general").await;

    assert_eq!(root.status(), AgentStatus::Completed);
    assert_eq!(root.depth, 0);
    // Depth 0 spawns 1, depth 1 spawns 2; the depth-2 agent doesn't see spawn tools and its
    // call is rejected before reaching the executor.
    assert_eq!(*depths.lock().unwrap(), [2, 1]);
    assert_eq!(root.caller_depth, 0);
    for a in o.get_all_agents().iter().filter(|a| a.depth > 0) {
        assert_eq!(a.depth, a.caller_depth);
    }
    assert!(!o.get_all_agents().iter().any(|a| a.depth >= 3));
}

#[tokio::test]
async fn agent_at_depth_limit_sees_no_agent_tools_including_fan_out_and_subagent() {
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let s2 = seen.clone();
    let client = FuncClient::seq(move |_, options, _| {
        s2.lock()
            .unwrap()
            .extend(options.tools.iter().map(|t| t.name.clone()));
        text("done")
    });
    let mut config = OrcaConfig::default();
    config.agent.max_spawn_depth = 1;
    let o = native(client, Some(config));
    o.set_tools(tools(&[
        "read_file",
        "spawn_agent",
        "spawn_agents",
        "subagent",
    ]));

    spawn_at(&o, "t", "general", 0).await;

    assert_eq!(*seen.lock().unwrap(), ["read_file"]);
}

#[tokio::test]
async fn concurrent_spawns_every_subscriber_sees_both_agents() {
    let o = native(FuncClient::stub("done"), None);
    let a = record_events(&o);
    o.subscribe(|_| panic!("broken renderer"));
    let b = record_events(&o);

    let (r1, r2) = tokio::join!(
        o.spawn_agent("one", "general"),
        o.spawn_agent("two", "general")
    );

    for events in [a, b] {
        let events = events.lock().unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, AgentEvent::Started { .. }))
                .count(),
            2
        );
        let mut completed: Vec<&str> = events
            .iter()
            .filter(|e| matches!(e, AgentEvent::Completed { .. }))
            .map(|e| e.agent_id())
            .collect();
        completed.sort();
        completed.dedup();
        assert_eq!(completed.len(), 2);
    }
    assert_eq!(r1.status(), AgentStatus::Completed);
    assert_eq!(r2.status(), AgentStatus::Completed);
    assert_ne!(r1.number, r2.number);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_agent_stops_only_that_agent() {
    let release = Gate::new();
    let r = release.clone();
    let client = FuncClient::new(move |m, _, _| {
        let r = r.clone();
        let slow = task_of(&m) == "slow";
        Box::pin(async move {
            if slow {
                std::future::pending::<()>().await;
            }
            r.wait().await;
            Ok(text("done"))
        })
    });
    let o = native(client, None);

    let o1 = o.clone();
    let slow = tokio::spawn(async move { o1.spawn_agent("slow", "general").await });
    let o2 = o.clone();
    let fast = tokio::spawn(async move { o2.spawn_agent("fast", "general").await });
    wait_until(|| o.get_running_agents().len() == 2).await;

    let slow_agent = o
        .get_all_agents()
        .into_iter()
        .find(|x| x.task == "slow")
        .unwrap();
    assert!(o.cancel_agent(&slow_agent.number.to_string()));
    release.open();

    let slow_result = slow.await.unwrap();
    let fast_result = fast.await.unwrap();
    assert_eq!(slow_result.status(), AgentStatus::Cancelled);
    assert_eq!(
        slow_result.error().as_deref(),
        Some("Agent was cancelled by the user.")
    );
    assert_eq!(fast_result.status(), AgentStatus::Completed);
    assert!(
        !o.cancel_agent(&slow_agent.id),
        "a finished agent can't be cancelled again"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parent_cancellation_is_reported_as_cancelled_not_timeout() {
    let client = FuncClient::new(|_, _, _| {
        Box::pin(async {
            std::future::pending::<()>().await;
            Ok(text("never"))
        })
    });
    let o = native(client, None);
    let ct = CancelToken::new();
    let (o1, ct1) = (o.clone(), ct.clone());
    let run = tokio::spawn(async move {
        o1.spawn_agent_with("t", "general", 0, &ct1, SpawnOptions::default())
            .await
    });
    wait_until(|| o.get_running_agents().len() == 1).await;
    ct.cancel();

    let c = run.await.unwrap();
    assert_eq!(c.status(), AgentStatus::Cancelled);
    assert_eq!(c.error().as_deref(), Some("Agent was cancelled."));
}

/// The requests a client saw, in order.
type Requests = Arc<Mutex<Vec<Vec<ChatMessage>>>>;

/// First request waits for `proceed` (after signalling `started`) and reads a.cs; then `answer`.
fn two_step_client(started: Gate, proceed: Gate, answer: &str) -> (Arc<FuncClient>, Requests) {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let r = requests.clone();
    let answer = answer.to_string();
    let client = FuncClient::new(move |m, _, _| {
        let first = {
            let mut r = r.lock().unwrap();
            r.push(m);
            r.len() == 1
        };
        let (started, proceed, answer) = (started.clone(), proceed.clone(), answer.clone());
        Box::pin(async move {
            if first {
                started.open();
                proceed.wait().await;
                return Ok(call("read_file", json!({"path": "a.cs"})));
            }
            Ok(text(&answer))
        })
    });
    (client, requests)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn send_message_to_running_agent_is_delivered_at_its_next_turn() {
    let (started, proceed) = (Gate::new(), Gate::new());
    let (client, requests) = two_step_client(started.clone(), proceed.clone(), "done");
    let o = native(client, None);
    o.set_tools(tools(&["read_file"]));
    o.set_executor(executor(|_, _| "contents".into()));
    let events = record_events(&o);

    let o1 = o.clone();
    let run = tokio::spawn(async move { o1.spawn_agent("t", "general").await });
    started.wait().await;
    let agent = o.get_running_agents().into_iter().next().unwrap();
    assert_eq!(
        o.send_message(&agent.number.to_string(), "also check b.cs"),
        AgentMessageResult::Queued
    );
    assert_eq!(agent.pending_message_count(), 1);
    proceed.open();
    run.await.unwrap();

    let requests = requests.lock().unwrap().clone();
    assert!(
        requests[1]
            .iter()
            .any(|m| m.role == Role::User && text_of(m).contains("also check b.cs"))
    );
    let delivered = events
        .lock()
        .unwrap()
        .iter()
        .filter(|e| matches!(e, AgentEvent::MessageDelivered { .. }))
        .count();
    assert_eq!(delivered, 1);
    assert!(
        agent
            .transcript
            .snapshot()
            .iter()
            .any(|e| e.kind == AgentTranscriptKind::UserMessage && e.text == "also check b.cs")
    );
}

#[tokio::test]
async fn message_arriving_with_the_final_answer_gets_one_more_turn() {
    let slot: Arc<Mutex<Option<Weak<AgentOrchestrator>>>> = Arc::new(Mutex::new(None));
    let s2 = slot.clone();
    let client = FuncClient::seq(move |_, _, n| {
        if n == 0 {
            let o = s2.lock().unwrap().clone().unwrap().upgrade().unwrap();
            let id = o.get_running_agents()[0].id.clone();
            o.send_message(&id, "wait, one more thing");
        }
        text(if n == 0 {
            "first answer"
        } else {
            "revised answer"
        })
    });
    let o = native(client.clone(), None);
    *slot.lock().unwrap() = Some(Arc::downgrade(&o));

    let c = o.spawn_agent("t", "general").await;

    assert_eq!(client.calls.load(Ordering::SeqCst), 2);
    // The Lead reads what the user told the agent along with its answer.
    assert!(result(&c).starts_with("revised answer"));
    assert!(result(&c).contains("wait, one more thing"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn background_agent_returns_immediately_and_can_be_resumed_with_a_follow_up() {
    let gate = Gate::new();
    let requests = Arc::new(Mutex::new(Vec::<Vec<ChatMessage>>::new()));
    let (g, r) = (gate.clone(), requests.clone());
    let client = FuncClient::new(move |m, _, _| {
        let n = {
            let mut r = r.lock().unwrap();
            r.push(m);
            r.len()
        };
        let g = g.clone();
        Box::pin(async move {
            if n == 1 {
                g.wait().await;
            }
            Ok(text(if n == 1 {
                "found it in Foo.cs"
            } else {
                "line 42"
            }))
        })
    });
    let o = native(client, None);

    let agent = o.start_background_agent("find the bug", "explore", None);
    assert_eq!(agent.origin, AgentOrigin::User);
    assert!(agent.is_active());
    gate.open();
    agent.wait().await;
    assert_eq!(agent.status(), AgentStatus::Completed);
    assert_eq!(result(&agent), "found it in Foo.cs");

    assert_eq!(
        o.send_message(&agent.id, "which line?"),
        AgentMessageResult::Resumed
    );
    agent.wait().await;

    assert_eq!(agent.status(), AgentStatus::Completed);
    assert!(result(&agent).starts_with("line 42"));
    assert!(result(&agent).contains("which line?"));
    // The resumed run continued the same conversation.
    let requests = requests.lock().unwrap().clone();
    assert!(
        requests[1]
            .iter()
            .any(|m| m.role == Role::Assistant && text_of(m) == "found it in Foo.cs")
    );
    assert_eq!(text_of(requests[1].last().unwrap()), "which line?");
}

#[tokio::test]
async fn model_spawned_agent_releases_its_conversation_and_refuses_follow_ups() {
    let mut hands_on = OrcaConfig::default();
    hands_on.agent.lead_mode = "hands-on".into();
    let o = native(FuncClient::stub("done"), Some(hands_on));
    let c = o.spawn_agent("t", "general").await;

    assert!(c.messages().is_empty());
    assert_eq!(
        o.send_message(&c.id, "more"),
        AgentMessageResult::NotAccepting
    );
    assert_eq!(o.send_message("999", "more"), AgentMessageResult::NotFound);
}

#[tokio::test]
async fn lead_spawned_agent_keeps_its_conversation_for_follow_ups_when_the_lead_can_delegate() {
    for lead_mode in ["auto", "orchestrator"] {
        let mut config = OrcaConfig::default();
        config.agent.lead_mode = lead_mode.into();
        let o = native(FuncClient::stub("done"), Some(config));
        let c = o.spawn_agent("t", "general").await;
        assert!(!c.messages().is_empty(), "{lead_mode}");
    }
}

#[tokio::test]
async fn background_agent_cannot_prompt_for_permission() {
    // C# checks a PermissionManager with a counting prompter; here the tool pipeline reads the
    // interaction scope the session's permission check reads.
    let seen: Arc<Mutex<Vec<bool>>> = Arc::new(Mutex::new(Vec::new()));
    // A user message last (the task, or a message resuming it) gets a bash call.
    let client = FuncClient::seq(|m, _, _| {
        if m.last().is_some_and(|l| l.role == Role::User) {
            call("bash", json!({"command": "x"}))
        } else {
            text("done")
        }
    });
    let o = native(client, None);
    o.set_tools(tools(&["bash"]));
    let s2 = seen.clone();
    o.set_executor(executor(move |_, _| {
        let non_interactive = orca_core::agent::permissions::is_non_interactive();
        s2.lock().unwrap().push(non_interactive);
        if non_interactive {
            "Error: permission denied".into()
        } else {
            "ran".into()
        }
    }));

    let agent = o.start_background_agent("run it", "general", None);
    wait_until(|| !agent.is_active()).await;
    assert_eq!(*seen.lock().unwrap(), vec![true]);
    // A foreground spawn (the Lead waits on it) is still interactive.
    o.spawn_agent("run it", "general").await;
    assert_eq!(*seen.lock().unwrap(), vec![true, false]);
    // A resumed background agent can't prompt either.
    assert_eq!(
        o.send_message(&agent.id, "again"),
        AgentMessageResult::Resumed
    );
    wait_until(|| !agent.is_active()).await;
    assert_eq!(seen.lock().unwrap().last(), Some(&true));
}

#[tokio::test]
async fn progress_marker_updates_reported_progress_and_raises_an_event() {
    let client = FuncClient::seq(|m, _, _| {
        if has_tool_message(m) {
            text("[progress 100%] done\nAll good.")
        } else {
            let mut r = call("read_file", json!({"path": "src/a.cs"}));
            r.text = "[progress 40%] reading sources".into();
            r
        }
    });
    let o = native(client, None);
    o.set_tools(tools(&["read_file"]));
    o.set_executor(executor(|_, _| "line1\nline2".into()));
    let events = record_events(&o);

    let c = o.spawn_agent("t", "general").await;

    let reports: Vec<(f64, Option<String>)> = events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ProgressReported { fraction, note, .. } => Some((*fraction, note.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(reports.iter().map(|r| r.0).collect::<Vec<_>>(), [0.4, 1.0]);
    assert_eq!(reports[0].1.as_deref(), Some("reading sources"));
    assert_eq!(c.reported_progress(), Some(1.0));
    assert_eq!(
        kinds_of(&c),
        [
            AgentTranscriptKind::Task,
            AgentTranscriptKind::AssistantText,
            AgentTranscriptKind::ToolCall,
            AgentTranscriptKind::ToolResult,
            AgentTranscriptKind::AssistantText,
            AgentTranscriptKind::Status
        ]
    );
    assert!(
        c.transcript
            .snapshot()
            .iter()
            .any(|e| e.kind == AgentTranscriptKind::ToolCall && e.text == "read src/a.cs")
    );
}

#[tokio::test]
async fn huge_result_is_truncated_before_reaching_the_parent() {
    let o = native(FuncClient::stub(&"x".repeat(50_000)), None);
    let c = o.spawn_agent("t", "general").await;
    assert!(result(&c).len() < 17_000);
    assert!(result(&c).contains("Result truncated"));
}

#[tokio::test]
async fn streaming_raises_text_deltas_and_still_executes_tool_calls() {
    let client = FuncClient::seq(|m, _, _| {
        if has_tool_message(m) {
            text("final answer")
        } else {
            let mut r = call("read_file", json!({"path": "a"}));
            r.text = "let me look".into();
            r
        }
    });
    let o = native(client.clone(), None);
    o.set_use_streaming(true);
    o.set_tools(tools(&["read_file"]));
    let executed = Arc::new(AtomicUsize::new(0));
    let e = executed.clone();
    o.set_executor(executor(move |_, _| {
        e.fetch_add(1, Ordering::SeqCst);
        "ok".into()
    }));
    let deltas = Arc::new(Mutex::new(String::new()));
    let d = deltas.clone();
    o.subscribe(move |e| {
        if let AgentEvent::Started { context, .. } = e {
            let d = d.clone();
            context
                .transcript
                .on_text_delta(Box::new(move |t| d.lock().unwrap().push_str(t)));
        }
    });

    let c = o.spawn_agent("t", "general").await;

    assert_eq!(executed.load(Ordering::SeqCst), 1);
    assert_eq!(result(&c), "final answer");
    assert!(deltas.lock().unwrap().contains("let me look"));
    assert!(client.streaming_calls.load(Ordering::SeqCst) >= 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn eviction_never_drops_a_running_agent() {
    let hold = Gate::new();
    let h = hold.clone();
    let client = FuncClient::new(move |m, _, _| {
        let h = h.clone();
        let long = task_of(&m) == "long";
        Box::pin(async move {
            if long {
                h.wait().await;
            }
            Ok(text("done"))
        })
    });
    let o = native(client, None);
    let o1 = o.clone();
    let long_run = tokio::spawn(async move { o1.spawn_agent("long", "general").await });
    wait_until(|| o.get_running_agents().len() == 1).await;

    for i in 0..40 {
        o.spawn_agent(&format!("short {i}"), "general").await;
    }

    assert!(o.get_all_agents().iter().any(|a| a.task == "long"));
    assert!(o.get_all_agents().len() <= 33);
    hold.open();
    long_run.await.unwrap();
}

#[tokio::test]
async fn final_result_has_progress_markers_removed() {
    let o = native(FuncClient::stub("[progress 100%] all done"), None);
    let c = o.spawn_agent("t", "general").await;
    assert_eq!(result(&c), "all done");
    assert_eq!(c.reported_progress(), Some(1.0));
}

#[test]
fn timeout_report_keeps_the_last_notes_and_tool_calls() {
    let c = AgentContext::new(AgentSpec::default());
    c.state().error = Some("Agent timed out after 1200 seconds (31 iterations).".into());
    c.transcript.add(
        AgentTranscriptKind::AssistantText,
        "Writing board.py first.",
        false,
    );
    c.transcript
        .add(AgentTranscriptKind::ToolCall, "write gems/board.py", false);
    c.transcript.add(
        AgentTranscriptKind::AssistantText,
        "Tests: 3 of 5 pass; fixing gravity.",
        false,
    );
    c.transcript.add(
        AgentTranscriptKind::ToolCall,
        "bash python -m unittest",
        false,
    );

    let report = timeout_report(&c, None);

    assert!(report.starts_with(
        "Agent timed out after 1200 seconds (31 iterations). Its work so far is on disk"
    ));
    assert!(report.contains("Last notes:\nTests: 3 of 5 pass; fixing gravity."));
    assert!(!report.contains("Writing board.py first."));
    assert!(report.contains("- write gems/board.py\n- bash python -m unittest"));
    let bare = AgentContext::new(AgentSpec::default());
    bare.state().error = Some("x.".into());
    assert_eq!(
        timeout_report(&bare, None),
        "x. Its work so far is on disk; check it and finish the task."
    );
}

#[tokio::test]
async fn system_prompt_factory_replaces_the_type_prompt_and_gets_the_filtered_tools() {
    let system = Arc::new(Mutex::new(String::new()));
    let s2 = system.clone();
    let client = FuncClient::seq(move |m, _, _| {
        *s2.lock().unwrap() = text_of(&m[0]).to_string();
        text("done")
    });
    let o = native(client, None);
    o.set_tools(tools(&["read", "write"]));
    o.type_registry_mut().use_pi_profile(false);
    let tools_seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let t2 = tools_seen.clone();
    o.set_system_prompt_factory(Some(Arc::new(
        move |ty: &AgentTypeDefinition, tools: &[ToolDefinition], task: &str| {
            *t2.lock().unwrap() = tools.iter().map(|t| t.name.clone()).collect();
            Some(format!("PI PROMPT for {}: {task}", ty.name))
        },
    )));

    o.spawn_agent("look around", "explore").await;

    let system = system.lock().unwrap().clone();
    assert!(
        system.starts_with("PI PROMPT for scout: look around"),
        "{system}"
    );
    assert!(system.contains(PROGRESS_HINT));
    assert_eq!(*tools_seen.lock().unwrap(), ["read"]);
}

// ── AgentFooterTests ─────────────────────────────────────────────────────────

#[tokio::test]
async fn parallel_spawns_get_distinct_colours_and_their_labels() {
    let gate = Gate::new();
    let g = gate.clone();
    let client = FuncClient::new(move |_, _, _| {
        let g = g.clone();
        Box::pin(async move {
            g.wait().await;
            Ok(text("done"))
        })
    });
    let o = native(client, None);
    let batch = vec![
        ParallelAgentSpec {
            label: Some("Read alpha".into()),
            ..ParallelAgentSpec::new("Survey alpha.txt", "explore")
        },
        ParallelAgentSpec::new("Survey beta.txt", "explore"),
        ParallelAgentSpec::new("Survey gamma", "general"),
    ];
    gate.open();
    let agents = o.spawn_parallel(&batch, 0, &none()).await.unwrap();

    let mut slots: Vec<i32> = agents.iter().map(|a| a.color_slot).collect();
    slots.sort();
    assert_eq!(slots, [0, 1, 2]);
    assert_eq!(agents[0].label(), "Read alpha");
    assert_eq!(agents[1].label(), "Survey beta.txt");
    assert_eq!(
        agents.iter().find(|a| a.color_slot == 0).unwrap().tag(),
        "Agent A"
    );
}

#[tokio::test]
async fn finished_agents_release_their_colour_after_their_linger() {
    let o = native(FuncClient::stub("done"), None);
    let first = o.spawn_agent("one", "general").await;
    // Still lingering on the footer: the next agent takes the next colour.
    let second = o.spawn_agent("two", "general").await;
    assert_eq!(first.color_slot, 0);
    assert_eq!(second.color_slot, 1);

    let long_ago = SystemTime::now() - MODEL_HOLD - Duration::from_secs(1);
    first.state().completed_at = Some(long_ago);
    second.state().completed_at = Some(long_ago);
    // Off the footer, but the letters stay theirs for the session: the third agent is C.
    let third = o.spawn_agent("three", "general").await;
    assert_eq!(third.color_slot, 2);
    assert_eq!(third.tag(), "Agent C");
}

#[tokio::test]
async fn letters_are_unique_for_the_session_across_batches() {
    let o = native(FuncClient::stub("done"), None);
    let first = o
        .spawn_parallel(&specs(&["one", "two"]), 0, &none())
        .await
        .unwrap();
    let second = o
        .spawn_parallel(
            &[
                spec("three", Some("Third"), None, None),
                spec("four", None, None, None),
            ],
            0,
            &none(),
        )
        .await
        .unwrap();
    let single = o.spawn_agent("five", "general").await;

    let tags: Vec<String> = first
        .iter()
        .chain(second.iter())
        .chain([&single])
        .map(|a| a.tag())
        .collect();
    assert_eq!(
        tags,
        ["Agent A", "Agent B", "Agent C", "Agent D", "Agent E"]
    );
    assert!(Arc::ptr_eq(&second[0], &o.find_agent("C").unwrap()));
    assert!(Arc::ptr_eq(&second[0], &o.find_agent("Agent C").unwrap()));
}

#[tokio::test]
async fn batch_letters_follow_the_listed_order_not_the_start_order() {
    let o = native(FuncClient::stub("done"), None);
    // The biggest task (listed last) and a dependent one start out of order; letters stay A, B, C.
    let agents = o
        .spawn_parallel(
            &[
                spec("small", Some("Small"), Some(10), None),
                spec("needs small", Some("Dependent"), Some(50), Some(vec![0])),
                spec("big", Some("Big"), Some(900), None),
            ],
            0,
            &none(),
        )
        .await
        .unwrap();
    let tags: Vec<String> = agents.iter().map(|a| a.tag()).collect();
    assert_eq!(tags, ["Agent A", "Agent B", "Agent C"]);
}

#[tokio::test]
async fn follow_up_to_an_unknown_letter_lists_the_valid_names() {
    let o = native(FuncClient::stub("done"), None);
    o.spawn_parallel(
        &[
            spec("one", Some("Scaffold"), None, None),
            spec("two", Some("Build UI"), None, None),
        ],
        0,
        &none(),
    )
    .await
    .unwrap();
    let error = o.follow_up("C", "more", &none()).await.unwrap_err();
    assert!(error.contains("No agent \"C\""), "{error}");
    assert!(error.contains("A (Scaffold, finished)"), "{error}");
    assert!(error.contains("B (Build UI, finished)"), "{error}");
}

#[test]
fn report_progress_from_the_main_loop_moves_the_lead_bar() {
    let o = native(FuncClient::stub("x"), None);
    let events = record_events(&o);

    let reply = o.report_progress(55, Some("running the whole test suite"));

    assert_eq!(o.lead_progress(), Some(0.55));
    assert_eq!(o.lead_note().as_deref(), Some("running the whole test"));
    assert_eq!(reply, "Progress recorded: 55%.");
    let ids: Vec<String> = events
        .lock()
        .unwrap()
        .iter()
        .map(|e| e.agent_id().to_string())
        .collect();
    assert_eq!(ids, [LEAD_ID]);

    o.reset_lead_progress();
    assert!(o.lead_progress().is_none());
    assert!(o.lead_note().is_none());
}

#[tokio::test]
async fn report_progress_inside_a_sub_agent_moves_that_agents_bar_even_for_restricted_types() {
    let client = FuncClient::seq(|m, _, _| {
        if has_tool_message(m) {
            text("finished")
        } else {
            call(
                "report_progress",
                json!({"percent": 40, "label": "reading tests"}),
            )
        }
    });
    let o = native(client, None);
    o.set_tools(tools(&["read_file", REPORT_PROGRESS_TOOL_NAME]));
    let weak = Arc::downgrade(&o);
    o.set_executor(executor(move |name, args| {
        assert_eq!(name, "report_progress");
        let v: Value = serde_json::from_str(args).unwrap();
        weak.upgrade()
            .unwrap()
            .report_progress(v["percent"].as_i64().unwrap() as i32, v["label"].as_str())
    }));

    // explore only lists read-only tools — report_progress is always allowed.
    let agent = o.spawn_agent("look", "explore").await;

    assert_eq!(agent.status(), AgentStatus::Completed);
    assert_eq!(agent.reported_progress(), Some(0.4));
    assert_eq!(agent.progress_note().as_deref(), Some("reading tests"));
    assert!(o.lead_progress().is_none()); // the Lead's bar is untouched
    let explore = o.type_registry().resolve("explore").unwrap();
    assert!(is_tool_allowed(&explore, "report_progress"));
}

#[tokio::test]
async fn system_prompt_teaches_the_progress_tool_when_the_agent_has_it() {
    let system = Arc::new(Mutex::new(String::new()));
    let s2 = system.clone();
    let o = native(
        FuncClient::seq(move |m, _, _| {
            *s2.lock().unwrap() = text_of(&m[0]).to_string();
            text("done")
        }),
        None,
    );
    o.set_tools(tools(&["report_progress"]));

    o.spawn_agent("look", "explore").await;

    let system = system.lock().unwrap().clone();
    assert!(system.contains(PROGRESS_TOOL_HINT));
    assert!(!system.contains(PROGRESS_HINT));
}

#[tokio::test]
async fn progress_markers_still_work() {
    let o = native(
        FuncClient::stub("[progress 70%] wrapping up\nall done"),
        None,
    );
    let agent = o.spawn_agent("look", "general").await;
    assert_eq!(agent.reported_progress(), Some(0.7));
    assert_eq!(result(&agent).replace('\r', ""), "wrapping up\nall done");
}

// ── PeerReplyGuardTests ──────────────────────────────────────────────────────

#[test]
fn cut_off_stub_keeps_only_the_start() {
    let reply = format!("Re-run the check:\n<tool_call>{}", "1.2.3-".repeat(5000));
    let stub = cut_off_stub(&reply, Some("1.2.3-"));
    assert!(stub.chars().count() < CUT_OFF_STUB_CHARS + 120);
    assert!(stub.starts_with("Re-run the check:"));
    assert!(stub.ends_with("[reply stopped: it kept repeating \"1.2.3-\"; nothing ran]"));
    assert_eq!(
        cut_off_stub("", None),
        "[reply cut off at the output limit; nothing ran]"
    );
}

#[test]
fn peer_cap_takes_the_lower() {
    assert_eq!(peer_cap(Some(32768), 0), Some(32768));
    assert_eq!(peer_cap(Some(32768), 8192), Some(8192));
    assert_eq!(peer_cap(Some(4096), 8192), Some(4096));
    assert_eq!(peer_cap(None, 8192), Some(8192));
    assert_eq!(peer_cap(None, 0), None);
}

fn peer_config(peer: bool) -> OrcaConfig {
    let mut config = OrcaConfig::default();
    config.agent.max_iterations = 10;
    config.agent.peer_mode = peer;
    config.lm_studio.max_tokens = Some(32768);
    config.lm_studio.native_tool_calling = true;
    config
}

/// One scripted streamed update: content and finish reason.
type Update = (String, Option<&'static str>);

/// Streams scripted updates per call and counts how many were consumed.
struct ScriptedStreamClient {
    script: Box<dyn Fn(usize) -> Vec<Update> + Send + Sync>,
    calls: AtomicUsize,
    requests: Mutex<Vec<Vec<ChatMessage>>>,
    options: Mutex<Vec<ChatOptions>>,
    yielded: Mutex<Vec<usize>>,
}

impl ScriptedStreamClient {
    fn new(script: impl Fn(usize) -> Vec<Update> + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            script: Box::new(script),
            calls: AtomicUsize::new(0),
            requests: Mutex::new(Vec::new()),
            options: Mutex::new(Vec::new()),
            yielded: Mutex::new(Vec::new()),
        })
    }

    fn record(&self, messages: &[ChatMessage], options: &ChatOptions) -> Vec<Update> {
        self.requests.lock().unwrap().push(messages.to_vec());
        self.options.lock().unwrap().push(options.clone());
        (self.script)(self.calls.fetch_add(1, Ordering::SeqCst))
    }
}

fn assemble(updates: &[Update]) -> ChatResponse {
    ChatResponse {
        text: updates.iter().map(|u| u.0.as_str()).collect(),
        finish_reason: updates.iter().rev().find_map(|u| u.1).map(str::to_string),
        content_items: updates.len(),
        text_parts: updates.len(),
        updates: updates.len(),
        ..ChatResponse::default()
    }
}

#[async_trait::async_trait]
impl AgentChatClient for ScriptedStreamClient {
    async fn complete(
        &self,
        _: &str,
        messages: &[ChatMessage],
        options: &ChatOptions,
    ) -> Result<ChatResponse, ChatError> {
        let updates = self.record(messages, options);
        self.yielded.lock().unwrap().push(updates.len());
        Ok(assemble(&updates))
    }

    async fn stream(
        &self,
        _: &str,
        messages: &[ChatMessage],
        options: &ChatOptions,
        on_event: &mut (dyn FnMut(StreamEvent) -> StreamControl + Send),
    ) -> Result<ChatResponse, ChatError> {
        let updates = self.record(messages, options);
        let index = {
            let mut y = self.yielded.lock().unwrap();
            y.push(0);
            y.len() - 1
        };
        for (i, u) in updates.iter().enumerate() {
            self.yielded.lock().unwrap()[index] += 1;
            on_event(StreamEvent::Text(u.0.clone()));
            if on_event(StreamEvent::UpdateEnd {
                tool_call_delta: false,
            }) == StreamControl::Stop
            {
                let mut r = assemble(&updates[..=i]);
                r.finish_reason = None;
                r.stopped = true;
                return Ok(r);
            }
            tokio::task::yield_now().await;
        }
        Ok(assemble(&updates))
    }
}

fn repeating(head: &str, unit: &str, count: usize) -> Vec<Update> {
    let mut updates = vec![(head.to_string(), None)];
    for _ in 0..count {
        updates.push((unit.to_string(), None));
    }
    updates.last_mut().unwrap().1 = Some(if count >= 20000 { "length" } else { "stop" });
    updates
}

fn peer_orchestrator(
    client: Arc<ScriptedStreamClient>,
    config: OrcaConfig,
) -> Arc<AgentOrchestrator> {
    let o = orchestrator(client, config);
    o.set_use_streaming(true);
    o.set_tools(tools(&["bash"]));
    o.set_executor(executor(|_, _| "ok".into()));
    o
}

#[tokio::test]
async fn peer_mode_repeating_reply_is_stopped_early_stubbed_in_history_and_the_helper_is_told_why()
{
    let client = ScriptedStreamClient::new(|n| match n {
        0 => repeating(
            "Re-run the check:\n<tool_call>\n<function=bash>\n",
            "1.2.3-",
            20000,
        ),
        _ => vec![("All checks pass.".into(), Some("stop"))],
    });
    let o = peer_orchestrator(client.clone(), peer_config(true));

    let c = o.spawn_agent("finish semver.py", "general").await;

    assert_eq!(c.status(), AgentStatus::Completed);
    assert_eq!(result(&c).trim(), "All checks pass.");
    // The repeating stream was abandoned long before its end (20k updates).
    let yielded = client.yielded.lock().unwrap()[0];
    assert!(yielded < 1000, "yielded {yielded} chunks");
    let requests = client.requests.lock().unwrap().clone();
    let second = &requests[1];
    let stub = &second[second.len() - 2];
    assert_eq!(stub.role, Role::Assistant);
    assert!(text_of(stub).len() < 1000);
    assert!(text_of(stub).contains("kept repeating"));
    assert!(
        text_of(second.last().unwrap())
            .contains("[Output stopped] Your last reply got stuck repeating the same text")
    );
}

#[tokio::test]
async fn peer_mode_reply_cut_off_at_the_limit_is_stubbed_and_after_three_in_a_row_the_helper_stops_with_a_partial_report()
 {
    let client = ScriptedStreamClient::new(|n| {
        if n < 3 {
            let numbers: Vec<String> = (0..3000).map(|i| i.to_string()).collect();
            vec![(
                format!("<tool_call>\n{}{}", "a".repeat(50), numbers.join(" ")),
                Some("length"),
            )]
        } else {
            vec![(
                "## Completed\nparse\n## Remaining\nsatisfies".into(),
                Some("stop"),
            )]
        }
    });
    let o = peer_orchestrator(client.clone(), peer_config(true));

    let c = o.spawn_agent("finish semver.py", "general").await;

    assert_eq!(c.status(), AgentStatus::CompletedPartial);
    let requests = client.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 4);
    assert!(result(&c).contains("## Remaining"));
    let last = &requests[3];
    assert!(text_of(last.last().unwrap()).contains("Your last replies were stopped or cut off"));
    assert!(
        last.iter()
            .filter(|m| m.role == Role::Assistant)
            .all(|m| text_of(m).len() < 1000)
    );
    let second_last = text_of(requests[1].last().unwrap());
    assert!(second_last.contains("[Output limit]"));
    assert!(second_last.contains(CUT_OFF_NO_REPEAT_HINT));
}

#[tokio::test]
async fn without_peer_mode_nothing_changes_the_repeating_stream_runs_to_its_end() {
    let client = ScriptedStreamClient::new(|n| match n {
        0 => repeating("x", "1.2.3-", 2000),
        _ => vec![("done".into(), Some("stop"))],
    });
    let o = peer_orchestrator(client.clone(), peer_config(false));

    let c = o.spawn_agent("task", "general").await;

    // Not cut off (no Length finish), so the repeated text is the (bad) answer, as before.
    assert_eq!(client.yielded.lock().unwrap()[0], 2001);
    assert_eq!(client.requests.lock().unwrap().len(), 1);
    assert_eq!(c.status(), AgentStatus::Completed);
}

#[tokio::test]
async fn peer_mode_guard_switched_off_leaves_the_stream_alone() {
    let client = ScriptedStreamClient::new(|_| repeating("x", "1.2.3-", 2000));
    let mut config = peer_config(true);
    config.agent.peer_repetition_guard = false;
    let o = peer_orchestrator(client.clone(), config);

    o.spawn_agent("task", "general").await;

    assert_eq!(client.yielded.lock().unwrap()[0], 2001);
}

#[tokio::test]
async fn peer_max_tokens_caps_helper_requests_only_in_peer_mode() {
    for peer in [true, false] {
        let client = ScriptedStreamClient::new(|_| vec![("done".into(), Some("stop"))]);
        let mut config = peer_config(peer);
        config.agent.peer_max_tokens = 8192;
        peer_orchestrator(client.clone(), config)
            .spawn_agent("task", "general")
            .await;
        let expected = if peer { 8192 } else { 32768 };
        assert_eq!(
            client.options.lock().unwrap()[0].max_output_tokens,
            Some(expected),
            "peer {peer}"
        );
    }
}

// ── AgentUserMessageTests ────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn message_while_running_reaches_the_agent_and_its_result_tells_the_lead() {
    let (started, proceed) = (Gate::new(), Gate::new());
    let (client, requests) =
        two_step_client(started.clone(), proceed.clone(), "used b.cs as asked");
    let o = native(client, None);
    o.set_tools(tools(&["read_file"]));
    o.set_executor(executor(|_, _| "contents".into()));

    let o1 = o.clone();
    let run = tokio::spawn(async move { o1.spawn_agent("fix a.cs", "general").await });
    started.wait().await;
    let agent = o.get_running_agents().into_iter().next().unwrap();
    assert_eq!(
        o.send_message(&agent.id, "use b.cs instead"),
        AgentMessageResult::Queued
    );
    proceed.open();
    let done = run.await.unwrap();

    // The agent read it at its next turn…
    let requests = requests.lock().unwrap().clone();
    assert!(
        requests[1]
            .iter()
            .any(|m| m.role == Role::User && text_of(m).contains("use b.cs instead"))
    );
    // …its tab shows it as the user's…
    let messages: Vec<_> = done
        .transcript
        .snapshot()
        .into_iter()
        .filter(|e| e.kind == AgentTranscriptKind::UserMessage)
        .collect();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].from.as_deref(), Some("user"));
    // …and the result the Lead reads says what the user told it.
    assert!(result(&done).starts_with("used b.cs as asked"));
    assert!(result(&done).contains("User told this agent directly"));
    assert!(result(&done).contains("use b.cs instead"));
    // A run the Lead awaited needs no separate note.
    assert!(o.drain_user_agent_notes().is_none());
}

#[tokio::test]
async fn message_after_finish_continues_the_agent_and_leaves_a_note_for_the_lead() {
    let requests = Arc::new(Mutex::new(Vec::<Vec<ChatMessage>>::new()));
    let r = requests.clone();
    let client = FuncClient::seq(move |m, _, n| {
        r.lock().unwrap().push(m.to_vec());
        text(if n == 0 {
            "found it in Foo.cs"
        } else {
            "line 42"
        })
    });
    let mut config = OrcaConfig::default();
    config.agent.lead_mode = "auto".into(); // the Lead's agents keep their conversation
    let o = native(client, Some(config));
    let replied = Arc::new(Mutex::new(None::<(String, String)>));
    let rp = replied.clone();
    o.on_user_agent_replied(move |a, note| {
        *rp.lock().unwrap() = Some((a.id.clone(), note.to_string()))
    });

    let agent = o.spawn_agent("find the bug", "general").await;
    assert!(can_resume(&agent));
    assert_eq!(result(&agent), "found it in Foo.cs");

    assert_eq!(
        o.send_message(&agent.id, "which line?"),
        AgentMessageResult::Resumed
    );
    agent.wait().await;

    assert_eq!(agent.status(), AgentStatus::Completed);
    assert!(result(&agent).starts_with("line 42"));
    // Same conversation, continued.
    let requests = requests.lock().unwrap().clone();
    assert!(
        requests[1]
            .iter()
            .any(|m| m.role == Role::Assistant && text_of(m) == "found it in Foo.cs")
    );
    assert_eq!(text_of(requests[1].last().unwrap()), "which line?");
    assert!(
        agent
            .transcript
            .snapshot()
            .iter()
            .any(|e| e.kind == AgentTranscriptKind::UserMessage
                && e.from.as_deref() == Some("user")
                && e.text == "which line?")
    );

    // The Lead gets one note, with the reply, at its next turn; the UI hears about it once.
    let note = o.drain_user_agent_notes().expect("a note for the Lead");
    assert!(
        note.contains(&format!("The user messaged {}", agent.tag())),
        "{note}"
    );
    assert!(note.contains("line 42"));
    assert!(note.contains("which line?"));
    assert!(o.drain_user_agent_notes().is_none());
    let replied = replied
        .lock()
        .unwrap()
        .clone()
        .expect("UserAgentReplied raised");
    assert_eq!(replied.0, agent.id);
    assert!(!agent.state().resumed_by_user);
}

#[tokio::test]
async fn lead_follow_up_is_marked_as_the_leads_and_leaves_no_user_note() {
    let client = FuncClient::seq(|_, _, n| text(if n == 0 { "first" } else { "second" }));
    let mut config = OrcaConfig::default();
    config.agent.lead_mode = "auto".into();
    let o = native(client, Some(config));
    let agent = o.spawn_agent("t", "general").await;

    let again = o
        .follow_up(&agent.id, "fix the test", &none())
        .await
        .unwrap();

    assert_eq!(result(&again), "second");
    assert!(
        agent.transcript.snapshot().iter().any(
            |e| e.kind == AgentTranscriptKind::UserMessage && e.from.as_deref() == Some("lead")
        )
    );
    assert!(o.drain_user_agent_notes().is_none());
}

#[tokio::test]
async fn finished_agent_without_its_conversation_refuses_a_message() {
    let mut config = OrcaConfig::default();
    config.agent.lead_mode = "hands-on".into();
    let o = native(FuncClient::stub("done"), Some(config));
    let agent = o.spawn_agent("t", "general").await;

    assert!(!can_resume(&agent));
    assert_eq!(
        o.send_message(&agent.id, "more"),
        AgentMessageResult::NotAccepting
    );
    assert_eq!(result(&agent), "done");
}
