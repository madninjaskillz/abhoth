//! Something that happened in a bridge turn, in OpenOrca's terms (`AgentEngineEvent`).

/// The turn ended (`EngineTurnCompleted`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct EngineTurnCompleted {
    /// "success", "cancelled" or "error" (with the CLI's subtype in `detail`).
    pub outcome: String,
    pub detail: Option<String>,
    pub session_id: Option<String>,
    /// The last assistant message's text.
    pub final_text: Option<String>,
    pub cost_usd: Option<f64>,
    pub input_tokens: Option<i32>,
    pub output_tokens: Option<i32>,
    pub duration_ms: Option<i64>,
    pub num_turns: Option<i32>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AgentEngineEvent {
    /// The CLI reported its session (every turn starts with one).
    SessionStarted {
        session_id: Option<String>,
        model: Option<String>,
        working_directory: Option<String>,
    },
    /// Streamed answer text.
    TextDelta(String),
    /// Streamed reasoning text.
    ThinkingDelta(String),
    /// The model is thinking; estimated tokens so far in this block, when reported.
    ThinkingProgress(Option<i32>),
    /// A tool call starts. `name` is OpenOrca's name for it, `args_json` uses OpenOrca's keys.
    ToolCall {
        id: String,
        name: String,
        original_name: String,
        args_json: String,
    },
    /// A tool call finished.
    ToolResult {
        id: String,
        name: String,
        result: String,
        is_error: bool,
        args_json: Option<String>,
    },
    /// A tool call changed a file (full path).
    FileEdited { path: String, tool_call_id: String },
    /// A line for the transcript: "info", "warning" or "error".
    Notice { level: String, text: String },
    /// The turn ended.
    TurnCompleted(EngineTurnCompleted),
}
