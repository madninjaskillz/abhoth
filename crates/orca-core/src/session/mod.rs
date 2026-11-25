//! Sessions and checkpoints (port of `OpenOrca.Core/Session`).
//!
//! Session files are interchangeable with the C# build: same location
//! (`<config dir>/sessions/<id>.json`), field names, `JsonDefaults` formatting and escaping.

pub mod checkpoint;
pub mod data;
pub mod datetime;
pub mod manager;

pub use checkpoint::{CheckpointEntry, CheckpointManager};
pub use data::{
    SessionData, SessionMessage, SessionToolCall, SessionToolResult, messages_to_session,
    new_session_id, session_to_messages,
};
pub use datetime::DotNetDateTime;
pub use manager::{EngineRef, SessionError, SessionManager, generate_title};

/// The checkpoint session id when no session is active (`CurrentSessionId ?? "default"`).
pub const DEFAULT_CHECKPOINT_SESSION: &str = "default";
