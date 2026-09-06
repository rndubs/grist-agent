//! Record/replay (`kernel-interface.md` §3.16, `event-schema.md` §5; P1.4).
//!
//! The cassette is the log: [`Cassette::from_log`] builds the replay store from the effective
//! events, and the [`Recorder`] middleware builds the same structure in memory from hook values
//! during a live run (a test asserts the two are equal, which is how the log's completeness as a
//! cassette is itself tested). [`ReplayProvider`] and [`ReplayTool`] serve recorded values keyed by
//! `(checkpoint_hash, request_hash)`; a miss is an error, never a live call. [`ReplayDriver`]
//! re-delivers the recorded inputs at their recorded [`AppliedAt`]. [`diff_logs`] is D16.
//!
//! # Decisions made here (candidates for "[clarified in P1.4]")
//!
//! - **Inputs in the `Recorder`.** No hook observes user messages or task updates, so the
//!   `Recorder` copies `cassette.inputs` from the kernel's post-write event broadcast once
//!   [`Recorder::attach`] is called with the kernel's handle (the model/tool halves come from
//!   `after_model`/`after_tool` as the spec says). Without `attach`, `inputs` stays empty.
//! - **What the `Recorder` never sees.** Unregistered calls and the synthetic results of a
//!   turn-scope cancellation bypass `after_tool`; `Cassette::from_log` skips them too
//!   (`tool_call{registered: false}`; every pending call when `cancelled{scope: turn}` is seen).
//!   Both reproduce themselves on replay without the cassette.
//! - **Task handles.** `ToolOutput.task` is a full `TaskHandle`, but `tool_result.task` carries only
//!   `{task_id, status}`; `from_log` completes it from the `task_started` that follows
//!   (`eta_secs`, `description`, `check_hint`, `in_process_waker`). The `Recorder` truncates `eta`
//!   to whole seconds, which is what the log can hold. `ModelResponse.response_id` is volatile and
//!   not logged, so the `Recorder` records it as `None`.
//! - **`in_process_waker`.** Added to `ToolOutput` (`tool.rs`) so `after_tool` sees it. The
//!   `ReplayTool` registers a never-resolving stand-in waker for a served task whose recorded flag
//!   is set, so `task_started`, `State.pending_tasks`, and `suspended.in_process_wakers` replay
//!   byte-identical; the recorded `TaskUpdate` (waker kind `"replay"`) completes the task.
//! - **Complete outputs.** `ToolResult::Replayed(ToolOutput)` (`tool.rs`) lets a replay tool hand
//!   the kernel the recorded post-spill, post-redaction output verbatim; the kernel skips
//!   normalization and spill and rebuilds `tool_result.spill` from the recorded `Spilled` content.
//! - **The tool-side checkpoint hash.** `ToolContext` has no checkpoint hash. A
//!   [`CheckpointTracker`] shared by the [`ReplayDriver`] (which stores `kernel.checkpoint_hash()`
//!   before every `run_turn`) and every `ReplayTool` built through [`ReplayDriver::tool`] gives the
//!   exact key. A `ReplayTool` without a tracker, or a tracker miss (a mid-turn compaction moves the
//!   checkpoint), falls back to the entries whose `request_hash` matches; that is unambiguous
//!   whenever `tool_use_id`s are unique, and a miss or an ambiguity is `ToolError::ReplayMiss`.
//! - **`UserAnswer` inputs** need no delivery: `ask_user` is an ordinary tool whose answer is the
//!   recorded tool result, served from `cassette.tools`. The driver skips them.
//! - **Batching.** Consecutive inputs recorded at the same `(turn, at)` outside a turn are
//!   delivered as one batch (one checkpoint), which is the only possibility for user messages and
//!   terminal task updates (each makes the session `Running`). Several *progress* updates recorded
//!   in separate batches would replay as one; P1 wakers deliver terminal updates only.
//! - **Dangling `model_request`.** `kernel-interface.md` §3.16 says `from_log` errors; `event-schema.md`
//!   §5.1 says it contributes nothing and replay stops with `EarlyStop` or reproduces a
//!   replay-independent failure. §5.1 is followed; `ReplayError::Incomplete` is reserved for a log
//!   with no events at all (no session id) and for a cassette with no input to start from.
//! - **Not reproducible:** a turn-scope cancellation (`cancel(Turn)`) is timing-dependent; the
//!   driver cannot cancel at the recorded point. Tool-scope cancellations, timeouts, middleware
//!   replacements, unregistered calls, spills, tasks, and `ask_user` all replay.
//! - **`after_tool` hooks below `RECORDER_PRIORITY` run again on the served (final) output on
//!   replay; they must be idempotent for the logs to match.** The replay kernel MUST carry the same
//!   middleware chain (names and priorities are compared in `middleware_chain_resolved`), including
//!   a `Recorder` entry, whose cassette then equals the recorded one.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::broadcast;

use crate::SessionId;
use crate::artifact::Spilled;
use crate::capability::Capability;
use crate::config::{RunStop, Suspension, TurnOutcome};
use crate::content::{ContentBlock, Message, Role, ToolResultContent};
pub use crate::event::{AppliedAt, AppliedPoint};
use crate::event::{CancelScopeKind, Event, EventBody, SpillRef};
use crate::hash::{Hash, HashError, canonical_json_value};
use crate::host::{Command, UserAnswer};
use crate::log::{EventLogReader, FileEventLog, LogError};
use crate::loop_::{Kernel, KernelError, KernelHandle};
use crate::middleware::{
    HookContext, Middleware, MiddlewareEntry, MiddlewareError, MiddlewareSource, RECORDER_PRIORITY,
};
use crate::provider::{ModelRequest, ModelResponse, Provider, ProviderError};
use crate::state::{SessionStatus, State};
use crate::task::{TaskHandle, TaskId, TaskUpdate, WakerSource};
use crate::tool::{
    Tool, ToolCall, ToolContext, ToolDefinition, ToolError, ToolKind, ToolOutput, ToolResult,
};

/// Name of the `Recorder`'s middleware entry (`Recorder::entry`).
pub const RECORDER_NAME: &str = "recorder";

/// `WakerSource.kind` of a replayed `TaskUpdate` (`event-schema.md` §5.1).
pub const REPLAY_WAKER_KIND: &str = "replay";

// ---- cassette --------------------------------------------------------------------------------

/// The replay key (`event-schema.md` §5.2).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CassetteKey {
    /// `state_hash` of the most recent checkpoint when the request was built.
    pub checkpoint_hash: Hash,
    /// `ModelRequest::request_hash` or `ToolCall::request_hash`.
    pub request_hash: Hash,
}

/// Everything needed to replay a session without network: built from the log (`event-schema.md` §5).
///
/// The keyed maps are serialized as arrays of `{checkpoint_hash, request_hash, value}` entries
/// (JSON object keys must be strings).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Cassette {
    /// The recorded session.
    pub session_id: SessionId,
    /// Model responses by replay key.
    #[serde(with = "keyed_map")]
    pub model: BTreeMap<CassetteKey, ModelResponse>,
    /// Tool outputs by replay key.
    #[serde(with = "keyed_map")]
    pub tools: BTreeMap<CassetteKey, ToolOutput>,
    /// Inputs the replay driver re-delivers, in log order: `user_message`, `task_update`, `user_answer`.
    pub inputs: Vec<ReplayInput>,
}

impl Default for Cassette {
    fn default() -> Self {
        Cassette {
            session_id: SessionId(String::new()),
            model: BTreeMap::new(),
            tools: BTreeMap::new(),
            inputs: Vec::new(),
        }
    }
}

/// Serde for `BTreeMap<CassetteKey, V>` as a sequence of entries.
mod keyed_map {
    use super::*;
    use serde::{Deserializer, Serializer};

    #[derive(Serialize, Deserialize)]
    struct Entry<V> {
        checkpoint_hash: Hash,
        request_hash: Hash,
        value: V,
    }

    pub fn serialize<V: Serialize, S: Serializer>(
        map: &BTreeMap<CassetteKey, V>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        let entries: Vec<Entry<&V>> = map
            .iter()
            .map(|(k, v)| Entry {
                checkpoint_hash: k.checkpoint_hash.clone(),
                request_hash: k.request_hash.clone(),
                value: v,
            })
            .collect();
        entries.serialize(s)
    }

    pub fn deserialize<'de, V: Deserialize<'de>, D: Deserializer<'de>>(
        d: D,
    ) -> Result<BTreeMap<CassetteKey, V>, D::Error> {
        let entries: Vec<Entry<V>> = Vec::deserialize(d)?;
        Ok(entries
            .into_iter()
            .map(|e| {
                (
                    CassetteKey {
                        checkpoint_hash: e.checkpoint_hash,
                        request_hash: e.request_hash,
                    },
                    e.value,
                )
            })
            .collect())
    }
}

/// One recorded input, with where it was applied.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "input", rename_all = "snake_case")]
pub enum ReplayInput {
    /// A `user_message` event.
    UserMessage {
        /// Where it was applied.
        applied: AppliedAt,
        /// The message (role `User`).
        message: Message,
    },
    /// A `task_update` event that was applied (`applied.at != ignored`); `update.source` is the
    /// replay waker (`kind: "replay"`, no tier, no detail), so a replay's own cassette equals this one.
    TaskUpdate {
        /// Where it was applied.
        applied: AppliedAt,
        /// The update.
        update: TaskUpdate,
    },
    /// A `user_answer` event. Not delivered by the driver: the answer is the `ask_user` tool's
    /// recorded result, served from `Cassette::tools`.
    UserAnswer {
        /// The question.
        question_id: String,
        /// The answer.
        answer: UserAnswer,
    },
}

impl ReplayInput {
    /// The application point of a deliverable input; `None` for `UserAnswer`.
    pub fn applied(&self) -> Option<&AppliedAt> {
        match self {
            ReplayInput::UserMessage { applied, .. } | ReplayInput::TaskUpdate { applied, .. } => {
                Some(applied)
            }
            ReplayInput::UserAnswer { .. } => None,
        }
    }
}

/// The waker source every replayed update carries (`event-schema.md` §5.1: the recorded kind is
/// rewritten, which is why `task_update.waker` is volatile).
pub fn replay_waker() -> WakerSource {
    WakerSource {
        kind: REPLAY_WAKER_KIND.to_owned(),
        trust_tier: None,
        detail: Value::Null,
    }
}

/// The `ReplayInput` an event contributes, if any (`event-schema.md` §5.1, third row).
pub fn input_of_event(body: &EventBody) -> Option<ReplayInput> {
    match body {
        EventBody::UserMessage(p) => Some(ReplayInput::UserMessage {
            applied: p.applied.clone(),
            message: Message {
                role: Role::User,
                content: p.content.clone(),
            },
        }),
        EventBody::TaskUpdate(p) if p.applied.at != AppliedPoint::Ignored => {
            Some(ReplayInput::TaskUpdate {
                applied: p.applied.clone(),
                update: TaskUpdate {
                    id: p.task_id.clone(),
                    status: p.status,
                    outcome: p.outcome.clone(),
                    eta: p.eta_secs.map(Duration::from_secs),
                    check_hint: p.check_hint.clone(),
                    source: replay_waker(),
                },
            })
        }
        EventBody::UserAnswer(p) => Some(ReplayInput::UserAnswer {
            question_id: p.question_id.clone(),
            answer: UserAnswer {
                question_id: p.question_id.clone(),
                answer: p.answer.clone(),
            },
        }),
        _ => None,
    }
}

/// `TaskHandle.eta` as the log can hold it (whole seconds).
fn truncate_eta(out: &mut ToolOutput) {
    if let Some(h) = &mut out.task {
        h.eta = h.eta.map(|d| Duration::from_secs(d.as_secs()));
    }
}

impl Cassette {
    /// From the effective events of a log (`event-schema.md` §5.1). A `model_request` without a
    /// matching `model_response`, a `tool_call` without a `tool_result`, an unregistered call, and
    /// the synthetic results of a turn-scope cancellation contribute nothing (see the module doc).
    /// Errors (`Incomplete`) only for a log with no events.
    pub fn from_log(reader: &dyn EventLogReader) -> Result<Cassette, ReplayError> {
        let events: Vec<Event> = reader.effective().collect::<Result<_, LogError>>()?;
        let session_id = events
            .first()
            .map(|e| e.session_id.clone())
            .ok_or_else(|| ReplayError::Incomplete("log has no events".to_owned()))?;
        let mut c = Cassette {
            session_id,
            ..Cassette::default()
        };
        let mut pending_model: Option<CassetteKey> = None;
        let mut pending_tools: HashMap<String, CassetteKey> = HashMap::new();
        let mut pending_task: Option<(CassetteKey, TaskId)> = None;
        for e in &events {
            match &e.body {
                EventBody::ModelRequest(p) => {
                    pending_model = Some(CassetteKey {
                        checkpoint_hash: p.checkpoint_hash.clone(),
                        request_hash: p.request_hash.clone(),
                    });
                    // A new turn: calls of a failed turn never got a result.
                    pending_tools.clear();
                }
                EventBody::ModelResponse(p) => {
                    if let Some(key) = pending_model.take_if(|k| k.request_hash == p.request_hash) {
                        c.model.insert(
                            key,
                            ModelResponse {
                                content: p.content.clone(),
                                stop_reason: p.stop_reason.clone(),
                                usage: p.usage.clone(),
                                model_id: p.model_id.clone(),
                                raw_response_hash: p.raw_response_hash.clone(),
                                response_id: None,
                            },
                        );
                    }
                }
                EventBody::ToolCall(p) if p.registered => {
                    pending_tools.insert(
                        p.tool_use_id.clone(),
                        CassetteKey {
                            checkpoint_hash: p.checkpoint_hash.clone(),
                            request_hash: p.request_hash.clone(),
                        },
                    );
                }
                EventBody::ToolResult(p) => {
                    if let Some(key) = pending_tools.remove(&p.tool_use_id) {
                        let task = p.task.as_ref().map(|t| TaskHandle {
                            id: t.task_id.clone(),
                            status: t.status,
                            eta: None,
                            check_hint: None,
                            description: None,
                        });
                        pending_task = task.as_ref().map(|h| (key.clone(), h.id.clone()));
                        c.tools.insert(
                            key,
                            ToolOutput {
                                content: p.content.clone(),
                                is_error: p.is_error,
                                artifact_handles: p.artifact_handles.clone(),
                                spilled: p.spilled,
                                task,
                                origin: p.origin,
                                in_process_waker: false,
                            },
                        );
                    }
                }
                EventBody::TaskStarted(p) => {
                    if let Some((key, id)) = pending_task.take_if(|(_, id)| *id == p.task_id)
                        && let Some(out) = c.tools.get_mut(&key)
                    {
                        out.in_process_waker = p.in_process_waker;
                        if let Some(h) = &mut out.task {
                            debug_assert_eq!(h.id, id);
                            h.eta = p.eta_secs.map(Duration::from_secs);
                            h.description = p.description.clone();
                            h.check_hint = p.check_hint.clone();
                        }
                    }
                }
                EventBody::Cancelled(p) if p.scope == CancelScopeKind::Turn => {
                    // §7.1 synthetic results never reach `after_tool`.
                    pending_tools.clear();
                }
                EventBody::TurnFailed(_) => {
                    pending_model = None;
                    pending_tools.clear();
                }
                other => {
                    if let Some(input) = input_of_event(other) {
                        c.inputs.push(input);
                    }
                }
            }
        }
        Ok(c)
    }

    /// Write as pretty-printed JSON.
    pub fn write_to(&self, path: &Path) -> Result<(), ReplayError> {
        let bytes = serde_json::to_vec_pretty(self).map_err(|e| ReplayError::Io(e.to_string()))?;
        FileEventLog::write_file_bytes(path, &bytes).map_err(|e| ReplayError::Io(e.to_string()))
    }

    /// Read a cassette written by `write_to`.
    pub fn read_from(path: &Path) -> Result<Cassette, ReplayError> {
        let bytes =
            FileEventLog::read_file_bytes(path).map_err(|e| ReplayError::Io(e.to_string()))?;
        serde_json::from_slice(&bytes).map_err(|e| ReplayError::Io(e.to_string()))
    }
}

// ---- recorder --------------------------------------------------------------------------------

struct RecorderInner {
    session_id: Option<SessionId>,
    model: BTreeMap<CassetteKey, ModelResponse>,
    tools: BTreeMap<CassetteKey, ToolOutput>,
    inputs: Vec<ReplayInput>,
    /// `(checkpoint_hash, request)` remembered by `before_model` for the next `after_model`.
    pending: Option<(Hash, ModelRequest)>,
    events: Option<broadcast::Receiver<Arc<Event>>>,
    lagged: u64,
}

/// Middleware that builds the same `Cassette` in memory during a live run (`after_model`,
/// `after_tool`). Insert it at [`RECORDER_PRIORITY`] (`Recorder::entry`) so it observes final
/// values, and call [`Recorder::attach`] with the kernel's handle before the first input so it can
/// copy `inputs` from the event stream (see the module doc).
pub struct Recorder {
    inner: Mutex<RecorderInner>,
}

impl Default for Recorder {
    fn default() -> Self {
        Self::new()
    }
}

impl Recorder {
    /// An empty recorder.
    pub fn new() -> Recorder {
        Recorder {
            inner: Mutex::new(RecorderInner {
                session_id: None,
                model: BTreeMap::new(),
                tools: BTreeMap::new(),
                inputs: Vec::new(),
                pending: None,
                events: None,
                lagged: 0,
            }),
        }
    }

    /// The chain entry: name [`RECORDER_NAME`], priority [`RECORDER_PRIORITY`], source `Kernel`.
    pub fn entry(recorder: Arc<Recorder>) -> MiddlewareEntry {
        MiddlewareEntry {
            name: RECORDER_NAME.to_owned(),
            priority: RECORDER_PRIORITY,
            source: MiddlewareSource::Kernel,
            config_hash: None,
            middleware: recorder,
        }
    }

    /// Subscribe to the kernel's event stream to record `inputs` (`user_message`, `task_update`,
    /// `user_answer`). Call right after `Kernel::create`/`open`, before any input is delivered.
    pub fn attach(&self, handle: &KernelHandle) {
        let mut g = self.lock();
        g.session_id = Some(handle.session_id().clone());
        g.events = Some(handle.subscribe());
    }

    /// The cassette recorded so far.
    pub fn cassette(&self) -> Cassette {
        let mut g = self.lock();
        drain_events(&mut g);
        Cassette {
            session_id: g.session_id.clone().unwrap_or(SessionId(String::new())),
            model: g.model.clone(),
            tools: g.tools.clone(),
            inputs: g.inputs.clone(),
        }
    }

    /// How many broadcast events were lost because the recorder fell behind the kernel's event
    /// channel (`KernelConfig::event_channel_capacity`); non-zero means `inputs` may be incomplete.
    pub fn lagged(&self) -> u64 {
        self.lock().lagged
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, RecorderInner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }
}

fn drain_events(g: &mut RecorderInner) {
    let Some(rx) = &mut g.events else { return };
    let mut inputs = Vec::new();
    let mut lagged = 0;
    loop {
        match rx.try_recv() {
            Ok(ev) => {
                if let Some(i) = input_of_event(&ev.body) {
                    inputs.push(i);
                }
            }
            Err(broadcast::error::TryRecvError::Lagged(n)) => lagged += n,
            Err(_) => break,
        }
    }
    g.inputs.extend(inputs);
    g.lagged += lagged;
}

fn mw_err(hook: &'static str, e: impl std::fmt::Display) -> MiddlewareError {
    MiddlewareError::new(RECORDER_NAME, hook, e.to_string())
}

#[async_trait]
impl Middleware for Recorder {
    async fn before_model(
        &self,
        _state: &mut State,
        req: &mut ModelRequest,
        cx: &HookContext<'_>,
    ) -> Result<(), MiddlewareError> {
        let mut g = self.lock();
        drain_events(&mut g);
        g.session_id.get_or_insert_with(|| cx.session_id.clone());
        g.pending = Some((cx.checkpoint_hash.clone(), req.clone()));
        Ok(())
    }

    async fn after_model(
        &self,
        state: &mut State,
        resp: &mut ModelResponse,
        cx: &HookContext<'_>,
    ) -> Result<(), MiddlewareError> {
        let mut g = self.lock();
        drain_events(&mut g);
        let Some((checkpoint_hash, mut req)) = g.pending.take() else {
            return Ok(());
        };
        if &checkpoint_hash != cx.checkpoint_hash {
            // A compaction between `before_model` and the call rebuilt `req.messages` (§6 B2).
            req.messages = state.messages.clone();
        }
        let key = CassetteKey {
            checkpoint_hash: cx.checkpoint_hash.clone(),
            request_hash: req.request_hash().map_err(|e| mw_err("after_model", e))?,
        };
        let mut recorded = resp.clone();
        recorded.response_id = None;
        g.model.insert(key, recorded);
        Ok(())
    }

    async fn after_tool(
        &self,
        _state: &mut State,
        call: &ToolCall,
        out: &mut ToolOutput,
        cx: &HookContext<'_>,
    ) -> Result<(), MiddlewareError> {
        let mut g = self.lock();
        drain_events(&mut g);
        let key = CassetteKey {
            checkpoint_hash: cx.checkpoint_hash.clone(),
            request_hash: call.request_hash().map_err(|e| mw_err("after_tool", e))?,
        };
        let mut recorded = out.clone();
        truncate_eta(&mut recorded);
        g.tools.insert(key, recorded);
        Ok(())
    }
}

// ---- replay provider -------------------------------------------------------------------------

/// Serves `cassette.model[(trace.checkpoint_hash, request_hash)]`. A miss is `ProviderError::ReplayMiss`,
/// which is not retryable: the turn fails, the session is `Failed`. Holds no client and never
/// makes a network call.
pub struct ReplayProvider {
    cassette: Arc<Cassette>,
}

impl ReplayProvider {
    /// A provider over `cassette`.
    pub fn new(cassette: Arc<Cassette>) -> ReplayProvider {
        ReplayProvider { cassette }
    }
}

#[async_trait]
impl Provider for ReplayProvider {
    fn name(&self) -> &str {
        "replay"
    }

    async fn complete(&self, req: ModelRequest) -> Result<ModelResponse, ProviderError> {
        let request_hash = req
            .request_hash()
            .map_err(|e| ProviderError::InvalidResponse(format!("request is not hashable: {e}")))?;
        let key = CassetteKey {
            checkpoint_hash: req.trace.checkpoint_hash.clone(),
            request_hash,
        };
        match self.cassette.model.get(&key) {
            Some(resp) => Ok(resp.clone()),
            None => Err(ProviderError::ReplayMiss {
                checkpoint_hash: key.checkpoint_hash,
                request_hash: key.request_hash,
            }),
        }
    }
}

// ---- replay tool -----------------------------------------------------------------------------

/// The checkpoint hash the replay driver last saw, shared with the replay tools (see the module doc).
#[derive(Clone, Debug, Default)]
pub struct CheckpointTracker(Arc<RwLock<Option<Hash>>>);

impl CheckpointTracker {
    /// A tracker with no checkpoint yet.
    pub fn new() -> CheckpointTracker {
        CheckpointTracker::default()
    }

    /// Record the current checkpoint hash.
    pub fn set(&self, hash: Hash) {
        *self.0.write().unwrap_or_else(|p| p.into_inner()) = Some(hash);
    }

    /// The last recorded checkpoint hash.
    pub fn get(&self) -> Option<Hash> {
        self.0.read().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

/// Wraps a registered tool's definition and serves `cassette.tools[(checkpoint_hash, call.request_hash())]`
/// as `ToolResult::Replayed`. A miss is `ToolError::ReplayMiss`, which the kernel treats as a turn
/// failure. With a [`CheckpointTracker`] (`ReplayDriver::tool` / `with_tracker`) the key is exact;
/// without one, or on a tracker miss, the entries with a matching `request_hash` are used when
/// they agree on one output.
pub struct ReplayTool {
    def: ToolDefinition,
    kind: ToolKind,
    caps: Vec<Capability>,
    cassette: Arc<Cassette>,
    tracker: Option<CheckpointTracker>,
}

impl ReplayTool {
    /// A replay tool for `def`, reporting `kind` and `caps` like the recorded tool.
    pub fn new(
        def: ToolDefinition,
        kind: ToolKind,
        caps: Vec<Capability>,
        cassette: Arc<Cassette>,
    ) -> ReplayTool {
        ReplayTool {
            def,
            kind,
            caps,
            cassette,
            tracker: None,
        }
    }

    /// Key lookups on the checkpoint hash a driver publishes through `tracker`.
    pub fn with_tracker(mut self, tracker: CheckpointTracker) -> ReplayTool {
        self.tracker = Some(tracker);
        self
    }

    fn lookup(&self, request_hash: &Hash) -> Option<ToolOutput> {
        if let Some(ck) = self.tracker.as_ref().and_then(CheckpointTracker::get) {
            let key = CassetteKey {
                checkpoint_hash: ck,
                request_hash: request_hash.clone(),
            };
            if let Some(out) = self.cassette.tools.get(&key) {
                return Some(out.clone());
            }
        }
        let mut found: Option<&ToolOutput> = None;
        for (k, out) in &self.cassette.tools {
            if &k.request_hash != request_hash {
                continue;
            }
            match found {
                None => found = Some(out),
                Some(prev) if prev == out => {}
                Some(_) => return None,
            }
        }
        found.cloned()
    }
}

#[async_trait]
impl Tool for ReplayTool {
    fn name(&self) -> &str {
        &self.def.name
    }

    fn description(&self) -> &str {
        &self.def.description
    }

    fn schema(&self) -> Value {
        self.def.input_schema.clone()
    }

    fn kind(&self) -> ToolKind {
        self.kind
    }

    fn capabilities(&self) -> Vec<Capability> {
        self.caps.clone()
    }

    /// A stand-in for `Session` tools (the registry requires one); the kernel launches it under the
    /// replay kernel's sandbox backend and the tool never calls it.
    fn session_command(&self) -> Option<Command> {
        (self.kind == ToolKind::Session).then(|| Command::new("replay-session-stub"))
    }

    fn definition(&self) -> ToolDefinition {
        self.def.clone()
    }

    async fn invoke(&self, ctx: &ToolContext<'_>, input: Value) -> Result<ToolResult, ToolError> {
        let call = ToolCall {
            tool_use_id: ctx.tool_use_id.to_owned(),
            name: self.def.name.clone(),
            input,
        };
        let request_hash = call
            .request_hash()
            .map_err(|e| ToolError::Internal(Box::new(e)))?;
        let Some(out) = self.lookup(&request_hash) else {
            return Err(ToolError::ReplayMiss {
                tool: self.def.name.clone(),
                request_hash,
            });
        };
        if out.in_process_waker
            && let Some(h) = &out.task
        {
            // The recorded waker fired in the recorded process; here the recorded `TaskUpdate`
            // (delivered by the driver) completes the task, so the stand-in never resolves.
            ctx.watch_task(h.id.clone(), Box::pin(std::future::pending()))?;
        }
        Ok(ToolResult::Replayed(out))
    }
}

/// `tool_result.spill` for a replayed output: rebuilt from the recorded `Spilled` content
/// (`{handle, head, tail, size, mime}`) of the first stored unit; `None` when nothing was stored.
pub(crate) fn recorded_spill_ref(out: &ToolOutput) -> Option<SpillRef> {
    if !out.spilled {
        return None;
    }
    let spilled = match &out.content {
        ToolResultContent::Json(v) => serde_json::from_value::<Spilled>(v.clone()).ok(),
        ToolResultContent::Blocks(blocks) => blocks.iter().find_map(|b| match b {
            ContentBlock::Text { text } => serde_json::from_str::<Spilled>(text).ok(),
            _ => None,
        }),
    }?;
    Some(SpillRef {
        handle: spilled.handle,
        size: spilled.size,
        mime: spilled.mime,
    })
}

// ---- driver ----------------------------------------------------------------------------------

/// Drives a kernel built with `ReplayProvider` + `ReplayTool`s through the recorded inputs.
pub struct ReplayDriver {
    cassette: Arc<Cassette>,
    tracker: CheckpointTracker,
}

impl ReplayDriver {
    /// A driver over `cassette`.
    pub fn new(cassette: Arc<Cassette>) -> ReplayDriver {
        ReplayDriver {
            cassette,
            tracker: CheckpointTracker::new(),
        }
    }

    /// The tracker this driver updates before every turn.
    pub fn tracker(&self) -> CheckpointTracker {
        self.tracker.clone()
    }

    /// A `ReplayProvider` over this driver's cassette.
    pub fn provider(&self) -> ReplayProvider {
        ReplayProvider::new(self.cassette.clone())
    }

    /// A `ReplayTool` over this driver's cassette, keyed exactly through the driver's tracker.
    pub fn tool(&self, def: ToolDefinition, kind: ToolKind, caps: Vec<Capability>) -> ReplayTool {
        ReplayTool::new(def, kind, caps, self.cassette.clone()).with_tracker(self.tracker.clone())
    }

    /// Delivers each `ReplayInput` at its recorded `AppliedAt` and calls `run_turn` until the
    /// inputs are exhausted and the kernel stops. Returns the final `RunStop`, or
    /// `ReplayError::EarlyStop` when the kernel stopped (or reached a point no recorded input
    /// matches) with inputs left.
    pub async fn drive(&self, kernel: &mut Kernel) -> Result<RunStop, ReplayError> {
        let handle = kernel.handle();
        let mut queue: VecDeque<&ReplayInput> = self
            .cassette
            .inputs
            .iter()
            .filter(|i| i.applied().is_some())
            .collect();
        let mut last_suspension: Option<Suspension> = None;
        let mut last_error_class: Option<String> = None;
        loop {
            self.tracker.set(kernel.checkpoint_hash().clone());
            let status = kernel.status();
            match status {
                SessionStatus::Done => return Err(KernelError::Done.into()),
                SessionStatus::Running => {
                    let turn = kernel.state().turn + 1;
                    deliver_matching(&handle, &mut queue, AppliedPoint::EndOfTurn, turn)?;
                    match kernel.run_turn().await? {
                        TurnOutcome::Suspended(s) => last_suspension = Some(s),
                        TurnOutcome::Failed { error_class } => last_error_class = Some(error_class),
                        TurnOutcome::Continue | TurnOutcome::Idle | TurnOutcome::Cancelled => {}
                    }
                }
                SessionStatus::Created
                | SessionStatus::Idle
                | SessionStatus::Suspended
                | SessionStatus::Failed => {
                    let point = point_of(status);
                    let turn = kernel.state().turn;
                    if deliver_matching(&handle, &mut queue, point, turn)? > 0 {
                        kernel.apply_queued_input().await?;
                        continue;
                    }
                    let stop = match status {
                        SessionStatus::Idle => RunStop::Idle,
                        SessionStatus::Suspended => {
                            let mut s = last_suspension.take().unwrap_or(Suspension {
                                pending_task_ids: Vec::new(),
                                in_process_wakers: 0,
                                checkpoint_hash: kernel.checkpoint_hash().clone(),
                            });
                            s.pending_task_ids =
                                kernel.state().open_tasks().map(|t| t.id.clone()).collect();
                            s.pending_task_ids.sort();
                            s.checkpoint_hash = kernel.checkpoint_hash().clone();
                            RunStop::Suspended(s)
                        }
                        SessionStatus::Failed => RunStop::Failed {
                            error_class: last_error_class
                                .take()
                                .unwrap_or_else(|| "unknown".to_owned()),
                        },
                        _ => {
                            return Err(ReplayError::Incomplete(
                                "cassette has no input that starts the session".to_owned(),
                            ));
                        }
                    };
                    return if queue.is_empty() {
                        Ok(stop)
                    } else {
                        Err(ReplayError::EarlyStop(stop))
                    };
                }
            }
        }
    }
}

fn point_of(status: SessionStatus) -> AppliedPoint {
    match status {
        SessionStatus::Created => AppliedPoint::Created,
        SessionStatus::Idle => AppliedPoint::Idle,
        SessionStatus::Suspended => AppliedPoint::Suspended,
        SessionStatus::Failed => AppliedPoint::Failed,
        SessionStatus::Running | SessionStatus::Done => AppliedPoint::EndOfTurn,
    }
}

/// Enqueue every input at the head of `queue` recorded at `(turn, at)`; returns how many.
fn deliver_matching(
    handle: &KernelHandle,
    queue: &mut VecDeque<&ReplayInput>,
    at: AppliedPoint,
    turn: u64,
) -> Result<usize, ReplayError> {
    let mut n = 0;
    while let Some(next) = queue.front() {
        let matches = next.applied().is_some_and(|a| a.at == at && a.turn == turn);
        if !matches {
            break;
        }
        match queue.pop_front() {
            Some(ReplayInput::UserMessage { message, .. }) => {
                handle.enqueue_user_message(message.clone())?;
            }
            Some(ReplayInput::TaskUpdate { update, .. }) => {
                handle.deliver_task_update(update.clone())?;
            }
            Some(ReplayInput::UserAnswer { .. }) | None => {}
        }
        n += 1;
    }
    Ok(n)
}

// ---- diff-logs -------------------------------------------------------------------------------

/// Kinds that are entirely volatile (`event-schema.md` §5.3 step 2).
pub const VOLATILE_KINDS: &[&str] = &["provider_retry", "warning", "recovered"];

/// The volatile fields per kind (`event-schema.md` §5.3 step 3). Dotted names are nested.
pub const VOLATILE_FIELDS: &[(&str, &[&str])] = &[
    ("log_opened", &["kernel_version", "mode"]),
    (
        "session_created",
        &[
            "session_id",
            "created_at",
            "kernel_version",
            "sandbox_backend",
            "sandbox_policy_hash",
            "artifact_store",
            "memory",
            "provider",
        ],
    ),
    ("profile_load", &["path"]),
    ("model_response", &["attempts"]),
    ("tool_call", &["policy_hash"]),
    ("tool_result", &["duration_ms"]),
    ("task_update", &["waker"]),
    (
        "checkpoint",
        &[
            "state.session_id",
            "state.created_at",
            "state.sandbox_policy_hash",
            "state.sandbox_backend",
        ],
    ),
    ("resumed", &["waker", "new_process", "kernel_version"]),
    ("turn_failed", &["attempts", "message"]),
    ("session_failed", &["cause_seq"]),
    ("spawn", &["child_session_id", "child_log_path"]),
    ("child_completed", &["child_session_id"]),
];

/// Remove the §5.3 volatile fields of `kind` from `payload` in place.
pub fn strip_volatile(kind: &str, payload: &mut Value) {
    let Some((_, fields)) = VOLATILE_FIELDS.iter().find(|(k, _)| *k == kind) else {
        return;
    };
    for field in *fields {
        let mut target = &mut *payload;
        let mut parts = field.split('.').peekable();
        while let Some(part) = parts.next() {
            if parts.peek().is_none() {
                if let Some(obj) = target.as_object_mut() {
                    obj.remove(part);
                }
            } else {
                match target.get_mut(part) {
                    Some(next) => target = next,
                    None => break,
                }
            }
        }
    }
}

/// The comparable form of one log: `(kind, canonical_json(strip(payload)))` per effective event,
/// volatile kinds dropped.
pub fn comparable(reader: &dyn EventLogReader) -> Result<Vec<(String, Vec<u8>)>, ReplayError> {
    let mut out = Vec::new();
    for e in reader.effective() {
        let e = e?;
        let kind = e.body.kind();
        if VOLATILE_KINDS.contains(&kind) {
            continue;
        }
        let mut payload = e
            .body
            .payload_value()
            .map_err(|err| ReplayError::Io(format!("seq {}: {err}", e.seq)))?;
        strip_volatile(kind, &mut payload);
        out.push((kind.to_owned(), canonical_json_value(&payload)?));
    }
    Ok(out)
}

/// `diff-logs` (D16): compares two logs' effective `(kind, payload)` sequences after stripping the
/// volatile fields listed in `event-schema.md` §5.3. Also exposed as `kernel/src/bin/diff-logs.rs`.
pub fn diff_logs(
    recorded: &dyn EventLogReader,
    replayed: &dyn EventLogReader,
) -> Result<DiffReport, ReplayError> {
    let a = comparable(recorded)?;
    let b = comparable(replayed)?;
    for (i, (x, y)) in a.iter().zip(b.iter()).enumerate() {
        if x.0 != y.0 {
            return Ok(DiffReport {
                identical: false,
                first_diff: Some((i, format!("kind `{}` recorded, `{}` replayed", x.0, y.0))),
                compared: i,
            });
        }
        if x.1 != y.1 {
            return Ok(DiffReport {
                identical: false,
                first_diff: Some((i, describe_payload_diff(&x.0, &x.1, &y.1))),
                compared: i,
            });
        }
    }
    let compared = a.len().min(b.len());
    if a.len() != b.len() {
        return Ok(DiffReport {
            identical: false,
            first_diff: Some((
                compared,
                format!(
                    "recorded has {} events, replayed has {} (after stripping)",
                    a.len(),
                    b.len()
                ),
            )),
            compared,
        });
    }
    Ok(DiffReport {
        identical: true,
        first_diff: None,
        compared,
    })
}

fn describe_payload_diff(kind: &str, a: &[u8], b: &[u8]) -> String {
    let at = a
        .iter()
        .zip(b)
        .position(|(x, y)| x != y)
        .unwrap_or(a.len().min(b.len()));
    let window = |s: &[u8]| {
        let start = at.saturating_sub(40);
        let end = (at + 40).min(s.len());
        String::from_utf8_lossy(&s[start..end]).into_owned()
    };
    format!(
        "`{kind}` payload differs at byte {at}: recorded …{}… vs replayed …{}…",
        window(a),
        window(b)
    )
}

/// Result of `diff_logs`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DiffReport {
    /// Byte-identical after stripping (D16).
    pub identical: bool,
    /// First divergence: index into the effective (stripped) sequences and a short description.
    pub first_diff: Option<(usize, String)>,
    /// How many events were compared before the divergence (all of them when identical).
    pub compared: usize,
}

// ---- errors ----------------------------------------------------------------------------------

/// Replay errors.
#[derive(Debug, thiserror::Error)]
pub enum ReplayError {
    /// The log or cassette lacks something replay needs.
    #[error("log is incomplete: {0}")]
    Incomplete(String),
    /// The kernel stopped before the inputs were exhausted.
    #[error("kernel stopped before inputs were exhausted: {0:?}")]
    EarlyStop(RunStop),
    /// Kernel failure.
    #[error(transparent)]
    Kernel(#[from] KernelError),
    /// Log failure.
    #[error(transparent)]
    Log(#[from] LogError),
    /// Hash failure.
    #[error(transparent)]
    Hash(#[from] HashError),
    /// I/O or serialization failure.
    #[error("io error: {0}")]
    Io(String),
}
