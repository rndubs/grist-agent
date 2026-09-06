//! Unsandboxed spawn for the native host (`Host::spawn`, §3.9): exact environment (D10), piped
//! stdio, `policy.timeout` enforced from spawn, SIGTERM → SIGKILL escalation via `nix`.

use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use kernel::{ChildProcess, Command, HostError, PolicyError, ProcPolicy, ProcessOutput};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Child;
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// Grace between SIGTERM and SIGKILL when the policy timeout fires.
const TIMEOUT_GRACE: Duration = Duration::from_secs(1);
/// How long to wait for the stdout/stderr readers after the child exited (a grandchild may
/// still hold the pipes).
const READER_DRAIN: Duration = Duration::from_secs(2);

/// Program allowlist: exact match on the program string or its basename.
fn check_program(policy: &ProcPolicy, program: &str) -> Result<(), HostError> {
    if policy.programs.is_empty() || policy.programs.contains(program) {
        return Ok(());
    }
    let base = Path::new(program)
        .file_name()
        .map(|b| b.to_string_lossy().into_owned());
    match base {
        Some(b) if policy.programs.contains(&b) => Ok(()),
        _ => Err(HostError::Denied(PolicyError::ProgramDenied(
            program.to_owned(),
        ))),
    }
}

fn send_signal(pid: u32, sig: Signal) -> Result<(), HostError> {
    match kill(Pid::from_raw(pid as i32), sig) {
        Ok(()) | Err(nix::errno::Errno::ESRCH) => Ok(()),
        Err(e) => Err(HostError::Io(format!("kill({pid}, {sig:?}): {e}"))),
    }
}

struct Reader {
    buf: Arc<Mutex<Vec<u8>>>,
    task: Option<JoinHandle<()>>,
}

impl Reader {
    fn spawn<R: tokio::io::AsyncRead + Unpin + Send + 'static>(src: Option<R>) -> Reader {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let task = src.map(|mut r| {
            let buf = Arc::clone(&buf);
            tokio::spawn(async move {
                let mut chunk = [0u8; 8192];
                loop {
                    match r.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if let Ok(mut b) = buf.lock() {
                                b.extend_from_slice(&chunk[..n]);
                            }
                        }
                    }
                }
            })
        });
        Reader { buf, task }
    }

    async fn finish(&mut self) -> Vec<u8> {
        if let Some(task) = self.task.take() {
            // On timeout a grandchild still holds the pipe; keep what was read so far.
            let _ = tokio::time::timeout(READER_DRAIN, task).await;
        }
        self.buf
            .lock()
            .map(|mut b| std::mem::take(&mut *b))
            .unwrap_or_default()
    }
}

enum State {
    Running {
        child: Child,
        stdout: Reader,
        stderr: Reader,
    },
    Done(ProcessOutput),
}

/// A child of the native host.
pub(crate) struct NativeChild {
    pid: Option<u32>,
    timeout: Duration,
    started: Instant,
    exited: watch::Sender<bool>,
    state: tokio::sync::Mutex<State>,
}

pub(crate) async fn spawn(
    policy: &ProcPolicy,
    cmd: Command,
) -> Result<Box<dyn ChildProcess>, HostError> {
    check_program(policy, &cmd.program)?;
    let mut c = tokio::process::Command::new(&cmd.program);
    c.args(&cmd.args)
        .env_clear()
        .envs(&cmd.env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(cwd) = &cmd.cwd {
        c.current_dir(cwd);
    }
    let started = Instant::now();
    let mut child = c.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            HostError::NotFound(format!("program `{}`", cmd.program))
        } else {
            HostError::Io(format!("spawn `{}`: {e}", cmd.program))
        }
    })?;
    let pid = child.id();
    if let Some(mut stdin) = child.stdin.take() {
        let bytes = cmd.stdin.unwrap_or_default();
        tokio::spawn(async move {
            let _ = stdin.write_all(&bytes).await;
            let _ = stdin.shutdown().await;
            drop(stdin);
        });
    }
    let stdout = Reader::spawn(child.stdout.take());
    let stderr = Reader::spawn(child.stderr.take());
    let (exited, _) = watch::channel(false);
    Ok(Box::new(NativeChild {
        pid,
        timeout: policy.timeout,
        started,
        exited,
        state: tokio::sync::Mutex::new(State::Running {
            child,
            stdout,
            stderr,
        }),
    }))
}

impl NativeChild {
    async fn escalate(&self, child: &mut Child, grace: Duration) -> Result<ExitStatus, HostError> {
        if let Some(pid) = self.pid {
            send_signal(pid, Signal::SIGTERM)?;
        }
        match tokio::time::timeout(grace, child.wait()).await {
            Ok(r) => r.map_err(|e| HostError::Io(format!("wait: {e}"))),
            Err(_) => {
                if let Some(pid) = self.pid {
                    send_signal(pid, Signal::SIGKILL)?;
                }
                child
                    .wait()
                    .await
                    .map_err(|e| HostError::Io(format!("wait: {e}")))
            }
        }
    }

    async fn collect(
        &self,
        child: &mut Child,
        stdout: &mut Reader,
        stderr: &mut Reader,
    ) -> Result<ProcessOutput, HostError> {
        let deadline = self.started + self.timeout;
        let mut timed_out = false;
        let status = match tokio::time::timeout_at(deadline.into(), child.wait()).await {
            Ok(r) => r.map_err(|e| HostError::Io(format!("wait: {e}")))?,
            Err(_) => {
                timed_out = true;
                self.escalate(child, TIMEOUT_GRACE).await?
            }
        };
        let _ = self.exited.send(true);
        let stdout = stdout.finish().await;
        let stderr = stderr.finish().await;
        Ok(ProcessOutput {
            exit_code: status.code(),
            signal: status.signal(),
            stdout,
            stderr,
            timed_out,
            duration: self.started.elapsed(),
        })
    }
}

#[async_trait]
impl ChildProcess for NativeChild {
    fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// Waits for exit and collects stdout/stderr. `policy.timeout` counts from spawn; when it
    /// fires the child gets SIGTERM, one second of grace, then SIGKILL, and `timed_out` is set.
    /// A second call returns the same output.
    async fn wait(&self) -> Result<ProcessOutput, HostError> {
        let mut guard = self.state.lock().await;
        if let State::Done(out) = &*guard {
            return Ok(out.clone());
        }
        let out = {
            let State::Running {
                child,
                stdout,
                stderr,
            } = &mut *guard
            else {
                unreachable!("state is Running")
            };
            self.collect(child, stdout, stderr).await?
        };
        *guard = State::Done(out.clone());
        Ok(out)
    }

    /// SIGTERM now; SIGKILL after `grace` unless a concurrent `wait` observed the exit first.
    /// Does not reap: call `wait` to collect the status.
    async fn terminate(&self, grace: Duration) -> Result<(), HostError> {
        let Some(pid) = self.pid else {
            return Ok(());
        };
        let mut rx = self.exited.subscribe();
        if *rx.borrow_and_update() {
            return Ok(());
        }
        send_signal(pid, Signal::SIGTERM)?;
        let observed_exit = tokio::time::timeout(grace, async {
            while !*rx.borrow_and_update() {
                if rx.changed().await.is_err() {
                    return false;
                }
            }
            true
        })
        .await
        .unwrap_or(false);
        if !observed_exit {
            send_signal(pid, Signal::SIGKILL)?;
        }
        Ok(())
    }
}
