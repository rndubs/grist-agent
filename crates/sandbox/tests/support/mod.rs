//! Test support: a minimal real-filesystem `Host` that enforces `FsPolicy::check` on canonicalized
//! paths and spawns through `tokio::process`, plus a `TaskRegistrar` that stores futures, a stub
//! backend, and a `ToolContext` fixture. Independent of the `host` crate on purpose.
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex as StdMutex, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures_core::future::BoxFuture;
use kernel::SessionId;
use kernel::artifact::NoopArtifactStore;
use kernel::cancel::CancellationToken;
use kernel::capability::{FsMode, NetAllow};
use kernel::host::{
    AskUserRequest, ChildProcess, Command, DirEntry, FsPolicy, Host, HostError, Metadata, Mount,
    NetHandle, NetPolicy, ProcPolicy, ProcessOutput, SecretHandle, UserAnswer,
};
use kernel::sandbox::{SandboxBackend, SandboxError, SandboxPolicy, SessionProcess};
use kernel::task::{TaskId, TaskOutcome};
use kernel::tool::{TaskRegistrar, ToolContext, ToolError};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout};
use tokio::sync::Mutex;

/// A `Host` over the real filesystem. Every fs call canonicalizes the path (for a file that does
/// not exist yet, its parent) and runs `FsPolicy::check`; `spawn` enforces `ProcPolicy.programs`
/// and passes exactly `cmd.env`.
pub struct FsHost;

fn canonical(path: &Path) -> Result<PathBuf, HostError> {
    if let Ok(real) = path.canonicalize() {
        return Ok(real);
    }
    let parent = path
        .parent()
        .ok_or_else(|| HostError::NotFound(path.display().to_string()))?;
    let name = path
        .file_name()
        .ok_or_else(|| HostError::NotFound(path.display().to_string()))?;
    let real_parent = parent
        .canonicalize()
        .map_err(|_| HostError::NotFound(path.display().to_string()))?;
    Ok(real_parent.join(name))
}

fn check(policy: &FsPolicy, path: &Path, mode: FsMode) -> Result<PathBuf, HostError> {
    let real = canonical(path)?;
    policy.check(&real, mode)?;
    Ok(real)
}

fn io_err(e: std::io::Error, path: &Path) -> HostError {
    if e.kind() == std::io::ErrorKind::NotFound {
        HostError::NotFound(path.display().to_string())
    } else {
        HostError::Io(e.to_string())
    }
}

#[async_trait]
impl Host for FsHost {
    fn name(&self) -> &str {
        "test-fs"
    }

    async fn read_file(&self, policy: &FsPolicy, path: &Path) -> Result<Vec<u8>, HostError> {
        let real = check(policy, path, FsMode::Ro)?;
        tokio::fs::read(&real).await.map_err(|e| io_err(e, path))
    }

    async fn write_file(
        &self,
        policy: &FsPolicy,
        path: &Path,
        bytes: &[u8],
    ) -> Result<(), HostError> {
        let real = check(policy, path, FsMode::Rw)?;
        tokio::fs::write(&real, bytes)
            .await
            .map_err(|e| io_err(e, path))
    }

    async fn list_dir(&self, policy: &FsPolicy, path: &Path) -> Result<Vec<DirEntry>, HostError> {
        let real = check(policy, path, FsMode::Ro)?;
        let mut out = Vec::new();
        let mut rd = tokio::fs::read_dir(&real)
            .await
            .map_err(|e| io_err(e, path))?;
        while let Some(entry) = rd.next_entry().await.map_err(|e| io_err(e, path))? {
            let meta = entry.metadata().await.map_err(|e| io_err(e, path))?;
            out.push(DirEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                is_dir: meta.is_dir(),
                size: meta.len(),
            });
        }
        Ok(out)
    }

    async fn stat(&self, policy: &FsPolicy, path: &Path) -> Result<Metadata, HostError> {
        let real = check(policy, path, FsMode::Ro)?;
        let meta = tokio::fs::metadata(&real)
            .await
            .map_err(|e| io_err(e, path))?;
        Ok(Metadata {
            is_dir: meta.is_dir(),
            size: meta.len(),
            modified: None,
        })
    }

    async fn remove(&self, policy: &FsPolicy, path: &Path) -> Result<(), HostError> {
        let real = check(policy, path, FsMode::Rw)?;
        let meta = tokio::fs::metadata(&real)
            .await
            .map_err(|e| io_err(e, path))?;
        if meta.is_dir() {
            tokio::fs::remove_dir(&real).await
        } else {
            tokio::fs::remove_file(&real).await
        }
        .map_err(|e| io_err(e, path))
    }

    async fn spawn(
        &self,
        policy: &ProcPolicy,
        cmd: Command,
    ) -> Result<Box<dyn ChildProcess>, HostError> {
        if !policy.programs.is_empty() {
            let base = Path::new(&cmd.program)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if !policy.programs.contains(&cmd.program) && !policy.programs.contains(&base) {
                return Err(HostError::Denied(
                    kernel::sandbox::PolicyError::ProgramDenied(cmd.program.clone()),
                ));
            }
        }
        let mut command = tokio::process::Command::new(&cmd.program);
        command
            .args(&cmd.args)
            .env_clear()
            .envs(&cmd.env)
            .stdin(if cmd.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(cwd) = &cmd.cwd {
            command.current_dir(cwd);
        }
        let mut child = command
            .spawn()
            .map_err(|e| HostError::Io(format!("spawn {}: {e}", cmd.program)))?;
        let pid = child.id().unwrap_or(0);
        let io = Io {
            stdin: child.stdin.take(),
            stdout: child.stdout.take().expect("piped stdout"),
            stderr: child.stderr.take().expect("piped stderr"),
            input: cmd.stdin,
        };
        Ok(Box::new(TokioChild {
            pid,
            child: Mutex::new(child),
            io: Mutex::new(Some(io)),
            start: Instant::now(),
        }))
    }

    fn network(&self, _policy: &NetPolicy) -> Result<Arc<dyn NetHandle>, HostError> {
        Err(HostError::Net("no network in the test host".into()))
    }

    fn secret(&self, name: &str) -> Result<SecretHandle, HostError> {
        Err(HostError::UnknownSecret(name.to_owned()))
    }

    async fn ask_user(&self, _req: AskUserRequest) -> Result<UserAnswer, HostError> {
        Err(HostError::NoUser("no user in the test host".into()))
    }
}

struct Io {
    stdin: Option<ChildStdin>,
    stdout: ChildStdout,
    stderr: ChildStderr,
    input: Option<Vec<u8>>,
}

/// A `ChildProcess` over `tokio::process::Child`: `wait` feeds stdin and collects both pipes;
/// `terminate` is SIGTERM, poll for the reap up to `grace`, then SIGKILL.
pub struct TokioChild {
    pid: u32,
    child: Mutex<Child>,
    io: Mutex<Option<Io>>,
    start: Instant,
}

impl TokioChild {
    async fn reaped(&self) -> bool {
        match self.child.try_lock() {
            Ok(mut c) => !matches!(c.try_wait(), Ok(None)),
            Err(_) => false,
        }
    }

    async fn wait_reaped(&self, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            if self.reaped().await {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        self.reaped().await
    }
}

#[async_trait]
impl ChildProcess for TokioChild {
    fn pid(&self) -> Option<u32> {
        Some(self.pid)
    }

    async fn wait(&self) -> Result<ProcessOutput, HostError> {
        let io = self.io.lock().await.take();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        if let Some(Io {
            stdin,
            stdout: mut out,
            stderr: mut err,
            input,
        }) = io
        {
            let feed = async move {
                if let Some(mut s) = stdin {
                    if let Some(bytes) = input {
                        let _ = s.write_all(&bytes).await;
                    }
                    let _ = s.shutdown().await;
                }
            };
            let _ = tokio::join!(
                feed,
                out.read_to_end(&mut stdout),
                err.read_to_end(&mut stderr)
            );
        }
        let status = self
            .child
            .lock()
            .await
            .wait()
            .await
            .map_err(|e| HostError::Io(e.to_string()))?;
        Ok(ProcessOutput {
            exit_code: status.code(),
            signal: status.signal(),
            stdout,
            stderr,
            timed_out: false,
            duration: self.start.elapsed(),
        })
    }

    async fn terminate(&self, grace: Duration) -> Result<(), HostError> {
        if self.reaped().await {
            return Ok(());
        }
        let pid = Pid::from_raw(self.pid as i32);
        let _ = kill(pid, Signal::SIGTERM);
        if self.wait_reaped(grace).await {
            return Ok(());
        }
        let _ = kill(pid, Signal::SIGKILL);
        self.wait_reaped(Duration::from_secs(10)).await;
        Ok(())
    }
}

/// A `TaskRegistrar` that stores every registered future.
#[derive(Default)]
pub struct StoreRegistrar {
    pub futures: StdMutex<Vec<(TaskId, BoxFuture<'static, TaskOutcome>)>>,
}

impl StoreRegistrar {
    pub fn take(&self) -> Vec<(TaskId, BoxFuture<'static, TaskOutcome>)> {
        std::mem::take(&mut *self.futures.lock().unwrap_or_else(PoisonError::into_inner))
    }

    pub fn len(&self) -> usize {
        self.futures
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

impl TaskRegistrar for StoreRegistrar {
    fn watch(&self, id: TaskId, done: BoxFuture<'static, TaskOutcome>) -> Result<(), ToolError> {
        self.futures
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((id, done));
        Ok(())
    }
}

/// A backend that refuses every launch (for in-process tool tests that need a `ToolContext`).
pub struct StubBackend;

#[async_trait]
impl SandboxBackend for StubBackend {
    fn name(&self) -> &'static str {
        "stub"
    }
    async fn launch_stateless(
        &self,
        _: &SandboxPolicy,
        _: Command,
        _: CancellationToken,
    ) -> Result<ProcessOutput, SandboxError> {
        Err(SandboxError::Launch("stub backend".into()))
    }
    async fn launch_session(
        &self,
        _: &SandboxPolicy,
        _: Command,
    ) -> Result<Box<dyn SessionProcess>, SandboxError> {
        Err(SandboxError::Launch("stub backend".into()))
    }
}

/// A policy with the given mounts, `timeout`, no network, the default allowlist (`PATH`, `HOME`,
/// `LANG`), and no program restriction.
pub fn policy(mounts: &[(&Path, FsMode)], timeout: Duration) -> SandboxPolicy {
    let mut ms: Vec<Mount> = mounts
        .iter()
        .map(|(p, m)| Mount {
            path: p.to_path_buf(),
            mode: *m,
        })
        .collect();
    ms.sort_by(|a, b| a.path.cmp(&b.path));
    SandboxPolicy {
        mounts: ms,
        scratch_tmpfs_mb: 64,
        net: NetPolicy {
            enabled: false,
            allow: NetAllow::Hosts(BTreeSet::new()),
        },
        timeout,
        env_allowlist: ["PATH", "HOME", "LANG"]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        programs: BTreeSet::new(),
    }
}

/// A canonicalized temporary directory (so `FsPolicy::check` on canonical paths matches).
pub fn workdir() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().canonicalize().expect("canonicalize");
    (dir, path)
}

/// True when `/proc/<pid>` is gone or the process is a zombie.
pub fn process_gone(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/status")) {
        Err(_) => true,
        Ok(s) => s
            .lines()
            .any(|l| l.starts_with("State:") && l.contains('Z')),
    }
}

/// Poll until `process_gone(pid)` or `limit` elapses.
pub async fn wait_gone(pid: u32, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if process_gone(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    process_gone(pid)
}

/// Owned parts of a `ToolContext`.
pub struct Fixture {
    pub host: FsHost,
    pub cancel: CancellationToken,
    pub session_id: SessionId,
    pub turn: u64,
    pub tool_use_id: String,
    pub policy: SandboxPolicy,
    pub sandbox: Box<dyn SandboxBackend>,
    pub artifacts: NoopArtifactStore,
    pub registrar: StoreRegistrar,
}

impl Fixture {
    pub fn new(policy: SandboxPolicy, sandbox: Box<dyn SandboxBackend>) -> Fixture {
        Fixture {
            host: FsHost,
            cancel: CancellationToken::new(),
            session_id: SessionId("s1".into()),
            turn: 3,
            tool_use_id: "tu1".into(),
            policy,
            sandbox,
            artifacts: NoopArtifactStore,
            registrar: StoreRegistrar::default(),
        }
    }

    pub fn ctx<'a>(&'a self, session: Option<&'a dyn SessionProcess>) -> ToolContext<'a> {
        ToolContext::new(
            &self.host,
            self.cancel.clone(),
            &self.session_id,
            self.turn,
            &self.tool_use_id,
            &self.policy,
            self.sandbox.as_ref(),
            &self.artifacts,
            session,
            &self.registrar,
        )
    }
}

/// Environment map helper for the pure tests.
pub fn env_map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}
