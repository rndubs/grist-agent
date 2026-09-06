//! The stateless supervisor shared by every backend: waits on a `ChildProcess` spawned through
//! `Host::spawn`, enforcing the policy timeout (→ `SandboxError::Timeout`) and the cancellation
//! token (→ SIGTERM, grace, SIGKILL via `ChildProcess::terminate`, then `SandboxError::Cancelled`),
//! per D15 and §7.1.

use std::sync::Arc;
use std::time::Duration;

use kernel::cancel::CancellationToken;
use kernel::host::{ChildProcess, HostError, ProcessOutput};
use kernel::sandbox::SandboxError;

/// Terminates the child if the supervising future is dropped before the child finished (a
/// dropped `run_script` task future, a dropped `bash` invocation). Best effort: it needs a tokio
/// runtime to spawn the termination onto; without one the child is left to the host's own
/// `kill_on_drop`.
struct TerminateOnDrop {
    child: Option<Arc<dyn ChildProcess>>,
    grace: Duration,
}

impl TerminateOnDrop {
    fn disarm(&mut self) {
        self.child = None;
    }
}

impl Drop for TerminateOnDrop {
    fn drop(&mut self) {
        if let Some(child) = self.child.take()
            && let Ok(handle) = tokio::runtime::Handle::try_current()
        {
            let grace = self.grace;
            handle.spawn(async move {
                let _ = child.terminate(grace).await;
            });
        }
    }
}

/// Map a `HostError` from `spawn`/`wait` onto the sandbox error space.
pub fn host_error(context: &str, err: HostError) -> SandboxError {
    match err {
        HostError::Cancelled => SandboxError::Cancelled,
        other => SandboxError::Launch(format!("{context}: {other}")),
    }
}

/// Wait for `child` under `timeout` and `cancel`.
///
/// - The child exits first: its `ProcessOutput` is returned (exit code or signal as reported).
/// - `cancel` fires first: `terminate(grace)` (SIGTERM, then SIGKILL after `grace`), then
///   `Err(SandboxError::Cancelled)`.
/// - `timeout` elapses first: `terminate(grace)`, then `Err(SandboxError::Timeout(timeout))`.
/// - The returned future is dropped before any of the above: the child is terminated in the
///   background (see `TerminateOnDrop`).
pub async fn supervise(
    child: Box<dyn ChildProcess>,
    timeout: Duration,
    cancel: CancellationToken,
    grace: Duration,
) -> Result<ProcessOutput, SandboxError> {
    let child: Arc<dyn ChildProcess> = Arc::from(child);
    let mut guard = TerminateOnDrop {
        child: Some(child.clone()),
        grace,
    };
    let result = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            let _ = child.terminate(grace).await;
            Err(SandboxError::Cancelled)
        }
        _ = tokio::time::sleep(timeout) => {
            let _ = child.terminate(grace).await;
            Err(SandboxError::Timeout(timeout))
        }
        out = child.wait() => out.map_err(|e| host_error("wait", e)),
    };
    guard.disarm();
    result
}
