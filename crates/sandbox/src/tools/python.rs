//! `python`: a persistent Python REPL as a `Session` tool (ADR-0001, D5).

use std::path::PathBuf;

use async_trait::async_trait;
use kernel::capability::{Capability, FsMode};
use kernel::host::Command;
use kernel::sandbox::{RpcRequest, RpcResponse, SandboxError};
use kernel::tool::{Tool, ToolContext, ToolError, ToolKind, ToolResult};
use serde_json::{Value, json};

use super::{fs_cap, proc_cap, require_str};

/// The REPL server the session runs: stdlib-only Python 3.11+, newline-delimited JSON-RPC 2.0 on
/// stdio. Methods: `eval {code}` → `{ok, stdout, stderr, value, error{type, message,
/// traceback}}` in a persistent namespace; `reset`; `info` → `{python, cwd, variables}`; `ping`.
/// Unknown method → `-32601`; parse error → `-32700`; bad params → `-32602`. Installs a SIGTERM
/// handler that exits promptly (the process is PID 1 under `--unshare-pid`).
pub const REPL_SERVER_SOURCE: &str = include_str!("repl_server.py");

/// Evaluates Python code in a process that lives for the session. Declares `fs.rw:<workdir>` and
/// `proc:python3`.
///
/// `session_command()` is `python3 -c <REPL_SERVER_SOURCE>` with the workdir as cwd; the kernel
/// launches it lazily under the derived policy and terminates it on suspend/end (§7.9). Input
/// `{code}`; the tool sends `eval` and returns the REPL's result object as-is: an exception in
/// user code is a *successful* call with `ok:false` and a structured `error` the model sees
/// (`Ok(Value)`). A JSON-RPC protocol error is `ToolError::Failed`; a per-call timeout or cancel
/// kills the process (`ToolError::Timeout` / `Cancelled`) and the kernel relaunches it fresh.
pub struct PythonTool {
    workdir: PathBuf,
}

impl PythonTool {
    /// A `python` tool for a session rooted at `workdir` (absolute, normalized).
    pub fn new(workdir: impl Into<PathBuf>) -> PythonTool {
        PythonTool {
            workdir: super::normalize_workdir(workdir),
        }
    }
}

#[async_trait]
impl Tool for PythonTool {
    fn name(&self) -> &str {
        "python"
    }

    fn description(&self) -> &str {
        "Run Python code in a persistent interpreter. Variables, imports and functions persist \
         across calls within this session (they are lost when the session is suspended or the \
         interpreter is restarted). Returns captured stdout/stderr, the value of a trailing \
         expression, and a structured error if the code raised."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "code": {"type": "string", "description": "Python source to execute. If the last statement is an expression its repr is returned as `value`."}
            },
            "required": ["code"],
            "additionalProperties": false
        })
    }

    fn kind(&self) -> ToolKind {
        ToolKind::Session
    }

    fn capabilities(&self) -> Vec<Capability> {
        vec![fs_cap(&self.workdir, FsMode::Rw), proc_cap("python3")]
    }

    fn session_command(&self) -> Option<Command> {
        Some(Command {
            program: "python3".to_owned(),
            args: vec!["-c".to_owned(), REPL_SERVER_SOURCE.to_owned()],
            cwd: Some(self.workdir.clone()),
            env: Default::default(),
            stdin: None,
        })
    }

    async fn invoke(&self, ctx: &ToolContext<'_>, input: Value) -> Result<ToolResult, ToolError> {
        let code = require_str(&input, "code")?;
        let session = ctx.session_process()?;
        let req = RpcRequest {
            method: "eval".to_owned(),
            params: json!({ "code": code }),
        };
        match session.call(req, ctx.cancel.clone()).await {
            Ok(RpcResponse::Result(value)) => Ok(ToolResult::Value(value)),
            Ok(RpcResponse::Error {
                code,
                message,
                data,
            }) => Err(ToolError::Failed(format!(
                "python session protocol error {code}: {message}{}",
                data.map(|d| format!(" ({d})")).unwrap_or_default()
            ))),
            Err(SandboxError::Timeout(d)) => Err(ToolError::Timeout(d)),
            Err(SandboxError::Cancelled) => Err(ToolError::Cancelled),
            Err(e) => Err(ToolError::Failed(format!("python session: {e}"))),
        }
    }
}
