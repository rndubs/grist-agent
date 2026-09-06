//! Content blocks and messages (`kernel-interface.md` §3.3, D15).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::artifact::ArtifactHandle;
use crate::hash::Hash;
use crate::task::{TaskId, TaskStatus};

/// One block of message content; tagged by `"type"`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// Plain text.
    Text {
        /// The text.
        text: String,
    },
    /// Reasoning content. `signature` is an opaque provider token that must round-trip if present.
    Thinking {
        /// Reasoning text.
        text: String,
        /// Opaque provider signature (Anthropic via LiteLLM); round-trips verbatim.
        signature: Option<String>,
    },
    /// A tool invocation requested by the model.
    ToolUse {
        /// Provider-assigned id; the kernel echoes it and derives no meaning from it.
        id: String,
        /// Tool name.
        name: String,
        /// Arguments.
        input: Value,
    },
    /// The result of a `ToolUse`, in a `Role::Tool` message.
    ToolResult {
        /// The `ToolUse` id this answers.
        tool_use_id: String,
        /// Result content.
        content: ToolResultContent,
        /// Whether the tool failed.
        is_error: bool,
    },
    /// Bytes live in the artifact store; the provider encodes them at request time (D15).
    Image {
        /// Content-addressed handle.
        artifact_handle: ArtifactHandle,
        /// MIME type, e.g. `image/png`.
        mime: String,
    },
    /// Synthetic completion of a `Task` (D1), appended by the kernel, never by a tool.
    /// Providers render it as the wire format allows (for OpenAI-compatible endpoints: a user-role
    /// text message containing the JSON of this block), because a late tool_result for an old
    /// tool_use id is rejected by most chat APIs.
    TaskResult {
        /// The task.
        task_id: TaskId,
        /// The `ToolUse` that started it.
        tool_use_id: String,
        /// `Succeeded`, `Failed`, or `Cancelled`.
        status: TaskStatus,
        /// Outcome content.
        content: ToolResultContent,
        /// Whether the task failed.
        is_error: bool,
    },
}

/// Externally tagged: `{"blocks": [...]}` or `{"json": ...}` (an untagged union would be ambiguous).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolResultContent {
    /// Text and Image only; nested ToolUse/ToolResult are rejected on validation.
    Blocks(Vec<ContentBlock>),
    /// An arbitrary JSON value.
    Json(Value),
}

impl ToolResultContent {
    /// True iff every block is `Text` or `Image` (the only kinds a tool result may carry).
    pub fn is_valid(&self) -> bool {
        match self {
            ToolResultContent::Json(_) => true,
            ToolResultContent::Blocks(blocks) => blocks
                .iter()
                .all(|b| matches!(b, ContentBlock::Text { .. } | ContentBlock::Image { .. })),
        }
    }
}

/// Message role.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// System (never in `State.messages`; the system prompt travels in `ModelRequest.system`).
    System,
    /// User.
    User,
    /// Assistant.
    Assistant,
    /// Tool results.
    Tool,
}

/// One conversation message. No timestamps, ids, or other volatile data: `Message` is hashed as part of `State`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Message {
    /// Who produced it.
    pub role: Role,
    /// Content blocks.
    pub content: Vec<ContentBlock>,
}

impl Message {
    /// A single-text-block user message.
    pub fn user_text(text: impl Into<String>) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }
}

/// One block of the assembled system prompt (D7). Order is fixed by `profiles`; the kernel concatenates.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PromptBlock {
    /// Which block this is.
    pub kind: PromptBlockKind,
    /// Source name (profile name, skill name, `AGENTS.md` path, notebook path).
    pub name: String,
    /// The block text.
    pub text: String,
    /// Content hash of `text`; logged in `profile_load` and `model_request`.
    pub hash: Hash,
}

impl PromptBlock {
    /// Build a block, computing `hash = Hash::of_bytes(text)`.
    pub fn new(kind: PromptBlockKind, name: impl Into<String>, text: impl Into<String>) -> Self {
        let text = text.into();
        let hash = Hash::of_bytes(text.as_bytes());
        PromptBlock {
            kind,
            name: name.into(),
            text,
            hash,
        }
    }
}

/// D7 block order; names match `profile-schema.md` §8. `Skills` may appear once per active skill.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptBlockKind {
    /// Model prompt variant.
    Model,
    /// Agent role prompt.
    Role,
    /// Project instructions (`AGENTS.md`).
    AgentsMd,
    /// One active skill.
    Skills,
    /// Notebook (on resume).
    Notebook,
}
