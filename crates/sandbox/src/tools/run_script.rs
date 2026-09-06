//! `run_script`: a Stateless tool that returns a `Task` (D1) and completes it through the
//! kernel's in-process process-exit waker.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use kernel::cancel::CancellationToken;
use kernel::capability::{Capability, FsMode};
use kernel::content::ToolResultContent;
use kernel::host::Command;
use kernel::sandbox::{SandboxBackend, SandboxError};
use kernel::task::{TaskHandle, TaskOutcome, TaskStatus};
use kernel::tool::{Tool, ToolContext, ToolError, ToolKind, ToolResult};
use serde_json::{Value, json};
use tokio::task::AbortHandle;

use super::{fs_cap, opt_str, opt_str_list, output_json, proc_cap, require_str, resolve_path};

/// Starts a script in the background and returns a `TaskHandle`; the result arrives later as a
/// synthetic tool result. Declares `fs.rw:<workdir>` and `proc:bash` (the script runs as
/// `bash <path> <args…>`, so no execute bit is needed).
///
/// Input `{path, args?, cwd?}`. The launch goes through `launch_stateless` on the backend, so the
/// policy (mounts, env scrub, timeout, program check) applies exactly as for `bash`; the tool
/// spawns it on a detached tokio task at invoke time (status `Running` is honest) and registers a
/// completion future with `ctx.watch_task(ctx.task_id(), …)`. When the kernel's process-exit
/// waker resolves that future it delivers a `TaskUpdate` whose outcome is
/// `Json({exit_code, signal, stdout, stderr, timed_out, duration_ms})` with `is_error` iff the
/// exit code is non-zero, the process timed out, or it was killed/cancelled.
///
/// **Cancellation (`CancelScope::Task`, §7.1):** the kernel drops the registered future. A drop
/// guard inside it cancels the launch's own token and aborts the detached task, and the launcher
/// then terminates the child (SIGTERM → grace → SIGKILL). The token is a fresh one, not a child of
/// the invocation's: §7.1 says a cancelled *turn* leaves open tasks untouched, so the task's
/// lifetime is bound to its future (the Task scope) rather than to the tool call.
///
/// **Why it owns a backend:** `ToolContext` lends `&dyn SandboxBackend` for the invocation only,
/// but the completion future must be `'static`. The launcher constructs this tool with the same
/// `Arc` it hands the kernel; `invoke` checks the two agree by name and fails otherwise.
pub struct RunScriptTool {
    workdir: PathBuf,
    sandbox: Arc<dyn SandboxBackend>,
}

impl RunScriptTool {
    /// A `run_script` tool for a session rooted at `workdir`, launching through `sandbox`.
    pub fn new(workdir: impl Into<PathBuf>, sandbox: Arc<dyn SandboxBackend>) -> RunScriptTool {
        RunScriptTool {
            workdir: super::normalize_workdir(workdir),
            sandbox,
        }
    }
}

/// A declaration-only instance for `tool_decls` (never invoked).
pub(crate) fn decl(workdir: PathBuf) -> RunScriptTool {
    RunScriptTool::new(workdir, Arc::new(NoBackend))
}

/// Placeholder backend for declaration-only instances.
struct NoBackend;

#[async_trait]
impl SandboxBackend for NoBackend {
    fn name(&self) -> &'static str {
        "unset"
    }
    async fn launch_stateless(
        &self,
        _: &kernel::sandbox::SandboxPolicy,
        _: Command,
        _: CancellationToken,
    ) -> Result<kernel::host::ProcessOutput, SandboxError> {
        Err(SandboxError::Launch("no sandbox backend configured".into()))
    }
    async fn launch_session(
        &self,
        _: &kernel::sandbox::SandboxPolicy,
        _: Command,
    ) -> Result<Box<dyn kernel::sandbox::SessionProcess>, SandboxError> {
        Err(SandboxError::Launch("no sandbox backend configured".into()))
    }
}

/// Cancels the launch token and aborts the detached task when the completion future is dropped.
struct StopOnDrop {
    token: CancellationToken,
    abort: AbortHandle,
    armed: bool,
}

impl StopOnDrop {
    /// A method (not a field write) on purpose: an `async move` block captures disjoint fields,
    /// and writing `guard.armed` from inside would capture only the `bool`, leaving the guard —
    /// and its `Drop` — behind in `invoke`. A `&mut self` call captures the whole guard.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.token.cancel();
            self.abort.abort();
        }
    }
}

/// The outcome the waker delivers, for every way the launch can end.
pub fn outcome_of(
    result: Result<kernel::host::ProcessOutput, SandboxError>,
    elapsed: Duration,
) -> TaskOutcome {
    let ms = elapsed.as_millis() as u64;
    let (content, is_error) = match result {
        Ok(out) => {
            let is_error = out.exit_code != Some(0) || out.timed_out;
            (output_json(&out), is_error)
        }
        Err(SandboxError::Timeout(d)) => (
            json!({
                "exit_code": null, "signal": null, "stdout": "", "stderr": "",
                "timed_out": true, "duration_ms": ms,
                "error": format!("timed out after {} s", d.as_secs()),
            }),
            true,
        ),
        Err(SandboxError::Cancelled) => (
            json!({
                "exit_code": null, "signal": null, "stdout": "", "stderr": "",
                "timed_out": false, "cancelled": true, "duration_ms": ms,
            }),
            true,
        ),
        Err(e) => (
            json!({
                "exit_code": null, "signal": null, "stdout": "", "stderr": "",
                "timed_out": false, "duration_ms": ms, "error": e.to_string(),
            }),
            true,
        ),
    };
    TaskOutcome {
        content: ToolResultContent::Json(content),
        is_error,
        artifact_handles: Vec::new(),
    }
}

#[async_trait]
impl Tool for RunScriptTool {
    fn name(&self) -> &str {
        "run_script"
    }

    fn description(&self) -> &str {
        "Run a script in the background and return immediately with a task id. Use it for \
         anything that takes longer than a minute; keep working and you will be told when it \
         finishes, with its exit code and output. The script runs as `bash <path> <args>` in a \
         fresh sandbox rooted at the working directory."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Script path; relative paths resolve against the working directory."},
                "args": {"type": "array", "items": {"type": "string"}, "description": "Arguments passed to the script."},
                "cwd": {"type": "string", "description": "Working directory for the script. Default: the working directory."}
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }

    fn kind(&self) -> ToolKind {
        ToolKind::Stateless
    }

    fn capabilities(&self) -> Vec<Capability> {
        vec![fs_cap(&self.workdir, FsMode::Rw), proc_cap("bash")]
    }

    async fn invoke(&self, ctx: &ToolContext<'_>, input: Value) -> Result<ToolResult, ToolError> {
        if ctx.sandbox.name() != self.sandbox.name() {
            return Err(ToolError::Failed(format!(
                "run_script was built with the `{}` backend but the kernel runs `{}`",
                self.sandbox.name(),
                ctx.sandbox.name()
            )));
        }
        let path = resolve_path(&self.workdir, require_str(&input, "path")?);
        let args = opt_str_list(&input, "args")?;
        let cwd = opt_str(&input, "cwd")?
            .map(|c| resolve_path(&self.workdir, c))
            .unwrap_or_else(|| self.workdir.clone());

        let mut argv = vec![path.to_string_lossy().into_owned()];
        argv.extend(args.iter().cloned());
        let cmd = Command {
            program: "bash".to_owned(),
            args: argv.clone(),
            cwd: Some(cwd),
            env: Default::default(),
            stdin: None,
        };

        let token = CancellationToken::new();
        let launch_token = token.clone();
        let sandbox = self.sandbox.clone();
        let policy = ctx.policy.clone();
        let join = tokio::spawn(async move {
            let start = Instant::now();
            let result = sandbox.launch_stateless(&policy, cmd, launch_token).await;
            (result, start.elapsed())
        });
        // Armed here, outside the future: a future the kernel drops without ever polling it must
        // still stop the process, so the guard has to exist before the first poll.
        let mut guard = StopOnDrop {
            token,
            abort: join.abort_handle(),
            armed: true,
        };

        let id = ctx.task_id();
        let done = async move {
            let outcome = match join.await {
                Ok((result, elapsed)) => outcome_of(result, elapsed),
                Err(e) => outcome_of(
                    Err(SandboxError::Launch(format!("launch task failed: {e}"))),
                    Duration::ZERO,
                ),
            };
            guard.disarm();
            outcome
        };
        ctx.watch_task(id.clone(), Box::pin(done))?;

        Ok(ToolResult::Task(TaskHandle {
            id,
            status: TaskStatus::Running,
            eta: None,
            check_hint: Some(json!({
                "path": path.to_string_lossy(),
                "sandbox": self.sandbox.name(),
            })),
            description: Some(argv.join(" ")),
        }))
    }
}
