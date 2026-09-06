//! `bash`: one shell command per call in a fresh sandbox (Stateless, D5).

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use kernel::capability::{Capability, FsMode};
use kernel::host::Command;
use kernel::tool::{Tool, ToolContext, ToolError, ToolKind, ToolResult};
use serde_json::{Value, json};

use super::{
    fs_cap, opt_str, opt_u64, output_json, proc_cap, require_str, resolve_path, sandbox_err,
};

/// Runs `bash -c <command>` through `ctx.sandbox.launch_stateless` under the tool's policy.
/// Declares `fs.rw:<workdir>` and `proc:bash`.
///
/// Input `{command, cwd?, timeout_secs?}`; `cwd` defaults to the workdir; `timeout_secs` may
/// only *lower* the policy timeout (a larger value is `InvalidInput`). Output
/// `{exit_code, signal, stdout, stderr, timed_out, duration_ms}` (stdout/stderr lossy UTF-8; the
/// kernel spills oversized results). A policy timeout is `ToolError::Timeout`, a cancel
/// `ToolError::Cancelled`. Shell state (cwd, variables, background jobs) does not persist between
/// calls; the filesystem does.
pub struct BashTool {
    workdir: PathBuf,
}

impl BashTool {
    /// A `bash` tool for a session rooted at `workdir` (absolute, normalized).
    pub fn new(workdir: impl Into<PathBuf>) -> BashTool {
        BashTool {
            workdir: super::normalize_workdir(workdir),
        }
    }
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "Run one shell command with bash in a fresh sandbox rooted at the working directory. \
         Shell state does not persist between calls (each call starts a new shell); files do. \
         Returns exit code, stdout and stderr."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "The command line to run with `bash -c`."},
                "cwd": {"type": "string", "description": "Working directory for the command; relative to the working directory. Default: the working directory."},
                "timeout_secs": {"type": "integer", "minimum": 1, "description": "Wall-clock limit in seconds. May only lower the session's sandbox timeout."}
            },
            "required": ["command"],
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
        let command = require_str(&input, "command")?.to_owned();
        let cwd = opt_str(&input, "cwd")?
            .map(|c| resolve_path(&self.workdir, c))
            .unwrap_or_else(|| self.workdir.clone());

        let mut policy = ctx.policy.clone();
        if let Some(secs) = opt_u64(&input, "timeout_secs")? {
            let requested = Duration::from_secs(secs);
            if secs == 0 || requested > policy.timeout {
                return Err(ToolError::InvalidInput(format!(
                    "`timeout_secs` must be between 1 and {} (the sandbox timeout)",
                    policy.timeout.as_secs()
                )));
            }
            policy.timeout = requested;
        }

        let cmd = Command {
            program: "bash".to_owned(),
            args: vec!["-c".to_owned(), command],
            cwd: Some(cwd),
            env: Default::default(),
            stdin: None,
        };
        let out = ctx
            .sandbox
            .launch_stateless(&policy, cmd, ctx.cancel.clone())
            .await
            .map_err(sandbox_err)?;
        Ok(ToolResult::Value(output_json(&out)))
    }
}
