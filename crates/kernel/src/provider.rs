//! `Provider` (§3.8): model requests, responses, streaming deltas, and errors.

use std::pin::Pin;
use std::time::Duration;

use async_trait::async_trait;
use futures_core::Stream;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::SessionId;
use crate::content::{ContentBlock, Message, PromptBlock};
use crate::hash::{Hash, HashError};
use crate::tool::{ToolCall, ToolDefinition};

/// Sampling parameters.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelParams {
    /// Temperature; `None` means "do not send".
    pub temperature: Option<f64>,
    /// Nucleus sampling.
    pub top_p: Option<f64>,
    /// Output token cap.
    pub max_tokens: Option<u32>,
    /// Stop sequences.
    #[serde(default)]
    pub stop: Vec<String>,
    /// Extended reasoning.
    pub thinking: Option<ThinkingConfig>,
    /// Provider-specific pass-through (vLLM guided decoding flags, LiteLLM metadata). Hashed.
    #[serde(default)]
    pub extra: Value,
}

impl Default for ModelParams {
    fn default() -> Self {
        ModelParams {
            temperature: None,
            top_p: None,
            max_tokens: None,
            stop: Vec::new(),
            thinking: None,
            extra: Value::Null,
        }
    }
}

/// Reasoning configuration.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ThinkingConfig {
    /// Request extended reasoning.
    pub enabled: bool,
    /// Reasoning budget.
    pub budget_tokens: Option<u32>,
}

/// Non-hashed request context. Filled by the kernel; providers may log it, never send it to the model.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RequestTrace {
    /// The session.
    pub session_id: SessionId,
    /// The turn.
    pub turn: u64,
    /// 1-based retry attempt.
    pub attempt: u32,
    /// Replay key, first half.
    pub checkpoint_hash: Hash,
    /// Unique per attempt (UUIDv7); goes into an `X-Request-Id`-style header if the endpoint supports one.
    pub request_id: String,
}

/// One model call.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelRequest {
    /// Opaque endpoint model string from the model profile (`served-name` or `provider/model`).
    pub model_id: String,
    /// System prompt blocks in D7 order.
    pub system: Vec<PromptBlock>,
    /// The conversation.
    pub messages: Vec<Message>,
    /// Exposed tools.
    pub tools: Vec<ToolDefinition>,
    /// Sampling parameters.
    pub params: ModelParams,
    /// Volatile: excluded from `request_hash`.
    pub trace: RequestTrace,
}

/// The hashed part of a `ModelRequest`.
#[derive(Serialize)]
struct HashedRequest<'a> {
    model_id: &'a str,
    system: &'a [PromptBlock],
    messages: &'a [Message],
    tools: &'a [ToolDefinition],
    params: &'a ModelParams,
}

impl ModelRequest {
    /// `Hash::of_canonical_json` of `{model_id, system, messages, tools, params}` (`event-schema.md` §3.1).
    pub fn request_hash(&self) -> Result<Hash, HashError> {
        Hash::of_canonical_json(&HashedRequest {
            model_id: &self.model_id,
            system: &self.system,
            messages: &self.messages,
            tools: &self.tools,
            params: &self.params,
        })
    }

    /// The system prompt as one string: blocks joined with `"\n\n"`.
    pub fn system_text(&self) -> String {
        self.system
            .iter()
            .map(|b| b.text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

/// Why the model stopped.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// Natural end.
    EndTurn,
    /// Stopped to call tools.
    ToolUse,
    /// Hit `max_tokens`.
    MaxTokens,
    /// Hit a stop sequence.
    StopSequence,
    /// Provider content filter.
    ContentFilter,
    /// Anything else, verbatim.
    Other(String),
}

/// Token usage (D13).
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Usage {
    /// Prompt tokens.
    pub input_tokens: u64,
    /// Completion tokens.
    pub output_tokens: u64,
    /// Cache-read tokens, when reported.
    pub cache_read_tokens: Option<u64>,
    /// Reasoning tokens, when reported.
    pub reasoning_tokens: Option<u64>,
}

/// A completed model response.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelResponse {
    /// Content blocks.
    pub content: Vec<ContentBlock>,
    /// Stop reason.
    pub stop_reason: StopReason,
    /// Usage.
    pub usage: Usage,
    /// What the endpoint reported it served (may differ from `ModelRequest.model_id` behind a proxy).
    pub model_id: String,
    /// `Hash::of_bytes` of the raw provider body (for streaming: the concatenated SSE data lines).
    pub raw_response_hash: Hash,
    /// Provider's own id, if any. Volatile (excluded from `response_hash`).
    pub response_id: Option<String>,
}

/// The hashed part of a `ModelResponse`.
#[derive(Serialize)]
struct HashedResponse<'a> {
    content: &'a [ContentBlock],
    stop_reason: &'a StopReason,
    usage: &'a Usage,
    model_id: &'a str,
}

impl ModelResponse {
    /// `Hash::of_canonical_json` of `{content, stop_reason, usage, model_id}` (`event-schema.md` §3.2).
    pub fn response_hash(&self) -> Result<Hash, HashError> {
        Hash::of_canonical_json(&HashedResponse {
            content: &self.content,
            stop_reason: &self.stop_reason,
            usage: &self.usage,
            model_id: &self.model_id,
        })
    }

    /// `ToolUse` blocks in order, as `ToolCall`s.
    pub fn tool_calls(&self) -> Vec<ToolCall> {
        self.content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolUse { id, name, input } => Some(ToolCall {
                    tool_use_id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                }),
                _ => None,
            })
            .collect()
    }
}

/// Streaming increments. Forwarded to `KernelHandle::subscribe_deltas`; never logged.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModelDelta {
    /// Text appended to block `index`.
    TextDelta {
        /// Block index.
        index: u32,
        /// Text.
        text: String,
    },
    /// Reasoning appended to block `index`.
    ThinkingDelta {
        /// Block index.
        index: u32,
        /// Text.
        text: String,
    },
    /// A tool-use block started.
    ToolUseStart {
        /// Block index.
        index: u32,
        /// Tool-use id.
        id: String,
        /// Tool name.
        name: String,
    },
    /// Partial JSON for a tool-use block's input.
    ToolUseInputDelta {
        /// Block index.
        index: u32,
        /// JSON fragment.
        partial_json: String,
    },
    /// Block `index` is complete.
    BlockStop {
        /// Block index.
        index: u32,
    },
    /// Always last. The provider assembles the final response; the kernel hashes and logs only this.
    Complete(ModelResponse),
}

/// Provider errors (D15 retry classes).
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    /// 429.
    #[error("rate limited (retry_after: {retry_after:?})")]
    RateLimited {
        /// Server-suggested wait.
        retry_after: Option<Duration>,
    },
    /// 5xx.
    #[error("server error {status}: {message}")]
    Server {
        /// HTTP status.
        status: u16,
        /// Body or reason.
        message: String,
    },
    /// The per-attempt timeout fired.
    #[error("request timed out after {0:?}")]
    Timeout(Duration),
    /// Connection-level failure.
    #[error("transport error: {0}")]
    Transport(String),
    /// 4xx other than auth/context.
    #[error("client error {status}: {message}")]
    Client {
        /// HTTP status.
        status: u16,
        /// Body or reason.
        message: String,
    },
    /// 401/403.
    #[error("authentication failed: {0}")]
    Auth(String),
    /// The request exceeded the context window.
    #[error("context length exceeded: {0}")]
    ContextTooLong(String),
    /// The body could not be parsed.
    #[error("unparseable response: {0}")]
    InvalidResponse(String),
    /// Record/replay cache miss (P1.4).
    #[error("replay miss for request {request_hash} at checkpoint {checkpoint_hash}")]
    ReplayMiss {
        /// Replay key, first half.
        checkpoint_hash: Hash,
        /// Replay key, second half.
        request_hash: Hash,
    },
    /// Cancelled.
    #[error("cancelled")]
    Cancelled,
}

impl ProviderError {
    /// D15 plus `Transport`: `RateLimited`, `Server` (5xx), `Timeout`, `Transport`
    /// are retryable; everything else is not. `Server` with status 501/505 is NOT retryable.
    pub fn retryable(&self) -> bool {
        match self {
            ProviderError::RateLimited { .. }
            | ProviderError::Timeout(_)
            | ProviderError::Transport(_) => true,
            ProviderError::Server { status, .. } => !matches!(status, 501 | 505),
            _ => false,
        }
    }

    /// Stable string for `turn_failed.error_class`, e.g. `"provider_rate_limited"`.
    pub fn class(&self) -> &'static str {
        match self {
            ProviderError::RateLimited { .. } => "provider_rate_limited",
            ProviderError::Server { .. } => "provider_server",
            ProviderError::Timeout(_) => "provider_timeout",
            ProviderError::Transport(_) => "provider_transport",
            ProviderError::Client { .. } => "provider_client",
            ProviderError::Auth(_) => "provider_auth",
            ProviderError::ContextTooLong(_) => "provider_context_too_long",
            ProviderError::InvalidResponse(_) => "provider_invalid_response",
            ProviderError::ReplayMiss { .. } => "replay_miss",
            ProviderError::Cancelled => "cancelled",
        }
    }
}

/// A stream of deltas ending in exactly one `Complete` or an `Err`.
pub type DeltaStream = Pin<Box<dyn Stream<Item = Result<ModelDelta, ProviderError>> + Send>>;

/// A model client.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Human-readable provider name for logs (`"openai_compat"`, `"replay"`).
    fn name(&self) -> &str;
    /// One completion.
    async fn complete(&self, req: ModelRequest) -> Result<ModelResponse, ProviderError>;
    /// Default: one `Complete` item from `complete`. Real providers override with SSE.
    /// The stream MUST end with exactly one `Complete` or an `Err`.
    async fn complete_stream(&self, req: ModelRequest) -> Result<DeltaStream, ProviderError> {
        let resp = self.complete(req).await?;
        Ok(Box::pin(crate::stream::once(Ok(ModelDelta::Complete(
            resp,
        )))))
    }
}
