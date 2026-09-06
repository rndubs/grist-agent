//! The `None` backend (D14): **no isolation**. Development builds only.
//!
//! Compiled only under the `dev-sandbox-none` feature; `name() == "none"`, which the kernel logs
//! as `warning{class: "sandbox_backend_none"}` at every session start and resume. A release build
//! of this crate MUST NOT contain it; CI asserts the symbol is absent from a default build (see
//! `examples/none_backend_symbol.rs`).
//!
//! What it still does: the same scrubbed environment as `bwrap` ([`crate::scrubbed_env`]: only the
//! allowlisted names, never a secret-like one, `HOME=/tmp`), the same program check
//! ([`crate::check_program`]), the same timeout and cancel handling (SIGTERM → grace → SIGKILL),
//! and the same JSON-RPC session transport. What it does not do: mounts, namespaces, tmpfs,
//! network isolation. Stateless commands run through `Host::spawn`; sessions through
//! [`JsonRpcSession`] directly (piped stdio).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use kernel::cancel::CancellationToken;
use kernel::host::{Command, Host, ProcessOutput};
use kernel::sandbox::{SandboxBackend, SandboxError, SandboxPolicy, SessionProcess};

use crate::bwrap::DEFAULT_GRACE;
use crate::env::{SANDBOX_HOME, check_program, scrubbed_env};
use crate::launch::{host_error, supervise};
use crate::session::{JsonRpcSession, SessionSpec};

/// `SandboxBackend` that runs commands directly. See the module docs.
pub struct NoneBackend {
    host: Arc<dyn Host>,
    grace: Duration,
}

impl NoneBackend {
    /// A backend over `host` with [`DEFAULT_GRACE`].
    pub fn new(host: Arc<dyn Host>) -> NoneBackend {
        NoneBackend {
            host,
            grace: DEFAULT_GRACE,
        }
    }

    /// Override the SIGTERM → SIGKILL grace period.
    pub fn with_grace(mut self, grace: Duration) -> NoneBackend {
        self.grace = grace;
        self
    }

    /// The SIGTERM → SIGKILL grace period.
    pub fn grace(&self) -> Duration {
        self.grace
    }

    /// The command as it will actually run: program check applied, env scrubbed, cwd defaulted
    /// to `/tmp` (the sandbox's `$HOME` and default cwd).
    fn prepare(&self, policy: &SandboxPolicy, cmd: Command) -> Result<Command, SandboxError> {
        check_program(policy, &cmd)?;
        let env = scrubbed_env(policy, &cmd)?;
        Ok(Command {
            program: cmd.program,
            args: cmd.args,
            cwd: Some(cmd.cwd.unwrap_or_else(|| PathBuf::from(SANDBOX_HOME))),
            env,
            stdin: cmd.stdin,
        })
    }
}

#[async_trait]
impl SandboxBackend for NoneBackend {
    fn name(&self) -> &'static str {
        "none"
    }

    async fn launch_stateless(
        &self,
        policy: &SandboxPolicy,
        cmd: Command,
        cancel: CancellationToken,
    ) -> Result<ProcessOutput, SandboxError> {
        let cmd = self.prepare(policy, cmd)?;
        let child = self
            .host
            .spawn(&policy.proc_(), cmd)
            .await
            .map_err(|e| host_error("spawn", e))?;
        supervise(child, policy.timeout, cancel, self.grace).await
    }

    async fn launch_session(
        &self,
        policy: &SandboxPolicy,
        cmd: Command,
    ) -> Result<Box<dyn SessionProcess>, SandboxError> {
        let cmd = self.prepare(policy, cmd)?;
        let session = JsonRpcSession::spawn(SessionSpec {
            program: cmd.program,
            args: cmd.args,
            cwd: cmd.cwd,
            env: cmd.env,
            timeout: policy.timeout,
            grace: self.grace,
        })
        .await?;
        Ok(Box::new(session))
    }
}
