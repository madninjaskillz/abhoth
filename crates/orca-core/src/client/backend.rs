//! A chat backend: what answers a request (`IChatClient` in C#). [`super::OpenAiClient`] is
//! one; the slot-pinned client ([`crate::slots::SlottedChatClient`]) routes to one per copy.

use async_trait::async_trait;

use super::{ChatError, ChatMessage, ChatOptions, ChatResponse, StreamControl, StreamEvent};

#[async_trait]
pub trait ChatBackend: Send + Sync {
    /// Non-streaming request.
    async fn complete(
        &self,
        actor: &str,
        messages: &[ChatMessage],
        options: &ChatOptions,
    ) -> Result<ChatResponse, ChatError>;

    /// Streaming request with a stop hook (see [`super::OpenAiClient::stream_with_control`]).
    async fn stream_with_control(
        &self,
        actor: &str,
        messages: &[ChatMessage],
        options: &ChatOptions,
        on_event: &mut (dyn FnMut(StreamEvent) -> StreamControl + Send),
    ) -> Result<ChatResponse, ChatError>;
}

#[async_trait]
impl ChatBackend for super::OpenAiClient {
    async fn complete(
        &self,
        actor: &str,
        messages: &[ChatMessage],
        options: &ChatOptions,
    ) -> Result<ChatResponse, ChatError> {
        super::OpenAiClient::complete(self, actor, messages, options).await
    }

    async fn stream_with_control(
        &self,
        actor: &str,
        messages: &[ChatMessage],
        options: &ChatOptions,
        on_event: &mut (dyn FnMut(StreamEvent) -> StreamControl + Send),
    ) -> Result<ChatResponse, ChatError> {
        super::OpenAiClient::stream_with_control(self, actor, messages, options, on_event).await
    }
}

/// `FailoverChatClient.IsTransportFailure`: the server could not be reached or failed (5xx),
/// so another server may answer.
pub fn is_transport_failure(e: &ChatError) -> bool {
    match e {
        ChatError::Transport { .. } | ChatError::Timeout(_) => true,
        ChatError::Http { status, .. } => *status >= 500,
        _ => false,
    }
}
