//! Tools (§3.6): the trait, the invocation context, and the kernel's normalized view of a result.

use std::time::Duration;

use async_trait::async_trait;
use futures_core::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::SessionId;
use crate::artifact::{ArtifactHandle, ArtifactStore};
use crate::cancel::CancellationToken;
use crate::capability::Capability;
use crate::content::{ContentBlock, ToolResultContent};
use crate::hash::{Hash, HashError};
use crate::host::{Command, Host};
use crate::sandbox::{SandboxBackend, SandboxPolicy, SessionProcess};
use crate::task::{TaskHandle, TaskId, TaskOutcome};

/// How a tool is isolated (D5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    /// Fresh sandbox per call; nothing persists between calls (D5).
    Stateless,
    /// One sandboxed process per session; calls are RPC into it (D5).
    Session,
}

/// A tool invocation as extracted from the model response (after the `after_model` chain).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// From the `tool_use` block.
    pub tool_use_id: String,
    /// Tool name.
    pub name: String,
    /// Arguments (possibly edited by `before_tool`).
    pub input: Value,
}

impl ToolCall {
    /// `Hash::of_canonical_json(&self.input)`; the `args_hash` of D13.
    pub fn args_hash(&self) -> Result<Hash, HashError> {
        Hash::of_canonical_json(&self.input)
    }

    /// `Hash::of_canonical_json(&self)` (all three fields); the `request_hash` used as the tool
    /// replay key (`event-schema.md` §3.3). Including `tool_use_id` keeps two identical calls in one
    /// turn distinct.
    pub fn request_hash(&self) -> Result<Hash, HashError> {
        Hash::of_canonical_json(self)
    }
}

/// What `Tool::invoke` returns. `Blocks` exists so a tool can return `Image` blocks (P2.5) without a
/// post-freeze kernel change.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolResult {
    /// A JSON value.
    Value(Value),
    /// Text and Image blocks.
    Blocks(Vec<ContentBlock>),
    /// A started task (D1).
    Task(TaskHandle),
}

/// Tool errors. The doc on each variant says how the kernel treats it.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    /// Input failed the tool's own validation. Becomes an `is_error` result; the turn continues.
    #[error("invalid input: {0}")]
    InvalidInput(String),
    /// The tool ran and failed in a way the model should see. Becomes an `is_error` result.
    #[error("{0}")]
    Failed(String),
    /// Refused by policy (Host path check, sandbox derivation). Becomes an `is_error` result.
    #[error("denied by policy: {0}")]
    Denied(String),
    /// The policy timeout fired.
    #[error("timed out after {0:?}")]
    Timeout(Duration),
    /// The cancellation token fired. The kernel handles this on the cancel path (§7.1).
    #[error("cancelled")]
    Cancelled,
    /// A `TaskHandle` with a terminal status or a foreign id.
    #[error("task handle is invalid: {0}")]
    InvalidTaskHandle(String),
    /// Record/replay cache miss (P1.4); non-recoverable, fails the turn.
    #[error("replay miss for {tool} ({request_hash})")]
    ReplayMiss {
        /// The tool.
        tool: String,
        /// The missing key's request half.
        request_hash: Hash,
    },
    /// Anything else.
    #[error("internal tool error: {0}")]
    Internal(#[source] Box<dyn std::error::Error + Send + Sync>),
}

/// Wire-facing description of a tool, as placed in `ModelRequest.tools`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    /// Tool name.
    pub name: String,
    /// Description the model sees.
    pub description: String,
    /// JSON Schema (draft 2020-12) for `ToolCall::input`.
    pub input_schema: Value,
}

/// A tool the kernel can invoke.
#[async_trait]
pub trait Tool: Send + Sync {
    /// Unique within a kernel; `[a-z][a-z0-9_.-]*`, at most 64 chars. Dots namespace extension tools
    /// (`mcp.<server>.<tool>`, `ext.<manifest>.<tool>`, `profile-schema.md` §3.3).
    fn name(&self) -> &str;
    /// Description the model sees.
    fn description(&self) -> &str;
    /// JSON Schema for the input.
    fn schema(&self) -> Value;
    /// Isolation kind (D5).
    fn kind(&self) -> ToolKind;
    /// Atoms this tool needs (D6). Checked against grants at kernel construction (§7.7).
    fn capabilities(&self) -> Vec<Capability>;
    /// For `ToolKind::Session` tools: the command that starts the long-lived process. The kernel
    /// launches it lazily on first invoke under the derived policy and terminates it on suspend/end.
    /// MUST return `None` for `Stateless`; MUST return `Some` for `Session`.
    fn session_command(&self) -> Option<Command> {
        None
    }
    /// Invoke. The kernel applies the policy timeout and cancellation around this call.
    async fn invoke(&self, ctx: &ToolContext<'_>, input: Value) -> Result<ToolResult, ToolError>;

    /// The wire-facing definition.
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_owned(),
            description: self.description().to_owned(),
            input_schema: self.schema(),
        }
    }
}

/// True iff `name` is `[a-z][a-z0-9_.-]*` and at most 64 chars.
pub fn is_valid_tool_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    match bytes.next() {
        Some(b'a'..=b'z') => {}
        _ => return false,
    }
    name.len() <= 64 && bytes.all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-'))
}

/// What a tool may see. No secrets: `Host` exposes handles only (§3.9); the resolver is not reachable from here.
pub struct ToolContext<'a> {
    /// The host (handles only).
    pub host: &'a dyn Host,
    /// This invocation's cancellation token.
    pub cancel: CancellationToken,
    /// The session.
    pub session_id: &'a SessionId,
    /// Current turn.
    pub turn: u64,
    /// This invocation's `tool_use_id`.
    pub tool_use_id: &'a str,
    /// Derived policy for THIS tool (§3.12). In-process tools pass `policy.fs()` to Host calls.
    pub policy: &'a SandboxPolicy,
    /// The sandbox backend.
    pub sandbox: &'a dyn SandboxBackend,
    /// The artifact store.
    pub artifacts: &'a dyn ArtifactStore,
    /// For `Session` tools: the running process, launched by the kernel. `Err` for `Stateless` tools.
    session: Option<&'a dyn SessionProcess>,
    tasks: &'a dyn TaskRegistrar,
}

impl<'a> ToolContext<'a> {
    /// Build a context. The kernel calls this; tests may too.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        host: &'a dyn Host,
        cancel: CancellationToken,
        session_id: &'a SessionId,
        turn: u64,
        tool_use_id: &'a str,
        policy: &'a SandboxPolicy,
        sandbox: &'a dyn SandboxBackend,
        artifacts: &'a dyn ArtifactStore,
        session: Option<&'a dyn SessionProcess>,
        tasks: &'a dyn TaskRegistrar,
    ) -> ToolContext<'a> {
        ToolContext {
            host,
            cancel,
            session_id,
            turn,
            tool_use_id,
            policy,
            sandbox,
            artifacts,
            session,
            tasks,
        }
    }

    /// The session process for a `Session` tool; `ToolError::Failed` for `Stateless` tools.
    pub fn session_process(&self) -> Result<&'a dyn SessionProcess, ToolError> {
        self.session
            .ok_or_else(|| ToolError::Failed("no session process for a stateless tool".to_owned()))
    }

    /// Deterministic task id for this invocation: `format!("t{turn}-{tool_use_id}")`. A tool call may
    /// start at most one task.
    pub fn task_id(&self) -> TaskId {
        TaskId(format!("t{}-{}", self.turn, self.tool_use_id))
    }

    /// Register an in-process completion future (the D1 process-exit waker). The kernel spawns it;
    /// when it resolves the kernel delivers a `TaskUpdate` with `source.kind = "in_process_exit"`.
    pub fn watch_task(
        &self,
        id: TaskId,
        done: BoxFuture<'static, TaskOutcome>,
    ) -> Result<(), ToolError> {
        self.tasks.watch(id, done)
    }
}

/// Kernel-internal; exposed as a trait so tests can fake it.
pub trait TaskRegistrar: Send + Sync {
    /// Register a completion future for `id`.
    fn watch(&self, id: TaskId, done: BoxFuture<'static, TaskOutcome>) -> Result<(), ToolError>;
}

/// A registrar that refuses every watch (for tools that never start tasks, and for tests).
#[derive(Debug, Default, Clone, Copy)]
pub struct NoTaskRegistrar;

impl TaskRegistrar for NoTaskRegistrar {
    fn watch(&self, id: TaskId, _done: BoxFuture<'static, TaskOutcome>) -> Result<(), ToolError> {
        Err(ToolError::InvalidTaskHandle(format!(
            "no task registrar available for {id}"
        )))
    }
}

/// The kernel's normalized view of a tool outcome after `invoke`, redaction and spill; what
/// `after_tool` sees and what the `tool_result` event records.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolOutput {
    /// Post-spill, post-redaction content.
    pub content: ToolResultContent,
    /// Whether the tool failed.
    pub is_error: bool,
    /// Every handle in the result.
    pub artifact_handles: Vec<ArtifactHandle>,
    /// Whether spill replaced the content.
    pub spilled: bool,
    /// `Some` when the tool returned `ToolResult::Task`.
    pub task: Option<TaskHandle>,
    /// How the output was produced; copied verbatim on replay.
    pub origin: ToolOutputOrigin,
}

/// How a `ToolOutput` came to be.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolOutputOrigin {
    /// `Tool::invoke` ran.
    Invoke,
    /// A `before_tool` hook replaced the result.
    Middleware,
    /// The name was not registered.
    Unregistered,
    /// Cancelled before or during invoke.
    Cancelled,
    /// The policy timeout fired.
    Timeout,
}
