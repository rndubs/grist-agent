//! One turn (§6), step by step, with the cancellation (§7.1) and failure (§7.2) exits.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{Value, json};

use super::retry::{CallFailure, call_with_retry, new_request_id};
use super::{ASK_USER_TOOL_NAME, Kernel, KernelError, ask_user_question_id, lock, spill};
use crate::cancel::CancellationToken;
use crate::config::{Suspension, TurnOutcome};
use crate::content::{ContentBlock, Message, Role, ToolResultContent};
use crate::event::{
    AskUserPayload, CancelScopeKind, CancelledPayload, CheckpointReason, EventBody, LoopPhase,
    ModelRequestPayload, ModelResponsePayload, PromptBlockRef, SessionFailedPayload, SpillRef,
    SuspendReason, SuspendedPayload, TaskRef, TaskStartedPayload, ToolCallPayload,
    ToolResultPayload, TurnFailedPayload, UserAnswerPayload, WarningPayload,
};
use crate::hash::{Hash, HashError};
use crate::log::LogError;
use crate::middleware::{MiddlewareError, ToolFlow};
use crate::provider::{ModelRequest, RequestTrace};
use crate::sandbox::SandboxPolicy;
use crate::state::{SessionStatus, State};
use crate::task::{Task, TaskId, TaskStatus};
use crate::tool::{
    Tool, ToolCall, ToolContext, ToolError, ToolKind, ToolOutput, ToolOutputOrigin, ToolResult,
};

/// How a turn body exits other than normally.
pub(super) enum Abort {
    /// §7.2: the turn fails and the session enters `Failed`.
    Fail {
        class: String,
        attempts: u32,
        message: String,
        middleware: Option<String>,
        hook: Option<String>,
    },
    /// §7.1: the turn token fired.
    Cancelled {
        phase: LoopPhase,
        /// The call whose `tool_call` was already logged when the token fired.
        in_flight: Option<String>,
        /// Every call not yet answered by a `tool_result` (the in-flight one first).
        unanswered: Vec<ToolCall>,
    },
    /// `run_turn` returns this error (e.g. the failure checkpoint could not be written).
    Fatal(KernelError),
}

impl Abort {
    fn fail(class: &str, message: impl Into<String>) -> Abort {
        Abort::Fail {
            class: class.to_owned(),
            attempts: 1,
            message: message.into(),
            middleware: None,
            hook: None,
        }
    }
}

impl From<LogError> for Abort {
    fn from(e: LogError) -> Self {
        Abort::fail("log", e.to_string())
    }
}

impl From<HashError> for Abort {
    fn from(e: HashError) -> Self {
        Abort::fail("internal", e.to_string())
    }
}

impl From<MiddlewareError> for Abort {
    fn from(e: MiddlewareError) -> Self {
        Abort::Fail {
            class: "middleware".to_owned(),
            attempts: 1,
            message: e.to_string(),
            middleware: Some(e.name.clone()),
            hook: Some(e.hook.to_owned()),
        }
    }
}

impl From<KernelError> for Abort {
    fn from(e: KernelError) -> Self {
        match e {
            KernelError::Log(l) => l.into(),
            KernelError::Hash(h) => h.into(),
            KernelError::Internal(m) => Abort::fail("internal", m),
            other => Abort::Fatal(other),
        }
    }
}

/// A `✂` check point with no tool calls outstanding.
pub(super) fn check_cancel(tt: &CancellationToken, phase: LoopPhase) -> Result<(), Abort> {
    if tt.is_cancelled() {
        Err(Abort::Cancelled {
            phase,
            in_flight: None,
            unanswered: Vec::new(),
        })
    } else {
        Ok(())
    }
}

/// `result_hash` input (`event-schema.md` §3.4).
#[derive(Serialize)]
struct HashedResult<'a> {
    content: &'a ToolResultContent,
    is_error: bool,
}

fn result_hash(out: &ToolOutput) -> Result<Hash, HashError> {
    Hash::of_canonical_json(&HashedResult {
        content: &out.content,
        is_error: out.is_error,
    })
}

fn error_output(msg: String, origin: ToolOutputOrigin) -> ToolOutput {
    ToolOutput {
        content: ToolResultContent::Json(json!({ "error": msg })),
        is_error: true,
        artifact_handles: Vec::new(),
        spilled: false,
        task: None,
        origin,
    }
}

fn cancelled_output() -> ToolOutput {
    ToolOutput {
        content: ToolResultContent::Json(json!({ "cancelled": true })),
        is_error: true,
        artifact_handles: Vec::new(),
        spilled: false,
        task: None,
        origin: ToolOutputOrigin::Cancelled,
    }
}

enum Invoked {
    Done(Result<ToolResult, ToolError>),
    Timeout(Duration),
    Cancelled,
}

impl Kernel {
    /// Exactly one turn (§6). Requires status `Running`, else `KernelError::NotRunning`.
    pub async fn run_turn(&mut self) -> Result<TurnOutcome, KernelError> {
        match self.status() {
            SessionStatus::Done => return Err(KernelError::Done),
            SessionStatus::Running => {}
            s => return Err(KernelError::NotRunning(s)),
        }
        self.state.turn += 1;
        self.sh.emitter.set_turn(self.state.turn);
        let snapshot = self.state.clone();
        let tt = {
            let mut c = lock(&self.sh.cancel);
            match &c.turn {
                Some(t) => t.clone(),
                None => {
                    let t = c.session.child_token();
                    c.turn = Some(t.clone());
                    t
                }
            }
        };
        let result = self.turn_body(&tt).await;
        {
            // A fresh token for the next turn (if any); a cancel that landed after the last
            // check point of this turn had no effect and logs nothing (§7.1).
            let mut c = lock(&self.sh.cancel);
            c.tools.clear();
            c.turn = (self.state.session_status == SessionStatus::Running)
                .then(|| c.session.child_token());
        }
        self.sh.emitter.allow_compaction(false);
        match result {
            Ok(outcome) => Ok(outcome),
            Err(Abort::Fatal(e)) => Err(e),
            Err(Abort::Fail {
                class,
                attempts,
                message,
                middleware,
                hook,
            }) => {
                self.fail_turn(snapshot, class, attempts, message, middleware, hook)
                    .await
            }
            Err(Abort::Cancelled {
                phase,
                in_flight,
                unanswered,
            }) => self.cancel_turn(phase, in_flight, unanswered).await,
        }
    }

    async fn turn_body(&mut self, tt: &CancellationToken) -> Result<TurnOutcome, Abort> {
        let turn = self.state.turn;
        check_cancel(tt, LoopPhase::BeforeModel)?;

        // A. Build the request.
        let mut req = ModelRequest {
            model_id: self.sh.model_id.clone(),
            system: self.sh.system_prompt.clone(),
            messages: self.state.messages.clone(),
            tools: self.sh.registry.definitions().to_vec(),
            params: self.sh.model_params.clone(),
            trace: RequestTrace {
                session_id: self.sh.session_id.clone(),
                turn,
                attempt: 1,
                checkpoint_hash: self.checkpoint_hash.clone(),
                request_id: new_request_id(),
            },
        };

        // B. before_model.
        self.sh.emitter.allow_compaction(true);
        for e in &self.sh.chain {
            check_cancel(tt, LoopPhase::BeforeModel)?;
            let cx = self.sh.hook_cx(&self.checkpoint_hash, turn, tt.clone());
            e.middleware
                .before_model(&mut self.state, &mut req, &cx)
                .await?;
            self.flush_emits().await?;
        }
        self.sh.emitter.allow_compaction(false);
        if let Some(strategy) = self.sh.emitter.take_compaction() {
            self.run_compaction(&strategy, tt, turn).await?;
            req.messages = self.state.messages.clone();
        }
        for t in &req.tools {
            if self.sh.registry.get(&t.name).is_none() {
                return Err(Abort::Fail {
                    class: "middleware".to_owned(),
                    attempts: 1,
                    message: format!("request names unregistered tool `{}`", t.name),
                    middleware: None,
                    hook: Some("before_model".to_owned()),
                });
            }
        }
        let request_hash = req.request_hash()?;
        self.log(EventBody::ModelRequest(ModelRequestPayload {
            turn,
            request_hash: request_hash.clone(),
            checkpoint_hash: self.checkpoint_hash.clone(),
            model_id: req.model_id.clone(),
            profiles: self.sh.profiles.clone(),
            system_prompt_hash: Hash::of_canonical_json(&req.system)?,
            prompt_blocks: req
                .system
                .iter()
                .map(|b| PromptBlockRef {
                    kind: b.kind,
                    name: b.name.clone(),
                    hash: b.hash.clone(),
                })
                .collect(),
            message_count: req.messages.len() as u32,
            tool_names: req.tools.iter().map(|t| t.name.clone()).collect(),
            params_hash: Hash::of_canonical_json(&req.params)?,
        }))
        .await?;

        // C. Model call with retry.
        let (mut resp, attempts) = match call_with_retry(&self.sh, turn, req, tt).await {
            Ok(r) => r,
            Err(CallFailure::Cancelled) => {
                return Err(Abort::Cancelled {
                    phase: LoopPhase::ModelCall,
                    in_flight: None,
                    unanswered: Vec::new(),
                });
            }
            Err(CallFailure::Failed { error, attempts }) => {
                let (message, _) = self.sh.redactor.redact_str(&error.to_string());
                return Err(Abort::Fail {
                    class: error.class().to_owned(),
                    attempts,
                    message,
                    middleware: None,
                    hook: None,
                });
            }
            Err(CallFailure::Log(e)) => return Err(e.into()),
        };
        self.sh
            .redact(&mut resp.content)
            .map_err(|e| Abort::fail("internal", format!("redaction: {e}")))?;
        check_cancel(tt, LoopPhase::AfterModel)?;

        // D. after_model.
        for e in &self.sh.chain {
            check_cancel(tt, LoopPhase::AfterModel)?;
            let cx = self.sh.hook_cx(&self.checkpoint_hash, turn, tt.clone());
            e.middleware
                .after_model(&mut self.state, &mut resp, &cx)
                .await?;
            self.flush_emits().await?;
        }
        self.log(EventBody::ModelResponse(ModelResponsePayload {
            turn,
            request_hash,
            response_hash: resp.response_hash()?,
            raw_response_hash: resp.raw_response_hash.clone(),
            model_id: resp.model_id.clone(),
            stop_reason: resp.stop_reason.clone(),
            usage: resp.usage.clone(),
            content: resp.content.clone(),
            attempts,
        }))
        .await?;
        self.state.messages.push(Message {
            role: Role::Assistant,
            content: resp.content.clone(),
        });
        let calls = resp.tool_calls();

        // E. Tool calls, in order.
        self.run_tool_calls(tt, turn, &calls).await?;

        // F. Turn boundary.
        let drained_user = self.drain_inbox_end_of_turn().await?;
        let next = if drained_user || !calls.is_empty() {
            SessionStatus::Running
        } else if self.state.open_tasks().next().is_some() {
            SessionStatus::Suspended
        } else {
            SessionStatus::Idle
        };
        self.set_status(next);
        self.checkpoint(CheckpointReason::TurnEnd)
            .await
            .map_err(Abort::Fatal)?;
        match next {
            SessionStatus::Suspended => {
                self.terminate_session_processes().await;
                let s: Suspension = self.suspension();
                self.log(EventBody::Suspended(SuspendedPayload {
                    turn,
                    reason: SuspendReason::PendingTasks,
                    pending_task_ids: s.pending_task_ids.clone(),
                    in_process_wakers: s.in_process_wakers as u32,
                    checkpoint_hash: s.checkpoint_hash.clone(),
                }))
                .await?;
                Ok(TurnOutcome::Suspended(s))
            }
            SessionStatus::Idle => Ok(TurnOutcome::Idle),
            _ => Ok(TurnOutcome::Continue),
        }
    }

    async fn run_tool_calls(
        &mut self,
        tt: &CancellationToken,
        turn: u64,
        calls: &[ToolCall],
    ) -> Result<(), Abort> {
        for (i, call) in calls.iter().enumerate() {
            let mut call = call.clone();
            let this_id = call.tool_use_id.clone();
            let cancelled = move |phase: LoopPhase, in_flight: bool| Abort::Cancelled {
                phase,
                in_flight: in_flight.then(|| this_id.clone()),
                unanswered: calls[i..].to_vec(),
            };
            // E1.
            if tt.is_cancelled() {
                return Err(cancelled(LoopPhase::BeforeTool, false));
            }
            // E2. Registry check.
            let Some((tool, kind, capabilities, policy, policy_hash)) =
                self.sh.registry.get(&call.name).map(|e| {
                    (
                        e.tool.clone(),
                        e.kind,
                        e.capabilities.clone(),
                        e.policy.clone(),
                        e.policy_hash.clone(),
                    )
                })
            else {
                let out = ToolOutput {
                    content: ToolResultContent::Json(json!({ "error": "unknown tool" })),
                    is_error: true,
                    artifact_handles: Vec::new(),
                    spilled: false,
                    task: None,
                    origin: ToolOutputOrigin::Unregistered,
                };
                self.log(EventBody::ToolCall(ToolCallPayload {
                    turn,
                    tool_use_id: call.tool_use_id.clone(),
                    name: call.name.clone(),
                    args_hash: call.args_hash()?,
                    request_hash: call.request_hash()?,
                    checkpoint_hash: self.checkpoint_hash.clone(),
                    input: call.input.clone(),
                    registered: false,
                    kind: None,
                    capabilities: Vec::new(),
                    policy_hash: None,
                }))
                .await?;
                self.log(EventBody::Warning(WarningPayload::kernel(
                    turn,
                    "unregistered_tool",
                    format!("model called unregistered tool `{}`", call.name),
                    Some(json!({ "tool_use_id": call.tool_use_id, "name": call.name })),
                )))
                .await?;
                self.append_tool_output(turn, &call, out, 0, None).await?;
                continue;
            };
            // E3. before_tool; first Replace wins.
            let mut flow = ToolFlow::Continue;
            for e in &self.sh.chain {
                if tt.is_cancelled() {
                    return Err(cancelled(LoopPhase::BeforeTool, false));
                }
                let cx = self.sh.hook_cx(&self.checkpoint_hash, turn, tt.clone());
                let f = e
                    .middleware
                    .before_tool(&mut self.state, &mut call, &cx)
                    .await?;
                self.flush_emits().await?;
                if let ToolFlow::Replace(r) = f {
                    flow = ToolFlow::Replace(r);
                    break;
                }
            }
            // E4.
            self.log(EventBody::ToolCall(ToolCallPayload {
                turn,
                tool_use_id: call.tool_use_id.clone(),
                name: call.name.clone(),
                args_hash: call.args_hash()?,
                request_hash: call.request_hash()?,
                checkpoint_hash: self.checkpoint_hash.clone(),
                input: call.input.clone(),
                registered: true,
                kind: Some(kind),
                capabilities,
                policy_hash: Some(policy_hash),
            }))
            .await?;
            let is_ask_user = call.name == ASK_USER_TOOL_NAME;
            let question_id = ask_user_question_id(turn, &call.tool_use_id);
            if is_ask_user {
                let q = &call.input;
                self.log(EventBody::AskUser(AskUserPayload {
                    turn,
                    tool_use_id: call.tool_use_id.clone(),
                    question_id: question_id.clone(),
                    question: q
                        .get("question")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    options: q
                        .get("options")
                        .and_then(Value::as_array)
                        .map(|a| {
                            a.iter()
                                .filter_map(Value::as_str)
                                .map(str::to_owned)
                                .collect()
                        })
                        .unwrap_or_default(),
                    allow_free_text: q
                        .get("allow_free_text")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                }))
                .await?;
            }
            // E5.
            let task_id = TaskId(format!("t{turn}-{}", call.tool_use_id));
            let started = Instant::now();
            let (result, origin) = match flow {
                ToolFlow::Replace(r) => (r, ToolOutputOrigin::Middleware),
                ToolFlow::Continue => {
                    let tool_token = tt.child_token();
                    lock(&self.sh.cancel)
                        .tools
                        .insert(call.tool_use_id.clone(), tool_token.clone());
                    let invoked = self
                        .invoke_tool(tool, kind, &policy, tool_token.clone(), turn, &call)
                        .await;
                    lock(&self.sh.cancel).tools.remove(&call.tool_use_id);
                    match invoked {
                        Invoked::Done(r) => (r, ToolOutputOrigin::Invoke),
                        Invoked::Timeout(d) => {
                            (Err(ToolError::Timeout(d)), ToolOutputOrigin::Timeout)
                        }
                        Invoked::Cancelled => {
                            self.sh.registrar.abort(&task_id);
                            if tt.is_cancelled() {
                                return Err(cancelled(LoopPhase::Tool, true));
                            }
                            (Err(ToolError::Cancelled), ToolOutputOrigin::Cancelled)
                        }
                    }
                }
            };
            let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            // E6. Normalize.
            let mut out = match result {
                Ok(ToolResult::Value(v)) => match Hash::of_canonical_json(&v) {
                    Ok(_) => ToolOutput {
                        content: ToolResultContent::Json(v),
                        is_error: false,
                        artifact_handles: Vec::new(),
                        spilled: false,
                        task: None,
                        origin,
                    },
                    Err(e) => error_output(
                        ToolError::InvalidInput(format!("result is not canonicalizable: {e}"))
                            .to_string(),
                        origin,
                    ),
                },
                Ok(ToolResult::Blocks(blocks)) => {
                    let content = ToolResultContent::Blocks(blocks);
                    if content.is_valid() && Hash::of_canonical_json(&content).is_ok() {
                        let handles = match &content {
                            ToolResultContent::Blocks(b) => b
                                .iter()
                                .filter_map(|b| match b {
                                    ContentBlock::Image {
                                        artifact_handle, ..
                                    } => Some(artifact_handle.clone()),
                                    _ => None,
                                })
                                .collect(),
                            ToolResultContent::Json(_) => Vec::new(),
                        };
                        ToolOutput {
                            content,
                            is_error: false,
                            artifact_handles: handles,
                            spilled: false,
                            task: None,
                            origin,
                        }
                    } else {
                        error_output(
                            ToolError::InvalidInput(
                                "tool result blocks must be Text or Image".to_owned(),
                            )
                            .to_string(),
                            origin,
                        )
                    }
                }
                Ok(ToolResult::Task(h)) => {
                    if !matches!(h.status, TaskStatus::Pending | TaskStatus::Running) {
                        error_output(
                            ToolError::InvalidTaskHandle(format!(
                                "status {:?} is terminal",
                                h.status
                            ))
                            .to_string(),
                            origin,
                        )
                    } else if h.id != task_id {
                        error_output(
                            ToolError::InvalidTaskHandle(format!(
                                "id {} does not match {task_id}",
                                h.id
                            ))
                            .to_string(),
                            origin,
                        )
                    } else {
                        ToolOutput {
                            content: ToolResultContent::Json(json!({
                                "task_id": h.id,
                                "status": h.status,
                                "eta_secs": h.eta.map(|d| d.as_secs()),
                                "description": h.description,
                            })),
                            is_error: false,
                            artifact_handles: Vec::new(),
                            spilled: false,
                            task: Some(h),
                            origin,
                        }
                    }
                }
                Err(ToolError::Cancelled) => {
                    self.sh.registrar.abort(&task_id);
                    if tt.is_cancelled() {
                        return Err(cancelled(LoopPhase::Tool, true));
                    }
                    let mut out = cancelled_output();
                    out.origin = ToolOutputOrigin::Cancelled;
                    out
                }
                Err(ToolError::ReplayMiss { tool, request_hash }) => {
                    return Err(Abort::fail(
                        "replay_miss",
                        format!("replay miss for {tool} ({request_hash})"),
                    ));
                }
                Err(e) => error_output(e.to_string(), origin),
            };
            if out.origin == ToolOutputOrigin::Cancelled {
                // A tool-scope cancel (or a tool that reported cancellation): one `cancelled` event.
                self.log(EventBody::Cancelled(CancelledPayload {
                    turn,
                    scope: CancelScopeKind::Tool,
                    tool_use_id: Some(call.tool_use_id.clone()),
                    task_id: None,
                    phase: LoopPhase::Tool,
                    signalled: false,
                    skipped_tool_use_ids: Vec::new(),
                }))
                .await?;
            }
            if out.task.is_none() {
                self.sh.registrar.abort(&task_id);
            }
            // Ingress redaction (§7.5) before hashing, spill, and `after_tool`.
            self.sh
                .redact(&mut out)
                .map_err(|e| Abort::fail("internal", format!("redaction: {e}")))?;
            if is_ask_user {
                let (answer, declined) = match &out.content {
                    ToolResultContent::Json(v) if !out.is_error => (
                        v.get("answer").and_then(Value::as_str).map(str::to_owned),
                        v.get("declined").and_then(Value::as_bool).unwrap_or(false),
                    ),
                    _ => (None, true),
                };
                self.log(EventBody::UserAnswer(UserAnswerPayload {
                    turn,
                    tool_use_id: call.tool_use_id.clone(),
                    question_id,
                    declined: declined || answer.is_none(),
                    answer,
                }))
                .await?;
            }
            // Spill (§7.4).
            let (spill_ref, warnings) =
                spill::apply(&mut out, &self.sh.spill, &*self.sh.artifacts, turn).await;
            for w in warnings {
                self.log(EventBody::Warning(w)).await?;
            }
            // E7. after_tool.
            for e in &self.sh.chain {
                if tt.is_cancelled() {
                    return Err(cancelled(LoopPhase::AfterTool, true));
                }
                let cx = self.sh.hook_cx(&self.checkpoint_hash, turn, tt.clone());
                e.middleware
                    .after_tool(&mut self.state, &call, &mut out, &cx)
                    .await?;
                self.flush_emits().await?;
            }
            // E8–E10.
            self.append_tool_output(turn, &call, out, duration_ms, spill_ref)
                .await?;
        }
        Ok(())
    }

    /// E5: launch the session process if needed, race `invoke` against the tool token and the
    /// policy timeout.
    async fn invoke_tool(
        &mut self,
        tool: Arc<dyn Tool>,
        kind: ToolKind,
        policy: &SandboxPolicy,
        token: CancellationToken,
        turn: u64,
        call: &ToolCall,
    ) -> Invoked {
        if kind == ToolKind::Session
            && let Err(e) = self.ensure_session_process(&tool, policy).await
        {
            return Invoked::Done(Err(e));
        }
        let session = self.session_procs.get(&call.name).map(|p| &**p);
        let ctx = ToolContext::new(
            &*self.sh.host,
            token.clone(),
            &self.sh.session_id,
            turn,
            &call.tool_use_id,
            policy,
            &*self.sh.sandbox,
            &*self.sh.artifacts,
            session,
            &*self.sh.registrar,
        );
        tokio::select! {
            biased;
            _ = token.cancelled() => Invoked::Cancelled,
            r = tokio::time::timeout(policy.timeout, tool.invoke(&ctx, call.input.clone())) => {
                match r {
                    Ok(r) => Invoked::Done(r),
                    Err(_) => Invoked::Timeout(policy.timeout),
                }
            }
        }
    }

    /// §7.9: lazily launch; relaunch once if the process died; a second failure is `Failed`.
    async fn ensure_session_process(
        &mut self,
        tool: &Arc<dyn Tool>,
        policy: &SandboxPolicy,
    ) -> Result<(), ToolError> {
        let name = tool.name().to_owned();
        if let Some(p) = self.session_procs.get(&name) {
            if p.is_alive() {
                return Ok(());
            }
            if let Some(dead) = self.session_procs.remove(&name) {
                let _ = dead.terminate().await;
            }
        }
        let cmd = tool
            .session_command()
            .ok_or_else(|| ToolError::Failed("session tool has no session_command".to_owned()))?;
        let p = self
            .sh
            .sandbox
            .launch_session(policy, cmd)
            .await
            .map_err(|e| ToolError::Failed(format!("session process launch failed: {e}")))?;
        if !p.is_alive() {
            let _ = p.terminate().await;
            return Err(ToolError::Failed(
                "session process exited immediately after relaunch".to_owned(),
            ));
        }
        self.session_procs.insert(name, p);
        Ok(())
    }

    /// E8–E10: log `tool_result`, append the `Tool` message, register a started task.
    async fn append_tool_output(
        &mut self,
        turn: u64,
        call: &ToolCall,
        out: ToolOutput,
        duration_ms: u64,
        spill_ref: Option<SpillRef>,
    ) -> Result<(), Abort> {
        self.log(EventBody::ToolResult(ToolResultPayload {
            turn,
            tool_use_id: call.tool_use_id.clone(),
            name: call.name.clone(),
            result_hash: result_hash(&out)?,
            is_error: out.is_error,
            content: out.content.clone(),
            artifact_handles: out.artifact_handles.clone(),
            spilled: out.spilled,
            spill: spill_ref,
            duration_ms,
            origin: out.origin,
            task: out.task.as_ref().map(|h| TaskRef {
                task_id: h.id.clone(),
                status: h.status,
            }),
        }))
        .await?;
        self.state.messages.push(Message {
            role: Role::Tool,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: call.tool_use_id.clone(),
                content: out.content,
                is_error: out.is_error,
            }],
        });
        if let Some(h) = out.task {
            let in_process_waker = self.sh.registrar.was_watched(&h.id);
            self.state.pending_tasks.insert(
                h.id.clone(),
                Task {
                    id: h.id.clone(),
                    tool_use_id: call.tool_use_id.clone(),
                    tool_name: call.name.clone(),
                    status: h.status,
                    started_turn: turn,
                    completed_turn: None,
                    eta: h.eta,
                    check_hint: h.check_hint.clone(),
                    description: h.description.clone(),
                    outcome: None,
                    in_process_waker,
                },
            );
            self.log(EventBody::TaskStarted(TaskStartedPayload {
                turn,
                task_id: h.id,
                tool_use_id: call.tool_use_id.clone(),
                tool_name: call.name.clone(),
                status: h.status,
                eta_secs: h.eta.map(|d| d.as_secs()),
                description: h.description,
                check_hint: h.check_hint,
                in_process_waker,
            }))
            .await?;
        }
        Ok(())
    }

    /// §7.1: state after `cancel(Turn)`.
    async fn cancel_turn(
        &mut self,
        phase: LoopPhase,
        in_flight: Option<String>,
        unanswered: Vec<ToolCall>,
    ) -> Result<TurnOutcome, KernelError> {
        let turn = self.state.turn;
        let skipped: Vec<String> = unanswered
            .iter()
            .map(|c| c.tool_use_id.clone())
            .filter(|id| Some(id) != in_flight.as_ref())
            .collect();
        self.log(EventBody::Cancelled(CancelledPayload {
            turn,
            scope: CancelScopeKind::Turn,
            tool_use_id: in_flight,
            task_id: None,
            phase,
            signalled: false,
            skipped_tool_use_ids: skipped,
        }))
        .await?;
        for call in &unanswered {
            self.sh
                .registrar
                .abort(&TaskId(format!("t{turn}-{}", call.tool_use_id)));
            self.append_tool_output(turn, call, cancelled_output(), 0, None)
                .await
                .map_err(abort_to_kernel)?;
        }
        self.drain_inbox_end_of_turn().await?;
        self.set_status(SessionStatus::Idle);
        self.checkpoint(CheckpointReason::Cancel).await?;
        Ok(TurnOutcome::Cancelled)
    }

    /// §7.2: discard the partial turn, drain the inbox, `Failed`, `checkpoint{failure}`, `session_failed`.
    async fn fail_turn(
        &mut self,
        snapshot: State,
        class: String,
        attempts: u32,
        message: String,
        middleware: Option<String>,
        hook: Option<String>,
    ) -> Result<TurnOutcome, KernelError> {
        let turn = self.state.turn;
        // Wakers registered by tasks that started in the discarded turn die with it.
        let started_here: Vec<TaskId> = self
            .state
            .pending_tasks
            .keys()
            .filter(|id| !snapshot.pending_tasks.contains_key(*id))
            .cloned()
            .collect();
        for id in &started_here {
            self.sh.registrar.abort(id);
        }
        self.state = snapshot;
        self.terminate_session_processes().await;
        let (message, _) = self.sh.redactor.redact_str(&message);
        let tf = self
            .log(EventBody::TurnFailed(TurnFailedPayload {
                turn,
                error_class: class.clone(),
                attempts,
                message,
                middleware,
                hook,
            }))
            .await?;
        self.drain_inbox_end_of_turn().await?;
        self.set_status(SessionStatus::Failed);
        self.checkpoint(CheckpointReason::Failure).await?;
        self.log(EventBody::SessionFailed(SessionFailedPayload {
            turn,
            cause_seq: tf.seq,
            error_class: class.clone(),
            checkpoint_hash: self.checkpoint_hash.clone(),
            resumable: true,
        }))
        .await?;
        self.last_error_class = Some(class.clone());
        Ok(TurnOutcome::Failed { error_class: class })
    }
}

fn abort_to_kernel(a: Abort) -> KernelError {
    match a {
        Abort::Fatal(e) => e,
        Abort::Fail { message, .. } => KernelError::Internal(message),
        Abort::Cancelled { .. } => KernelError::Internal("cancelled".to_owned()),
    }
}
