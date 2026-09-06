//! `Middleware` (§3.7): the six async hooks, the hook context, and chain entries (D7).

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::SessionId;
use crate::artifact::ArtifactStore;
use crate::cancel::CancellationToken;
use crate::config::ResumeCause;
use crate::event::{
    ChildCompletedPayload, ContextUsagePayload, HarnessEditPayload, ProfileLoadPayload,
    SpawnPayload, WarningPayload,
};
use crate::hash::Hash;
use crate::host::Host;
use crate::log::LogError;
use crate::memory::Memory;
use crate::provider::{ModelRequest, ModelResponse};
use crate::state::{ActiveProfiles, State};
use crate::tool::{ToolCall, ToolDefinition, ToolError, ToolOutput, ToolResult};

/// Outcome of `before_tool`: run the tool, or short-circuit with a result (capability gate, P2.1).
pub enum ToolFlow {
    /// Run the tool.
    Continue,
    /// Skip the tool and use this result instead.
    Replace(Result<ToolResult, ToolError>),
}

/// The middleware hooks. Every hook runs in the same chain order (§7.6).
#[async_trait]
pub trait Middleware: Send + Sync {
    /// After the kernel built `req` from `state`, before the provider call. May edit both. Edits to
    /// `state.messages` do not change this turn's `req` unless the hook edits `req` too.
    async fn before_model(
        &self,
        state: &mut State,
        req: &mut ModelRequest,
        cx: &HookContext<'_>,
    ) -> Result<(), MiddlewareError> {
        let _ = (state, req, cx);
        Ok(())
    }
    /// After the provider returned, before the assistant message is appended and tool calls are
    /// extracted. The model profile's tool-call parser runs here in the fixed early slot (D7).
    async fn after_model(
        &self,
        state: &mut State,
        resp: &mut ModelResponse,
        cx: &HookContext<'_>,
    ) -> Result<(), MiddlewareError> {
        let _ = (state, resp, cx);
        Ok(())
    }
    /// Before each tool invocation. `call.input` may be edited; the edited call is what gets logged.
    async fn before_tool(
        &self,
        state: &mut State,
        call: &mut ToolCall,
        cx: &HookContext<'_>,
    ) -> Result<ToolFlow, MiddlewareError> {
        let _ = (state, call, cx);
        Ok(ToolFlow::Continue)
    }
    /// After invoke + redaction + spill, before the tool result message is appended and logged.
    async fn after_tool(
        &self,
        state: &mut State,
        call: &ToolCall,
        out: &mut ToolOutput,
        cx: &HookContext<'_>,
    ) -> Result<(), MiddlewareError> {
        let _ = (state, call, out, cx);
        Ok(())
    }
    /// Compaction (P2.6). Invoked by the kernel when compaction is requested (§6, step B4).
    async fn on_compact(
        &self,
        state: &mut State,
        cx: &HookContext<'_>,
    ) -> Result<(), MiddlewareError> {
        let _ = (state, cx);
        Ok(())
    }
    /// After a checkpoint was restored in a new process, before any turn (P2.6 notebook re-injection, D7).
    async fn on_resume(
        &self,
        state: &mut State,
        cause: &ResumeCause,
        cx: &HookContext<'_>,
    ) -> Result<(), MiddlewareError> {
        let _ = (state, cause, cx);
        Ok(())
    }
}

/// A hook failed; the turn fails with `error_class: "middleware"`.
#[derive(Debug, thiserror::Error)]
#[error("middleware `{name}` failed in {hook}: {source}")]
pub struct MiddlewareError {
    /// The entry's name.
    pub name: String,
    /// The hook name.
    pub hook: &'static str,
    /// Cause.
    #[source]
    pub source: Box<dyn std::error::Error + Send + Sync>,
}

impl MiddlewareError {
    /// Build an error from any message.
    pub fn new(name: impl Into<String>, hook: &'static str, msg: impl Into<String>) -> Self {
        MiddlewareError {
            name: name.into(),
            hook,
            source: msg.into().into(),
        }
    }
}

/// Where the kernel routes extension-emitted events and compaction requests. Implemented by the
/// kernel; exposed as a trait so tests can build a `HookContext`.
pub trait ExtensionEventSink: Send + Sync {
    /// Append an extension event at the current point in the log.
    fn emit(&self, ev: ExtensionEvent) -> Result<(), LogError>;
    /// Record a compaction request (honored only from `before_model`).
    fn request_compaction(&self, strategy: &str);
}

/// What a hook may see and do besides `State`.
pub struct HookContext<'a> {
    /// The session.
    pub session_id: &'a SessionId,
    /// Current turn.
    pub turn: u64,
    /// `state_hash` of the most recent checkpoint; the first half of the replay key.
    pub checkpoint_hash: &'a Hash,
    /// The turn token.
    pub cancel: CancellationToken,
    /// The host (handles only).
    pub host: &'a dyn Host,
    /// The artifact store.
    pub artifacts: &'a dyn ArtifactStore,
    /// The memory module.
    pub memory: &'a dyn Memory,
    /// Active profile hashes.
    pub profiles: &'a ActiveProfiles,
    /// The tool definitions registered in this kernel (read-only).
    pub registry: &'a [ToolDefinition],
    emitter: &'a dyn ExtensionEventSink,
}

impl<'a> HookContext<'a> {
    /// Build a context. The kernel calls this; tests may too.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        session_id: &'a SessionId,
        turn: u64,
        checkpoint_hash: &'a Hash,
        cancel: CancellationToken,
        host: &'a dyn Host,
        artifacts: &'a dyn ArtifactStore,
        memory: &'a dyn Memory,
        profiles: &'a ActiveProfiles,
        registry: &'a [ToolDefinition],
        emitter: &'a dyn ExtensionEventSink,
    ) -> HookContext<'a> {
        HookContext {
            session_id,
            turn,
            checkpoint_hash,
            cancel,
            host,
            artifacts,
            memory,
            profiles,
            registry,
            emitter,
        }
    }

    /// Append one of the extension-emittable events (closed set; kernel-only kinds are unreachable).
    pub fn emit(&self, ev: ExtensionEvent) -> Result<(), LogError> {
        self.emitter.emit(ev)
    }

    /// Ask the kernel to run the `on_compact` chain before this turn's model call (valid only from
    /// `before_model`; ignored elsewhere with a `warning`).
    pub fn request_compaction(&self, strategy: &str) {
        self.emitter.request_compaction(strategy)
    }
}

/// Events an extension may write. Everything else is written only by the kernel (provenance honesty, §10 of the dev plan).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionEvent {
    /// A profile-like artifact was loaded (skills, sub-agent definitions).
    ProfileLoad(ProfileLoadPayload),
    /// Context budget measurement (P2.9).
    ContextUsage(ContextUsagePayload),
    /// A sub-agent was spawned (P2.4).
    Spawn(SpawnPayload),
    /// A sub-agent finished (P2.4).
    ChildCompleted(ChildCompletedPayload),
    /// The evolve loop edited the harness (P4).
    HarnessEdit(HarnessEditPayload),
    /// A non-fatal condition.
    Warning(WarningPayload),
}

/// One entry of the middleware chain as given to the kernel.
pub struct MiddlewareEntry {
    /// Unique within a kernel; duplicates are a `KernelError::DuplicateMiddleware`.
    pub name: String,
    /// Lower runs first, for every hook (no onion/reverse order for `after_*`).
    pub priority: i32,
    /// Which layer contributed it.
    pub source: MiddlewareSource,
    /// Hash of the entry's `config` table as resolved by `profiles`; `None` for config-less entries.
    pub config_hash: Option<Hash>,
    /// The implementation.
    pub middleware: Arc<dyn Middleware>,
}

/// Reserved priority of the model profile's tool-call parser (D7 "fixed early slot").
/// The kernel rejects any other entry with `priority <= TOOL_CALL_PARSER_PRIORITY`.
/// Ranges are owned by `profile-schema.md` §2.7: model profile 100–199 (parser at exactly 100),
/// agent profile and project overrides 200–899, kernel-contributed entries 900–999 (Recorder at 990).
pub const TOOL_CALL_PARSER_PRIORITY: i32 = 100;
/// Reserved name of that entry.
pub const TOOL_CALL_PARSER_NAME: &str = "tool_call_parser";
/// Priority of the P1.4 `Recorder` when the launcher adds it (kernel range).
pub const RECORDER_PRIORITY: i32 = 990;

/// Which layer contributed a middleware entry (logged in `middleware_chain_resolved`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MiddlewareSource {
    /// Inserted from code.
    Kernel,
    /// Model profile.
    Model,
    /// Agent profile.
    Agent,
    /// Project overrides.
    Project,
}
