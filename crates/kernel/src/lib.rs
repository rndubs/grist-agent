//! `kernel` — Agent loop, typed State, Tool trait, Middleware trait, event log, checkpoints, suspend/resume.
//!
//! The public surface is `docs/specs/kernel-interface.md` (P1.0, D20), implemented by
//! P1.1 (types, hashing), P1.2 (loop), P1.3 (event log, checkpoints), P1.4 (record/replay).
//! `kernel` depends on no other in-repo crate (`crates/README.md`, enforced by
//! `tests/no_in_repo_deps.rs`).

use serde::{Deserialize, Serialize};

pub mod artifact;
pub mod cancel;
pub mod capability;
pub mod config;
pub mod content;
pub mod event;
pub mod hash;
pub mod host;
pub mod log;
pub mod memory;
pub mod middleware;
pub mod provider;
pub mod redact;
pub mod sandbox;
pub mod serde_util;
pub mod state;
pub mod stream;
pub mod task;
pub mod time;
pub mod tool;

pub use artifact::{
    ArtifactError, ArtifactHandle, ArtifactMeta, ArtifactStore, NoopArtifactStore, Spilled,
};
pub use cancel::{CancelScope, CancellationToken};
pub use capability::{Capability, CapabilityParseError, FsMode, NetAllow};
pub use config::{ResumeCause, RetryPolicy, RunStop, SpillConfig, Suspension, TurnOutcome};
pub use content::{ContentBlock, Message, PromptBlock, PromptBlockKind, Role, ToolResultContent};
pub use event::*;
pub use hash::{Hash, HashError, canonical_json};
pub use host::{
    AskUserRequest, ChildProcess, Command, DirEntry, FsPolicy, Host, HostError, HttpMethod,
    HttpRequest, HttpResponse, Metadata, Mount, NetHandle, NetPolicy, ProcPolicy, ProcessOutput,
    SecretHandle, SecretResolver, SecretString, UserAnswer,
};
pub use log::{EventLog, EventLogReader, LogError, MemoryEventLog, RestoreError};
pub use memory::{Memory, MemoryError, MemoryItem, MemoryPointer, MemoryQuery, NoopMemory};
pub use middleware::{
    ExtensionEvent, ExtensionEventSink, HookContext, Middleware, MiddlewareEntry, MiddlewareError,
    MiddlewareSource, RECORDER_PRIORITY, TOOL_CALL_PARSER_NAME, TOOL_CALL_PARSER_PRIORITY,
    ToolFlow,
};
pub use provider::{
    DeltaStream, ModelDelta, ModelParams, ModelRequest, ModelResponse, Provider, ProviderError,
    RequestTrace, StopReason, ThinkingConfig, Usage,
};
pub use redact::{RedactionReport, Redactor};
pub use sandbox::{
    PolicyError, RpcRequest, RpcResponse, SECRET_LIKE_ENV, SandboxBackend, SandboxError,
    SandboxLimits, SandboxPolicy, SessionProcess, derive_policy, derive_policy_with,
};
pub use state::{
    ActiveProfiles, Migrated, MigrationError, MigrationRegistry, SessionStatus, State,
    StateMigration,
};
pub use task::{
    Task, TaskHandle, TaskId, TaskOutcome, TaskStatus, TaskUpdate, TrustTier, WakerSource,
};
pub use tool::{
    NoTaskRegistrar, TaskRegistrar, Tool, ToolCall, ToolContext, ToolDefinition, ToolError,
    ToolKind, ToolOutput, ToolOutputOrigin, ToolResult,
};

/// Opaque session identifier. The launcher chooses it (UUIDv7 recommended); the kernel never parses it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(pub String);

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Kernel crate version, written into `log_opened`, `session_created`, `resumed`, `recovered`.
pub const KERNEL_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Bumped when `State`'s serialized shape changes; see `state` migrations.
pub const STATE_SCHEMA_VERSION: u32 = 1;
/// Bumped when the event envelope or any payload changes incompatibly; see `event-schema.md` §1.
pub const EVENT_SCHEMA_VERSION: u32 = 1;
