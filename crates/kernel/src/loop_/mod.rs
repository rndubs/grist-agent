//! The loop and the `Kernel` (`kernel-interface.md` §3.15, §4–§7).
//!
//! `Kernel::create` / `Kernel::open` build a kernel; `run` drives it; `KernelHandle` is the
//! clonable side for the protocol server and wakers. The turn itself is in `turn`, task-update
//! application in `tasks`, retry in `retry`, spill in `spill`, registry and chain validation in
//! `registry` / `chain`.
//!
//! # `ask_user` convention (§7.10)
//!
//! The kernel cannot see inside a tool, so it logs `ask_user` from the call's input and
//! `user_answer` from the result JSON of the tool registered under the reserved name
//! [`ASK_USER_TOOL_NAME`]. The tool MUST take input `{question: string, options?: string[],
//! allow_free_text?: bool}` and return `{question_id: string, answer: string | null, declined: bool}`;
//! the question id it uses is `q<turn>-<tool_use_id>` (`ask_user_question_id`), the same one the
//! kernel logs. The host crate's `AskUserTool` follows this convention.

mod chain;
mod registry;
mod retry;
pub mod spill;
mod support;
mod tasks;
mod turn;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::sync::{broadcast, mpsc};

use crate::SessionId;
use crate::artifact::ArtifactStore;
use crate::cancel::{CancelScope, CancellationToken};
use crate::capability::Capability;
use crate::config::{ResumeCause, RetryPolicy, RunStop, SpillConfig, Suspension, TurnOutcome};
use crate::content::{Message, PromptBlock};
use crate::event::{
    AppliedAt, AppliedPoint, CancelScopeKind, CancelledPayload, CheckpointPayload,
    CheckpointReason, CompactionPayload, EndedBy, Event, EventBody, LogMode, LogOpenedPayload,
    LoopPhase, MiddlewareChainResolvedPayload, ProfileLoadPayload, RecoveredPayload,
    ResumeCauseKind, ResumedPayload, SeqRange, SessionCreatedPayload, SessionEndedPayload,
    SuspendReason, SuspendedPayload, UserMessagePayload, WarningPayload,
};
use crate::hash::{Hash, HashError};
use crate::host::Host;
use crate::log::{EventLog, LogError, RestoreError, reader};
use crate::memory::{Memory, MemoryPointer};
use crate::middleware::{HookContext, MiddlewareEntry};
use crate::provider::{ModelDelta, ModelParams, Provider};
use crate::redact::Redactor;
use crate::sandbox::{PolicyError, SandboxBackend, SandboxLimits, SessionProcess};
use crate::state::{ActiveProfiles, MigrationRegistry, SessionStatus, State};
use crate::task::{TaskId, TaskUpdate};
use crate::tool::Tool;
use crate::{EVENT_SCHEMA_VERSION, KERNEL_VERSION, STATE_SCHEMA_VERSION};

use support::{CancelState, Emitter, Inbound, Logger, Registrar, WakerMap, lock};

/// The reserved tool name the kernel recognizes for `ask_user` / `user_answer` logging (§7.10).
pub const ASK_USER_TOOL_NAME: &str = "ask_user";

/// `q<turn>-<tool_use_id>`: the question id the kernel logs and the `ask_user` tool must use.
pub fn ask_user_question_id(turn: u64, tool_use_id: &str) -> String {
    format!("q{turn}-{tool_use_id}")
}

/// Everything a kernel needs besides the session's own inputs (§3.15).
pub struct KernelConfig {
    /// The tool registry (§6 level 1). Only these can ever be invoked. Names MUST be unique.
    pub tools: Vec<Arc<dyn Tool>>,
    /// Middleware entries from the resolved profile, in any order (the kernel sorts, §7.6).
    pub middleware: Vec<MiddlewareEntry>,
    /// The model client.
    pub provider: Arc<dyn Provider>,
    /// The host.
    pub host: Arc<dyn Host>,
    /// The artifact store (spill target, D12).
    pub artifact_store: Arc<dyn ArtifactStore>,
    /// The memory module.
    pub memory: Arc<dyn Memory>,
    /// The sandbox backend.
    pub sandbox: Arc<dyn SandboxBackend>,
    /// The session's event log. `create` appends `log_opened` iff the log is empty.
    pub event_log: Arc<dyn EventLog>,
    /// Shared redactor (ingress redaction, §7.5).
    pub redactor: Arc<Redactor>,
    /// Spill configuration; `cap_bytes` is clamped (§7.4).
    pub spill: SpillConfig,
    /// Provider retry policy (§7.2).
    pub retry: RetryPolicy,
    /// The `[sandbox]` limits every policy is derived under.
    pub sandbox_limits: SandboxLimits,
    /// State migrations for `open`.
    pub migrations: MigrationRegistry,
    /// Model id, params and system prompt come from the resolved profile (not `State`, §3.3).
    pub model_id: String,
    /// Sampling parameters.
    pub model_params: ModelParams,
    /// System prompt blocks in D7 order.
    pub system_prompt: Vec<PromptBlock>,
    /// Atoms granted by the resolved profile; every tool's capabilities are checked against these.
    pub grants: Vec<Capability>,
    /// If set, streaming deltas are broadcast here (protocol server). Absent → `complete` is used.
    pub delta_sink: Option<broadcast::Sender<ModelDelta>>,
    /// Capacity of the event broadcast channel behind `KernelHandle::subscribe`. Default 1024.
    pub event_channel_capacity: usize,
}

/// Inputs that exist only when creating a NEW session.
pub struct SessionInit {
    /// The session id (chosen by the launcher).
    pub session_id: SessionId,
    /// Active profile hashes.
    pub profiles: ActiveProfiles,
    /// Logged as `profile_load` events right after `session_created`, in this order.
    pub profile_loads: Vec<ProfileLoadPayload>,
    /// Layer-4 runtime overrides the client sent, echoed into `session_created`.
    pub runtime_overrides: Option<Value>,
    /// Notebook path.
    pub notebook_path: Option<PathBuf>,
    /// Initial memory pointer.
    pub memory: Option<MemoryPointer>,
}

/// Kernel errors (§3.15).
#[derive(Debug, thiserror::Error)]
pub enum KernelError {
    /// The call requires `Running`.
    #[error("session is not running (status {0:?})")]
    NotRunning(SessionStatus),
    /// The session ended; every later call fails with this.
    #[error("session is done")]
    Done,
    /// `suspend` with no open tasks.
    #[error("no open tasks to wait for")]
    NothingToWaitFor,
    /// Two tools share a name.
    #[error("duplicate tool name `{0}`")]
    DuplicateTool(String),
    /// A tool name is not `[a-z][a-z0-9_.-]*` (≤ 64 chars).
    #[error("invalid tool name `{0}`")]
    InvalidToolName(String),
    /// Two middleware entries share a name.
    #[error("duplicate middleware name `{0}`")]
    DuplicateMiddleware(String),
    /// An entry uses the parser slot, or the parser is not at its priority (§7.6).
    #[error("middleware `{0}` uses a reserved priority")]
    ReservedPriority(String),
    /// A tool's capabilities are not covered by the grants (§7.7).
    #[error("tool `{tool}` exceeds grants: {source}")]
    ToolExceedsGrants {
        /// The tool.
        tool: String,
        /// The derivation error.
        #[source]
        source: PolicyError,
    },
    /// A `Session` tool without a `session_command`.
    #[error("session tool `{0}` has no session_command")]
    MissingSessionCommand(String),
    /// Reserved (the kernel clamps instead of failing, §7.4).
    #[error("spill cap {0} out of range")]
    SpillCapOutOfRange(u64),
    /// `open` on a log without a checkpoint.
    #[error("no checkpoint in log; cannot resume")]
    NoCheckpoint,
    /// Restore failed.
    #[error(transparent)]
    Restore(#[from] RestoreError),
    /// Log failure.
    #[error(transparent)]
    Log(#[from] LogError),
    /// Hash failure.
    #[error(transparent)]
    Hash(#[from] HashError),
    /// A turn failed outside the normal failure path.
    #[error("turn failed: {0}")]
    TurnFailed(String),
    /// Anything else.
    #[error("internal: {0}")]
    Internal(String),
}

struct HandleInner {
    session_id: SessionId,
    inbox: mpsc::UnboundedSender<Inbound>,
    events: broadcast::Sender<Arc<Event>>,
    deltas: Option<broadcast::Sender<ModelDelta>>,
    cancel: Arc<Mutex<CancelState>>,
    wakers: WakerMap,
    status: Arc<Mutex<SessionStatus>>,
}

/// Clonable handle for the protocol server, wakers, and the replay driver.
#[derive(Clone)]
pub struct KernelHandle {
    inner: Arc<HandleInner>,
}

impl KernelHandle {
    fn status(&self) -> SessionStatus {
        *lock(&self.inner.status)
    }

    /// D2: in `Idle`/`Suspended`/`Failed` the message starts a turn at the next `run`; in `Running` it is
    /// queued until the end of the current turn. Never blocks. Errors only if the session is `Done`.
    pub fn enqueue_user_message(&self, msg: Message) -> Result<(), KernelError> {
        if self.status() == SessionStatus::Done {
            return Err(KernelError::Done);
        }
        self.inner
            .inbox
            .send(Inbound::UserMessage(msg))
            .map_err(|_| KernelError::Done)
    }

    /// Waker entry point (P1: internal; P3.4: protocol). Same queueing rules as user messages (§4).
    pub fn deliver_task_update(&self, update: TaskUpdate) -> Result<(), KernelError> {
        match self.status() {
            SessionStatus::Done => return Err(KernelError::Done),
            SessionStatus::Created => return Err(KernelError::NotRunning(SessionStatus::Created)),
            _ => {}
        }
        self.inner
            .inbox
            .send(Inbound::TaskUpdate(update))
            .map_err(|_| KernelError::Done)
    }

    /// D15. Sets the matching token; the loop notices at the next check point (§6). Returns at once.
    pub fn cancel(&self, scope: CancelScope) {
        match scope {
            CancelScope::Turn => {
                if let Some(t) = &lock(&self.inner.cancel).turn {
                    t.cancel();
                }
            }
            CancelScope::Tool { tool_use_id } => {
                if let Some(t) = lock(&self.inner.cancel).tools.get(&tool_use_id) {
                    t.cancel();
                }
            }
            CancelScope::Task { task_id } => {
                if let Some(jh) = lock(&self.inner.wakers).remove(&task_id) {
                    jh.abort();
                }
                if self.status() != SessionStatus::Done {
                    let _ = self.inner.inbox.send(Inbound::CancelTask(task_id));
                }
            }
        }
    }

    /// Every event as written (post-redaction), for the protocol server. Lagging receivers get `Lagged`.
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<Event>> {
        self.inner.events.subscribe()
    }

    /// Streaming deltas, when the kernel was configured with a `delta_sink`.
    pub fn subscribe_deltas(&self) -> Option<broadcast::Receiver<ModelDelta>> {
        self.inner.deltas.as_ref().map(|d| d.subscribe())
    }

    /// The session.
    pub fn session_id(&self) -> &SessionId {
        &self.inner.session_id
    }
}

/// Immutable-per-process parts of a kernel, separated from `State` so hooks can borrow both.
pub(crate) struct Shared {
    session_id: SessionId,
    host: Arc<dyn Host>,
    artifacts: Arc<dyn ArtifactStore>,
    memory: Arc<dyn Memory>,
    sandbox: Arc<dyn SandboxBackend>,
    provider: Arc<dyn Provider>,
    redactor: Arc<Redactor>,
    registry: registry::Registry,
    chain: Vec<MiddlewareEntry>,
    profiles: ActiveProfiles,
    emitter: Emitter,
    logger: Logger,
    registrar: Arc<Registrar>,
    cancel: Arc<Mutex<CancelState>>,
    spill: SpillConfig,
    retry: RetryPolicy,
    limits: SandboxLimits,
    model_id: String,
    model_params: ModelParams,
    system_prompt: Vec<PromptBlock>,
    grants: Vec<Capability>,
    delta_sink: Option<broadcast::Sender<ModelDelta>>,
}

impl Shared {
    fn hook_cx<'a>(
        &'a self,
        checkpoint_hash: &'a Hash,
        turn: u64,
        cancel: CancellationToken,
    ) -> HookContext<'a> {
        HookContext::new(
            &self.session_id,
            turn,
            checkpoint_hash,
            cancel,
            &*self.host,
            &*self.artifacts,
            &*self.memory,
            &self.profiles,
            self.registry.definitions(),
            &self.emitter,
        )
    }

    /// Ingress redaction of any serializable value (§7.5).
    fn redact<T: Serialize + DeserializeOwned>(&self, v: &mut T) -> Result<(), serde_json::Error> {
        let mut raw = serde_json::to_value(&*v)?;
        if self.redactor.redact_value(&mut raw).replacements > 0 {
            *v = serde_json::from_value(raw)?;
        }
        Ok(())
    }
}

struct Built {
    sh: Shared,
    inbox: mpsc::UnboundedReceiver<Inbound>,
    handle: KernelHandle,
    chain_payload: MiddlewareChainResolvedPayload,
    spill_clamped: bool,
    migrations: MigrationRegistry,
}

fn build(
    config: KernelConfig,
    session_id: SessionId,
    profiles: ActiveProfiles,
    status: SessionStatus,
) -> Result<Built, KernelError> {
    let registry = registry::Registry::build(config.tools, &config.grants, &config.sandbox_limits)?;
    let (chain, chain_payload) = chain::resolve(config.middleware)?;
    let (spill, spill_clamped) = config.spill.clamped();
    let (events, _) = broadcast::channel(config.event_channel_capacity.max(1));
    let (inbox_tx, inbox_rx) = mpsc::unbounded_channel();
    let cancel = Arc::new(Mutex::new(CancelState::new()));
    let wakers: WakerMap = Arc::new(Mutex::new(HashMap::new()));
    let registrar = Arc::new(Registrar::new(wakers.clone()));
    let handle = KernelHandle {
        inner: Arc::new(HandleInner {
            session_id: session_id.clone(),
            inbox: inbox_tx,
            events: events.clone(),
            deltas: config.delta_sink.clone(),
            cancel: cancel.clone(),
            wakers,
            status: Arc::new(Mutex::new(status)),
        }),
    };
    registrar.set_handle(handle.clone());
    let sh = Shared {
        session_id,
        host: config.host,
        artifacts: config.artifact_store,
        memory: config.memory,
        sandbox: config.sandbox,
        provider: config.provider,
        redactor: config.redactor,
        registry,
        chain,
        profiles,
        emitter: Emitter::new(),
        logger: Logger {
            log: config.event_log,
            events,
        },
        registrar,
        cancel,
        spill,
        retry: config.retry,
        limits: config.sandbox_limits,
        model_id: config.model_id,
        model_params: config.model_params,
        system_prompt: config.system_prompt,
        grants: config.grants,
        delta_sink: config.delta_sink,
    };
    Ok(Built {
        sh,
        inbox: inbox_rx,
        handle,
        chain_payload,
        spill_clamped,
        migrations: config.migrations,
    })
}

/// A session's kernel: the loop, the state, the handle (§3.15).
pub struct Kernel {
    sh: Shared,
    state: State,
    checkpoint_hash: Hash,
    inbox: mpsc::UnboundedReceiver<Inbound>,
    handle: KernelHandle,
    session_procs: HashMap<String, Box<dyn SessionProcess>>,
    last_error_class: Option<String>,
    #[allow(dead_code)]
    // Held so `open` on a later log can reuse the registry (P1.4 replay driver).
    migrations: MigrationRegistry,
}

impl Kernel {
    /// New session: validates config (§7.7), derives policies, writes `log_opened`, `session_created`,
    /// `profile_load`*, `middleware_chain_resolved`, and any start-up `warning`s. Status `Created`.
    pub async fn create(config: KernelConfig, init: SessionInit) -> Result<Kernel, KernelError> {
        if config.event_log.session_id() != &init.session_id {
            return Err(LogError::WrongSession {
                found: config.event_log.session_id().clone(),
                expected: init.session_id.clone(),
            }
            .into());
        }
        let built = build(
            config,
            init.session_id.clone(),
            init.profiles.clone(),
            SessionStatus::Created,
        )?;
        let state = State {
            schema_version: STATE_SCHEMA_VERSION,
            session_id: init.session_id.clone(),
            created_at: crate::time::now_rfc3339_ms(),
            turn: 0,
            session_status: SessionStatus::Created,
            messages: Vec::new(),
            pending_tasks: Default::default(),
            profiles: init.profiles.clone(),
            memory: init.memory,
            notebook_path: init.notebook_path.clone(),
            sandbox_policy_hash: built.sh.registry.envelope_hash.clone(),
            sandbox_backend: built.sh.sandbox.name().to_owned(),
        };
        let checkpoint_hash = state.state_hash()?;
        let k = Kernel {
            sh: built.sh,
            state,
            checkpoint_hash,
            inbox: built.inbox,
            handle: built.handle,
            session_procs: HashMap::new(),
            last_error_class: None,
            migrations: built.migrations,
        };
        if k.sh.logger.log.last_seq().is_none() {
            k.log(EventBody::LogOpened(LogOpenedPayload {
                event_schema_version: EVENT_SCHEMA_VERSION,
                state_schema_version: STATE_SCHEMA_VERSION,
                kernel_version: KERNEL_VERSION.to_owned(),
                mode: LogMode::Live,
            }))
            .await?;
        }
        let mut grants = k.sh.grants.clone();
        grants.sort();
        k.log(EventBody::SessionCreated(SessionCreatedPayload {
            session_id: init.session_id,
            created_at: k.state.created_at.clone(),
            kernel_version: KERNEL_VERSION.to_owned(),
            profiles: init.profiles,
            model_id: k.sh.model_id.clone(),
            tools: k.sh.registry.names_sorted(),
            grants,
            sandbox_backend: k.sh.sandbox.name().to_owned(),
            sandbox_policy_hash: k.sh.registry.envelope_hash.clone(),
            artifact_store: k.sh.artifacts.name().to_owned(),
            memory: k.sh.memory.name().to_owned(),
            provider: k.sh.provider.name().to_owned(),
            spill: k.sh.spill.clone(),
            retry: k.sh.retry.clone(),
            notebook_path: init.notebook_path,
            sandbox_limits: k.sh.limits.clone(),
            overrides: init.runtime_overrides,
            parent: None,
        }))
        .await?;
        for p in init.profile_loads {
            k.log(EventBody::ProfileLoad(p)).await?;
        }
        k.log(EventBody::MiddlewareChainResolved(built.chain_payload))
            .await?;
        k.startup_warnings(built.spill_clamped).await?;
        Ok(k)
    }

    /// Existing log: restores the latest checkpoint (migrating if needed), decides between
    /// `resumed` (log ended cleanly) and `recovered` (it did not, §7.3), runs `on_resume`, then applies
    /// `cause` (a delivered `TaskUpdate` or `Message`) and checkpoints if it changed state.
    pub async fn open(config: KernelConfig, cause: ResumeCause) -> Result<Kernel, KernelError> {
        let rdr = config.event_log.reader();
        let effective: Vec<Event> = rdr.effective().collect::<Result<_, _>>()?;
        let Some(migrated) = rdr.restore_latest(&config.migrations)? else {
            return Err(KernelError::NoCheckpoint);
        };
        let (ck_event, ck_payload) =
            reader::latest_checkpoint(&effective).ok_or(KernelError::NoCheckpoint)?;
        let last_seq = config.event_log.last_seq().unwrap_or(ck_event.seq);
        let mut state = migrated.state;
        if state.session_status == SessionStatus::Done {
            return Err(KernelError::Done);
        }
        if config.event_log.session_id() != &state.session_id {
            return Err(LogError::WrongSession {
                found: config.event_log.session_id().clone(),
                expected: state.session_id.clone(),
            }
            .into());
        }
        let clean = reader::ends_cleanly(&effective) && !matches!(cause, ResumeCause::Recovery);
        let last_error_class = effective.iter().rev().find_map(|e| match &e.body {
            EventBody::SessionFailed(p) => Some(p.error_class.clone()),
            _ => None,
        });
        let restored_status = state.session_status;
        let built = build(
            config,
            state.session_id.clone(),
            state.profiles.clone(),
            restored_status,
        )?;
        // Volatile fields describe THIS machine (event-schema §3.5).
        state.sandbox_policy_hash = built.sh.registry.envelope_hash.clone();
        state.sandbox_backend = built.sh.sandbox.name().to_owned();
        let mut k = Kernel {
            sh: built.sh,
            state,
            checkpoint_hash: ck_payload.state_hash.clone(),
            inbox: built.inbox,
            handle: built.handle,
            session_procs: HashMap::new(),
            last_error_class,
            migrations: built.migrations,
        };
        let mut changed = migrated.migrated.is_some();
        if clean {
            let (cause_kind, waker) = match &cause {
                ResumeCause::TaskUpdate(u) => (ResumeCauseKind::TaskUpdate, Some(u.source.clone())),
                ResumeCause::UserMessage(_) => (ResumeCauseKind::UserMessage, None),
                ResumeCause::Operator | ResumeCause::Recovery => (ResumeCauseKind::Operator, None),
            };
            k.log(EventBody::Resumed(ResumedPayload {
                from_checkpoint_hash: ck_payload.state_hash.clone(),
                from_checkpoint_seq: ck_event.seq,
                from_status: restored_status,
                cause: cause_kind,
                waker,
                new_process: true,
                kernel_version: KERNEL_VERSION.to_owned(),
                event_schema_version: EVENT_SCHEMA_VERSION,
            }))
            .await?;
        } else {
            // §7.3 step 4: in-process wakers died with the previous process.
            let lost: Vec<TaskId> = k
                .state
                .open_tasks()
                .filter(|t| t.in_process_waker)
                .map(|t| t.id.clone())
                .collect();
            for id in &lost {
                k.cancel_task_in_state(
                    id,
                    serde_json::json!({ "error": "in-process waker lost in crash" }),
                );
            }
            changed |= !lost.is_empty();
            let discarded_seq = (ck_event.seq < last_seq).then_some(SeqRange {
                from: ck_event.seq + 1,
                to: last_seq,
            });
            k.log(EventBody::Recovered(RecoveredPayload {
                checkpoint_hash: ck_payload.state_hash.clone(),
                checkpoint_seq: ck_event.seq,
                discarded_seq,
                restored_status,
                tasks_cancelled: lost,
                kernel_version: KERNEL_VERSION.to_owned(),
                event_schema_version: EVENT_SCHEMA_VERSION,
            }))
            .await?;
        }
        if let Some((from, to)) = migrated.migrated {
            k.warn(
                "state_migrated",
                format!("checkpoint state migrated from schema_version {from} to {to}"),
                Some(serde_json::json!({ "from": from, "to": to })),
            )
            .await?;
        }
        k.log(EventBody::MiddlewareChainResolved(built.chain_payload))
            .await?;
        k.startup_warnings(built.spill_clamped).await?;
        // `on_resume` hooks.
        let turn = k.state.turn;
        let token = lock(&k.sh.cancel).session.clone();
        let before_hooks = k.state.state_hash()?;
        for e in &k.sh.chain {
            let cx = k.sh.hook_cx(&k.checkpoint_hash, turn, token.clone());
            e.middleware
                .on_resume(&mut k.state, &cause, &cx)
                .await
                .map_err(|e| KernelError::Internal(format!("on_resume: {e}")))?;
            k.flush_emits().await?;
        }
        changed |= k.state.state_hash()? != before_hooks;
        // Apply the cause.
        let point = applied_point(restored_status);
        match cause {
            ResumeCause::TaskUpdate(u) => {
                let applied = k.apply_task_update(u, point).await?;
                changed |= applied != tasks::Applied::Ignored;
                if matches!(
                    restored_status,
                    SessionStatus::Suspended | SessionStatus::Failed
                ) && applied != tasks::Applied::Ignored
                {
                    k.set_status(SessionStatus::Running);
                }
            }
            ResumeCause::UserMessage(m) => {
                k.apply_user_message(m, point).await?;
                changed = true;
                k.set_status(SessionStatus::Running);
            }
            ResumeCause::Operator => {
                if matches!(
                    restored_status,
                    SessionStatus::Suspended | SessionStatus::Failed
                ) {
                    k.set_status(SessionStatus::Running);
                }
            }
            ResumeCause::Recovery => {}
        }
        changed |= k.state.session_status != restored_status;
        if changed {
            k.checkpoint(if clean {
                CheckpointReason::Resume
            } else {
                CheckpointReason::Recovery
            })
            .await?;
        }
        Ok(k)
    }

    /// Clonable handle for the protocol server, wakers, and the replay driver.
    pub fn handle(&self) -> KernelHandle {
        self.handle.clone()
    }

    /// The current state.
    pub fn state(&self) -> &State {
        &self.state
    }

    /// The session status (D2).
    pub fn status(&self) -> SessionStatus {
        self.state.session_status
    }

    /// `state_hash` of the most recent checkpoint (the first half of the replay key).
    pub fn checkpoint_hash(&self) -> &Hash {
        &self.checkpoint_hash
    }

    /// Drives the session: in `Created`/`Idle`/`Suspended`/`Failed` awaits the inbox (user message, task
    /// update), applies it, and runs turns while `Running`. Returns when the session reaches `Idle`,
    /// `Suspended`, `Done`, or `Failed`. The launcher calls it again after delivering more input.
    pub async fn run(&mut self) -> Result<RunStop, KernelError> {
        loop {
            match self.status() {
                SessionStatus::Done => return Err(KernelError::Done),
                SessionStatus::Running => match self.run_turn().await? {
                    TurnOutcome::Continue => continue,
                    TurnOutcome::Idle | TurnOutcome::Cancelled => return Ok(RunStop::Idle),
                    TurnOutcome::Suspended(s) => return Ok(RunStop::Suspended(s)),
                    TurnOutcome::Failed { error_class } => {
                        return Ok(RunStop::Failed { error_class });
                    }
                },
                from => {
                    let Some(first) = self.inbox.recv().await else {
                        return Err(KernelError::Internal("inbox closed".to_owned()));
                    };
                    self.apply_inbound_batch(first, from).await?;
                    match self.status() {
                        SessionStatus::Running | SessionStatus::Created => continue,
                        SessionStatus::Idle => return Ok(RunStop::Idle),
                        SessionStatus::Suspended => {
                            return Ok(RunStop::Suspended(self.suspension()));
                        }
                        SessionStatus::Failed => {
                            return Ok(RunStop::Failed {
                                error_class: self.error_class(),
                            });
                        }
                        SessionStatus::Done => return Err(KernelError::Done),
                    }
                }
            }
        }
    }

    /// Non-blocking variant of the input step of `run`: in `Created`/`Idle`/`Suspended`/`Failed`,
    /// drain whatever the inbox already holds and apply it (the §5.1 transition, with its
    /// checkpoint) without running a turn. Returns whether anything was applied. For launchers and
    /// the replay driver (§3.16) that drive turns one at a time with `run_turn`. In `Running` it is a
    /// no-op (queued input waits for the turn boundary, D2).
    pub async fn apply_queued_input(&mut self) -> Result<bool, KernelError> {
        match self.status() {
            SessionStatus::Done => return Err(KernelError::Done),
            SessionStatus::Running => return Ok(false),
            _ => {}
        }
        let from = self.status();
        let Ok(first) = self.inbox.try_recv() else {
            return Ok(false);
        };
        self.apply_inbound_batch(first, from).await?;
        Ok(true)
    }

    /// Explicit suspend from `Running` (between turns) or `Idle`: terminates session processes, writes
    /// a checkpoint and `suspended`. Only valid when open tasks exist; else `KernelError::NothingToWaitFor`.
    pub async fn suspend(&mut self) -> Result<Suspension, KernelError> {
        match self.status() {
            SessionStatus::Done => return Err(KernelError::Done),
            SessionStatus::Running | SessionStatus::Idle => {}
            s => return Err(KernelError::NotRunning(s)),
        }
        if self.state.open_tasks().next().is_none() {
            return Err(KernelError::NothingToWaitFor);
        }
        self.terminate_session_processes().await;
        self.set_status(SessionStatus::Suspended);
        self.checkpoint(CheckpointReason::Suspend).await?;
        let s = self.suspension();
        self.log(EventBody::Suspended(SuspendedPayload {
            turn: self.state.turn,
            reason: SuspendReason::Explicit,
            pending_task_ids: s.pending_task_ids.clone(),
            in_process_wakers: s.in_process_wakers as u32,
            checkpoint_hash: s.checkpoint_hash.clone(),
        }))
        .await?;
        Ok(s)
    }

    /// In-process resume from `Suspended`/`Failed` (same process still alive): applies `cause`,
    /// checkpoints, sets `Running`. Cross-process resume is `Kernel::open`.
    pub async fn resume(&mut self, cause: ResumeCause) -> Result<(), KernelError> {
        let from = self.status();
        match from {
            SessionStatus::Done => return Err(KernelError::Done),
            SessionStatus::Suspended | SessionStatus::Failed => {}
            s => return Err(KernelError::NotRunning(s)),
        }
        let (cause_kind, waker) = match &cause {
            ResumeCause::TaskUpdate(u) => (ResumeCauseKind::TaskUpdate, Some(u.source.clone())),
            ResumeCause::UserMessage(_) => (ResumeCauseKind::UserMessage, None),
            ResumeCause::Operator => (ResumeCauseKind::Operator, None),
            ResumeCause::Recovery => {
                return Err(KernelError::Internal(
                    "ResumeCause::Recovery is only valid for Kernel::open".to_owned(),
                ));
            }
        };
        let from_seq = self.sh.logger.log.last_seq().unwrap_or(0);
        self.log(EventBody::Resumed(ResumedPayload {
            from_checkpoint_hash: self.checkpoint_hash.clone(),
            from_checkpoint_seq: from_seq,
            from_status: from,
            cause: cause_kind,
            waker,
            new_process: false,
            kernel_version: KERNEL_VERSION.to_owned(),
            event_schema_version: EVENT_SCHEMA_VERSION,
        }))
        .await?;
        let point = applied_point(from);
        match cause {
            ResumeCause::TaskUpdate(u) => {
                self.apply_task_update(u, point).await?;
            }
            ResumeCause::UserMessage(m) => self.apply_user_message(m, point).await?,
            ResumeCause::Operator | ResumeCause::Recovery => {}
        }
        self.set_status(SessionStatus::Running);
        self.checkpoint(CheckpointReason::Resume).await?;
        Ok(())
    }

    /// Explicit end (D2): terminates processes, cancels open tasks, writes `checkpoint` + `session_ended`.
    /// Terminal. `session_ended.by` is `user` (the only caller in P1 is the user's client).
    pub async fn end(&mut self) -> Result<(), KernelError> {
        if self.status() == SessionStatus::Done {
            return Err(KernelError::Done);
        }
        lock(&self.sh.cancel).session.cancel();
        self.terminate_session_processes().await;
        self.sh.registrar.abort_all();
        let open: Vec<TaskId> = self.state.open_tasks().map(|t| t.id.clone()).collect();
        for id in &open {
            self.cancel_task_in_state(id, serde_json::json!({ "cancelled": true }));
            self.log(EventBody::Cancelled(CancelledPayload {
                turn: self.state.turn,
                scope: CancelScopeKind::Task,
                tool_use_id: None,
                task_id: Some(id.clone()),
                phase: LoopPhase::Idle,
                signalled: false,
                skipped_tool_use_ids: Vec::new(),
            }))
            .await?;
        }
        self.set_status(SessionStatus::Done);
        self.checkpoint(CheckpointReason::End).await?;
        self.log(EventBody::SessionEnded(SessionEndedPayload {
            turn: self.state.turn,
            by: EndedBy::User,
            checkpoint_hash: self.checkpoint_hash.clone(),
            cancelled_task_ids: open,
        }))
        .await?;
        Ok(())
    }

    /// Runs the `on_compact` chain now (UI-triggered compaction). Valid between turns.
    pub async fn compact(&mut self, strategy: &str) -> Result<(), KernelError> {
        if self.status() == SessionStatus::Done {
            return Err(KernelError::Done);
        }
        let token = lock(&self.sh.cancel).session.clone();
        let turn = self.state.turn;
        match self.run_compaction(strategy, &token, turn).await {
            Ok(()) => Ok(()),
            Err(turn::Abort::Fatal(e)) => Err(e),
            Err(turn::Abort::Fail { message, .. }) => Err(KernelError::TurnFailed(message)),
            Err(turn::Abort::Cancelled { .. }) => {
                Err(KernelError::Internal("compaction cancelled".to_owned()))
            }
        }
    }

    // ---- internals shared by the lifecycle methods and the turn --------------------------------

    async fn log(&self, body: EventBody) -> Result<Event, LogError> {
        self.sh.logger.log(body).await
    }

    async fn warn(
        &self,
        class: &str,
        message: impl Into<String>,
        detail: Option<Value>,
    ) -> Result<(), LogError> {
        self.log(EventBody::Warning(WarningPayload::kernel(
            self.state.turn,
            class,
            message,
            detail,
        )))
        .await?;
        Ok(())
    }

    async fn startup_warnings(&self, spill_clamped: bool) -> Result<(), LogError> {
        if spill_clamped {
            self.warn(
                "spill_cap_clamped",
                format!(
                    "spill cap clamped to {} bytes (allowed range {}..={})",
                    self.sh.spill.cap_bytes,
                    SpillConfig::MIN_CAP_BYTES,
                    SpillConfig::MAX_CAP_BYTES
                ),
                Some(serde_json::json!({ "cap_bytes": self.sh.spill.cap_bytes })),
            )
            .await?;
        }
        if self.sh.sandbox.name() == "none" {
            self.warn(
                "sandbox_backend_none",
                "sandbox backend `none` is in use; tools run WITHOUT isolation (development build)",
                None,
            )
            .await?;
        }
        if self.sh.artifacts.name() == "noop" {
            self.warn(
                "artifact_store_noop",
                "artifact store is `noop`; spilled results are not retrievable",
                None,
            )
            .await?;
        }
        if self.sh.memory.name() == "noop" {
            self.warn("memory_noop", "memory module is `noop`", None)
                .await?;
        }
        Ok(())
    }

    /// Flush events queued by `HookContext::emit` since the last hook (§1.4 ordering).
    async fn flush_emits(&self) -> Result<(), LogError> {
        for body in self.sh.emitter.take_queue() {
            self.log(body).await?;
        }
        Ok(())
    }

    /// Sets the status on `State` and the handle's mirror. `Running` means "a turn is running or
    /// about to run", so the turn token exists for the whole `Running` span: `cancel(Turn)` between
    /// turns cancels the next turn at its first `✂` (§7.1).
    fn set_status(&mut self, s: SessionStatus) {
        self.state.session_status = s;
        *lock(&self.handle.inner.status) = s;
        let mut c = lock(&self.sh.cancel);
        if s == SessionStatus::Running {
            if c.turn.is_none() {
                c.turn = Some(c.session.child_token());
            }
        } else {
            c.turn = None;
        }
    }

    fn error_class(&self) -> String {
        self.last_error_class
            .clone()
            .unwrap_or_else(|| "unknown".to_owned())
    }

    /// `ckpt(reason)` of §6: write the full state, remember its hash.
    async fn checkpoint(&mut self, reason: CheckpointReason) -> Result<(), KernelError> {
        let state_hash = self.state.state_hash()?;
        self.log(EventBody::Checkpoint(CheckpointPayload {
            turn: self.state.turn,
            reason,
            session_status: self.state.session_status,
            state_hash: state_hash.clone(),
            state: self.state.clone(),
        }))
        .await?;
        self.checkpoint_hash = state_hash;
        Ok(())
    }

    fn suspension(&self) -> Suspension {
        let mut ids: Vec<TaskId> = self.state.open_tasks().map(|t| t.id.clone()).collect();
        ids.sort();
        // Open tasks whose waker was registered in this process, whether or not that future has
        // already resolved: a resolved-but-undelivered `TaskUpdate` is queued in this process's
        // inbox and is lost if the launcher lets the process exit. Counting only still-live futures
        // raced against fast-exiting scripts and made `suspended.in_process_wakers` differ between
        // a recording and its replay (seen as a CI flake in the P1 exit-criteria replay test).
        let in_process_wakers = self
            .state
            .open_tasks()
            .filter(|t| t.in_process_waker)
            .count();
        Suspension {
            pending_task_ids: ids,
            in_process_wakers,
            checkpoint_hash: self.checkpoint_hash.clone(),
        }
    }

    async fn terminate_session_processes(&mut self) {
        for (_, p) in self.session_procs.drain() {
            let _ = p.terminate().await;
        }
    }

    /// Append a user message (ingress-redacted) and log `user_message`.
    async fn apply_user_message(
        &mut self,
        mut msg: Message,
        at: AppliedPoint,
    ) -> Result<(), KernelError> {
        self.sh
            .redact(&mut msg)
            .map_err(|e| KernelError::Internal(format!("redaction: {e}")))?;
        let turn = self.state.turn;
        self.log(EventBody::UserMessage(UserMessagePayload {
            turn,
            applied: AppliedAt { turn, at },
            content: msg.content.clone(),
        }))
        .await?;
        self.state.messages.push(msg);
        Ok(())
    }

    /// Drain the inbox at a transition out of `Created`/`Idle`/`Suspended`/`Failed` (§5.1) and
    /// checkpoint once if anything changed.
    async fn apply_inbound_batch(
        &mut self,
        first: Inbound,
        from: SessionStatus,
    ) -> Result<(), KernelError> {
        let point = applied_point(from);
        let mut items = vec![first];
        while let Ok(i) = self.inbox.try_recv() {
            items.push(i);
        }
        let mut any_user = false;
        let mut changed = false;
        let mut terminal_update = false;
        for item in items {
            match item {
                Inbound::UserMessage(m) => {
                    self.apply_user_message(m, point).await?;
                    any_user = true;
                    changed = true;
                }
                Inbound::TaskUpdate(u) => match self.apply_task_update(u, point).await? {
                    tasks::Applied::Ignored => {}
                    tasks::Applied::Progress => changed = true,
                    tasks::Applied::Terminal => {
                        changed = true;
                        terminal_update = true;
                    }
                },
                Inbound::CancelTask(id) => {
                    if self.apply_cancel_task(&id, LoopPhase::Idle).await? {
                        changed = true;
                        terminal_update = true;
                    }
                }
            }
        }
        if any_user || (terminal_update && from == SessionStatus::Suspended) {
            self.set_status(SessionStatus::Running);
        }
        if changed {
            self.checkpoint(if any_user {
                CheckpointReason::UserInput
            } else {
                CheckpointReason::TaskUpdate
            })
            .await?;
        }
        Ok(())
    }

    /// F1 of §6 (also used on failure): drain queued input at the turn boundary. Returns whether a
    /// user message was drained.
    async fn drain_inbox_end_of_turn(&mut self) -> Result<bool, KernelError> {
        // In-process wakers deliver from spawned tasks; let one whose future already resolved
        // reach the inbox before the boundary decision (a task that completed mid-turn must not
        // cause a suspension, §4.2).
        tokio::task::yield_now().await;
        let mut any_user = false;
        while let Ok(item) = self.inbox.try_recv() {
            match item {
                Inbound::UserMessage(m) => {
                    self.apply_user_message(m, AppliedPoint::EndOfTurn).await?;
                    any_user = true;
                }
                Inbound::TaskUpdate(u) => {
                    self.apply_task_update(u, AppliedPoint::EndOfTurn).await?;
                }
                Inbound::CancelTask(id) => {
                    self.apply_cancel_task(&id, LoopPhase::Idle).await?;
                }
            }
        }
        Ok(any_user)
    }

    /// `compaction` around the `on_compact` chain (B2 of §6 and `Kernel::compact`).
    async fn run_compaction(
        &mut self,
        strategy: &str,
        token: &CancellationToken,
        turn: u64,
    ) -> Result<(), turn::Abort> {
        let before = self.state.state_hash()?;
        let messages_before = self.state.messages.len() as u32;
        for e in &self.sh.chain {
            turn::check_cancel(token, LoopPhase::BeforeModel)?;
            let cx = self.sh.hook_cx(&self.checkpoint_hash, turn, token.clone());
            e.middleware.on_compact(&mut self.state, &cx).await?;
            self.flush_emits().await?;
        }
        let after = self.state.state_hash()?;
        self.log(EventBody::Compaction(CompactionPayload {
            turn,
            strategy: strategy.to_owned(),
            before_state_hash: before,
            after_state_hash: after,
            notebook_hash: None,
            messages_before,
            messages_after: self.state.messages.len() as u32,
            tokens_before: None,
            summary_artifact: None,
        }))
        .await?;
        self.checkpoint(CheckpointReason::Compaction)
            .await
            .map_err(turn::Abort::Fatal)?;
        Ok(())
    }
}

fn applied_point(from: SessionStatus) -> AppliedPoint {
    match from {
        SessionStatus::Created => AppliedPoint::Created,
        SessionStatus::Idle => AppliedPoint::Idle,
        SessionStatus::Suspended => AppliedPoint::Suspended,
        SessionStatus::Failed => AppliedPoint::Failed,
        SessionStatus::Running | SessionStatus::Done => AppliedPoint::EndOfTurn,
    }
}
