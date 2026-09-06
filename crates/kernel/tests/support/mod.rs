//! Fakes for the P1.2 loop tests: provider, tools, host, sandbox, middleware, and a kernel builder.
#![allow(dead_code, clippy::type_complexity)]

use std::collections::{BTreeMap, VecDeque};
use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use futures_core::Stream;
use kernel::loop_::{Kernel, KernelConfig, KernelHandle, SessionInit};
use kernel::*;
use serde_json::{Value, json};
use tokio::sync::oneshot;

pub fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

pub fn h(byte: &str) -> Hash {
    Hash::parse(&format!("b3:{}", byte.repeat(32))).unwrap()
}

pub fn profiles() -> ActiveProfiles {
    ActiveProfiles {
        model_profile_hash: h("1a"),
        agent_profile_hash: h("2b"),
        resolved_profile_hash: h("3c"),
        project_profile_hash: None,
        bundles_hash: None,
    }
}

// ---- provider ------------------------------------------------------------------------------

pub fn text_response(text: &str) -> ModelResponse {
    ModelResponse {
        content: vec![ContentBlock::Text {
            text: text.to_owned(),
        }],
        stop_reason: StopReason::EndTurn,
        usage: Usage {
            input_tokens: 10,
            output_tokens: 5,
            cache_read_tokens: None,
            reasoning_tokens: None,
        },
        model_id: "fake-model".to_owned(),
        raw_response_hash: Hash::of_bytes(text.as_bytes()),
        response_id: None,
    }
}

pub fn tool_use_response(calls: Vec<(&str, &str, Value)>) -> ModelResponse {
    let mut content = vec![ContentBlock::Text {
        text: "calling tools".to_owned(),
    }];
    for (id, name, input) in calls {
        content.push(ContentBlock::ToolUse {
            id: id.to_owned(),
            name: name.to_owned(),
            input,
        });
    }
    ModelResponse {
        content,
        stop_reason: StopReason::ToolUse,
        usage: Usage::default(),
        model_id: "fake-model".to_owned(),
        raw_response_hash: Hash::of_bytes(b"tool-use"),
        response_id: None,
    }
}

/// Scripted provider: pops one `Result` per call; an empty script yields a plain "done" text.
pub struct FakeProvider {
    script: Mutex<VecDeque<Result<ModelResponse, ProviderError>>>,
    pub requests: Mutex<Vec<ModelRequest>>,
    pub calls: AtomicUsize,
    /// When set, `complete_stream` emits a text delta per block before `Complete`.
    pub stream_deltas: bool,
}

impl FakeProvider {
    pub fn new(script: Vec<Result<ModelResponse, ProviderError>>) -> Arc<FakeProvider> {
        Arc::new(FakeProvider {
            script: Mutex::new(script.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
            stream_deltas: false,
        })
    }

    pub fn streaming(script: Vec<Result<ModelResponse, ProviderError>>) -> Arc<FakeProvider> {
        Arc::new(FakeProvider {
            script: Mutex::new(script.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
            stream_deltas: true,
        })
    }

    pub fn responses(script: Vec<ModelResponse>) -> Arc<FakeProvider> {
        Self::new(script.into_iter().map(Ok).collect())
    }

    pub fn push(&self, r: Result<ModelResponse, ProviderError>) {
        lock(&self.script).push_back(r);
    }

    pub fn requests(&self) -> Vec<ModelRequest> {
        lock(&self.requests).clone()
    }

    fn next(&self, req: ModelRequest) -> Result<ModelResponse, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        lock(&self.requests).push(req);
        lock(&self.script)
            .pop_front()
            .unwrap_or_else(|| Ok(text_response("done")))
    }
}

pub struct VecStream(VecDeque<Result<ModelDelta, ProviderError>>);

impl Stream for VecStream {
    type Item = Result<ModelDelta, ProviderError>;
    fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(self.0.pop_front())
    }
}

#[async_trait]
impl Provider for FakeProvider {
    fn name(&self) -> &str {
        "fake"
    }

    async fn complete(&self, req: ModelRequest) -> Result<ModelResponse, ProviderError> {
        self.next(req)
    }

    async fn complete_stream(&self, req: ModelRequest) -> Result<DeltaStream, ProviderError> {
        let resp = self.next(req)?;
        let mut items = VecDeque::new();
        if self.stream_deltas {
            for (i, b) in resp.content.iter().enumerate() {
                if let ContentBlock::Text { text } = b {
                    items.push_back(Ok(ModelDelta::TextDelta {
                        index: i as u32,
                        text: text.clone(),
                    }));
                }
            }
        }
        items.push_back(Ok(ModelDelta::Complete(resp)));
        Ok(Box::pin(VecStream(items)))
    }
}

/// A provider that never responds until cancelled.
pub struct HangingProvider;

#[async_trait]
impl Provider for HangingProvider {
    fn name(&self) -> &str {
        "hanging"
    }
    async fn complete(&self, _req: ModelRequest) -> Result<ModelResponse, ProviderError> {
        std::future::pending::<()>().await;
        unreachable!()
    }
}

// ---- host ----------------------------------------------------------------------------------

pub struct FakeHost {
    pub answers: Mutex<VecDeque<UserAnswer>>,
    pub asked: Mutex<Vec<AskUserRequest>>,
}

impl FakeHost {
    pub fn new() -> Arc<FakeHost> {
        Arc::new(FakeHost {
            answers: Mutex::new(VecDeque::new()),
            asked: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait]
impl Host for FakeHost {
    fn name(&self) -> &str {
        "fake"
    }
    async fn read_file(&self, _p: &FsPolicy, path: &Path) -> Result<Vec<u8>, HostError> {
        Err(HostError::NotFound(path.display().to_string()))
    }
    async fn write_file(&self, _p: &FsPolicy, _path: &Path, _b: &[u8]) -> Result<(), HostError> {
        Ok(())
    }
    async fn list_dir(&self, _p: &FsPolicy, _path: &Path) -> Result<Vec<DirEntry>, HostError> {
        Ok(Vec::new())
    }
    async fn stat(&self, _p: &FsPolicy, path: &Path) -> Result<Metadata, HostError> {
        Err(HostError::NotFound(path.display().to_string()))
    }
    async fn remove(&self, _p: &FsPolicy, _path: &Path) -> Result<(), HostError> {
        Ok(())
    }
    async fn spawn(
        &self,
        _p: &ProcPolicy,
        _cmd: Command,
    ) -> Result<Box<dyn ChildProcess>, HostError> {
        Err(HostError::Io("no spawn in fake host".into()))
    }
    fn network(&self, _p: &NetPolicy) -> Result<Arc<dyn NetHandle>, HostError> {
        Err(HostError::Net("no network in fake host".into()))
    }
    fn secret(&self, name: &str) -> Result<SecretHandle, HostError> {
        Ok(SecretHandle::new(name))
    }
    async fn ask_user(&self, req: AskUserRequest) -> Result<UserAnswer, HostError> {
        let qid = req.question_id.clone();
        lock(&self.asked).push(req);
        Ok(lock(&self.answers).pop_front().unwrap_or(UserAnswer {
            question_id: qid,
            answer: Some("yes".to_owned()),
        }))
    }
}

// ---- sandbox -------------------------------------------------------------------------------

pub struct FakeSessionProcess {
    pub alive: Arc<AtomicBool>,
    pub calls: Arc<AtomicUsize>,
    pub terminated: Arc<AtomicBool>,
}

#[async_trait]
impl SessionProcess for FakeSessionProcess {
    async fn call(
        &self,
        req: RpcRequest,
        _cancel: CancellationToken,
    ) -> Result<RpcResponse, SandboxError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(RpcResponse::Result(json!({ "echo": req.params })))
    }
    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }
    async fn terminate(&self) -> Result<(), SandboxError> {
        self.terminated.store(true, Ordering::SeqCst);
        self.alive.store(false, Ordering::SeqCst);
        Ok(())
    }
}

pub struct FakeSandbox {
    pub backend_name: &'static str,
    pub launches: Mutex<Vec<(SandboxPolicy, Command)>>,
    /// Shared with the processes it launches so tests can kill them.
    pub alive_flags: Mutex<Vec<Arc<AtomicBool>>>,
    pub terminated_flags: Mutex<Vec<Arc<AtomicBool>>>,
}

impl FakeSandbox {
    pub fn new() -> Arc<FakeSandbox> {
        Self::named("fake")
    }
    pub fn named(name: &'static str) -> Arc<FakeSandbox> {
        Arc::new(FakeSandbox {
            backend_name: name,
            launches: Mutex::new(Vec::new()),
            alive_flags: Mutex::new(Vec::new()),
            terminated_flags: Mutex::new(Vec::new()),
        })
    }
    pub fn launch_count(&self) -> usize {
        lock(&self.launches).len()
    }
}

#[async_trait]
impl SandboxBackend for FakeSandbox {
    fn name(&self) -> &'static str {
        self.backend_name
    }
    async fn launch_stateless(
        &self,
        _policy: &SandboxPolicy,
        _cmd: Command,
        _cancel: CancellationToken,
    ) -> Result<ProcessOutput, SandboxError> {
        Err(SandboxError::Launch("not supported in fake".into()))
    }
    async fn launch_session(
        &self,
        policy: &SandboxPolicy,
        cmd: Command,
    ) -> Result<Box<dyn SessionProcess>, SandboxError> {
        lock(&self.launches).push((policy.clone(), cmd));
        let alive = Arc::new(AtomicBool::new(true));
        let terminated = Arc::new(AtomicBool::new(false));
        lock(&self.alive_flags).push(alive.clone());
        lock(&self.terminated_flags).push(terminated.clone());
        Ok(Box::new(FakeSessionProcess {
            alive,
            calls: Arc::new(AtomicUsize::new(0)),
            terminated,
        }))
    }
}

// ---- artifact store ------------------------------------------------------------------------

pub struct FailingArtifactStore;

#[async_trait]
impl ArtifactStore for FailingArtifactStore {
    fn name(&self) -> &str {
        "failing"
    }
    async fn put(&self, _bytes: &[u8], _mime: &str) -> Result<ArtifactHandle, ArtifactError> {
        Err(ArtifactError::ReadOnly)
    }
    async fn get(&self, h: &ArtifactHandle) -> Result<Vec<u8>, ArtifactError> {
        Err(ArtifactError::NotFound(h.clone()))
    }
    async fn get_range(
        &self,
        h: &ArtifactHandle,
        _r: std::ops::Range<u64>,
    ) -> Result<Vec<u8>, ArtifactError> {
        Err(ArtifactError::NotFound(h.clone()))
    }
    async fn head(&self, h: &ArtifactHandle, _n: u64) -> Result<Vec<u8>, ArtifactError> {
        Err(ArtifactError::NotFound(h.clone()))
    }
    async fn tail(&self, h: &ArtifactHandle, _n: u64) -> Result<Vec<u8>, ArtifactError> {
        Err(ArtifactError::NotFound(h.clone()))
    }
    async fn stat(&self, h: &ArtifactHandle) -> Result<ArtifactMeta, ArtifactError> {
        Err(ArtifactError::NotFound(h.clone()))
    }
}

// ---- tools ---------------------------------------------------------------------------------

/// Returns a fixed value (or a scripted error) and records its inputs.
pub struct ValueTool {
    pub name: String,
    pub result: Mutex<Option<Result<ToolResult, ToolError>>>,
    pub default: Value,
    pub inputs: Mutex<Vec<Value>>,
    pub capabilities: Vec<Capability>,
}

impl ValueTool {
    pub fn new(name: &str, value: Value) -> Arc<ValueTool> {
        Arc::new(ValueTool {
            name: name.to_owned(),
            result: Mutex::new(None),
            default: value,
            inputs: Mutex::new(Vec::new()),
            capabilities: Vec::new(),
        })
    }
    pub fn with_result(name: &str, r: Result<ToolResult, ToolError>) -> Arc<ValueTool> {
        Arc::new(ValueTool {
            name: name.to_owned(),
            result: Mutex::new(Some(r)),
            default: Value::Null,
            inputs: Mutex::new(Vec::new()),
            capabilities: Vec::new(),
        })
    }
    pub fn with_caps(name: &str, caps: Vec<Capability>) -> Arc<ValueTool> {
        Arc::new(ValueTool {
            name: name.to_owned(),
            result: Mutex::new(None),
            default: json!({"ok": true}),
            inputs: Mutex::new(Vec::new()),
            capabilities: caps,
        })
    }
    pub fn call_count(&self) -> usize {
        lock(&self.inputs).len()
    }
}

#[async_trait]
impl Tool for ValueTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        "fake value tool"
    }
    fn schema(&self) -> Value {
        json!({"type": "object"})
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Stateless
    }
    fn capabilities(&self) -> Vec<Capability> {
        self.capabilities.clone()
    }
    async fn invoke(&self, _ctx: &ToolContext<'_>, input: Value) -> Result<ToolResult, ToolError> {
        lock(&self.inputs).push(input);
        match lock(&self.result).take() {
            Some(r) => r,
            None => Ok(ToolResult::Value(self.default.clone())),
        }
    }
}

/// Starts a task; the test completes it through a `oneshot` completer. `external == true` skips
/// `watch_task` (the waker is elsewhere).
pub struct TaskTool {
    pub name: String,
    pub completers: Mutex<Vec<oneshot::Sender<TaskOutcome>>>,
    pub status: TaskStatus,
    pub external: bool,
    pub bad_id: bool,
}

impl TaskTool {
    pub fn new(name: &str) -> Arc<TaskTool> {
        Arc::new(TaskTool {
            name: name.to_owned(),
            completers: Mutex::new(Vec::new()),
            status: TaskStatus::Running,
            external: false,
            bad_id: false,
        })
    }
    pub fn external(name: &str) -> Arc<TaskTool> {
        Arc::new(TaskTool {
            name: name.to_owned(),
            completers: Mutex::new(Vec::new()),
            status: TaskStatus::Pending,
            external: true,
            bad_id: false,
        })
    }
    pub fn complete(&self, outcome: TaskOutcome) {
        let tx = lock(&self.completers).remove(0);
        let _ = tx.send(outcome);
    }
    pub fn pending_count(&self) -> usize {
        lock(&self.completers).len()
    }
}

#[async_trait]
impl Tool for TaskTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        "fake task tool"
    }
    fn schema(&self) -> Value {
        json!({"type": "object"})
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Stateless
    }
    fn capabilities(&self) -> Vec<Capability> {
        Vec::new()
    }
    async fn invoke(&self, ctx: &ToolContext<'_>, _input: Value) -> Result<ToolResult, ToolError> {
        let id = if self.bad_id {
            TaskId("t99-bogus".into())
        } else {
            ctx.task_id()
        };
        if !self.external {
            let (tx, rx) = oneshot::channel::<TaskOutcome>();
            lock(&self.completers).push(tx);
            ctx.watch_task(
                id.clone(),
                Box::pin(async move {
                    rx.await.unwrap_or(TaskOutcome {
                        content: ToolResultContent::Json(json!({"error": "completer dropped"})),
                        is_error: true,
                        artifact_handles: vec![],
                    })
                }),
            )?;
        }
        Ok(ToolResult::Task(TaskHandle {
            id,
            status: self.status,
            eta: Some(Duration::from_secs(60)),
            check_hint: Some(json!({"pid": 4242, "secret_hint": true})),
            description: Some("fake job".to_owned()),
        }))
    }
}

/// On invoke, performs a handle action (cancel / enqueue) then waits for its own token.
pub struct HandleTool {
    pub name: String,
    pub handle: Mutex<Option<KernelHandle>>,
    pub action: Mutex<Option<Box<dyn Fn(&KernelHandle) + Send + Sync>>>,
    /// If true, return `Ok` right after the action instead of waiting for cancellation.
    pub return_immediately: bool,
}

impl HandleTool {
    pub fn new(
        name: &str,
        return_immediately: bool,
        action: impl Fn(&KernelHandle) + Send + Sync + 'static,
    ) -> Arc<HandleTool> {
        Arc::new(HandleTool {
            name: name.to_owned(),
            handle: Mutex::new(None),
            action: Mutex::new(Some(Box::new(action))),
            return_immediately,
        })
    }
    pub fn attach(&self, handle: KernelHandle) {
        *lock(&self.handle) = Some(handle);
    }
}

#[async_trait]
impl Tool for HandleTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        "fake handle tool"
    }
    fn schema(&self) -> Value {
        json!({"type": "object"})
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Stateless
    }
    fn capabilities(&self) -> Vec<Capability> {
        Vec::new()
    }
    async fn invoke(&self, ctx: &ToolContext<'_>, _input: Value) -> Result<ToolResult, ToolError> {
        let handle = lock(&self.handle).clone().expect("handle attached");
        if let Some(action) = lock(&self.action).as_ref() {
            action(&handle);
        }
        if self.return_immediately {
            return Ok(ToolResult::Value(json!({"acted": true})));
        }
        ctx.cancel.cancelled().await;
        Err(ToolError::Cancelled)
    }
}

/// Never returns (until cancelled); does not observe the token cooperatively.
pub struct SleepTool;

#[async_trait]
impl Tool for SleepTool {
    fn name(&self) -> &str {
        "sleep"
    }
    fn description(&self) -> &str {
        "sleeps forever"
    }
    fn schema(&self) -> Value {
        json!({"type": "object"})
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Stateless
    }
    fn capabilities(&self) -> Vec<Capability> {
        Vec::new()
    }
    async fn invoke(&self, _ctx: &ToolContext<'_>, _input: Value) -> Result<ToolResult, ToolError> {
        tokio::time::sleep(Duration::from_secs(3600)).await;
        Ok(ToolResult::Value(json!({"slept": true})))
    }
}

/// A `Session`-kind tool that calls into its session process.
pub struct SessionTool;

#[async_trait]
impl Tool for SessionTool {
    fn name(&self) -> &str {
        "repl"
    }
    fn description(&self) -> &str {
        "fake session tool"
    }
    fn schema(&self) -> Value {
        json!({"type": "object"})
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Session
    }
    fn capabilities(&self) -> Vec<Capability> {
        Vec::new()
    }
    fn session_command(&self) -> Option<Command> {
        Some(Command::new("fake-repl"))
    }
    async fn invoke(&self, ctx: &ToolContext<'_>, input: Value) -> Result<ToolResult, ToolError> {
        let p = ctx.session_process()?;
        let r = p
            .call(
                RpcRequest {
                    method: "eval".into(),
                    params: input,
                },
                ctx.cancel.clone(),
            )
            .await
            .map_err(|e| ToolError::Failed(e.to_string()))?;
        match r {
            RpcResponse::Result(v) => Ok(ToolResult::Value(v)),
            RpcResponse::Error { message, .. } => Err(ToolError::Failed(message)),
        }
    }
}

/// A `Session` tool that forgot its `session_command`.
pub struct BadSessionTool;

#[async_trait]
impl Tool for BadSessionTool {
    fn name(&self) -> &str {
        "bad_repl"
    }
    fn description(&self) -> &str {
        "x"
    }
    fn schema(&self) -> Value {
        json!({})
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Session
    }
    fn capabilities(&self) -> Vec<Capability> {
        Vec::new()
    }
    async fn invoke(&self, _ctx: &ToolContext<'_>, _i: Value) -> Result<ToolResult, ToolError> {
        Err(ToolError::Failed("unreachable".into()))
    }
}

/// The `ask_user` convention of `kernel::loop_` (§7.10).
pub struct AskUserTool;

#[async_trait]
impl Tool for AskUserTool {
    fn name(&self) -> &str {
        "ask_user"
    }
    fn description(&self) -> &str {
        "ask the user"
    }
    fn schema(&self) -> Value {
        json!({"type": "object"})
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Stateless
    }
    fn capabilities(&self) -> Vec<Capability> {
        Vec::new()
    }
    async fn invoke(&self, ctx: &ToolContext<'_>, input: Value) -> Result<ToolResult, ToolError> {
        let req = AskUserRequest {
            question_id: kernel::loop_::ask_user_question_id(ctx.turn, ctx.tool_use_id),
            question: input["question"].as_str().unwrap_or("").to_owned(),
            options: input["options"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default(),
            allow_free_text: input["allow_free_text"].as_bool().unwrap_or(false),
        };
        let a = ctx
            .host
            .ask_user(req)
            .await
            .map_err(|e| ToolError::Failed(e.to_string()))?;
        Ok(ToolResult::Value(json!({
            "question_id": a.question_id,
            "answer": a.answer,
            "declined": a.answer.is_none(),
        })))
    }
}

// ---- middleware ----------------------------------------------------------------------------

pub type HookLog = Arc<Mutex<Vec<String>>>;

/// Records `"<name>:<hook>"` in a shared log.
pub struct RecordingMiddleware {
    pub name: String,
    pub log: HookLog,
}

impl RecordingMiddleware {
    pub fn entry(name: &str, priority: i32, log: &HookLog) -> MiddlewareEntry {
        MiddlewareEntry {
            name: name.to_owned(),
            priority,
            source: MiddlewareSource::Agent,
            config_hash: None,
            middleware: Arc::new(RecordingMiddleware {
                name: name.to_owned(),
                log: log.clone(),
            }),
        }
    }
    fn rec(&self, hook: &str) {
        lock(&self.log).push(format!("{}:{hook}", self.name));
    }
}

#[async_trait]
impl Middleware for RecordingMiddleware {
    async fn before_model(
        &self,
        _s: &mut State,
        _r: &mut ModelRequest,
        _cx: &HookContext<'_>,
    ) -> Result<(), MiddlewareError> {
        self.rec("before_model");
        Ok(())
    }
    async fn after_model(
        &self,
        _s: &mut State,
        _r: &mut ModelResponse,
        _cx: &HookContext<'_>,
    ) -> Result<(), MiddlewareError> {
        self.rec("after_model");
        Ok(())
    }
    async fn before_tool(
        &self,
        _s: &mut State,
        _c: &mut ToolCall,
        _cx: &HookContext<'_>,
    ) -> Result<ToolFlow, MiddlewareError> {
        self.rec("before_tool");
        Ok(ToolFlow::Continue)
    }
    async fn after_tool(
        &self,
        _s: &mut State,
        _c: &ToolCall,
        _o: &mut ToolOutput,
        _cx: &HookContext<'_>,
    ) -> Result<(), MiddlewareError> {
        self.rec("after_tool");
        Ok(())
    }
    async fn on_compact(
        &self,
        _s: &mut State,
        _cx: &HookContext<'_>,
    ) -> Result<(), MiddlewareError> {
        self.rec("on_compact");
        Ok(())
    }
    async fn on_resume(
        &self,
        _s: &mut State,
        _c: &ResumeCause,
        _cx: &HookContext<'_>,
    ) -> Result<(), MiddlewareError> {
        self.rec("on_resume");
        Ok(())
    }
}

/// A middleware driven by closures for one-off behaviors.
pub struct FnMiddleware {
    pub before_model: Option<
        Box<
            dyn Fn(&mut State, &mut ModelRequest, &HookContext<'_>) -> Result<(), MiddlewareError>
                + Send
                + Sync,
        >,
    >,
    pub before_tool: Option<
        Box<
            dyn Fn(&mut State, &mut ToolCall, &HookContext<'_>) -> Result<ToolFlow, MiddlewareError>
                + Send
                + Sync,
        >,
    >,
    pub after_tool: Option<
        Box<
            dyn Fn(
                    &mut State,
                    &ToolCall,
                    &mut ToolOutput,
                    &HookContext<'_>,
                ) -> Result<(), MiddlewareError>
                + Send
                + Sync,
        >,
    >,
    pub after_model: Option<
        Box<
            dyn Fn(&mut State, &mut ModelResponse, &HookContext<'_>) -> Result<(), MiddlewareError>
                + Send
                + Sync,
        >,
    >,
    pub on_compact: Option<
        Box<dyn Fn(&mut State, &HookContext<'_>) -> Result<(), MiddlewareError> + Send + Sync>,
    >,
    pub on_resume: Option<
        Box<
            dyn Fn(&mut State, &ResumeCause, &HookContext<'_>) -> Result<(), MiddlewareError>
                + Send
                + Sync,
        >,
    >,
}

impl FnMiddleware {
    pub fn empty() -> FnMiddleware {
        FnMiddleware {
            before_model: None,
            before_tool: None,
            after_tool: None,
            after_model: None,
            on_compact: None,
            on_resume: None,
        }
    }
    pub fn entry(name: &str, priority: i32, mw: FnMiddleware) -> MiddlewareEntry {
        MiddlewareEntry {
            name: name.to_owned(),
            priority,
            source: MiddlewareSource::Agent,
            config_hash: None,
            middleware: Arc::new(mw),
        }
    }
}

#[async_trait]
impl Middleware for FnMiddleware {
    async fn before_model(
        &self,
        s: &mut State,
        r: &mut ModelRequest,
        cx: &HookContext<'_>,
    ) -> Result<(), MiddlewareError> {
        match &self.before_model {
            Some(f) => f(s, r, cx),
            None => Ok(()),
        }
    }
    async fn after_model(
        &self,
        s: &mut State,
        r: &mut ModelResponse,
        cx: &HookContext<'_>,
    ) -> Result<(), MiddlewareError> {
        match &self.after_model {
            Some(f) => f(s, r, cx),
            None => Ok(()),
        }
    }
    async fn before_tool(
        &self,
        s: &mut State,
        c: &mut ToolCall,
        cx: &HookContext<'_>,
    ) -> Result<ToolFlow, MiddlewareError> {
        match &self.before_tool {
            Some(f) => f(s, c, cx),
            None => Ok(ToolFlow::Continue),
        }
    }
    async fn after_tool(
        &self,
        s: &mut State,
        c: &ToolCall,
        o: &mut ToolOutput,
        cx: &HookContext<'_>,
    ) -> Result<(), MiddlewareError> {
        match &self.after_tool {
            Some(f) => f(s, c, o, cx),
            None => Ok(()),
        }
    }
    async fn on_compact(&self, s: &mut State, cx: &HookContext<'_>) -> Result<(), MiddlewareError> {
        match &self.on_compact {
            Some(f) => f(s, cx),
            None => Ok(()),
        }
    }
    async fn on_resume(
        &self,
        s: &mut State,
        c: &ResumeCause,
        cx: &HookContext<'_>,
    ) -> Result<(), MiddlewareError> {
        match &self.on_resume {
            Some(f) => f(s, c, cx),
            None => Ok(()),
        }
    }
}

// ---- kernel builder ------------------------------------------------------------------------

pub fn fast_retry(max_attempts: u32) -> RetryPolicy {
    RetryPolicy {
        max_attempts,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(5),
        multiplier: 2.0,
        jitter: false,
        request_timeout: Duration::from_secs(5),
    }
}

pub struct Setup {
    pub session_id: SessionId,
    pub tools: Vec<Arc<dyn Tool>>,
    pub middleware: Vec<MiddlewareEntry>,
    pub provider: Arc<dyn Provider>,
    pub host: Arc<dyn Host>,
    pub artifact_store: Arc<dyn ArtifactStore>,
    pub memory: Arc<dyn Memory>,
    pub sandbox: Arc<dyn SandboxBackend>,
    pub log: Arc<MemoryEventLog>,
    pub redactor: Arc<Redactor>,
    pub spill: SpillConfig,
    pub retry: RetryPolicy,
    pub limits: SandboxLimits,
    /// Taken by `config()` (`MigrationRegistry` is not `Clone`).
    pub migrations: Mutex<MigrationRegistry>,
    pub grants: Vec<Capability>,
    pub delta_sink: Option<tokio::sync::broadcast::Sender<ModelDelta>>,
    pub system_prompt: Vec<PromptBlock>,
}

impl Setup {
    pub fn new(provider: Arc<dyn Provider>) -> Setup {
        let redactor = Arc::new(Redactor::new());
        let session_id = SessionId("s_test".to_owned());
        Setup {
            session_id: session_id.clone(),
            tools: Vec::new(),
            middleware: Vec::new(),
            provider,
            host: FakeHost::new(),
            artifact_store: Arc::new(NoopArtifactStore),
            memory: Arc::new(NoopMemory),
            sandbox: FakeSandbox::new(),
            log: Arc::new(MemoryEventLog::new(session_id, redactor.clone())),
            redactor,
            spill: SpillConfig::default(),
            retry: fast_retry(3),
            limits: SandboxLimits::default(),
            migrations: Mutex::new(MigrationRegistry::new()),
            grants: Vec::new(),
            delta_sink: None,
            system_prompt: vec![PromptBlock::new(
                PromptBlockKind::Role,
                "test",
                "You are a test.",
            )],
        }
    }

    pub fn tool(mut self, t: Arc<dyn Tool>) -> Setup {
        self.tools.push(t);
        self
    }

    pub fn middleware(mut self, e: MiddlewareEntry) -> Setup {
        self.middleware.push(e);
        self
    }

    /// Reuse an existing log (for `Kernel::open`), keeping the redactor consistent.
    pub fn with_log(mut self, log: Arc<MemoryEventLog>) -> Setup {
        self.redactor = log.redactor().clone();
        self.session_id = log.session_id().clone();
        self.log = log;
        self
    }

    pub fn config(&self) -> KernelConfig {
        KernelConfig {
            tools: self.tools.clone(),
            middleware: self
                .middleware
                .iter()
                .map(|e| MiddlewareEntry {
                    name: e.name.clone(),
                    priority: e.priority,
                    source: e.source,
                    config_hash: e.config_hash.clone(),
                    middleware: e.middleware.clone(),
                })
                .collect(),
            provider: self.provider.clone(),
            host: self.host.clone(),
            artifact_store: self.artifact_store.clone(),
            memory: self.memory.clone(),
            sandbox: self.sandbox.clone(),
            event_log: self.log.clone(),
            redactor: self.redactor.clone(),
            spill: self.spill.clone(),
            retry: self.retry.clone(),
            sandbox_limits: self.limits.clone(),
            migrations: std::mem::take(&mut *lock(&self.migrations)),
            model_id: "fake-model".to_owned(),
            model_params: ModelParams::default(),
            system_prompt: self.system_prompt.clone(),
            grants: self.grants.clone(),
            delta_sink: self.delta_sink.clone(),
            event_channel_capacity: 64,
        }
    }

    pub fn init(&self) -> SessionInit {
        SessionInit {
            session_id: self.session_id.clone(),
            profiles: profiles(),
            profile_loads: vec![ProfileLoadPayload {
                kind: ProfileKind::Model,
                name: "fake".into(),
                path: None,
                hash: h("1a"),
                rejected: false,
                turn: 0,
            }],
            runtime_overrides: None,
            notebook_path: None,
            memory: None,
        }
    }

    pub async fn create(&self) -> Kernel {
        Kernel::create(self.config(), self.init())
            .await
            .expect("kernel creates")
    }

    pub fn events(&self) -> Vec<Event> {
        self.log.events()
    }

    pub fn kinds(&self) -> Vec<String> {
        self.events()
            .iter()
            .map(|e| e.body.kind().to_owned())
            .collect()
    }
}

// ---- event helpers -------------------------------------------------------------------------

pub fn kinds(events: &[Event]) -> Vec<&str> {
    events.iter().map(|e| e.body.kind()).collect()
}

pub fn find_all<'a, T>(events: &'a [Event], f: impl Fn(&'a EventBody) -> Option<T>) -> Vec<T> {
    events.iter().filter_map(|e| f(&e.body)).collect()
}

pub fn checkpoints(events: &[Event]) -> Vec<&CheckpointPayload> {
    find_all(events, |b| match b {
        EventBody::Checkpoint(c) => Some(c),
        _ => None,
    })
}

pub fn warnings(events: &[Event]) -> Vec<&WarningPayload> {
    find_all(events, |b| match b {
        EventBody::Warning(w) => Some(w),
        _ => None,
    })
}

pub fn warning_classes(events: &[Event]) -> Vec<String> {
    warnings(events).iter().map(|w| w.class.clone()).collect()
}

pub fn tool_results(events: &[Event]) -> Vec<&ToolResultPayload> {
    find_all(events, |b| match b {
        EventBody::ToolResult(r) => Some(r),
        _ => None,
    })
}

pub fn tool_calls(events: &[Event]) -> Vec<&ToolCallPayload> {
    find_all(events, |b| match b {
        EventBody::ToolCall(r) => Some(r),
        _ => None,
    })
}

pub fn model_requests(events: &[Event]) -> Vec<&ModelRequestPayload> {
    find_all(events, |b| match b {
        EventBody::ModelRequest(r) => Some(r),
        _ => None,
    })
}

pub fn model_responses(events: &[Event]) -> Vec<&ModelResponsePayload> {
    find_all(events, |b| match b {
        EventBody::ModelResponse(r) => Some(r),
        _ => None,
    })
}

pub fn task_updates(events: &[Event]) -> Vec<&TaskUpdatePayload> {
    find_all(events, |b| match b {
        EventBody::TaskUpdate(r) => Some(r),
        _ => None,
    })
}

pub fn cancelled_events(events: &[Event]) -> Vec<&CancelledPayload> {
    find_all(events, |b| match b {
        EventBody::Cancelled(r) => Some(r),
        _ => None,
    })
}

/// Invariant 1: every `model_request.checkpoint_hash` equals the `state_hash` of the checkpoint
/// that precedes it in the log.
pub fn assert_invariant_1(events: &[Event]) {
    let mut last_ck: Option<&Hash> = None;
    let mut seen = 0;
    for e in events {
        match &e.body {
            EventBody::Checkpoint(c) => last_ck = Some(&c.state_hash),
            EventBody::ModelRequest(r) => {
                assert_eq!(
                    Some(&r.checkpoint_hash),
                    last_ck,
                    "model_request at seq {} not computed from the preceding checkpoint",
                    e.seq
                );
                seen += 1;
            }
            _ => {}
        }
    }
    assert!(seen > 0, "no model_request in log");
}

/// Invariant 2: every `ToolUse` in an assistant message is answered by exactly one `ToolResult`
/// in the immediately following `Tool` message(s), in order.
pub fn assert_invariant_2(state: &State) {
    let msgs = &state.messages;
    let mut i = 0;
    while i < msgs.len() {
        let m = &msgs[i];
        if m.role == Role::Assistant {
            let uses: Vec<&String> = m
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolUse { id, .. } => Some(id),
                    _ => None,
                })
                .collect();
            let mut results = Vec::new();
            let mut j = i + 1;
            while j < msgs.len() && msgs[j].role == Role::Tool {
                for b in &msgs[j].content {
                    match b {
                        ContentBlock::ToolResult { tool_use_id, .. } => results.push(tool_use_id),
                        other => panic!("non-ToolResult block in Tool message: {other:?}"),
                    }
                }
                j += 1;
            }
            assert_eq!(uses, results, "tool uses and results differ at message {i}");
            i = j;
        } else {
            i += 1;
        }
    }
}

/// Invariant 3: `check_hint` never appears in `State.messages` or in any `ModelRequest`.
pub fn assert_invariant_3(state: &State, requests: &[ModelRequest]) {
    let text = serde_json::to_string(&state.messages).unwrap();
    assert!(
        !text.contains("check_hint"),
        "check_hint leaked into messages"
    );
    assert!(
        !text.contains("secret_hint"),
        "check_hint payload leaked into messages"
    );
    for r in requests {
        let t = serde_json::to_string(&r.messages).unwrap();
        assert!(!t.contains("check_hint") && !t.contains("secret_hint"));
    }
}

pub fn assert_ordered(kinds: &[&str], expected: &[&str]) {
    let mut pos = 0;
    for want in expected {
        match kinds[pos..].iter().position(|k| k == want) {
            Some(p) => pos += p + 1,
            None => panic!("expected `{want}` after position {pos} in {kinds:?}"),
        }
    }
}

pub fn dummy_command_env() -> BTreeMap<String, String> {
    BTreeMap::new()
}
