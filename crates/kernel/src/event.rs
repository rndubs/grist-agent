//! Events (§3.14): the D3 envelope and one payload struct per kind in `event-schema.md` §2.

use std::path::PathBuf;

use serde::de::Error as _;
use serde::ser::SerializeStruct;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::SessionId;
use crate::artifact::ArtifactHandle;
use crate::capability::Capability;
use crate::config::{RetryPolicy, SpillConfig};
use crate::content::{ContentBlock, PromptBlockKind, ToolResultContent};
use crate::hash::Hash;
use crate::middleware::{ExtensionEvent, MiddlewareSource};
use crate::provider::{StopReason, Usage};
use crate::sandbox::SandboxLimits;
use crate::state::{ActiveProfiles, SessionStatus, State};
use crate::task::{TaskId, TaskOutcome, TaskStatus, WakerSource};
use crate::tool::{ToolKind, ToolOutputOrigin};

/// D3 envelope. `kind` and `payload` come from the flattened, tagged `EventBody`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// Position in this session's log; starts at 0, +1 per physical line.
    pub seq: u64,
    /// RFC 3339 UTC, millisecond precision.
    pub ts: String,
    /// The session.
    pub session_id: SessionId,
    /// `{"kind": ..., "payload": ...}`.
    #[serde(flatten)]
    pub body: EventBody,
}

macro_rules! event_kinds {
    ($( $variant:ident => $kind:literal : $payload:ty ),* $(,)?) => {
        /// `{"kind": "<snake_case>", "payload": {...}}`. One variant per kind in `event-schema.md` §2.
        #[derive(Clone, Debug, PartialEq)]
        #[allow(clippy::large_enum_variant)] // `Checkpoint` carries the full `State` by design (§2.13).
        pub enum EventBody {
            $(
                #[doc = concat!("`", $kind, "`")]
                $variant($payload),
            )*
            /// Forward compatibility: a reader MUST NOT fail on an unknown kind.
            Unknown {
                /// The unrecognized kind.
                kind: String,
                /// Its payload, verbatim.
                payload: Value,
            },
        }

        impl EventBody {
            /// The `kind` string.
            pub fn kind(&self) -> &str {
                match self {
                    $( EventBody::$variant(_) => $kind, )*
                    EventBody::Unknown { kind, .. } => kind,
                }
            }

            /// The payload as JSON.
            pub fn payload_value(&self) -> Result<Value, serde_json::Error> {
                match self {
                    $( EventBody::$variant(p) => serde_json::to_value(p), )*
                    EventBody::Unknown { payload, .. } => Ok(payload.clone()),
                }
            }

            /// Every kind this kernel knows, in `event-schema.md` order.
            pub const KNOWN_KINDS: &'static [&'static str] = &[ $( $kind ),* ];
        }

        impl Serialize for EventBody {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                let mut st = s.serialize_struct("EventBody", 2)?;
                match self {
                    $(
                        EventBody::$variant(p) => {
                            st.serialize_field("kind", $kind)?;
                            st.serialize_field("payload", p)?;
                        }
                    )*
                    EventBody::Unknown { kind, payload } => {
                        st.serialize_field("kind", kind)?;
                        st.serialize_field("payload", payload)?;
                    }
                }
                st.end()
            }
        }

        impl<'de> Deserialize<'de> for EventBody {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                #[derive(Deserialize)]
                struct Raw {
                    kind: String,
                    payload: Value,
                }
                let Raw { kind, payload } = Raw::deserialize(d)?;
                Ok(match kind.as_str() {
                    $(
                        $kind => EventBody::$variant(
                            serde_json::from_value(payload).map_err(D::Error::custom)?,
                        ),
                    )*
                    _ => EventBody::Unknown { kind, payload },
                })
            }
        }
    };
}

event_kinds! {
    LogOpened => "log_opened": LogOpenedPayload,
    SessionCreated => "session_created": SessionCreatedPayload,
    ProfileLoad => "profile_load": ProfileLoadPayload,
    MiddlewareChainResolved => "middleware_chain_resolved": MiddlewareChainResolvedPayload,
    UserMessage => "user_message": UserMessagePayload,
    ModelRequest => "model_request": ModelRequestPayload,
    ModelResponse => "model_response": ModelResponsePayload,
    ProviderRetry => "provider_retry": ProviderRetryPayload,
    ToolCall => "tool_call": ToolCallPayload,
    ToolResult => "tool_result": ToolResultPayload,
    TaskStarted => "task_started": TaskStartedPayload,
    TaskUpdate => "task_update": TaskUpdatePayload,
    Checkpoint => "checkpoint": CheckpointPayload,
    Suspended => "suspended": SuspendedPayload,
    Resumed => "resumed": ResumedPayload,
    Cancelled => "cancelled": CancelledPayload,
    TurnFailed => "turn_failed": TurnFailedPayload,
    SessionFailed => "session_failed": SessionFailedPayload,
    SessionEnded => "session_ended": SessionEndedPayload,
    Recovered => "recovered": RecoveredPayload,
    Compaction => "compaction": CompactionPayload,
    Spawn => "spawn": SpawnPayload,
    ChildCompleted => "child_completed": ChildCompletedPayload,
    ContextUsage => "context_usage": ContextUsagePayload,
    HarnessEdit => "harness_edit": HarnessEditPayload,
    AskUser => "ask_user": AskUserPayload,
    UserAnswer => "user_answer": UserAnswerPayload,
    Warning => "warning": WarningPayload,
}

impl From<ExtensionEvent> for EventBody {
    fn from(ev: ExtensionEvent) -> Self {
        match ev {
            ExtensionEvent::ProfileLoad(p) => EventBody::ProfileLoad(p),
            ExtensionEvent::ContextUsage(p) => EventBody::ContextUsage(p),
            ExtensionEvent::Spawn(p) => EventBody::Spawn(p),
            ExtensionEvent::ChildCompleted(p) => EventBody::ChildCompleted(p),
            ExtensionEvent::HarnessEdit(p) => EventBody::HarnessEdit(p),
            ExtensionEvent::Warning(p) => EventBody::Warning(p),
        }
    }
}

// ---- §2.1 log_opened ---------------------------------------------------------------------

/// `log_opened`: first event of every log file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LogOpenedPayload {
    /// `EVENT_SCHEMA_VERSION`.
    pub event_schema_version: u32,
    /// `STATE_SCHEMA_VERSION`.
    pub state_schema_version: u32,
    /// `KERNEL_VERSION`. Volatile.
    pub kernel_version: String,
    /// Live or replay. Volatile.
    pub mode: LogMode,
}

/// How a log was produced.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogMode {
    /// A live kernel.
    Live,
    /// A kernel driven by `ReplayDriver`.
    Replay,
}

// ---- §2.2 session_created ------------------------------------------------------------------

/// `session_created`: the PROV Agent identity record.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionCreatedPayload {
    /// Duplicates the envelope so the payload is self-describing. Volatile.
    pub session_id: SessionId,
    /// RFC 3339. Volatile.
    pub created_at: String,
    /// Volatile.
    pub kernel_version: String,
    /// Active profile hashes.
    pub profiles: ActiveProfiles,
    /// From the model profile.
    pub model_id: String,
    /// Registered tool names, sorted.
    pub tools: Vec<String>,
    /// Capability atoms in canonical string form, sorted.
    pub grants: Vec<Capability>,
    /// `SandboxBackend::name()` (D14). Volatile.
    pub sandbox_backend: String,
    /// The envelope policy hash. Volatile.
    pub sandbox_policy_hash: Hash,
    /// `ArtifactStore::name()`. Volatile.
    pub artifact_store: String,
    /// `Memory::name()`. Volatile.
    pub memory: String,
    /// `Provider::name()`. Volatile.
    pub provider: String,
    /// After clamping.
    pub spill: SpillConfig,
    /// Retry policy.
    pub retry: RetryPolicy,
    /// Notebook path.
    pub notebook_path: Option<PathBuf>,
    /// The `[sandbox]` limits.
    pub sandbox_limits: SandboxLimits,
    /// Layer-4 runtime overrides from the start-session message.
    pub overrides: Option<Value>,
    /// `{session_id, task_id}` when spawned by P2.4; `null` for root sessions.
    pub parent: Option<ParentRef>,
}

/// Parent session reference for spawned sessions.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ParentRef {
    /// Parent session.
    pub session_id: SessionId,
    /// Parent-side task.
    pub task_id: TaskId,
}

// ---- §2.3 profile_load ---------------------------------------------------------------------

/// `profile_load`: one per loaded profile-like artifact.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProfileLoadPayload {
    /// Which kind of artifact.
    pub kind: ProfileKind,
    /// Profile/skill name.
    pub name: String,
    /// Where it was read from. Volatile.
    pub path: Option<PathBuf>,
    /// `b3` of the source file bytes.
    pub hash: Hash,
    /// `true` when the loader refused it but logs the attempt.
    pub rejected: bool,
    /// `0` for start-up loads.
    pub turn: u64,
}

/// Profile-like artifact kinds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileKind {
    /// Model profile.
    Model,
    /// Agent profile.
    Agent,
    /// Project overrides.
    Project,
    /// Bundles file.
    Bundles,
    /// Catalog.
    Catalog,
    /// Skill (P2.3).
    Skill,
    /// Sub-agent definition (P2.4).
    Subagent,
}

// ---- §2.4 middleware_chain_resolved --------------------------------------------------------

/// `middleware_chain_resolved`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MiddlewareChainResolvedPayload {
    /// In execution order.
    pub chain: Vec<ChainEntry>,
    /// Hash of `chain`.
    pub chain_hash: Hash,
}

/// One resolved chain entry.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChainEntry {
    /// Position.
    pub index: u32,
    /// Name.
    pub name: String,
    /// Priority.
    pub priority: i32,
    /// Contributing layer.
    pub source: MiddlewareSource,
    /// Config hash.
    pub config_hash: Option<Hash>,
}

// ---- §2.5 user_message ---------------------------------------------------------------------

/// `user_message`: written when applied to `State.messages`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UserMessagePayload {
    /// `State.turn` at application.
    pub turn: u64,
    /// Where in the session it was applied.
    pub applied: AppliedAt,
    /// Post-redaction content.
    pub content: Vec<ContentBlock>,
}

/// Where in the session an input was applied (`event-schema.md` §2.5). Drives the replay schedule.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AppliedAt {
    /// Turn.
    pub turn: u64,
    /// Point.
    pub at: AppliedPoint,
}

/// Application points.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppliedPoint {
    /// From `Created`.
    Created,
    /// From `Idle`.
    Idle,
    /// From `Suspended`.
    Suspended,
    /// From `Failed`.
    Failed,
    /// Drained at a turn boundary.
    EndOfTurn,
    /// Only for task updates that were not applied.
    Ignored,
}

// ---- §2.6 model_request --------------------------------------------------------------------

/// `model_request` (D13): hashes and a summary.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelRequestPayload {
    /// Turn.
    pub turn: u64,
    /// §3.1.
    pub request_hash: Hash,
    /// `state_hash` of the checkpoint this request was computed from; replay key.
    pub checkpoint_hash: Hash,
    /// As sent.
    pub model_id: String,
    /// Active profile hashes (D13).
    pub profiles: ActiveProfiles,
    /// Hash of `req.system` (block list).
    pub system_prompt_hash: Hash,
    /// One per `PromptBlock`, in order.
    pub prompt_blocks: Vec<PromptBlockRef>,
    /// `req.messages.len()`.
    pub message_count: u32,
    /// `req.tools` names in order.
    pub tool_names: Vec<String>,
    /// Hash of `req.params`.
    pub params_hash: Hash,
}

/// A prompt block reference.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PromptBlockRef {
    /// Block kind.
    pub kind: PromptBlockKind,
    /// Block name.
    pub name: String,
    /// Block hash.
    pub hash: Hash,
}

// ---- §2.7 model_response -------------------------------------------------------------------

/// `model_response` (D13): full content blocks so the log is a complete cassette.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelResponsePayload {
    /// Turn.
    pub turn: u64,
    /// Correlates with `model_request`.
    pub request_hash: Hash,
    /// §3.2.
    pub response_hash: Hash,
    /// `Hash::of_bytes` of the raw provider body.
    pub raw_response_hash: Hash,
    /// What the endpoint reported it served.
    pub model_id: String,
    /// Stop reason.
    pub stop_reason: StopReason,
    /// Usage (D13).
    pub usage: Usage,
    /// Post-redaction, post-`after_model`.
    pub content: Vec<ContentBlock>,
    /// Provider attempts (1 = no retry). Volatile.
    pub attempts: u32,
}

// ---- §2.8 provider_retry -------------------------------------------------------------------

/// `provider_retry` (D15). Entire kind is volatile.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProviderRetryPayload {
    /// Turn.
    pub turn: u64,
    /// The attempt that failed (1-based).
    pub attempt: u32,
    /// `ProviderError::class()`.
    pub error_class: String,
    /// Redacted.
    pub message: String,
    /// Back-off before the next attempt.
    pub delay_ms: u64,
}

// ---- §2.9 tool_call ------------------------------------------------------------------------

/// `tool_call` (D13).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCallPayload {
    /// Turn.
    pub turn: u64,
    /// From the `tool_use` block.
    pub tool_use_id: String,
    /// Tool name.
    pub name: String,
    /// §3.3.
    pub args_hash: Hash,
    /// §3.3, replay key for the tool result.
    pub request_hash: Hash,
    /// Replay key, first half.
    pub checkpoint_hash: Hash,
    /// The (possibly middleware-edited) arguments, redacted.
    pub input: Value,
    /// `false` → no invoke happened.
    pub registered: bool,
    /// `null` when unregistered.
    pub kind: Option<ToolKind>,
    /// The tool's atoms; `[]` when unregistered.
    pub capabilities: Vec<Capability>,
    /// Hash of the derived `SandboxPolicy`; `null` when unregistered. Volatile.
    pub policy_hash: Option<Hash>,
}

// ---- §2.10 tool_result ---------------------------------------------------------------------

/// `tool_result` (D13): post-spill, post-redaction content.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolResultPayload {
    /// Turn.
    pub turn: u64,
    /// Tool-use id.
    pub tool_use_id: String,
    /// Tool name.
    pub name: String,
    /// §3.4.
    pub result_hash: Hash,
    /// Whether it failed.
    pub is_error: bool,
    /// Post-spill content.
    pub content: ToolResultContent,
    /// Every handle in the result.
    pub artifact_handles: Vec<ArtifactHandle>,
    /// Whether spill replaced the content.
    pub spilled: bool,
    /// Present iff `spilled`.
    pub spill: Option<SpillRef>,
    /// Wall time of `invoke` (0 for `middleware`/`unregistered`). Volatile.
    pub duration_ms: u64,
    /// Origin.
    pub origin: ToolOutputOrigin,
    /// When the result was a task handle.
    pub task: Option<TaskRef>,
}

/// Spill reference.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpillRef {
    /// Handle.
    pub handle: ArtifactHandle,
    /// Full size.
    pub size: u64,
    /// MIME type.
    pub mime: String,
}

/// Task reference on a `tool_result`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaskRef {
    /// Task id.
    pub task_id: TaskId,
    /// Status at start.
    pub status: TaskStatus,
}

// ---- §2.11 task_started --------------------------------------------------------------------

/// `task_started`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaskStartedPayload {
    /// Turn.
    pub turn: u64,
    /// `t<turn>-<tool_use_id>`.
    pub task_id: TaskId,
    /// Tool-use id.
    pub tool_use_id: String,
    /// Tool name.
    pub tool_name: String,
    /// `pending` or `running`.
    pub status: TaskStatus,
    /// ETA.
    pub eta_secs: Option<u64>,
    /// Description.
    pub description: Option<String>,
    /// For the polling sidecar; never in the model context, but in the log.
    pub check_hint: Option<Value>,
    /// `true` for P1 `run_script`.
    pub in_process_waker: bool,
}

// ---- §2.12 task_update ---------------------------------------------------------------------

/// `task_update`: one per `TaskUpdate` applied (or ignored).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaskUpdatePayload {
    /// Turn.
    pub turn: u64,
    /// Task id.
    pub task_id: TaskId,
    /// `null` when the task is unknown.
    pub from_status: Option<TaskStatus>,
    /// The update's status.
    pub status: TaskStatus,
    /// Present for terminal updates.
    pub outcome: Option<TaskOutcome>,
    /// ETA.
    pub eta_secs: Option<u64>,
    /// Polling hint.
    pub check_hint: Option<Value>,
    /// Who delivered it. Volatile.
    pub waker: WakerSource,
    /// Where it was applied, or `ignored`.
    pub applied: AppliedAt,
    /// `"terminal"`, `"unknown_task"`, `"missing_outcome"`.
    pub ignored_reason: Option<String>,
}

// ---- §2.13 checkpoint ----------------------------------------------------------------------

/// `checkpoint`: the durable state record.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CheckpointPayload {
    /// Turn.
    pub turn: u64,
    /// Why.
    pub reason: CheckpointReason,
    /// The status the session is in *after* this checkpoint.
    pub session_status: SessionStatus,
    /// §3.5.
    pub state_hash: Hash,
    /// The full state, inline.
    pub state: State,
}

/// Why a checkpoint was written.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointReason {
    /// A user message was applied.
    UserInput,
    /// End of turn.
    TurnEnd,
    /// A task update was applied outside a turn.
    TaskUpdate,
    /// Resume.
    Resume,
    /// Recovery with changes.
    Recovery,
    /// Compaction.
    Compaction,
    /// Cancellation.
    Cancel,
    /// Turn failure.
    Failure,
    /// Explicit end.
    End,
    /// Explicit suspend.
    Suspend,
}

// ---- §2.14 suspended -----------------------------------------------------------------------

/// `suspended`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SuspendedPayload {
    /// Turn.
    pub turn: u64,
    /// Why.
    pub reason: SuspendReason,
    /// Open tasks, sorted.
    pub pending_task_ids: Vec<TaskId>,
    /// How many are watched by a future in this process.
    pub in_process_wakers: u32,
    /// The checkpoint to resume from.
    pub checkpoint_hash: Hash,
}

/// Why a session suspended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SuspendReason {
    /// The D1 rule.
    PendingTasks,
    /// `Kernel::suspend`.
    Explicit,
}

// ---- §2.15 resumed -------------------------------------------------------------------------

/// `resumed`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResumedPayload {
    /// Checkpoint hash.
    pub from_checkpoint_hash: Hash,
    /// Checkpoint seq.
    pub from_checkpoint_seq: u64,
    /// `suspended`, `idle`, or `failed`.
    pub from_status: SessionStatus,
    /// `ResumeCause` tag.
    pub cause: ResumeCauseKind,
    /// When `cause == task_update`. Volatile.
    pub waker: Option<WakerSource>,
    /// `true` for `Kernel::open`. Volatile.
    pub new_process: bool,
    /// Volatile.
    pub kernel_version: String,
    /// The version the resuming kernel writes from here on.
    pub event_schema_version: u32,
}

/// `ResumeCause` tag as logged.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResumeCauseKind {
    /// A task update.
    TaskUpdate,
    /// A user message.
    UserMessage,
    /// Operator.
    Operator,
}

// ---- §2.16 cancelled -----------------------------------------------------------------------

/// `cancelled` (D15).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CancelledPayload {
    /// Turn.
    pub turn: u64,
    /// `CancelScope` tag.
    pub scope: CancelScopeKind,
    /// For `tool`, and for `turn` when a tool was in flight.
    pub tool_use_id: Option<String>,
    /// For `task`.
    pub task_id: Option<TaskId>,
    /// Where the loop was.
    pub phase: LoopPhase,
    /// Whether a child process received SIGTERM.
    pub signalled: bool,
    /// Calls in this turn that were never started (given synthetic results).
    pub skipped_tool_use_ids: Vec<String>,
}

/// `CancelScope` tag as logged.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelScopeKind {
    /// Turn.
    Turn,
    /// Tool.
    Tool,
    /// Task.
    Task,
}

/// Loop phases (§6 cancellation check points).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoopPhase {
    /// Before the model call.
    BeforeModel,
    /// During the model call.
    ModelCall,
    /// After the model call.
    AfterModel,
    /// Before a tool.
    BeforeTool,
    /// During a tool.
    Tool,
    /// After a tool.
    AfterTool,
    /// Between turns.
    Idle,
}

// ---- §2.17 turn_failed ---------------------------------------------------------------------

/// `turn_failed` (D15).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TurnFailedPayload {
    /// Turn.
    pub turn: u64,
    /// Error class.
    pub error_class: String,
    /// Provider attempts made (1 for non-provider classes). Volatile.
    pub attempts: u32,
    /// Redacted last error. Volatile.
    pub message: String,
    /// Middleware name, for `error_class == middleware`.
    pub middleware: Option<String>,
    /// Hook name, for `middleware`.
    pub hook: Option<String>,
}

// ---- §2.18 session_failed ------------------------------------------------------------------

/// `session_failed`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionFailedPayload {
    /// Turn.
    pub turn: u64,
    /// Seq of the `turn_failed`. Volatile.
    pub cause_seq: u64,
    /// Copied from `turn_failed`.
    pub error_class: String,
    /// The intact checkpoint (D15).
    pub checkpoint_hash: Hash,
    /// Always `true` (D2); reserved for a future non-resumable class.
    pub resumable: bool,
}

// ---- §2.19 session_ended -------------------------------------------------------------------

/// `session_ended`: explicit end only (D2).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionEndedPayload {
    /// Turn.
    pub turn: u64,
    /// Who called `end`.
    pub by: EndedBy,
    /// Final state.
    pub checkpoint_hash: Hash,
    /// Open tasks cancelled by ending.
    pub cancelled_task_ids: Vec<TaskId>,
}

/// Who ended a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndedBy {
    /// The user.
    User,
    /// An operator.
    Operator,
    /// The parent session.
    Parent,
}

// ---- §2.20 recovered -----------------------------------------------------------------------

/// `recovered` (D15). Entire kind is volatile.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecoveredPayload {
    /// Restored from.
    pub checkpoint_hash: Hash,
    /// Its seq.
    pub checkpoint_seq: u64,
    /// Inclusive range voided; `null` if the checkpoint was the last line.
    pub discarded_seq: Option<SeqRange>,
    /// Restored status.
    pub restored_status: SessionStatus,
    /// In-process-waker tasks cancelled by recovery.
    pub tasks_cancelled: Vec<TaskId>,
    /// Kernel version.
    pub kernel_version: String,
    /// As in `resumed`.
    pub event_schema_version: u32,
}

/// An inclusive seq range.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeqRange {
    /// First voided seq.
    pub from: u64,
    /// Last voided seq.
    pub to: u64,
}

// ---- §2.21 compaction ----------------------------------------------------------------------

/// `compaction` (P2.6, payload reserved now).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CompactionPayload {
    /// Turn.
    pub turn: u64,
    /// From `request_compaction(strategy)` or `Kernel::compact`.
    pub strategy: String,
    /// Before.
    pub before_state_hash: Hash,
    /// After.
    pub after_state_hash: Hash,
    /// Notebook hash after compaction.
    pub notebook_hash: Option<Hash>,
    /// Messages before.
    pub messages_before: u32,
    /// Messages after.
    pub messages_after: u32,
    /// Last known input token count.
    pub tokens_before: Option<u64>,
    /// Summary artifact.
    pub summary_artifact: Option<ArtifactHandle>,
}

// ---- §2.22 spawn ---------------------------------------------------------------------------

/// `spawn` (P2.4, reserved).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpawnPayload {
    /// Turn.
    pub turn: u64,
    /// Tool-use id.
    pub tool_use_id: String,
    /// The parent-side task.
    pub task_id: TaskId,
    /// Catalog name.
    pub catalog_name: String,
    /// Volatile.
    pub child_session_id: SessionId,
    /// Volatile.
    pub child_log_path: PathBuf,
    /// Child profiles.
    pub child_profiles: ActiveProfiles,
    /// Enforcement level 1 record.
    pub child_tools: Vec<String>,
    /// Every atom is `narrower_than` a parent atom (level 3).
    pub child_grants: Vec<Capability>,
}

// ---- §2.23 child_completed -----------------------------------------------------------------

/// `child_completed` (P2.4, reserved).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChildCompletedPayload {
    /// Turn.
    pub turn: u64,
    /// Task id.
    pub task_id: TaskId,
    /// Volatile.
    pub child_session_id: SessionId,
    /// `done` | `failed`.
    pub child_status: SessionStatus,
    /// Final checkpoint.
    pub child_final_checkpoint_hash: Hash,
    /// Summed usage.
    pub child_usage: Option<Usage>,
}

// ---- §2.24 context_usage -------------------------------------------------------------------

/// `context_usage` (P2.9, reserved).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ContextUsagePayload {
    /// Turn.
    pub turn: u64,
    /// From `usage` (or an estimate).
    pub input_tokens: u64,
    /// `context_budget_tokens`.
    pub budget_tokens: u64,
    /// Over budget.
    pub over_budget: bool,
    /// Source.
    pub source: UsageSource,
}

/// Where a usage figure came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageSource {
    /// Provider-reported.
    ProviderUsage,
    /// Estimated.
    Estimate,
}

// ---- §2.25 harness_edit --------------------------------------------------------------------

/// `harness_edit` (P4, reserved).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HarnessEditPayload {
    /// Turn.
    pub turn: u64,
    /// Repo-relative path.
    pub target: String,
    /// The P4.4 search space.
    pub target_kind: String,
    /// `null` when created.
    pub before_hash: Option<Hash>,
    /// After.
    pub after_hash: Hash,
    /// Rationale artifact.
    pub rationale_artifact: Option<ArtifactHandle>,
}

// ---- §2.26 ask_user ------------------------------------------------------------------------

/// `ask_user` (D17).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AskUserPayload {
    /// Turn.
    pub turn: u64,
    /// Tool-use id.
    pub tool_use_id: String,
    /// `q<turn>-<tool_use_id>`.
    pub question_id: String,
    /// The question.
    pub question: String,
    /// Choices.
    pub options: Vec<String>,
    /// Free text allowed.
    pub allow_free_text: bool,
}

// ---- §2.27 user_answer ---------------------------------------------------------------------

/// `user_answer` (D17).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UserAnswerPayload {
    /// Turn.
    pub turn: u64,
    /// Tool-use id.
    pub tool_use_id: String,
    /// Question id.
    pub question_id: String,
    /// `null` when declined.
    pub answer: Option<String>,
    /// Declined.
    pub declined: bool,
}

// ---- §2.28 warning -------------------------------------------------------------------------

/// `warning`: non-fatal conditions. Entire kind is volatile.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WarningPayload {
    /// Turn.
    pub turn: u64,
    /// Class, see `event-schema.md` §2.28.
    pub class: String,
    /// Redacted.
    pub message: String,
    /// Detail.
    pub detail: Option<Value>,
    /// `"kernel"` or the emitting middleware/tool name.
    pub source: String,
}

impl WarningPayload {
    /// A kernel-sourced warning.
    pub fn kernel(
        turn: u64,
        class: &str,
        message: impl Into<String>,
        detail: Option<Value>,
    ) -> Self {
        WarningPayload {
            turn,
            class: class.to_owned(),
            message: message.into(),
            detail,
            source: "kernel".to_owned(),
        }
    }
}
